//! The host-side connector for `pilus`.
//!
//! The connector is the *transport provider* the UHP docs leave to the
//! application: it launches `luvus uhp access`, reads its one-line descriptor,
//! and forwards the advertised loopback gateway to remote clients without
//! rewriting UHP frames. This crate owns the descriptor contract and the
//! gateway lifecycle; [`proxy`] carries the bytes over the first transport.
//!
//! The connector deliberately carries no rendering protocol of its own: UHP
//! flows through it unchanged. [`proxy`] is the trusted-network TCP transport
//! and [`p2p`] is the iroh/QUIC transport a phone uses, where [`enroll`] admits
//! devices and hands them the gateway's delegated token.
//!
//! Run as a background service ([`service`]) the connector outlives the
//! window: [`supervisor`] keeps the gateway alive and re-paired, [`hub`] gives
//! an enrolled device one front door to this machine and every SSH machine it
//! manages, and [`admin`] is the owner-only socket the window uses to read
//! status and mint pairing codes.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use serde_json::Value;

pub mod admin;
pub mod enroll;
pub mod hub;
pub mod p2p;
pub mod proxy;
pub mod service;
pub mod supervisor;
pub use enroll::{AuthState, CONTROL_ALPN, Control, DeviceStore, Enrollment, Level};
pub use hub::HUB_ALPN;
pub use proxy::serve;
pub use supervisor::GatewayLink;

/// Write `bytes` to `path`, owner-only, refusing to overwrite.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    Ok(())
}

