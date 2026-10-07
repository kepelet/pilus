//! The window's handle on Remote access.
//!
//! Remote access is a background service (`pilus-agent service`), not a child
//! of this process: it keeps running when the window closes, restarts at login,
//! and stays reachable from a phone until the user switches it off here. So
//! this module owns no process. It keeps one cached [`RemoteAccess`] that a
//! poller thread refreshes from the service's admin socket, and it runs every
//! install, removal, code mint and revocation on a short-lived thread, so the
//! GUI thread never waits on a process or a socket.
//!
//! The toggle in Settings is the user's intent (`Settings.connector`); the
//! cached state is what the service reports. Switching on is optimistic:
//! `Starting` until the admin socket answers.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pilus_client::agent_service::{self, DeviceLevel, PairingCode, ServiceStatus};

/// How often the poller asks the service how it is.
const POLL: Duration = Duration::from_secs(2);
/// How long a switched-on service may take to answer before the window says
/// so instead of showing "Starting" forever.
const START_PATIENCE: Duration = Duration::from_secs(20);

/// Where Remote access stands.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Phase {
    /// Switched off.
    #[default]
    Off,
    /// Switched on; the service has not answered yet.
    Starting,
    /// The service is up and answering.
    Running,
}

/// A snapshot of Remote access for painting.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RemoteAccess {
    /// Off, starting, or running.
    pub phase: Phase,
    /// The running service's report.
    pub status: Option<ServiceStatus>,
    /// The pairing code on screen, until it expires.
    pub code: Option<PairingCode>,
    /// The last failure, for the user to read.
    pub error: Option<String>,
    /// A code is being minted.
    pub minting: bool,
}

impl RemoteAccess {
    /// Whether Remote access is switched on, running or not yet.
    pub fn on(&self) -> bool {
        self.phase != Phase::Off
    }
}

#[derive(Default)]
struct Shared {
    view: RemoteAccess,
    wanted: bool,
    since: Option<Instant>,
}

static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
static POLLER: OnceLock<()> = OnceLock::new();

fn lock() -> MutexGuard<'static, Shared> {
    SHARED
        .get_or_init(|| Mutex::new(Shared::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The cached state. Cheap; call it every frame.
pub fn snapshot() -> RemoteAccess {
    lock().view.clone()
}

/// Make the service match the saved choice, at launch.
///
/// Installing is idempotent and leaves a healthy running service alone, so this
/// is safe on every start; it also moves a service installed from an old
/// build or app location onto the current binary.
pub fn ensure(enabled: bool) {
    if enabled {
        enable();
    } else {
        lock().wanted = false;
    }
}

/// Switch Remote access on: install and start the service.
pub fn enable() {
    {
        let mut shared = lock();
        shared.wanted = true;
        shared.since = Some(Instant::now());
        shared.view.phase = Phase::Starting;
        shared.view.error = None;
    }
    start_poller();
    thread::spawn(|| {
        if let Err(error) = agent_service::install() {
            let mut shared = lock();
            shared.wanted = false;
            shared.view = RemoteAccess {
                error: Some(error.to_string()),
                ..RemoteAccess::default()
            };
        }
    });
}

/// Switch Remote access off: stop and remove the service.
pub fn disable() {
    {
        let mut shared = lock();
        shared.wanted = false;
        shared.view = RemoteAccess::default();
    }
    thread::spawn(|| {
        if let Err(error) = agent_service::uninstall() {
            lock().view.error = Some(error.to_string());
        }
    });
}

/// Mint a pairing code that enrolls a device at `level`, and show it.
pub fn mint_code(level: DeviceLevel) {
    {
        let mut shared = lock();
        if shared.view.minting {
            return;
        }
        shared.view.minting = true;
        shared.view.error = None;
    }
    thread::spawn(move || {
        let outcome = agent_service::new_code(level);
        let mut shared = lock();
        shared.view.minting = false;
        match outcome {
            Ok(code) => shared.view.code = Some(code),
            Err(error) => shared.view.error = Some(error.to_string()),
        }
    });
}

/// Hide the pairing code on screen. It stays valid until it expires or is
/// replaced; this only stops showing it.
pub fn dismiss_code() {
    lock().view.code = None;
}

/// Lower an enrolled device's level. Raising one takes a new code.
pub fn lower(endpoint_id: String, level: DeviceLevel) {
    thread::spawn(
        move || match agent_service::lower_level(&endpoint_id, level) {
            Ok(()) => {
                if let Some(status) = agent_service::status() {
                    lock().view.status = Some(status);
                }
            }
            Err(error) => lock().view.error = Some(error.to_string()),
        },
    );
}

/// Remove an enrolled device.
pub fn revoke(endpoint_id: String) {
    thread::spawn(move || {
        match agent_service::revoke(&endpoint_id) {
            Ok(_) => {
                // Show the removal now rather than at the next poll.
                if let Some(status) = agent_service::status() {
                    lock().view.status = Some(status);
                }
            }
            Err(error) => lock().view.error = Some(error.to_string()),
        }
    });
}

/// Show `view` as if the service had reported it, for the visual-test shots.
/// The poller is never started, so nothing overwrites it.
#[cfg(feature = "visual-test")]
pub fn preview(view: RemoteAccess) {
    let mut shared = lock();
    shared.wanted = false;
    shared.view = view;
}

fn start_poller() {
    POLLER.get_or_init(|| {
        thread::Builder::new()
            .name("pilus-remote-access".to_string())
            .spawn(poll_forever)
            .ok();
    });
}

fn poll_forever() {
    loop {
        thread::sleep(POLL);
        if !lock().wanted {
            continue;
        }
        let status = agent_service::status();
        let now = now_unix();
        let mut shared = lock();
        if !shared.wanted {
            continue;
        }
        match status {
            Some(status) => {
                shared.view.phase = Phase::Running;
                shared.view.status = Some(status);
                if shared.view.error.as_deref().is_some_and(is_start_failure) {
                    shared.view.error = None;
                }
            }
            None => {
                shared.view.status = None;
                shared.view.phase = Phase::Starting;
                if shared.since.is_some_and(|at| at.elapsed() > START_PATIENCE)
                    && shared.view.error.is_none()
                {
                    shared.view.error = Some(format!(
                        "The connector did not start. See {}",
                        agent_service::log_path().display()
                    ));
                }
            }
        }
        // A code the service would now refuse is not worth showing.
        if shared
            .view
            .code
            .as_ref()
            .is_some_and(|code| code.expires_at <= now)
        {
            shared.view.code = None;
        }
    }
}

fn is_start_failure(error: &str) -> bool {
    error.starts_with("The connector did not start")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}
