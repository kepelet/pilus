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
//! [`CONTROL_ALPN`]: `{"type":"enroll","code":…}` adds the peer, and
//! `{"type":"token"}` returns the cached token for an enrolled peer. UHP itself
//! still flows verbatim on [`crate::p2p::ALPN`], authenticated by the token the
//! device sends as `auth`.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use iroh::endpoint::{Connection, RecvStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::SecretKey;
use serde_json::{Value, json};

use crate::{Error, Result};

/// The ALPN for the connector's enrollment and token channel.
pub const CONTROL_ALPN: &[u8] = b"luvus/agent/1";

/// How long an enrollment code stays valid.
pub const ENROLLMENT_TTL: u64 = 600;

/// One enrolled device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Device {
    /// The device's iroh `EndpointId`.
    pub endpoint_id: String,
    /// A human label supplied at enrollment.
    pub label: String,
    /// When it enrolled, as a Unix timestamp.
    pub enrolled_at: u64,
}

/// The persisted set of enrolled devices.
#[derive(Debug)]
pub struct DeviceStore {
    path: PathBuf,
    devices: Vec<Device>,
}

impl DeviceStore {
    /// Load the store at `path`, or start empty when it does not exist.
    pub fn load(path: PathBuf) -> Result<Self> {
        let devices = match std::fs::read(&path) {
            Ok(bytes) => parse_devices(&bytes),
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

    /// Add a device without persisting, for a static `--allow` seed.
    pub fn allow(&mut self, endpoint_id: &str) {
        if !endpoint_id.is_empty() && !self.is_allowed(endpoint_id) {
            self.devices.push(Device {
                endpoint_id: endpoint_id.to_string(),
                label: String::new(),
                enrolled_at: now(),
            });
        }
    }

    /// Persist an enrollment. Returns whether the device was newly added.
    pub fn enroll(&mut self, endpoint_id: &str, label: &str) -> Result<bool> {
        if self.is_allowed(endpoint_id) {
            return Ok(false);
        }
        self.devices.push(Device {
            endpoint_id: endpoint_id.to_string(),
            label: label.to_string(),
            enrolled_at: now(),
        });
        self.save()?;
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

    fn save(&self) -> Result<()> {
        let devices: Vec<Value> = self
            .devices
            .iter()
            .map(|device| {
                json!({
                    "endpoint_id": device.endpoint_id,
                    "label": device.label,
                    "enrolled_at": device.enrolled_at,
                })
            })
            .collect();
        let payload = json!({ "devices": devices }).to_string();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // The store is rewritten on every enrollment, so it truncates rather
        // than refusing to overwrite the way the identity key does.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path)?;
        file.write_all(payload.as_bytes())?;
        file.flush()?;
        Ok(())
    }
}

/// The short-lived code that admits a new device.
#[derive(Clone, Debug)]
pub struct Enrollment {
    code: String,
    expires_at: u64,
}

impl Enrollment {
    /// Mint a code valid for `ttl` seconds.
    pub fn generate(ttl: u64) -> Self {
        Self {
            code: enrollment_code(),
            expires_at: now() + ttl,
        }
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
    token: String,
    devices: Mutex<DeviceStore>,
    enrollment: Mutex<Enrollment>,
}

impl AuthState {
    /// Build the state for one connector run.
    ///
    /// `allow` seeds the in-memory allow-list without persisting.
    pub fn load(token: String, devices_path: PathBuf, allow: &[String], ttl: u64) -> Result<Self> {
        let mut devices = DeviceStore::load(devices_path)?;
        for endpoint_id in allow {
            devices.allow(endpoint_id);
        }
        Ok(Self {
            token,
            devices: Mutex::new(devices),
            enrollment: Mutex::new(Enrollment::generate(ttl)),
        })
    }

    /// Whether `endpoint_id` may reach the gateway.
    pub fn is_allowed(&self, endpoint_id: &str) -> bool {
        lock(&self.devices).is_allowed(endpoint_id)
    }

    /// The enrollment code and its expiry.
    pub fn enrollment(&self) -> (String, u64) {
        let enrollment = lock(&self.enrollment);
        (enrollment.code().to_string(), enrollment.expires_at())
    }

    /// The number of enrolled devices.
    pub fn device_count(&self) -> usize {
        lock(&self.devices).len()
    }

    fn enroll(&self, endpoint_id: &str, label: &str) -> Result<bool> {
        lock(&self.devices).enroll(endpoint_id, label)
    }

    fn accepts(&self, code: &str) -> bool {
        lock(&self.enrollment).accepts(code, now())
    }
}

/// The `CONTROL_ALPN` handler.
#[derive(Clone, Debug)]
pub struct Control {
    auth: Arc<AuthState>,
}

impl Control {
    /// Wrap the shared authorization state.
    pub fn new(auth: Arc<AuthState>) -> Self {
        Self { auth }
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
                if !self.auth.accepts(code) {
                    return error_json("the enrollment code is invalid or expired");
                }
                if let Err(error) = self.auth.enroll(remote, label) {
                    return error_json(&error.to_string());
                }
                json!({ "type": "enrolled", "endpoint_id": remote }).to_string()
            }
            Some("token") => {
                if !self.auth.is_allowed(remote) {
                    return error_json("this device is not enrolled");
                }
                json!({ "type": "token", "token": self.auth.token }).to_string()
            }
            Some("status") => json!({
                "type": "status",
                "devices": self.auth.device_count(),
            })
            .to_string(),
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

fn parse_devices(bytes: &[u8]) -> Vec<Device> {
    let value: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    value
        .get("devices")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|item| {
            Some(Device {
                endpoint_id: item.get("endpoint_id")?.as_str()?.to_string(),
                label: item
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                enrolled_at: item.get("enrolled_at").and_then(Value::as_u64).unwrap_or(0),
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
    let hex: String = bytes[..8].iter().map(|byte| format!("{byte:02X}")).collect();
    format!("{}-{}-{}-{}", &hex[0..4], &hex[4..8], &hex[8..12], &hex[12..16])
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
        assert!(store.enroll("device-a", "phone").expect("enroll"));
        assert!(!store.enroll("device-a", "phone").expect("repeat"));
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

        let (code, _) = auth.enrollment();
        let enrolled = control.dispatch("peer", &json!({ "type": "enroll", "code": code }));
        assert!(enrolled.contains("enrolled"));

        let token = control.dispatch("peer", &json!({ "type": "token" }));
        assert!(token.contains("\"token\":\"tok\""));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
