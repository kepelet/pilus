//! Saved SSH machine profiles, read through the `luvus` CLI.
//!
//! The daemon exposes no control method for machines; profiles live in the
//! state root, so `luvus machine list --json` is the stable reader. Failures
//! yield an empty list: machines are sidebar chrome, and a broken profile
//! must never disturb the window.

use serde_json::Value;
use std::path::Path;

/// One saved machine profile plus its live reachability.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MachineInfo {
    /// Stable profile id.
    pub id: String,
    /// Human label; may be empty.
    pub label: String,
    /// SSH destination, e.g. `user@host`.
    pub destination: String,
    /// Whether the profile is enabled.
    pub enabled: bool,
    /// Reachability from the last probe; `None` means not probed yet.
    pub online: Option<bool>,
    /// Probe round-trip in milliseconds, when measured.
    pub latency_ms: Option<u64>,
}

impl MachineInfo {
    /// The name to draw: the label, else the id.
    pub fn display(&self) -> &str {
        if self.label.is_empty() {
            &self.id
        } else {
            &self.label
        }
    }

    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_str()?.to_string(),
            label: value
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
        })
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
        label: "Local".to_string(),
        destination: local_hostname(),
        enabled: true,
        online: None,
        latency_ms: None,
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
}
