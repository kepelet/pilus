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
pub fn connect_machine(machine: &MachineInfo, cols: u16, rows: u16) -> Result<Connection> {
    let target = machine.ssh_target().ok_or_else(|| {
        Error::Machine(format!(
            "machine `{}` has no verified remote binary; run `luvus machine enable {}`",
            machine.id, machine.id
        ))
    })?;
    let mut command = target.command(&["remote-client-bridge"]);
    let stream = Stream::spawn(&mut command).map_err(|error| {
        Error::Machine(format!("could not launch ssh for {}: {error}", machine.id))
    })?;
    Connection::open_stream(stream, machine.display().to_string(), cols, rows)
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
            let (online, latency_ms) = if local {
                probe_local(&root)
            } else {
                probe_remote(&root, &id)
            };
            tx.send((index, online, latency_ms)).ok();
        });
    }
    drop(tx);
    while machines.iter().any(|machine| machine.online.is_none()) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok((index, online, latency_ms)) => {
                if let Some(machine) = machines.get_mut(index) {
                    machine.online = Some(online);
                    machine.latency_ms = latency_ms;
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

/// Time a control round trip on the root's own socket.
fn probe_local(root: &Path) -> (bool, Option<u64>) {
    let socket = root.join("luvus.sock");
    let start = std::time::Instant::now();
    let online = crate::control::ControlClient::connect(&socket)
        .and_then(|mut client| client.request("tab.list", serde_json::json!({})))
        .is_ok();
    (online, online.then(|| start.elapsed().as_millis() as u64))
}

/// Time a bounded `machine status` probe over SSH.
fn probe_remote(root: &Path, id: &str) -> (bool, Option<u64>) {
    let Some(binary) = crate::discovery::luvus_binary() else {
        return (false, None);
    };
    let start = std::time::Instant::now();
    let output = std::process::Command::new(binary)
        .arg("machine")
        .arg("status")
        .arg(id)
        .env(crate::discovery::HOME_ENV, root)
        .output();
    let online = match output {
        Ok(output) if output.status.success() => serde_json::from_slice::<Value>(&output.stdout)
            .ok()
            .and_then(|value| {
                value
                    .get("state")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .is_none_or(|state| state == "online"),
        _ => false,
    };
    (online, online.then(|| start.elapsed().as_millis() as u64))
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

/// Copy this build's connector onto `machine` and start it.
///
/// The agent is copied verbatim, so a remote platform that does not match this
/// build is refused before any bytes move. The remote path mirrors the verified
/// `luvus` binary's directory, so the two ship together.
pub fn install_agent(machine: &MachineInfo, agent: &Path) -> Result<String> {
    let target = machine.ssh_target().ok_or_else(|| {
        Error::Machine(format!(
            "machine `{}` has no verified remote binary; run `luvus machine enable {}`",
            machine.id, machine.id
        ))
    })?;
    let remote = run_ssh(&target, "uname -sm")?;
    let remote = remote.trim();
    let local = local_uname();
    if remote != local {
        return Err(Error::Machine(format!(
            "`{}` runs `{remote}`, this build is `{local}`; install a matching pilus-agent there",
            machine.id
        )));
    }
    let remote_dir = target
        .binary
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or(".");
    let remote_path = format!("{remote_dir}/pilus-agent");
    copy_agent(agent, &machine.destination, &remote_path)?;
    let start = format!(
        "chmod +x {path} && (nohup {path} serve --iroh --control >/dev/null 2>&1 & echo $!)",
        path = shell_quote(&remote_path)
    );
    let pid = run_ssh(&target, &start)?;
    let pid = pid.trim();
    Ok(if pid.is_empty() {
        format!("Installed pilus-agent on `{}`.", machine.id)
    } else {
        format!("Installed and started pilus-agent on `{}` (pid {pid}).", machine.id)
    })
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

/// Copy a local file to `destination:path` with the same batch SSH options.
fn copy_agent(local: &Path, destination: &str, remote_path: &str) -> Result<()> {
    let output = std::process::Command::new("scp")
        .arg("-q")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg(local)
        .arg(format!("{destination}:{remote_path}"))
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(Error::Machine(if stderr.is_empty() {
            format!("could not copy the agent to {destination}")
        } else {
            stderr
        }));
    }
    Ok(())
}

/// The local `uname -s -m` equivalent, matching OpenSSH's remote output.
fn local_uname() -> String {
    let system = match std::env::consts::OS {
        "macos" => "Darwin",
        "linux" => "Linux",
        other => other,
    };
    let machine = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    };
    format!("{system} {machine}")
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
    fn garbage_yields_no_machines() {
        assert!(parse_machines(b"not json").is_empty());
        assert!(parse_machines(b"{}").is_empty());
    }

    #[test]
    fn shell_quote_survives_spaces_and_quotes() {
        assert_eq!(shell_quote("/opt/my agent/pilus-agent"), "'/opt/my agent/pilus-agent'");
        assert_eq!(shell_quote("/a'b"), "'/a'\\''b'");
    }

    #[test]
    fn local_uname_looks_like_uname() {
        let value = local_uname();
        let (system, machine) = value.split_once(' ').expect("two fields");
        assert!(!system.is_empty());
        assert!(!machine.is_empty());
    }
}
