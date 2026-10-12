//! Saved SSH machine profiles, read through the `luvus` CLI.
//!
//! The daemon exposes no control method for machines; profiles live in the
//! state root, so `luvus machine list --json` is the stable reader. Failures
//! yield an empty list: machines are sidebar chrome, and a broken profile
//! must never disturb the window.

use serde_json::Value;
use std::path::Path;

use crate::connection::Connection;
use crate::error::{Error, Result};
use crate::transport::{SshTarget, Stream};

/// One saved machine profile plus its live reachability.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MachineInfo {
    /// Stable profile id.
    pub id: String,
    /// Human label; may be empty.
    pub display: String,
    /// SSH destination, e.g. `user@host`.
    pub destination: String,
    /// Whether the profile is enabled.
    pub enabled: bool,
    /// Reachability from the last probe; `None` means not probed yet.
    pub online: Option<bool>,
    /// Probe round-trip in milliseconds, when measured.
    pub latency_ms: Option<u64>,
    /// The daemon's reported version from the last probe.
    pub version: Option<String>,
    /// Verified absolute path to the remote `luvus` binary.
    pub remote_binary: String,
    /// Preferred remote named session; `None` is the remote default.
    pub preferred_session: Option<String>,
    /// Whether Luvus manages (and may replace) the remote binary.
    pub automatic_provisioning: bool,
}

impl MachineInfo {
    /// The name to draw: the label, else the id.
    pub fn display(&self) -> &str {
        if self.display.is_empty() {
            &self.id
        } else {
            &self.display
        }
    }

    /// The remote named session to attach to, if the profile pins one.
    ///
    /// A missing or `default` value means the remote default session.
    pub fn remote_session(&self) -> Option<String> {
        self.preferred_session
            .clone()
            .filter(|name| name != "default" && crate::discovery::validate_session(name).is_ok())
    }

