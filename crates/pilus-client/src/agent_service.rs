//! The background connector service, as the window sees it.
//!
//! Remote access runs as a service that outlives the window (see
//! `pilus-agent service`), so the window cannot own it as a child process or
//! read its stdout. It does two things instead: ask `pilus-agent` to install or
//! remove the service, and talk to the running connector over its owner-only
//! admin socket for status, fresh pairing codes, and device revocation.
//!
//! Every call here blocks on a process or a socket, so callers run them off the
//! GUI thread.

use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};

use crate::connector::agent_binary;
use crate::discovery::{config_dir, pilus_config_dir};
use crate::error::{Error, Result};

/// Whether this platform can run the connector as a background service. Windows
/// has no equivalent wired up, and the window says so rather than offering a
/// switch that cannot work.
pub const SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "linux"));

/// Where the connector keeps its identity, device list, admin socket, and log.
///
/// This is pilus's own directory, not the Luvus state root: the connector is
/// pilus's, and a Luvus session change or `LUVUS_HOME` must not move the
/// identity that paired phones dial.
pub fn agent_dir() -> PathBuf {
    pilus_config_dir().join("agent")
}

/// Files that were once written under the Luvus state root and are the
/// connector's durable state. The socket and log are rebuilt on every start.
const LEGACY_FILES: &[&str] = &["identity.key", "devices.json"];

/// Move the connector's durable state out of `<luvus root>/agent` into
/// [`agent_dir`], once.
///
/// `identity.key` is the host's iroh `EndpointId`, which every paired phone has
/// pinned in its ticket, and `devices.json` is who may call it. Leaving them
/// behind would mint a new identity and an empty allowlist, silently unpairing
/// everything, so a failure here is an error rather than a fresh start. A file
/// already present at the destination always wins and the old one is left alone.
pub fn adopt_legacy_state() -> Result<()> {
    adopt_from(&config_dir().join("agent"), &agent_dir())
}

fn adopt_from(legacy: &std::path::Path, current: &std::path::Path) -> Result<()> {
    if legacy == current {
        return Ok(());
    }
    for name in LEGACY_FILES {
        let (from, to) = (legacy.join(name), current.join(name));
        if !from.is_file() || to.exists() {
            continue;
        }
        std::fs::create_dir_all(current)?;
        // A rename keeps the file's mode; across volumes it falls back to a
        // copy, which `fs::copy` also gives the source's permissions.
        if std::fs::rename(&from, &to).is_err() {
            std::fs::copy(&from, &to)?;
            std::fs::remove_file(&from)?;
        }
    }
    Ok(())
}

/// The connector's admin socket.
pub fn admin_socket() -> PathBuf {
    agent_dir().join("agent.sock")
}

/// The service's log file.
pub fn log_path() -> PathBuf {
    agent_dir().join("agent.log")
}

/// How much a paired device may do, as the connector reports it.
///
/// The connector owns the meaning; this is only the spelling the window sends
/// and shows. Raising a level is not something the window can do: it takes a new
/// code at the higher level.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum DeviceLevel {
    /// Reads only.
    View,
    /// Prompts and interrupts agents, retries tasks, focuses things.
    #[default]
    Control,
    /// Control, plus starting work: tasks, agents, workspaces.
    Operate,
}

impl DeviceLevel {
    /// The wire spelling.
    pub fn wire(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Control => "control",
            Self::Operate => "operate",
        }
    }

    /// The display name.
    pub fn label(self) -> &'static str {
        match self {
            Self::View => "View only",
            Self::Control => "Control",
            Self::Operate => "Can start work",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "view" => Self::View,
            "operate" => Self::Operate,
            _ => Self::Control,
        }
    }
}

/// One device the connector has enrolled.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnrolledDevice {
    /// The device's iroh `EndpointId`.
    pub endpoint_id: String,
    /// The label it enrolled with; may be empty.
    pub label: String,
    /// When it enrolled, as a Unix timestamp.
    pub enrolled_at: u64,
    /// What it may do.
    pub level: DeviceLevel,
}

