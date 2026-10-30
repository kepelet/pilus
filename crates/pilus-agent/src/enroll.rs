//! Device enrollment and the control channel.
//!
//! A gateway's pairing code is one-use and short-lived, so it cannot be the
//! thing a phone carries. The connector takes over that job: it pairs with the
//! gateway once, keeps the delegated token, and hands it to devices it has
//! enrolled. A device proves itself once with a short enrollment code shown on
//! the host; afterwards its iroh `EndpointId` is the authorization, persisted so
//! it survives restarts.
//!
//! The control protocol is one JSON request per bidirectional stream on
//! [`CONTROL_ALPN`]: `{"type":"enroll","code":…}` adds the peer and rotates the
//! code, `{"type":"token"}` returns the cached token for an enrolled peer, and
//! `devices`/`revoke` let an authorized peer list and remove enrollments. UHP
//! itself still flows verbatim on [`crate::p2p::ALPN`], authenticated by the
//! token the device sends as `auth`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use iroh::SecretKey;
use iroh::endpoint::{Connection, RecvStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use serde_json::{Value, json};

use crate::{Error, Result};

/// The ALPN for the connector's enrollment and token channel.
pub const CONTROL_ALPN: &[u8] = b"luvus/agent/1";

/// How long an enrollment code stays valid.
pub const ENROLLMENT_TTL: u64 = 600;

/// How much a paired device may do.
///
/// The names follow Luvus's own: read-only is the default posture and control
/// is something the user asks for. `Operate` is above anything the gateway's
/// token allows, so it exists only here: the hub itself runs the extra methods.
/// Raising a level always takes a fresh enrollment code, which exists only on
/// the host, so a device can never promote itself or be promoted from a phone.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum Level {
    /// Reads only: lists, status, transcripts.
    View,
    /// What the gateway's control token allows: prompt and interrupt agents,
    /// retry tasks, focus things. The level every device had before levels.
    #[default]
    Control,
    /// Control, plus starting work: add and start tasks, start agents, open
    /// workspaces. Remote code execution by design, so it is opt-in per device.
    Operate,
}

impl Level {
    /// The wire and file spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Control => "control",
            Self::Operate => "operate",
        }
    }

    /// Parse the wire spelling.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "view" => Some(Self::View),
            "control" => Some(Self::Control),
            "operate" => Some(Self::Operate),
            _ => None,
        }
    }
}

/// One enrolled device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Device {
    /// The device's iroh `EndpointId`.
    pub endpoint_id: String,
    /// A human label supplied at enrollment.
    pub label: String,
    /// When it enrolled, as a Unix timestamp.
    pub enrolled_at: u64,
    /// What it may do.
    pub level: Level,
}

/// The persisted set of enrolled devices.
#[derive(Debug)]
pub struct DeviceStore {
    path: PathBuf,
    devices: Vec<Device>,
}