    /// The SSH destination and remote command for this profile.
    ///
    /// `None` when the profile has not been provisioned with a verified
    /// remote binary; the caller should surface that as "run machine enable".
    pub fn ssh_target(&self) -> Option<SshTarget> {
        if self.destination.is_empty() || self.remote_binary.is_empty() {
            return None;
        }
        Some(SshTarget {
            destination: self.destination.clone(),
            binary: self.remote_binary.clone(),
            session: self.remote_session(),
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_str()?.to_string(),
            display: value
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            destination: value
                .get("destination")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            enabled: value
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            online: None,
            latency_ms: None,
            version: None,
            remote_binary: value
                .get("remote_binary")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            preferred_session: value
                .get("preferred_session")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_string),
            automatic_provisioning: value
                .get("automatic_provisioning")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// Attach to a saved machine over its SSH bridge.
///
/// This mirrors the reference client: one `ssh -T` process per enabled
/// machine, carrying the ordinary binary render protocol. The bridge starts
/// the selected remote session when it is absent and never installs software.
/// A protocol mismatch rebuilds the SSH bridge at the other supported
/// revision, so an older remote daemon still attaches.
pub fn connect_machine(machine: &MachineInfo, cols: u16, rows: u16) -> Result<Connection> {
    let target = machine.ssh_target().ok_or_else(|| {
        Error::Machine(format!(
            "machine `{}` has no verified remote binary; run `luvus machine enable {}`",
            machine.id, machine.id
        ))
    })?;
    let label = machine.display().to_string();
    Connection::attach_negotiated(&label, cols, rows, || {
        let mut command = target.command(&["remote-client-bridge"]);
        Stream::spawn(&mut command).map_err(|error| {
            Error::Machine(format!("could not launch ssh for {}: {error}", machine.id))
        })
    })
}

/// Restart the remote daemon so it loads the binary the profile now points at.
///
/// Runs `<remote_binary> server restart` over SSH. After `machine enable
/// --install` the profile's `remote_binary` is the freshly provisioned copy,
/// so restarting through it is what moves a running daemon off the old
/// release. This stops every pane in that remote session; callers confirm
/// first.
pub fn restart_remote(machine: &MachineInfo) -> Result<String> {
    remote_server(machine, "restart")
}

/// Stop the remote daemon through the profile's current binary.
///
/// `luvus machine enable --install` finishes by handshaking with whatever
/// server is already running there, and a daemon older than this client
/// rejects that handshake ("protocol version mismatch") even though the new
/// binary is fine. Stopping the old daemon first leaves nothing to reject, so
/// the install can start a fresh server from the new binary. Callers confirm
/// first: it stops every pane in that remote session.
pub fn stop_remote(machine: &MachineInfo) -> Result<String> {
    let output = remote_server(machine, "stop")?;
    clear_stale_sockets(machine);
    Ok(output)
}

/// Remove the socket files an old daemon left behind after `server stop`.
///
/// The 0.14.2 binary leaves `luvus-client.sock` and `luvus.sock` in place. A
/// newer `remote-client-bridge --start-if-missing` then finds a socket with
/// nothing listening, refuses to start a server ("Connection refused ... no
/// server was started"), and SSH only reports the connection closed. The
/// files are removed only when `server status` says the daemon is not running.
fn clear_stale_sockets(machine: &MachineInfo) {
    let Some(target) = machine.ssh_target() else {
        return;
    };
    let Ok(status) = target.command(&["server", "status"]).output() else {
        return;
    };
    let status = String::from_utf8_lossy(&status.stdout);
    if !status.contains("not running") {
        return;
    }
    let Some(socket) = status_socket(&status) else {
        return;
    };
    let dir = socket.rsplit_once('/').map_or(".", |(dir, _)| dir);
    let _ = run_ssh(
        &target,
        &format!(
            "rm -f {} {}",
            shell_quote(&format!("{dir}/luvus-client.sock")),
            shell_quote(&format!("{dir}/luvus.sock"))
        ),
    );
}

/// The client socket path printed by `luvus server status`.
fn status_socket(status: &str) -> Option<String> {
    status
        .split_whitespace()
        .find(|token| token.starts_with('/') && token.ends_with("/luvus-client.sock"))
        .map(str::to_string)
}

fn remote_server(machine: &MachineInfo, verb: &str) -> Result<String> {
    let target = machine.ssh_target().ok_or_else(|| {
        Error::Machine(format!(
            "machine `{}` has no verified remote binary; run `luvus machine enable {}`",
            machine.id, machine.id
        ))
    })?;
    let output = target.command(&["server", verb]).output()?;
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_string();
    if !output.status.success() {
        let stderr = text(&output.stderr);
        return Err(Error::Machine(if stderr.is_empty() {
            format!("could not {verb} the daemon on `{}`", machine.id)
        } else {
            stderr
        }));
    }
    Ok(text(&output.stdout))
}

/// The provisioned `luvus` for `version` on the machine, when one exists.
///
/// `machine add --install` can copy a binary into the versioned remote
/// directory and still fail its closing handshake, leaving the install behind
/// with no profile pointing at it. This finds that copy so the profile can be
/// re-added against it.
pub fn find_installed_binary(machine: &MachineInfo, version: &str) -> Option<String> {
    let target = machine.ssh_target()?;
    // The version lands in a remote shell command, so refuse anything but a
    // plain version string.
    if version.is_empty()
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return None;
    }
    let found = run_ssh(
        &target,
        &format!(
            "ls -d \"$HOME\"/.local/share/luvus/remote/v{version}/*/luvus 2>/dev/null | head -n 1"
        ),
    )
    .ok()?;
    let path = found.trim();
    path.starts_with('/').then(|| path.to_string())
}

/// Whether `remote` is an older release than `local`.
///
/// Compares dotted numeric versions (a leading `v` and any `-pre`/`+build`
/// suffix are ignored). Anything unparseable is not "behind": an unknown
/// version should not nag the user to upgrade.
pub fn is_behind(remote: &str, local: &str) -> bool {
    fn parse(version: &str) -> Option<Vec<u64>> {
        let core = version.trim().trim_start_matches('v');
        let core = core.split(['-', '+']).next()?;
        core.split('.').map(|part| part.parse().ok()).collect()
    }
    match (parse(remote), parse(local)) {
        (Some(remote), Some(local)) => remote < local,
        _ => false,
    }
}

/// List profiles under `root` and probe each one's reachability.
///
/// Probes run in parallel with an overall deadline; anything unanswered in
/// time keeps `online: None`. The local machine always comes first: its
/// control plane answers over the root's own socket.
pub fn refresh_machines(root: &Path) -> Vec<MachineInfo> {
    let mut machines = list_machines(root);
    machines.insert(0, local_machine());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let (tx, rx) = std::sync::mpsc::channel();
    for (index, machine) in machines.iter().enumerate() {
        let tx = tx.clone();
        let root = root.to_path_buf();
        let id = machine.id.clone();
        let local = index == 0;
        std::thread::spawn(move || {
            let (online, latency_ms, version) = if local {
                probe_local(&root)
            } else {
                probe_remote(&root, &id)
            };
            tx.send((index, online, latency_ms, version)).ok();
        });
    }
    drop(tx);
    while machines.iter().any(|machine| machine.online.is_none()) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok((index, online, latency_ms, version)) => {
                if let Some(machine) = machines.get_mut(index) {
                    machine.online = Some(online);
                    machine.latency_ms = latency_ms;
                    machine.version = version;
                }
            }
            Err(_) => break,
        }
    }
    machines
}

/// The local machine row: hostname plus a control-plane round trip.
fn local_machine() -> MachineInfo {
    MachineInfo {
        id: "local".to_string(),
        display: "Local".to_string(),
        destination: local_hostname(),
        enabled: true,
        online: None,
        latency_ms: None,
        version: None,
        remote_binary: String::new(),
        preferred_session: None,
        automatic_provisioning: false,
    }
}