/// Errors from launching, describing, or pairing with a gateway.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The underlying socket or process operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The descriptor omitted a field the connector needs.
    #[error("the gateway descriptor is missing `{0}`")]
    MissingField(&'static str),

    /// The gateway exited before printing its descriptor line.
    #[error("the gateway closed before printing a descriptor")]
    NoDescriptor,

    /// The descriptor line was not the JSON object the connector expects.
    #[error("could not parse the gateway descriptor: {0}")]
    Descriptor(String),

    /// The gateway rejected pairing or returned an unreadable reply.
    #[error("gateway error: {0}")]
    Gateway(String),

    /// The iroh transport could not start, or a peer could not be served.
    #[error("iroh transport error: {0}")]
    P2p(String),

    /// The persisted device store could not be read or written.
    #[error("device store error: {0}")]
    DeviceStore(String),

    /// Installing, removing, or reading the background service failed.
    #[error("service error: {0}")]
    Service(String),

    /// A bounded wait on the gateway or iroh endpoint expired.
    #[error("{0} timed out")]
    Timeout(&'static str),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// The one-line JSON descriptor `luvus uhp access` prints.
///
/// Mirrors the installed `access.descriptor` schema: `protocol`, `endpoint`,
/// `pairing`, `authority`, and `features`. Parsing reads those fields and
/// ignores additive ones, so a newer daemon does not need a new connector. See
/// `luvus uhp schema` for the authoritative contract.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Descriptor {
    /// The `luvus-uhp` protocol identity, e.g. `luvus-uhp 1.0`.
    pub protocol_version: String,
    /// Loopback address the provider forwards, from `endpoint.host/port`.
    pub address: String,
    /// The one-use, short-lived pairing code, from `pairing.code`.
    pub pairing_code: String,
    /// `read_only` or `control`, from `authority.mode`.
    pub mode: Option<String>,
    /// Delegated scopes the gateway advertises, from `authority.scopes`.
    pub scopes: Vec<String>,
    /// Authority expiry as a Unix timestamp, from `authority.expires_at`.
    pub expires_at: Option<u64>,
    /// Whether authority ends when the gateway process closes.
    pub expires_on_close: bool,
    /// Whether `--machines` advertised the owner-local catalog.
    pub machines: bool,
}

impl Descriptor {
    /// Parse one descriptor line.
    pub fn parse(line: &str) -> Result<Self> {
        let value: Value =
            serde_json::from_str(line).map_err(|error| Error::Descriptor(error.to_string()))?;
        Self::from_json(&value)
    }

    /// Parse a descriptor from an already-decoded JSON value.
    pub fn from_json(value: &Value) -> Result<Self> {
        let address = address(value).ok_or(Error::MissingField("endpoint.host/port"))?;
        let pairing_code = pairing_code(value)
            .ok_or(Error::MissingField("pairing.code"))?
            .to_string();
        let authority = value.get("authority");
        Ok(Self {
            protocol_version: protocol_version(value),
            address,
            pairing_code,
            mode: authority
                .and_then(|authority| authority.get("mode"))
                .or_else(|| value.get("mode"))
                .or_else(|| value.get("access_mode"))
                .and_then(Value::as_str)
                .map(str::to_string),
            scopes: strings(
                authority
                    .and_then(|authority| authority.get("scopes"))
                    .or_else(|| value.get("scopes")),
            ),
            expires_at: authority
                .and_then(|authority| authority.get("expires_at"))
                .and_then(Value::as_u64),
            expires_on_close: authority
                .and_then(|authority| authority.get("expires_on_close"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            machines: value
                .get("features")
                .and_then(|features| features.get("machines"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// Options for spawning `luvus uhp access`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccessOptions {
    /// Named session; `None` is the default session.
    pub session: Option<String>,
    /// Advertise the owner-local machine catalog (`--machines`).
    pub machines: bool,
    /// Advertise the bounded control surface (`--control`).
    pub control: bool,
    /// Authority lifetime in seconds (`--ttl`), 1 through 86400.
    pub ttl_seconds: Option<u32>,
    /// Keep authority for the life of the process (`--no-expiry`).
    pub no_expiry: bool,
}

impl AccessOptions {
    /// Build the `luvus [--session NAME] uhp access [flags]` command.
    pub fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        if let Some(session) = &self.session {
            command.arg("--session").arg(session);
        }
        command.arg("uhp").arg("access");
        if self.machines {
            command.arg("--machines");
        }
        if self.control {
            command.arg("--control");
        }
        if let Some(ttl) = self.ttl_seconds {
            command.arg("--ttl").arg(ttl.to_string());
        }
        if self.no_expiry {
            command.arg("--no-expiry");
        }
        command
    }
}

/// The deadline for gateway startup output and pairing I/O.
const GATEWAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Map a pairing I/O failure: expired deadlines are `Timeout`, the rest stay `Io`.
fn pairing_io(error: std::io::Error) -> Error {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            Error::Timeout("gateway pairing")
        }
        _ => Error::Io(error),
    }
}

/// A running `luvus uhp access` gateway plus its parsed descriptor.
///
/// Dropping the gateway kills the child, which closes the loopback listener
/// and revokes the delegated token.
pub struct Gateway {
    child: Child,
    descriptor: Descriptor,
}

impl Gateway {
    /// Launch the gateway and wait up to ten seconds for its descriptor line.
    ///
    /// Any failure path kills and reaps the child so a broken gateway cannot
    /// leak a process or leave its delegated authority dangling.
    pub fn start(binary: &Path, options: &AccessOptions) -> Result<Self> {
        let mut command = options.command(binary);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn()?;
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::NoDescriptor);
        };
        let mut reader = BufReader::new(stdout);
        let (descriptor_tx, descriptor_rx) = std::sync::mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("pilus-gateway-descriptor".to_string())
            .spawn(move || {
                let mut line = String::new();
                let first = reader.read_line(&mut line).map(|_| line);
                let _ = descriptor_tx.send(first);
                // The descriptor is the only stdout the gateway writes; drain
                // the rest so a chatty gateway cannot fill its pipe and stall.
                let mut sink = std::io::sink();
                let _ = std::io::copy(&mut reader, &mut sink);
            });
        if let Err(error) = spawned {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Io(error));
        }
        let line = match descriptor_rx.recv_timeout(GATEWAY_TIMEOUT) {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Io(error));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Timeout("gateway descriptor"));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::NoDescriptor);
            }
        };
        let parsed = if line.trim().is_empty() {
            Err(Error::NoDescriptor)
        } else {
            Descriptor::parse(line.trim())
        };
        match parsed {
            Ok(descriptor) => Ok(Self { child, descriptor }),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(error)
            }
        }
    }

    /// Whether the gateway child is still alive.
    ///
    /// A gateway that exited has taken its delegated token with it, so the
    /// supervisor treats a `false` as the cue to start and pair again.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The parsed descriptor.
    pub fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }

    /// Spend the one-use pairing code and return the delegated token.
    pub fn pair(&self) -> Result<String> {
        pair(&self.descriptor)
    }

    /// A UHP client authenticated with a freshly minted token.
    pub fn client(&self) -> Result<pilus_client::UhpClient> {
        let token = self.pair()?;
        Ok(pilus_client::UhpClient::connect_tcp(self.descriptor.address.clone()).with_auth(token))
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Pair with a gateway and return the delegated token.
///
/// The pairing frame is the only UHP-adjacent message the connector decodes;
/// after this it forwards bytes verbatim. Connect, write, and read each get
/// ten seconds so a dead gateway cannot hang startup.
pub fn pair(descriptor: &Descriptor) -> Result<String> {
    use std::net::ToSocketAddrs;
    use std::time::Instant;
    let deadline = Instant::now() + GATEWAY_TIMEOUT;
    let mut last_error: Option<std::io::Error> = None;
    let mut stream = None;
    for address in descriptor.address.to_socket_addrs()? {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::Timeout("gateway pairing"));
        }
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let mut stream = match stream {
        Some(stream) => stream,
        None => match last_error {
            Some(error) => return Err(pairing_io(error)),
            None => {
                return Err(Error::Gateway(format!(
                    "`{}` resolves to nothing",
                    descriptor.address
                )));
            }
        },
    };
    stream
        .set_read_timeout(Some(GATEWAY_TIMEOUT))
        .map_err(pairing_io)?;
    stream
        .set_write_timeout(Some(GATEWAY_TIMEOUT))
        .map_err(pairing_io)?;
    let frame = serde_json::json!({ "type": "pair", "code": descriptor.pairing_code });
    stream
        .write_all(frame.to_string().as_bytes())
        .map_err(pairing_io)?;
    stream.write_all(b"\n").map_err(pairing_io)?;
    stream.flush().map_err(pairing_io)?;

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply).map_err(pairing_io)?;
    let value: Value =
        serde_json::from_str(reply.trim()).map_err(|error| Error::Gateway(error.to_string()))?;
    if let Some(error) = value.get("error") {
        return Err(Error::Gateway(
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("pairing rejected")
                .to_string(),
        ));
    }
    value
        .get("token")
        .or_else(|| value.get("auth"))
        .or_else(|| value.get("secret"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(Error::MissingField("token"))
}

/// The `luvus-uhp` protocol version, composed from `protocol.name/major/minor`.
fn protocol_version(value: &Value) -> String {
    if let Some(protocol) = value.get("protocol") {
        let name = protocol
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("luvus-uhp");
        if let (Some(major), Some(minor)) = (
            protocol.get("major").and_then(Value::as_u64),
            protocol.get("minor").and_then(Value::as_u64),
        ) {
            return format!("{name} {major}.{minor}");
        }
    }
    ["uhp_version", "protocol_version", "version"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

/// The loopback address a provider forwards, from `endpoint.host/port`.
fn address(value: &Value) -> Option<String> {
    if let Some(endpoint) = value.get("endpoint") {
        let host = endpoint
            .get("host")
            .and_then(Value::as_str)
            .unwrap_or("127.0.0.1");
        if let Some(port) = endpoint.get("port").and_then(Value::as_u64) {
            return Some(format!("{host}:{port}"));
        }
    }
    value
        .get("address")
        .and_then(Value::as_str)
        .map(strip_scheme)
}

/// The one-use pairing code, from `pairing.code`.
fn pairing_code(value: &Value) -> Option<&str> {
    value
        .get("pairing")
        .and_then(|pairing| pairing.get("code"))
        .or_else(|| value.get("pairing_code"))
        .or_else(|| value.get("code"))
        .and_then(Value::as_str)
}

fn strip_scheme(address: &str) -> String {
    address
        .strip_prefix("tcp://")
        .or_else(|| address.strip_prefix("http://"))
        .unwrap_or(address)
        .trim_end_matches('/')
        .to_string()
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use serde_json::json;

    /// Captured verbatim from a live `luvus uhp access --control --ttl 30`.
    const LIVE_DESCRIPTOR: &str = r#"{"$schema":"https://luvus.dev/protocol/uhp/v1/schema/access/descriptor.schema.json","access":{"major":1},"authority":{"expires_at":1790779087,"mode":"control","scopes":["read","workspace","agent","terminal","orchestration"]},"endpoint":{"framing":"ndjson","host":"127.0.0.1","port":50928,"transport":"tcp"},"pairing":{"code":"2452-V4ZH-Y3GJ","expires_at":1790779087,"type":"one_use_code"},"protocol":{"major":1,"minor":0,"name":"luvus-uhp"},"type":"luvus_uhp_access"}"#;

    #[test]
    fn parses_the_live_access_descriptor() {
        let descriptor = Descriptor::parse(LIVE_DESCRIPTOR).expect("descriptor");
        assert_eq!(descriptor.protocol_version, "luvus-uhp 1.0");
        assert_eq!(descriptor.address, "127.0.0.1:50928");
        assert_eq!(descriptor.pairing_code, "2452-V4ZH-Y3GJ");
        assert_eq!(descriptor.mode.as_deref(), Some("control"));
        assert_eq!(
            descriptor.scopes,
            vec!["read", "workspace", "agent", "terminal", "orchestration"]
        );
        assert_eq!(descriptor.expires_at, Some(1790779087));
        assert!(!descriptor.expires_on_close);
        assert!(!descriptor.machines, "the live run did not pass --machines");
    }

    #[test]
    fn reads_expiry_features_and_protocol_minor() {
        let descriptor = Descriptor::from_json(&json!({
            "authority": { "expires_on_close": true, "mode": "read_only", "scopes": ["read"] },
            "endpoint": { "host": "127.0.0.1", "port": 9000 },
            "pairing": { "code": "ZZZZ-YYYY-XXXX" },
            "features": { "machines": true },
            "protocol": { "name": "luvus-uhp", "major": 1, "minor": 3 }
        }))
        .expect("descriptor");
        assert_eq!(descriptor.protocol_version, "luvus-uhp 1.3");
        assert!(descriptor.expires_on_close);
        assert!(descriptor.machines);
        assert_eq!(descriptor.expires_at, None);
    }

    #[test]
    fn missing_endpoint_or_code_is_an_error() {
        assert!(matches!(
            Descriptor::from_json(&json!({ "pairing": { "code": "X" } })),
            Err(Error::MissingField("endpoint.host/port"))
        ));
        assert!(matches!(
            Descriptor::from_json(&json!({ "endpoint": { "port": 1 } })),
            Err(Error::MissingField("pairing.code"))
        ));
    }

    /// A fake gateway that records its PID, prints garbage, and sleeps, so the
    /// descriptor error path must still kill and reap it.
    #[cfg(unix)]
    #[test]
    fn malformed_descriptor_kills_the_child() {
        let dir =
            std::env::temp_dir().join(format!("pilus-agent-descriptor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let pid_file = dir.join("pid");
        let script = dir.join("fake-gateway.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{}'\necho 'not a descriptor'\nexec /bin/sleep 30\n",
                pid_file.display()
            ),
        )
        .expect("write script");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script, permissions).expect("chmod");
        }

        let result = Gateway::start(&script, &AccessOptions::default());
        assert!(
            matches!(result, Err(Error::Descriptor(_))),
            "expected a descriptor error, got {:?}",
            result.err()
        );
        let pid = std::fs::read_to_string(&pid_file)
            .expect("pid file")
            .trim()
            .to_string();
        let alive = Command::new("/bin/kill")
            .arg("-0")
            .arg(&pid)
            .stderr(Stdio::null())
            .status()
            .expect("kill -0")
            .success();
        assert!(!alive, "the failed gateway child must be dead: pid {pid}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn access_options_build_the_cli_command() {
        let options = AccessOptions {
            session: Some("review".to_string()),
            machines: true,
            control: true,
            ttl_seconds: Some(3600),
            no_expiry: false,
        };
        let command = options.command(Path::new("/usr/local/bin/luvus"));
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "--session",
                "review",
                "uhp",
                "access",
                "--machines",
                "--control",
                "--ttl",
                "3600"
            ]
        );
    }
}