impl DeviceStore {
    /// Load the store at `path`, or start empty when it does not exist.
    ///
    /// A file that exists but does not decode is a [`Error::DeviceStore`], not
    /// an empty store: silently dropping enrolled devices would quietly reopen
    /// enrollment for endpoints that were revoked.
    pub fn load(path: PathBuf) -> Result<Self> {
        let devices = match std::fs::read(&path) {
            Ok(bytes) => parse_devices(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(Error::Io(error)),
        };
        Ok(Self { path, devices })
    }

    /// Whether `endpoint_id` may reach the gateway.
    pub fn is_allowed(&self, endpoint_id: &str) -> bool {
        self.devices
            .iter()
            .any(|device| device.endpoint_id == endpoint_id)
    }

    /// The enrolled devices.
    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    /// Add a device without persisting, for a static `--allow` seed.
    pub fn allow(&mut self, endpoint_id: &str) {
        if !endpoint_id.is_empty() && !self.is_allowed(endpoint_id) {
            self.devices.push(Device {
                endpoint_id: endpoint_id.to_string(),
                label: String::new(),
                enrolled_at: now(),
                level: Level::Control,
            });
        }
    }

    /// Persist an enrollment. Returns whether the device was newly added.
    ///
    /// The candidate list is saved before it is committed, so a failed write
    /// leaves the in-memory authorization unchanged.
    pub fn enroll(&mut self, endpoint_id: &str, label: &str, level: Level) -> Result<bool> {
        if self.is_allowed(endpoint_id) {
            return Ok(false);
        }
        let mut candidate = self.devices.clone();
        candidate.push(Device {
            endpoint_id: endpoint_id.to_string(),
            label: label.to_string(),
            enrolled_at: now(),
            level,
        });
        save(&self.path, &candidate)?;
        self.devices = candidate;
        Ok(true)
    }

    /// The level of an enrolled device.
    pub fn level_of(&self, endpoint_id: &str) -> Option<Level> {
        self.devices
            .iter()
            .find(|device| device.endpoint_id == endpoint_id)
            .map(|device| device.level)
    }

    /// Persist a new level for an enrolled device. Returns whether the device
    /// was present and its level changed.
    pub fn set_level(&mut self, endpoint_id: &str, level: Level) -> Result<bool> {
        if self
            .level_of(endpoint_id)
            .is_none_or(|current| current == level)
        {
            return Ok(false);
        }
        let mut candidate = self.devices.clone();
        for device in &mut candidate {
            if device.endpoint_id == endpoint_id {
                device.level = level;
            }
        }
        save(&self.path, &candidate)?;
        self.devices = candidate;
        Ok(true)
    }

    /// Persist a revocation. Returns whether the device was present.
    pub fn revoke(&mut self, endpoint_id: &str) -> Result<bool> {
        if !self.is_allowed(endpoint_id) {
            return Ok(false);
        }
        let candidate: Vec<Device> = self
            .devices
            .iter()
            .filter(|device| device.endpoint_id != endpoint_id)
            .cloned()
            .collect();
        save(&self.path, &candidate)?;
        self.devices = candidate;
        Ok(true)
    }

    /// The number of enrolled devices.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Whether no device is enrolled.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

/// Write `devices` to `path` atomically (see [`write_atomic`]), so a crash
/// mid-write can never leave a truncated store that would re-open enrollment.
fn save(path: &std::path::Path, devices: &[Device]) -> Result<()> {
    let entries: Vec<Value> = devices
        .iter()
        .map(|device| {
            json!({
                "endpoint_id": device.endpoint_id,
                "label": device.label,
                "enrolled_at": device.enrolled_at,
                "level": device.level.as_str(),
            })
        })
        .collect();
    let payload = json!({ "devices": entries }).to_string();
    write_atomic(path, payload.as_bytes())
}

/// Replace `path` with `payload`: a same-directory temporary file, owner-only,
/// fsynced and renamed over the destination.
pub(crate) fn write_atomic(path: &std::path::Path, payload: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "devices.json".to_string());
    let temp = parent.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        SAVE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(payload)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// The short-lived code that admits a new device.
#[derive(Clone, Debug)]
pub struct Enrollment {
    code: String,
    expires_at: u64,
    level: Level,
}

impl Enrollment {
    /// Mint a code valid for `ttl` seconds that enrolls at control level.
    pub fn generate(ttl: u64) -> Self {
        Self::generate_at(ttl, Level::Control)
    }

    /// Mint a code valid for `ttl` seconds that enrolls at `level`.
    pub fn generate_at(ttl: u64, level: Level) -> Self {
        Self {
            code: enrollment_code(),
            expires_at: now() + ttl,
            level,
        }
    }

    /// The level a device enrolled with this code gets.
    pub fn level(&self) -> Level {
        self.level
    }

    /// The code to show on the host.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// When the code stops being accepted.
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    /// Whether `code` is the right code and still in its window.
    pub fn accepts(&self, code: &str, now: u64) -> bool {
        !code.is_empty() && code == self.code && now < self.expires_at
    }
}

/// The connector's delegated authorization state.
#[derive(Debug)]
pub struct AuthState {
    /// The gateway's delegated token. It changes whenever the supervisor
    /// restarts the gateway, so it sits behind a lock rather than being fixed
    /// for the process.
    token: Mutex<String>,
    devices: Mutex<DeviceStore>,
    enrollment: Mutex<Option<Enrollment>>,
    enrollment_ttl: u64,
    /// Non-empty `--allow` IDs; when this list is non-empty the run is in
    /// strict mode and it is the whole authorization set.
    allowed: Vec<String>,
}

/// The internal result of one enroll request.
enum EnrollOutcome {
    /// Strict `--allow` mode disabled enrollment entirely.
    Disabled,
    /// The code was wrong or out of its window.
    Invalid,
    /// The endpoint is already enrolled; nothing rotated.
    AlreadyEnrolled,
    /// A new device enrolled; carries the freshly minted replacement code for
    /// the host to print — never for the control response.
    Enrolled { code: String, expires_at: u64 },
}

impl AuthState {
    /// Build the state for one connector run.
    ///
    /// A non-empty `allow` makes the run strict: the persisted store is never
    /// read, only those IDs may reach the gateway, and enrollment is disabled.
    /// An empty effective `allow` uses the persisted devices and enables
    /// enrollment.
    pub fn load(token: String, devices_path: PathBuf, allow: &[String], ttl: u64) -> Result<Self> {
        let allowed: Vec<String> = allow
            .iter()
            .filter(|endpoint_id| !endpoint_id.is_empty())
            .cloned()
            .collect();
        let strict = !allowed.is_empty();
        let mut devices = if strict {
            DeviceStore {
                path: devices_path,
                devices: Vec::new(),
            }
        } else {
            DeviceStore::load(devices_path)?
        };
        for endpoint_id in &allowed {
            devices.allow(endpoint_id);
        }
        Ok(Self {
            token: Mutex::new(token),
            devices: Mutex::new(devices),
            enrollment: Mutex::new((!strict).then(|| Enrollment::generate(ttl))),
            enrollment_ttl: ttl,
            allowed,
        })
    }

    /// Whether `endpoint_id` may reach the gateway.
    pub fn is_allowed(&self, endpoint_id: &str) -> bool {
        if self.strict() {
            self.allowed.iter().any(|id| id == endpoint_id)
        } else {
            lock(&self.devices).is_allowed(endpoint_id)
        }
    }

    /// The enrollment code and its expiry, or `None` when enrollment is
    /// disabled by `--allow`.
    pub fn enrollment(&self) -> Option<(String, u64)> {
        lock(&self.enrollment)
            .as_ref()
            .map(|enrollment| (enrollment.code().to_string(), enrollment.expires_at()))
    }

    /// The gateway token an enrolled device is handed.
    pub fn token(&self) -> String {
        lock(&self.token).clone()
    }

    /// Replace the token after the gateway restarts and pairs again.
    ///
    /// Enrolled devices fetch the token on demand, so none of them has to
    /// re-enroll when this changes.
    pub fn set_token(&self, token: String) {
        *lock(&self.token) = token;
    }

    /// Mint a fresh enrollment code at `level`, replacing whatever was current.
    ///
    /// This is how a long-running service hands out a code without a restart.
    /// Returns `None` in strict `--allow` mode, where enrollment is disabled.
    pub fn mint_enrollment(&self, level: Level) -> Option<(String, u64)> {
        if self.strict() {
            return None;
        }
        let next = Enrollment::generate_at(self.enrollment_ttl, level);
        let minted = (next.code().to_string(), next.expires_at());
        *lock(&self.enrollment) = Some(next);
        Some(minted)
    }

    /// Remove an enrolled device. `None` in strict mode, where the authorized
    /// set is process configuration and cannot be edited.
    pub fn revoke_device(&self, endpoint_id: &str) -> Option<Result<bool>> {
        if self.strict() {
            return None;
        }
        Some(self.revoke(endpoint_id))
    }

    /// Whether enrollment and revocation are pinned off by `--allow`.
    pub fn is_strict(&self) -> bool {
        self.strict()
    }

    /// The devices this run considers authorized.
    pub fn devices(&self) -> Vec<Device> {
        lock(&self.devices).devices().to_vec()
    }

    /// The number of enrolled devices.
    pub fn device_count(&self) -> usize {
        lock(&self.devices).len()
    }

    /// The level of an authorized device. Strict `--allow` devices are control.
    pub fn level_of(&self, endpoint_id: &str) -> Option<Level> {
        if self.strict() {
            return self.is_allowed(endpoint_id).then_some(Level::Control);
        }
        lock(&self.devices).level_of(endpoint_id)
    }

    /// A short name for a device, for the audit log.
    pub fn device_name(&self, endpoint_id: &str) -> String {
        let devices = lock(&self.devices);
        let label = devices
            .devices()
            .iter()
            .find(|device| device.endpoint_id == endpoint_id)
            .map(|device| device.label.as_str())
            .filter(|label| !label.is_empty());
        let short: String = endpoint_id.chars().take(8).collect();
        match label {
            Some(label) => format!("{label} ({short})"),
            None => short,
        }
    }

    /// Lower a device's level. Raising is refused: it takes a fresh code, which
    /// only someone at the host can mint. `None` in strict mode.
    pub fn lower_level(&self, endpoint_id: &str, level: Level) -> Option<Result<bool>> {
        if self.strict() {
            return None;
        }
        let mut devices = lock(&self.devices);
        match devices.level_of(endpoint_id) {
            Some(current) if level < current => Some(devices.set_level(endpoint_id, level)),
            _ => Some(Ok(false)),
        }
    }

    /// Whether the `--allow` list pins the authorized set for this process.
    fn strict(&self) -> bool {
        !self.allowed.is_empty()
    }

    /// Atomically validate `code`, persist a new device, and rotate the code.
    ///
    /// Holding the enrollment lock across validation, persistence, and
    /// replacement keeps two racing enroll requests from spending one code or
    /// invalidating a code a peer is mid-flight with.
    fn try_enroll(&self, endpoint_id: &str, code: &str, label: &str) -> Result<EnrollOutcome> {
        let mut enrollment = lock(&self.enrollment);
        let Some(current) = enrollment.as_ref() else {
            return Ok(EnrollOutcome::Disabled);
        };
        let valid = current.accepts(code, now());
        if self.is_allowed(endpoint_id) {
            // A device that is already enrolled and presents no (or a dead) code
            // is simply re-pairing, and nothing changes. A *live* code is how a
            // level is raised, so it re-levels the device and is spent.
            let wanted = current.level();
            let changed = valid
                && lock(&self.devices)
                    .level_of(endpoint_id)
                    .is_some_and(|level| level != wanted)
                && lock(&self.devices).set_level(endpoint_id, wanted)?;
            if !changed {
                return Ok(EnrollOutcome::AlreadyEnrolled);
            }
        } else {
            if !valid {
                return Ok(EnrollOutcome::Invalid);
            }
            let level = current.level();
            if !lock(&self.devices).enroll(endpoint_id, label, level)? {
                return Ok(EnrollOutcome::AlreadyEnrolled);
            }
        }
        let next = Enrollment::generate(self.enrollment_ttl);
        let outcome = EnrollOutcome::Enrolled {
            code: next.code().to_string(),
            expires_at: next.expires_at(),
        };
        *enrollment = Some(next);
        Ok(outcome)
    }

    /// Persist the removal of `endpoint_id`. Callers must refuse this in
    /// strict mode, where the authorized set is process configuration.
    fn revoke(&self, endpoint_id: &str) -> Result<bool> {
        lock(&self.devices).revoke(endpoint_id)
    }
}

/// The `CONTROL_ALPN` handler.
#[derive(Clone, Debug)]
pub struct Control {
    auth: Arc<AuthState>,
    /// Whether a rotated enrollment code is printed. A background service
    /// writes stdout to a log file, which is no place for a code.
    announce: bool,
}

impl Control {
    /// Wrap the shared authorization state.
    pub fn new(auth: Arc<AuthState>) -> Self {
        Self {
            auth,
            announce: true,
        }
    }

    /// Like [`Control::new`], but never prints an enrollment code.
    pub fn quiet(auth: Arc<AuthState>) -> Self {
        Self {
            auth,
            announce: false,
        }
    }

    fn dispatch(&self, remote: &str, request: &Value) -> String {
        match request.get("type").and_then(Value::as_str) {
            Some("enroll") => {
                let code = request
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let label = request
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match self.auth.try_enroll(remote, code, label) {
                    Ok(EnrollOutcome::Disabled) => error_json("enrollment is disabled by --allow"),
                    Ok(EnrollOutcome::Invalid) => {
                        error_json("the enrollment code is invalid or expired")
                    }
                    Ok(EnrollOutcome::AlreadyEnrolled) => enrolled_json(remote),
                    Ok(EnrollOutcome::Enrolled { code, expires_at }) => {
                        if self.announce {
                            println!("enroll:      {code}");
                            eprintln!(
                                "pilus-agent: the enrollment code expires at unix {expires_at}"
                            );
                        }
                        enrolled_json(remote)
                    }
                    Err(error) => error_json(&error.to_string()),
                }
            }
            Some("token") => {
                if !self.auth.is_allowed(remote) {
                    return error_json("this device is not enrolled");
                }
                let token = self.auth.token();
                if token.is_empty() {
                    return error_json("the host's Luvus gateway is not running yet");
                }
                json!({ "type": "token", "token": token }).to_string()
            }
            Some("devices") => {
                if !self.auth.is_allowed(remote) {
                    return error_json("this device is not enrolled");
                }
                let devices: Vec<Value> = self
                    .auth
                    .devices()
                    .iter()
                    .map(|device| {
                        json!({
                            "endpoint_id": device.endpoint_id,
                            "label": device.label,
                            "enrolled_at": device.enrolled_at,
                            "level": device.level.as_str(),
                        })
                    })
                    .collect();
                json!({ "type": "devices", "devices": devices }).to_string()
            }
            Some("revoke") => {
                if !self.auth.is_allowed(remote) {
                    return error_json("this device is not enrolled");
                }
                if self.auth.strict() {
                    return error_json("revocation is disabled by --allow");
                }
                let target = request
                    .get("endpoint_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match self.auth.revoke(target) {
                    Ok(removed) => json!({
                        "type": "revoked",
                        "endpoint_id": target,
                        "removed": removed,
                    })
                    .to_string(),
                    Err(error) => error_json(&error.to_string()),
                }
            }
            Some("status") => {
                if self.auth.strict() && !self.auth.is_allowed(remote) {
                    return error_json("this device is not enrolled");
                }
                json!({
                    "type": "status",
                    "devices": self.auth.device_count(),
                })
                .to_string()
            }
            _ => error_json("unknown request"),
        }
    }
}

impl ProtocolHandler for Control {
    async fn accept(&self, connection: Connection) -> std::result::Result<(), AcceptError> {
        let remote = connection.remote_id().to_string();
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let request = read_line(&mut recv).await.unwrap_or_default();
            let value: Value = serde_json::from_str(&request).unwrap_or(Value::Null);
            let response = self.dispatch(&remote, &value);
            if send.write_all(response.as_bytes()).await.is_err() {
                break;
            }
            let _ = send.write_all(b"\n").await;
            let _ = send.finish();
        }
        Ok(())
    }
}

/// Read one newline-terminated request, bounded so a peer cannot grow memory.
async fn read_line(recv: &mut RecvStream) -> std::io::Result<String> {
    const LIMIT: usize = 64 * 1024;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match recv.read(&mut chunk).await {
            Ok(Some(n)) if n > 0 => {
                buffer.extend_from_slice(&chunk[..n]);
                if buffer.contains(&b'\n') || buffer.len() >= LIMIT {
                    break;
                }
            }
            _ => break,
        }
    }
    Ok(String::from_utf8_lossy(&buffer).trim().to_string())
}