/// What the running connector reports.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServiceStatus {
    /// The name devices show for this machine.
    pub label: String,
    /// The connector's stable iroh `EndpointId`.
    pub endpoint_id: String,
    /// A current ticket, for a device that already knows to trust this host.
    pub ticket: String,
    /// Whether the Luvus gateway behind it is up.
    pub gateway_up: bool,
    /// Why the gateway is down, when it is.
    pub gateway_error: Option<String>,
    /// Enrolled devices.
    pub devices: Vec<EnrolledDevice>,
    /// `--allow` pins the device set, so enrollment and revocation are off.
    pub strict: bool,
    /// The binary on disk is newer than the one this service is running, because
    /// an update replaced it. Restarting picks the new one up.
    pub restart_needed: bool,
    /// Whether the service survives logout. `None` where that is not a question
    /// (everything but a Linux user unit).
    pub linger: Option<bool>,
}

impl ServiceStatus {
    /// Whether the gateway is down because `luvus` cannot be found at all, which
    /// retrying will not fix, as opposed to the server simply not running.
    #[must_use]
    pub fn luvus_missing(&self) -> bool {
        !self.gateway_up
            && self
                .gateway_error
                .as_deref()
                .is_some_and(|why| why.contains("could not find the `luvus` binary"))
    }
}

/// A freshly minted enrollment code and the link that carries it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PairingCode {
    /// The level a device enrolled with this code gets.
    pub level: DeviceLevel,
    /// The human-readable code.
    pub code: String,
    /// When it stops working, as a Unix timestamp.
    pub expires_at: u64,
    /// `pilus://pair/<ticket>?code=…&name=…`, what a QR encodes.
    pub link: String,
}

/// Whether the service definition exists and is running.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Installation {
    /// This platform cannot run the connector as a service.
    Unsupported,
    /// Not installed.
    NotInstalled,
    /// Installed; `running` is the service manager's view.
    Installed {
        /// The process is up right now.
        running: bool,
    },
}

/// Install (or refresh) and start the service. Idempotent, and it leaves an
/// unchanged running service alone, so it is safe to call on every launch.
pub fn install() -> Result<()> {
    run_agent(&["service", "install"]).map(drop)
}

/// Stop and remove the service.
pub fn uninstall() -> Result<()> {
    run_agent(&["service", "uninstall"]).map(drop)
}

/// The service manager's view of the service.
pub fn installation() -> Installation {
    let Ok(output) = run_agent(&["service", "status"]) else {
        return Installation::Unsupported;
    };
    match output.trim() {
        "installed, running" => Installation::Installed { running: true },
        "installed, not running" => Installation::Installed { running: false },
        "not installed" => Installation::NotInstalled,
        _ => Installation::Unsupported,
    }
}

