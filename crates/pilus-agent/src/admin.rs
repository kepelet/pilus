//! The owner-only socket the window uses to talk to a running connector.
//!
//! A background service has no terminal to print its endpoint, ticket, or
//! enrollment code to, and a code that lives only in a log file is a code the
//! user cannot get without restarting the service. This socket is the other
//! way in: one JSON request per connection, answered with one JSON line.
//!
//! - `status` reports the endpoint, the freshest ticket, the gateway's health
//!   and the enrolled devices. It never includes an enrollment code.
//! - `new_code` mints a code on demand and returns it with the ready-made
//!   `pilus://pair/…` link a QR is made from.
//! - `revoke` removes an enrolled device.
//! - `retry` stops the gateway's backoff and starts Luvus if it is not running.
//!
//! `status` also says whether the service is running an older binary than the one
//! on disk (an update replaced it) and, on Linux, whether it will survive logout.
//!
//! Access is the socket file's `0600` mode inside a `0700` directory, the same
//! trust the Luvus daemon's own socket relies on.

use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime};

use iroh::Endpoint;
use iroh_tickets::endpoint::EndpointTicket;
use serde_json::{Value, json};

use crate::enroll::{AuthState, Level};
use crate::supervisor::GatewayLink;

/// When this process started, to tell whether its binary has been replaced since.
static STARTED: LazyLock<SystemTime> = LazyLock::new(SystemTime::now);

/// Whether the file this process runs from was written after it started.
///
/// An update replaces the binary in place and leaves the old process running it,
/// so the service would keep old behaviour until the next login. The window uses
/// this to restart it. A couple of seconds of slack keeps a binary written just
/// before launch from counting.
fn binary_replaced() -> bool {
    std::env::current_exe()
        .and_then(std::fs::metadata)
        .and_then(|meta| meta.modified())
        .is_ok_and(|modified| modified > *STARTED + Duration::from_secs(2))
}

/// What the admin socket reads from the running connector.
#[derive(Clone)]
pub struct Admin {
    /// The iroh endpoint, for the current ticket.
    pub endpoint: Endpoint,
    /// Enrollment state.
    pub auth: Arc<AuthState>,
    /// Gateway health.
    pub link: Arc<GatewayLink>,
    /// The name devices should show for this machine.
    pub label: String,
}

impl Admin {
    /// Answer one request.
    pub fn dispatch(&self, request: &Value) -> Value {
        match request.get("type").and_then(Value::as_str) {
            Some("status") => self.status(),
            Some("new_code") => {
                let level = request
                    .get("level")
                    .and_then(Value::as_str)
                    .map_or(Some(Level::Control), Level::parse);
                match level {
                    Some(level) => self.new_code(level),
                    None => error("level must be view, control, or operate"),
                }
            }
            Some("set_level") => self.set_level(request),
            Some("retry") => {
                self.link.kick();
                json!({ "type": "retrying" })
            }
            Some("revoke") => {
                let target = request
                    .get("endpoint_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match self.auth.revoke_device(target) {
                    None => error("revocation is disabled by --allow"),
                    Some(Ok(removed)) => json!({ "type": "revoked", "removed": removed }),
                    Some(Err(failure)) => error(&failure.to_string()),
                }
            }
            _ => error("unknown request"),
        }
    }

    fn status(&self) -> Value {
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
        json!({
            "type": "status",
            "version": env!("CARGO_PKG_VERSION"),
            "label": self.label,
            "endpoint_id": self.endpoint.id().to_string(),
            "ticket": self.ticket(),
            "gateway": { "up": self.link.is_up(), "error": self.link.error() },
            "strict": self.auth.is_strict(),
            "restart_needed": binary_replaced(),
            "linger": crate::service::linger(),
            "devices": devices,
        })
    }