fn local_hostname() -> String {
    std::process::Command::new("/bin/hostname")
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this machine".to_string())
}

/// Time a control round trip on the root's own socket and read its version.
fn probe_local(root: &Path) -> (bool, Option<u64>, Option<String>) {
    let socket = root.join("luvus.sock");
    let start = std::time::Instant::now();
    let ping = crate::control::ControlClient::connect(&socket)
        .and_then(|mut client| client.request("ping", serde_json::json!({})));
    let online = ping.is_ok();
    let version = ping.ok().and_then(|value| {
        value
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    (
        online,
        online.then(|| start.elapsed().as_millis() as u64),
        version,
    )
}

/// Time a bounded `machine status` probe over SSH and read its version.
fn probe_remote(root: &Path, id: &str) -> (bool, Option<u64>, Option<String>) {
    let Some(binary) = crate::discovery::luvus_binary() else {
        return (false, None, None);
    };
    let start = std::time::Instant::now();
    let output = std::process::Command::new(binary)
        .arg("machine")
        .arg("status")
        .arg(id)
        .env(crate::discovery::HOME_ENV, root)
        .output();
    let value = match output {
        Ok(output) if output.status.success() => {
            serde_json::from_slice::<Value>(&output.stdout).ok()
        }
        _ => None,
    };
    let online = match &value {
        Some(value) => value
            .get("state")
            .and_then(Value::as_str)
            .is_none_or(|state| state == "online"),
        None => false,
    };
    let version = value
        .as_ref()
        .and_then(|value| value.get("probe"))
        .and_then(|probe| probe.get("version"))
        .and_then(Value::as_str)
        .map(str::to_string);
    (
        online,
        online.then(|| start.elapsed().as_millis() as u64),
        version,
    )
}

/// List machine profiles under `root` via the CLI, without probing.
///
/// Fast enough for first paint; [`refresh_machines`] adds reachability.
pub fn list_machines(root: &Path) -> Vec<MachineInfo> {
    let Some(binary) = crate::discovery::luvus_binary() else {
        return Vec::new();
    };
    let output = std::process::Command::new(binary)
        .arg("machine")
        .arg("list")
        .arg("--json")
        .env(crate::discovery::HOME_ENV, root)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_machines(&output.stdout)
}

fn parse_machines(bytes: &[u8]) -> Vec<MachineInfo> {
    let value: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let mut machines: Vec<MachineInfo> = value
        .get("machines")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(MachineInfo::from_json)
        .collect();
    machines.sort_by(|a, b| a.display().cmp(b.display()));
    machines
}

/// Run one shell command on the machine and return its stdout.
fn run_ssh(target: &SshTarget, remote_command: &str) -> Result<String> {
    let output = target.shell(remote_command).output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(Error::Machine(if stderr.is_empty() {
            format!("`{remote_command}` failed on {}", target.destination)
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Single-quote a path for the remote POSIX shell.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_profiles_with_labels() {
        let value = json!({
            "machines": [
                {
                    "id": "mac-mini",
                    "label": "Mac Mini",
                    "destination": "rizaldy@mac-mini.duck-map.ts.net",
                    "enabled": true,
                },
                {"id": "lab", "enabled": false},
            ],
            "revision": 1,
        });
        let machines = parse_machines(value.to_string().as_bytes());
        assert_eq!(machines.len(), 2);
        assert_eq!(machines[0].display(), "Mac Mini");
        assert_eq!(machines[0].destination, "rizaldy@mac-mini.duck-map.ts.net");
        assert!(machines[0].enabled);
        assert_eq!(machines[1].display(), "lab");
        assert!(!machines[1].enabled);
    }

    #[test]
    fn status_socket_reads_the_client_socket_path() {
        let status = "\\   /   Luvus server\n \\_/    status   not running\n(o_o)   session  zz-old\n/|_|\\   socket   /Users/x/.luvus/sessions/zz-old/luvus-client.sock\n";
        assert_eq!(
            status_socket(status).as_deref(),
            Some("/Users/x/.luvus/sessions/zz-old/luvus-client.sock")
        );
        assert_eq!(status_socket("no path here"), None);
    }

    #[test]
    fn is_behind_compares_numerically() {
        assert!(is_behind("0.14.2", "0.15.0"));
        assert!(is_behind("v0.9.0", "0.10.0"));
        assert!(!is_behind("0.15.0", "0.15.0"));
        assert!(!is_behind("0.16.0", "0.15.9"));
        assert!(!is_behind("unknown", "0.15.0"));
    }

    #[test]
    fn garbage_yields_no_machines() {
        assert!(parse_machines(b"not json").is_empty());
        assert!(parse_machines(b"{}").is_empty());
    }

    #[test]
    fn shell_quote_survives_spaces_and_quotes() {
        assert_eq!(
            shell_quote("/opt/my agent/pilus-agent"),
            "'/opt/my agent/pilus-agent'"
        );
        assert_eq!(shell_quote("/a'b"), "'/a'\\''b'");
    }
}