fn parse_devices(bytes: &[u8]) -> Result<Vec<Device>> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|error| Error::DeviceStore(error.to_string()))?;
    let items = value
        .get("devices")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::DeviceStore("the store has no `devices` array".to_string()))?;
    items
        .iter()
        .map(|item| {
            let endpoint_id = item
                .get("endpoint_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Error::DeviceStore("a device entry is missing `endpoint_id`".to_string())
                })?;
            Ok(Device {
                endpoint_id: endpoint_id.to_string(),
                label: item
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                enrolled_at: item.get("enrolled_at").and_then(Value::as_u64).unwrap_or(0),
                // Files written before levels existed hold control devices.
                level: item
                    .get("level")
                    .and_then(Value::as_str)
                    .and_then(Level::parse)
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// A readable, high-entropy enrollment code.
///
/// `SecretKey::generate` is the library's CSPRNG path, so the code is random
/// without adding a dependency; the key itself is discarded.
fn enrollment_code() -> String {
    let bytes = SecretKey::generate().to_bytes();
    let hex: String = bytes[..8]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect();
    format!(
        "{}-{}-{}-{}",
        &hex[0..4],
        &hex[4..8],
        &hex[8..12],
        &hex[12..16]
    )
}

fn enrolled_json(endpoint_id: &str) -> String {
    json!({ "type": "enrolled", "endpoint_id": endpoint_id }).to_string()
}

fn error_json(message: &str) -> String {
    json!({ "type": "error", "message": message }).to_string()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn enrollment_codes_are_unique_and_shaped() {
        let a = enrollment_code();
        let b = enrollment_code();
        assert_ne!(a, b);
        assert_eq!(a.len(), 19, "XXXX-XXXX-XXXX-XXXX");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn enrollment_window_is_enforced() {
        let mut enrollment = Enrollment::generate(60);
        let now = now();
        assert!(enrollment.accepts(enrollment.code(), now));
        assert!(!enrollment.accepts("wrong", now));
        assert!(!enrollment.accepts(enrollment.code(), now + 3600));
        enrollment.expires_at = now - 1;
        assert!(!enrollment.accepts(enrollment.code(), now));
    }

    #[test]
    fn store_persists_and_matches_devices() {
        let dir = std::env::temp_dir().join(format!("pilus-agent-devices-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("devices.json");

        let mut store = DeviceStore::load(path.clone()).expect("load");
        assert!(
            store
                .enroll("device-a", "phone", Level::Control)
                .expect("enroll")
        );
        assert!(
            !store
                .enroll("device-a", "phone", Level::Control)
                .expect("repeat")
        );
        store.allow("device-b");

        let reloaded = DeviceStore::load(path).expect("reload");
        assert!(reloaded.is_allowed("device-a"));
        assert!(!reloaded.is_allowed("device-b"), "allow is in-memory only");
        assert_eq!(reloaded.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn control_dispatch_enrolls_then_tokens() {
        let dir = std::env::temp_dir().join(format!("pilus-agent-ctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let auth = Arc::new(
            AuthState::load("tok".to_string(), dir.join("devices.json"), &[], 60).expect("state"),
        );
        let control = Control::new(Arc::clone(&auth));

        let denied = control.dispatch("peer", &json!({ "type": "token" }));
        assert!(denied.contains("not enrolled"));

        let bad = control.dispatch("peer", &json!({ "type": "enroll", "code": "nope" }));
        assert!(bad.contains("invalid or expired"));

        let (code, _) = auth.enrollment().expect("enrollment enabled");
        let enrolled = control.dispatch("peer", &json!({ "type": "enroll", "code": code }));
        assert!(enrolled.contains("enrolled"));

        let token = control.dispatch("peer", &json!({ "type": "token" }));
        assert!(token.contains("\"token\":\"tok\""));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enrollment_rotates_after_a_new_device() {
        let dir = std::env::temp_dir().join(format!("pilus-agent-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let auth = Arc::new(
            AuthState::load("tok".to_string(), dir.join("devices.json"), &[], 60).expect("state"),
        );
        let control = Control::new(Arc::clone(&auth));

        let (first, _) = auth.enrollment().expect("enabled");
        let enrolled = control.dispatch("phone", &json!({ "type": "enroll", "code": first }));
        assert!(enrolled.contains("\"type\":\"enrolled\""));
        assert!(
            !enrolled.contains("code"),
            "the replacement code must not reach the device: {enrolled}"
        );

        let (second, _) = auth.enrollment().expect("still enabled");
        assert_ne!(first, second, "the code rotates after one new device");

        let stale = control.dispatch("tablet", &json!({ "type": "enroll", "code": first }));
        assert!(
            stale.contains("invalid or expired"),
            "old code spent: {stale}"
        );

        let retry = control.dispatch("phone", &json!({ "type": "enroll", "code": second }));
        assert!(retry.contains("\"type\":\"enrolled\""));
        let (third, _) = auth.enrollment().expect("still enabled");
        assert_eq!(second, third, "an enrolled retry does not rotate");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strict_allow_ignores_persisted_devices() {
        let dir = std::env::temp_dir().join(format!("pilus-agent-strict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("devices.json");
        let mut store = DeviceStore::load(path.clone()).expect("load");
        store
            .enroll("persisted-device", "old phone", Level::Control)
            .expect("enroll");
        drop(store);

        let auth = Arc::new(
            AuthState::load("tok".to_string(), path, &["configured-id".to_string()], 60)
                .expect("state"),
        );
        let control = Control::new(Arc::clone(&auth));

        assert!(auth.is_allowed("configured-id"));
        assert!(
            !auth.is_allowed("persisted-device"),
            "strict --allow ignores the persisted set"
        );
        assert!(auth.enrollment().is_none());

        let enroll = control.dispatch("new-device", &json!({ "type": "enroll", "code": "any" }));
        assert!(enroll.contains("disabled by --allow"), "{enroll}");

        let denied = control.dispatch("persisted-device", &json!({ "type": "token" }));
        assert!(denied.contains("not enrolled"), "{denied}");
        let token = control.dispatch("configured-id", &json!({ "type": "token" }));
        assert!(token.contains("\"token\":\"tok\""), "{token}");

        let revoke = control.dispatch(
            "configured-id",
            &json!({ "type": "revoke", "endpoint_id": "configured-id" }),
        );
        assert!(revoke.contains("disabled by --allow"), "{revoke}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn devices_and_revoke_remove_an_endpoint() {
        let dir = std::env::temp_dir().join(format!("pilus-agent-revoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("devices.json");
        let auth =
            Arc::new(AuthState::load("tok".to_string(), path.clone(), &[], 60).expect("state"));
        let control = Control::new(Arc::clone(&auth));

        let (code, _) = auth.enrollment().expect("enabled");
        control.dispatch("admin", &json!({ "type": "enroll", "code": code }));
        let (code, _) = auth.enrollment().expect("enabled");
        control.dispatch("phone", &json!({ "type": "enroll", "code": code }));

        let listed: Value =
            serde_json::from_str(&control.dispatch("admin", &json!({ "type": "devices" })))
                .expect("devices json");
        assert_eq!(listed["type"], "devices");
        let ids: Vec<&str> = listed["devices"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|device| device["endpoint_id"].as_str())
            .collect();
        assert!(ids.contains(&"admin") && ids.contains(&"phone"), "{ids:?}");

        let denied = control.dispatch(
            "stranger",
            &json!({ "type": "revoke", "endpoint_id": "phone" }),
        );
        assert!(denied.contains("not enrolled"), "{denied}");

        let revoked: Value = serde_json::from_str(&control.dispatch(
            "admin",
            &json!({ "type": "revoke", "endpoint_id": "phone" }),
        ))
        .expect("revoked json");
        assert_eq!(revoked["type"], "revoked");
        assert_eq!(revoked["endpoint_id"], "phone");
        assert_eq!(revoked["removed"], true);

        let token = control.dispatch("phone", &json!({ "type": "token" }));
        assert!(token.contains("not enrolled"), "{token}");

        let reloaded = DeviceStore::load(path).expect("reload");
        assert!(reloaded.is_allowed("admin"));
        assert!(!reloaded.is_allowed("phone"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_save_leaves_authorization_unchanged() {
        let dir = std::env::temp_dir().join(format!("pilus-agent-nosave-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        // A regular file where the store's parent directory belongs makes the
        // save fail on every platform.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"occupied").expect("blocker");
        let path = blocker.join("devices.json");

        let mut store = DeviceStore {
            path,
            devices: Vec::new(),
        };
        assert!(store.enroll("device-a", "phone", Level::Control).is_err());
        assert!(
            !store.is_allowed("device-a"),
            "a failed save must not authorize the candidate"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_store_is_a_typed_error() {
        let dir = std::env::temp_dir().join(format!("pilus-agent-badjson-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("devices.json");
        std::fs::write(&path, b"not json").expect("write");
        assert!(matches!(
            DeviceStore::load(path.clone()),
            Err(Error::DeviceStore(_))
        ));

        std::fs::write(&path, b"{\"devices\":{}}").expect("write");
        assert!(matches!(
            DeviceStore::load(path),
            Err(Error::DeviceStore(_))
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strict_allow_never_reads_the_store() {
        let dir =
            std::env::temp_dir().join(format!("pilus-agent-strict-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("devices.json");
        std::fs::write(&path, b"not json").expect("write malformed");

        let auth = AuthState::load(
            "tok".to_string(),
            path.clone(),
            &["configured-id".to_string()],
            60,
        )
        .expect("strict load ignores the persisted store");
        assert!(auth.is_allowed("configured-id"));
        assert!(!auth.is_allowed("anything-else"));
        assert!(auth.enrollment().is_none());
        assert_eq!(
            std::fs::read(&path).expect("read back"),
            b"not json",
            "strict mode must leave the on-disk store untouched"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_code_carries_the_level_and_the_device_keeps_it() {
        let dir = std::env::temp_dir().join(format!("pilus-agent-level-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("devices.json");
        let auth = AuthState::load("tok".to_string(), path.clone(), &[], 60).expect("state");

        let (code, _) = auth.mint_enrollment(Level::Operate).expect("code");
        let outcome = auth.try_enroll("phone", &code, "iPhone").expect("enroll");
        assert!(matches!(outcome, EnrollOutcome::Enrolled { .. }));
        assert_eq!(auth.level_of("phone"), Some(Level::Operate));

        // The level survives a restart, and a file from before levels reads as control.
        let reloaded = AuthState::load("tok".to_string(), path, &[], 60).expect("reload");
        assert_eq!(reloaded.level_of("phone"), Some(Level::Operate));
        assert_eq!(
            parse_devices(br#"{"devices":[{"endpoint_id":"old"}]}"#).expect("old file")[0].level,
            Level::Control
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn levels_are_raised_only_by_a_live_code_and_lowered_locally() {
        let dir = std::env::temp_dir().join(format!("pilus-agent-raise-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let auth =
            AuthState::load("tok".to_string(), dir.join("devices.json"), &[], 60).expect("state");
        let (code, _) = auth.mint_enrollment(Level::Control).expect("code");
        auth.try_enroll("phone", &code, "iPhone").expect("enroll");
        assert_eq!(auth.level_of("phone"), Some(Level::Control));

        // Re-pairing with no code, or a wrong one, changes nothing.
        assert!(matches!(
            auth.try_enroll("phone", "", "iPhone").expect("repair"),
            EnrollOutcome::AlreadyEnrolled
        ));
        assert!(matches!(
            auth.try_enroll("phone", "WRONG", "iPhone").expect("wrong"),
            EnrollOutcome::AlreadyEnrolled
        ));
        assert_eq!(auth.level_of("phone"), Some(Level::Control));

        // A live operate code raises it, and is spent.
        let (operate, _) = auth.mint_enrollment(Level::Operate).expect("code");
        auth.try_enroll("phone", &operate, "iPhone").expect("raise");
        assert_eq!(auth.level_of("phone"), Some(Level::Operate));
        assert!(matches!(
            auth.try_enroll("phone", &operate, "iPhone")
                .expect("replay"),
            EnrollOutcome::AlreadyEnrolled
        ));

        // Lowering works; "lowering" upward does not.
        assert!(
            auth.lower_level("phone", Level::View)
                .expect("lower")
                .expect("ok")
        );
        assert_eq!(auth.level_of("phone"), Some(Level::View));
        assert!(
            !auth
                .lower_level("phone", Level::Operate)
                .expect("raise")
                .expect("ok")
        );
        assert_eq!(auth.level_of("phone"), Some(Level::View));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
