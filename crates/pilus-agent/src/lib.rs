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

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use serde_json::Value;

pub mod enroll;
pub mod p2p;
pub mod proxy;
pub use enroll::{AuthState, CONTROL_ALPN, Control, DeviceStore, Enrollment};
pub use proxy::serve;

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

/// A running `luvus uhp access` gateway plus its parsed descriptor.
///
/// Dropping the gateway kills the child, which closes the loopback listener
/// and revokes the delegated token.
pub struct Gateway {
    child: Child,
    descriptor: Descriptor,
}

impl Gateway {
    /// Launch the gateway and block until its descriptor line arrives.
    pub fn start(binary: &Path, options: &AccessOptions) -> Result<Self> {
        let mut command = options.command(binary);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().ok_or(Error::NoDescriptor)?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() {
            let _ = child.wait();
            return Err(Error::NoDescriptor);
        }
        let descriptor = Descriptor::parse(line.trim())?;
        // The descriptor is the only stdout the gateway writes; drain the rest
        // so a chatty gateway cannot fill its pipe and stall.
        std::thread::spawn(move || {
            let mut sink = std::io::sink();
            let _ = std::io::copy(&mut reader, &mut sink);
        });
        Ok(Self { child, descriptor })
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
/// after this it forwards bytes verbatim.
pub fn pair(descriptor: &Descriptor) -> Result<String> {
    let mut stream = TcpStream::connect(&descriptor.address)?;
    let frame = serde_json::json!({ "type": "pair", "code": descriptor.pairing_code });
    stream.write_all(frame.to_string().as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
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
