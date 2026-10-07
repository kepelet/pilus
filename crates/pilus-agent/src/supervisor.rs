//! Keeps the Luvus gateway alive for as long as the connector runs.
//!
//! A gateway is a child process whose delegated token dies with it. A foreground
//! connector could simply exit when it did; a background service has to ride
//! out the daemon restarting, the Mac waking from sleep, or `luvus` not being
//! up yet at login. The supervisor starts the gateway, pairs, publishes the
//! token to [`AuthState`] (devices fetch it on demand, so none re-enrolls), then
//! waits for the child to exit and does it again with capped backoff.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pilus_client::UhpClient;

use crate::enroll::AuthState;
use crate::{AccessOptions, Gateway};

/// The longest wait between start attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How often a failed start may try to bring the Luvus daemon itself up.
const SERVER_START_EVERY: Duration = Duration::from_secs(60);

/// What the rest of the process may know about the gateway right now.
///
/// The iroh handlers and the hub read it per request, so a gateway restart is
/// picked up by the next stream with no handler rebuilt.
#[derive(Debug, Default)]
pub struct GatewayLink {
    state: Mutex<LinkState>,
}

#[derive(Debug, Default)]
struct LinkState {
    address: Option<String>,
    allowed: Vec<String>,
    error: Option<String>,
}

impl GatewayLink {
    /// A link that is always up at `address`, for callers that run their own
    /// gateway (the tests, and the plain TCP forwarder's iroh counterpart).
    pub fn fixed(address: impl Into<String>) -> Arc<Self> {
        let link = Self::default();
        lock(&link.state).address = Some(address.into());
        Arc::new(link)
    }

    /// A link that starts down; the supervisor brings it up.
    pub fn down() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The gateway's loopback address, while it is up.
    pub fn address(&self) -> Option<String> {
        lock(&self.state).address.clone()
    }

    /// Whether a gateway is currently reachable.
    pub fn is_up(&self) -> bool {
        lock(&self.state).address.is_some()
    }

    /// The methods the gateway's delegated token may call. Empty when unknown.
    pub fn allowed(&self) -> Vec<String> {
        lock(&self.state).allowed.clone()
    }

    /// Why the gateway is down, for the window to show.
    pub fn error(&self) -> Option<String> {
        lock(&self.state).error.clone()
    }

    #[cfg(test)]
    pub(crate) fn set_allowed_for_test(&self, allowed: Vec<String>) {
        lock(&self.state).allowed = allowed;
    }

    fn set_up(&self, address: String, allowed: Vec<String>) {
        let mut state = lock(&self.state);
        state.address = Some(address);
        state.allowed = allowed;
        state.error = None;
    }

    fn set_down(&self, error: String) {
        let mut state = lock(&self.state);
        state.address = None;
        state.error = Some(error);
    }
}

/// Run the supervision loop on the calling thread until `stop` is set.
///
/// `binary` pins the `luvus` path; without it the path is rediscovered on every
/// attempt, because a Luvus update can move the versioned binary a service was
/// installed against.
pub fn supervise(
    binary: Option<PathBuf>,
    options: AccessOptions,
    auth: Arc<AuthState>,
    link: Arc<GatewayLink>,
    stop: Arc<AtomicBool>,
) {
    let mut delay = Duration::from_secs(1);
    let mut last_server_start: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        let Some(path) = binary
            .clone()
            .filter(|path| path.is_file())
            .or_else(pilus_client::luvus_binary)
        else {
            link.set_down("could not find the `luvus` binary".to_string());
            nap(&stop, delay);
            delay = (delay * 2).min(MAX_BACKOFF);
            continue;
        };
        match start(&path, &options) {
            Ok((mut gateway, token, allowed)) => {
                let address = gateway.descriptor().address.clone();
                auth.set_token(token);
                link.set_up(address, allowed);
                eprintln!("pilus-agent: gateway is up");
                delay = Duration::from_secs(1);
                while !stop.load(Ordering::Relaxed) && gateway.is_running() {
                    nap(&stop, Duration::from_millis(500));
                }
                link.set_down("the gateway stopped; starting it again".to_string());
                eprintln!("pilus-agent: gateway stopped");
                // `gateway` drops here, which kills and reaps the child.
            }
            Err(error) => {
                link.set_down(error.to_string());
                eprintln!("pilus-agent: gateway is down: {error}");
                if last_server_start.is_none_or(|at| at.elapsed() >= SERVER_START_EVERY) {
                    last_server_start = Some(Instant::now());
                    ensure_server(&path, options.session.as_deref());
                }
                nap(&stop, delay);
                delay = (delay * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Start the gateway, pair once, and read what the token may call.
fn start(
    binary: &std::path::Path,
    options: &AccessOptions,
) -> crate::Result<(Gateway, String, Vec<String>)> {
    let gateway = Gateway::start(binary, options)?;
    let token = gateway.pair()?;
    let allowed = UhpClient::connect_tcp(gateway.descriptor().address.clone())
        .with_auth(token.clone())
        .capabilities()
        .map(|capabilities| capabilities.allowed_methods)
        .unwrap_or_default();
    Ok((gateway, token, allowed))
}

/// Bring the Luvus daemon up when it is not answering.
///
/// Remote access is only useful if there is a daemon to reach, and after a
/// reboot nothing has started one yet. This never restarts a running daemon.
fn ensure_server(binary: &std::path::Path, session: Option<&str>) {
    let luvus = |args: &[&str]| {
        let mut command = Command::new(binary);
        if let Some(session) = session {
            command.arg("--session").arg(session);
        }
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if !luvus(&["ping"]) {
        eprintln!("pilus-agent: the Luvus server is not running; starting it");
        luvus(&["server", "start"]);
    }
}

/// Sleep for `total`, waking early when `stop` is set.
fn nap(stop: &AtomicBool, total: Duration) {
    let deadline = Instant::now() + total;
    while !stop.load(Ordering::Relaxed) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_millis(200)));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