    /// Lower a device's level. Raising is deliberately not offered here: it
    /// takes a new code at the higher level, so promoting a device always
    /// means being at the host and choosing to.
    fn set_level(&self, request: &Value) -> Value {
        let target = request
            .get("endpoint_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(level) = request
            .get("level")
            .and_then(Value::as_str)
            .and_then(Level::parse)
        else {
            return error("level must be view, control, or operate");
        };
        match self.auth.level_of(target) {
            None => return error("no such device"),
            Some(current) if level > current => {
                return error(
                    "a level can only be lowered here; add the device again with a new code to raise it",
                );
            }
            Some(_) => {}
        }
        match self.auth.lower_level(target, level) {
            None => error("levels are fixed by --allow"),
            Some(Ok(changed)) => {
                json!({ "type": "level", "changed": changed, "level": level.as_str() })
            }
            Some(Err(failure)) => error(&failure.to_string()),
        }
    }

    fn new_code(&self, level: Level) -> Value {
        let Some((code, expires_at)) = self.auth.mint_enrollment(level) else {
            return error("enrollment is disabled by --allow");
        };
        json!({
            "type": "code",
            "level": level.as_str(),
            "code": code,
            "expires_at": expires_at,
            // No `name`: it would become the device's own label, and the
            // device is the one that knows what to call itself.
            "link": pairing_link(&self.ticket(), Some(&code), ""),
        })
    }

    fn ticket(&self) -> String {
        EndpointTicket::new(self.endpoint.addr()).to_string()
    }
}

/// The deep link a device scans or pastes: `pilus://pair/<ticket>?code=…`, with
/// an optional `&name=…` the minted links leave out.
///
/// The ticket is the path, not the host, because it is long base32 a URL
/// parser would otherwise lowercase or reject as a hostname.
pub fn pairing_link(ticket: &str, code: Option<&str>, name: &str) -> String {
    let mut link = format!("pilus://pair/{ticket}");
    let mut separator = '?';
    if let Some(code) = code {
        link.push(separator);
        link.push_str("code=");
        link.push_str(&percent_encode(code));
        separator = '&';
    }
    if !name.is_empty() {
        link.push(separator);
        link.push_str("name=");
        link.push_str(&percent_encode(name));
    }
    link
}

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn error(message: &str) -> Value {
    json!({ "type": "error", "message": message })
}

/// Listen on `path` until the task is dropped.
#[cfg(unix)]
pub async fn serve(path: &Path, admin: Admin) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    // Read the clock now, not at the first status request.
    LazyLock::force(&STARTED);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    // A socket left by a crashed run would make `bind` fail; nothing else may
    // be listening here because the iroh identity is single-instance anyway.
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    loop {
        let (stream, _) = listener.accept().await?;
        let admin = admin.clone();
        tokio::spawn(async move {
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read.take(64 * 1024));
            let mut line = String::new();
            let answer =
                match tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
                    .await
                {
                    Ok(Ok(n)) if n > 0 => match serde_json::from_str::<Value>(line.trim()) {
                        Ok(request) => admin.dispatch(&request),
                        Err(parse) => error(&parse.to_string()),
                    },
                    _ => error("no request received"),
                };
            let _ = write.write_all(answer.to_string().as_bytes()).await;
            let _ = write.write_all(b"\n").await;
        });
    }
}

/// There is no admin socket off Unix; the connector is not a service there.
#[cfg(not(unix))]
pub async fn serve(_path: &Path, _admin: Admin) -> std::io::Result<()> {
    std::future::pending().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_puts_the_ticket_in_the_path_and_encodes_the_rest() {
        let link = pairing_link("endpointabc", Some("AB12-CD34"), "Rizaldy's Mac");
        assert_eq!(
            link,
            "pilus://pair/endpointabc?code=AB12-CD34&name=Rizaldy%27s%20Mac"
        );
    }

    #[test]
    fn link_omits_what_it_was_not_given() {
        assert_eq!(
            pairing_link("endpointabc", None, ""),
            "pilus://pair/endpointabc"
        );
        assert_eq!(
            pairing_link("endpointabc", None, "Mac"),
            "pilus://pair/endpointabc?name=Mac"
        );
    }
}