fn run_agent(args: &[&str]) -> Result<String> {
    let binary = agent_binary().ok_or_else(|| {
        Error::Connector(
            "pilus-agent not found; set PILUS_AGENT_BIN or install it beside pilus".to_string(),
        )
    })?;
    let output = Command::new(binary).args(args).output()?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim().trim_start_matches("Error: ");
        return Err(Error::Connector(if detail.is_empty() {
            format!("pilus-agent {} failed ({})", args.join(" "), output.status)
        } else {
            detail.to_string()
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The running connector's status, or `None` when nothing answers.
pub fn status() -> Option<ServiceStatus> {
    let reply = admin(&json!({ "type": "status" })).ok()?;
    parse_status(&reply)
}

/// Mint a pairing code that enrolls a device at `level`. The previous code
/// stops working.
pub fn new_code(level: DeviceLevel) -> Result<PairingCode> {
    let reply = admin(&json!({ "type": "new_code", "level": level.wire() }))?;
    parse_code(&reply)
}

/// Stop the service waiting out its backoff and start Luvus now if it is down.
pub fn retry() -> Result<()> {
    admin(&json!({ "type": "retry" })).map(drop)
}

/// Restart the installed service so it runs the binary on disk.
pub fn restart() -> Result<()> {
    run_agent(&["service", "restart"]).map(drop)
}

/// The last `max_lines` of the service's log, oldest first.
///
/// Reads only the tail of the file: the log can reach a megabyte, and the window
/// polls this while it is open.
pub fn log_tail(max_lines: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};

    const WINDOW: u64 = 32 * 1024;
    let Ok(mut file) = std::fs::File::open(log_path()) else {
        return Vec::new();
    };
    let length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = length.saturating_sub(WINDOW);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    tail_lines(&String::from_utf8_lossy(&bytes), start > 0, max_lines)
}

/// The last `max_lines` of `text`. `cut` says the text begins mid-file, so its
/// first line is a fragment and is dropped.
fn tail_lines(text: &str, cut: bool, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<&str> = text.lines().collect();
    if cut && !lines.is_empty() {
        lines.remove(0);
    }
    let skip = lines.len().saturating_sub(max_lines);
    lines[skip..]
        .iter()
        .map(|line| line.chars().filter(|c| !c.is_control()).collect())
        .collect()
}

/// Lower an enrolled device's level. The connector refuses to raise one.
pub fn lower_level(endpoint_id: &str, level: DeviceLevel) -> Result<()> {
    admin(&json!({ "type": "set_level", "endpoint_id": endpoint_id, "level": level.wire() }))
        .map(drop)
}

/// Remove an enrolled device. Returns whether it was present.
pub fn revoke(endpoint_id: &str) -> Result<bool> {
    let reply = admin(&json!({ "type": "revoke", "endpoint_id": endpoint_id }))?;
    reply
        .get("removed")
        .and_then(Value::as_bool)
        .ok_or_else(|| Error::Connector("unexpected reply to revoke".to_string()))
}

fn parse_status(reply: &Value) -> Option<ServiceStatus> {
    if reply.get("type").and_then(Value::as_str) != Some("status") {
        return None;
    }
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let gateway = reply.get("gateway").cloned().unwrap_or(Value::Null);
    Some(ServiceStatus {
        label: text(reply, "label"),
        endpoint_id: text(reply, "endpoint_id"),
        ticket: text(reply, "ticket"),
        gateway_up: gateway.get("up").and_then(Value::as_bool).unwrap_or(false),
        gateway_error: gateway
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string),
        devices: reply
            .get("devices")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|device| EnrolledDevice {
                endpoint_id: text(device, "endpoint_id"),
                label: text(device, "label"),
                enrolled_at: device
                    .get("enrolled_at")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                level: DeviceLevel::parse(&text(device, "level")),
            })
            .collect(),
        strict: reply
            .get("strict")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        restart_needed: reply
            .get("restart_needed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        linger: reply.get("linger").and_then(Value::as_bool),
    })
}

fn parse_code(reply: &Value) -> Result<PairingCode> {
    if reply.get("type").and_then(Value::as_str) == Some("code") {
        return Ok(PairingCode {
            level: DeviceLevel::parse(
                reply
                    .get("level")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            ),
            code: reply
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            expires_at: reply.get("expires_at").and_then(Value::as_u64).unwrap_or(0),
            link: reply
                .get("link")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    Err(Error::Connector(
        reply
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the connector refused the request")
            .to_string(),
    ))
}

#[cfg(unix)]
fn admin(request: &Value) -> Result<Value> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let mut stream = UnixStream::connect(admin_socket())
        .map_err(|_| Error::Connector("the connector is not running".to_string()))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "{request}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let reply: Value = serde_json::from_str(line.trim())
        .map_err(|error| Error::Connector(format!("unreadable reply: {error}")))?;
    if reply.get("type").and_then(Value::as_str) == Some("error") {
        return Err(Error::Connector(
            reply
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the connector refused the request")
                .to_string(),
        ));
    }
    Ok(reply)
}

#[cfg(not(unix))]
fn admin(_request: &Value) -> Result<Value> {
    Err(Error::Connector(
        "the connector service is not available on this platform".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-adopt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn adopting_moves_identity_and_devices_but_not_the_socket_or_log() {
        let root = scratch("moves");
        let (legacy, current) = (root.join("luvus/agent"), root.join("pilus/agent"));
        std::fs::create_dir_all(&legacy).expect("legacy");
        for name in ["identity.key", "devices.json", "agent.log", "agent.sock"] {
            std::fs::write(legacy.join(name), name).expect("seed");
        }
        adopt_from(&legacy, &current).expect("adopt");
        assert_eq!(
            std::fs::read(current.join("identity.key")).expect("key"),
            b"identity.key"
        );
        assert!(current.join("devices.json").is_file());
        assert!(!legacy.join("identity.key").exists() && !legacy.join("devices.json").exists());
        assert!(
            legacy.join("agent.log").exists(),
            "ephemeral files stay put"
        );
        assert!(!current.join("agent.log").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn adopting_never_overwrites_the_new_identity() {
        let root = scratch("keeps");
        let (legacy, current) = (root.join("luvus/agent"), root.join("pilus/agent"));
        std::fs::create_dir_all(&legacy).expect("legacy");
        std::fs::create_dir_all(&current).expect("current");
        std::fs::write(legacy.join("identity.key"), "old").expect("old");
        std::fs::write(current.join("identity.key"), "new").expect("new");
        adopt_from(&legacy, &current).expect("adopt");
        assert_eq!(
            std::fs::read(current.join("identity.key")).expect("key"),
            b"new"
        );
        assert!(
            legacy.join("identity.key").exists(),
            "the old copy is left alone"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn adopting_with_nothing_to_move_creates_nothing() {
        let root = scratch("empty");
        adopt_from(&root.join("luvus/agent"), &root.join("pilus/agent")).expect("adopt");
        assert!(!root.join("pilus").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reads_a_status_reply() {
        let status = parse_status(&json!({
            "type": "status",
            "label": "Studio Mac",
            "endpoint_id": "abc",
            "ticket": "endpointxyz",
            "gateway": { "up": false, "error": "no luvus server running" },
            "strict": false,
            "devices": [{ "endpoint_id": "d1", "label": "phone", "enrolled_at": 7, "level": "operate" }],
        }))
        .expect("status");
        assert_eq!(status.label, "Studio Mac");
        assert!(!status.gateway_up);
        assert_eq!(
            status.gateway_error.as_deref(),
            Some("no luvus server running")
        );
        assert_eq!(status.devices[0].label, "phone");
        assert_eq!(status.devices[0].level, DeviceLevel::Operate);
    }

    #[test]
    fn reads_the_update_and_logout_fields_and_tolerates_their_absence() {
        let newer = parse_status(&json!({
            "type": "status", "restart_needed": true, "linger": false,
        }))
        .expect("status");
        assert!(newer.restart_needed);
        assert_eq!(newer.linger, Some(false));
        // An agent from before these fields existed, or a platform with no answer.
        let older = parse_status(&json!({ "type": "status", "linger": null })).expect("status");
        assert!(!older.restart_needed);
        assert_eq!(older.linger, None);
    }

    #[test]
    fn a_missing_luvus_is_told_apart_from_a_stopped_server() {
        let down = |why: &str| ServiceStatus {
            gateway_error: Some(why.to_string()),
            ..ServiceStatus::default()
        };
        assert!(down("could not find the `luvus` binary").luvus_missing());
        assert!(!down("no luvus server running").luvus_missing());
        let up = ServiceStatus {
            gateway_up: true,
            ..down("could not find the `luvus` binary")
        };
        assert!(!up.luvus_missing(), "a healthy gateway is not missing");
    }

    #[test]
    fn the_log_tail_keeps_whole_recent_lines() {
        let text = "partial fragment\nfirst\nsecond\nthird\n";
        assert_eq!(tail_lines(text, true, 2), ["second", "third"]);
        assert_eq!(tail_lines(text, false, 10).len(), 4);
        assert_eq!(tail_lines("a\u{1b}[31mb\n", false, 5), ["a[31mb"]);
        assert!(tail_lines("", true, 5).is_empty());
    }

    #[test]
    fn an_error_reply_is_not_a_status() {
        assert!(parse_status(&json!({ "type": "error", "message": "x" })).is_none());
    }

    #[test]
    fn reads_a_code_and_surfaces_a_refusal() {
        let code = parse_code(&json!({
            "type": "code", "code": "AB12", "expires_at": 9, "link": "pilus://pair/t?code=AB12"
        }))
        .expect("code");
        assert_eq!(code.link, "pilus://pair/t?code=AB12");
        let refused = parse_code(&json!({ "type": "error", "message": "disabled" }));
        assert!(matches!(refused, Err(Error::Connector(message)) if message == "disabled"));
    }
}
