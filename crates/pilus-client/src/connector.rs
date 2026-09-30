//! Local lifecycle for the "this node" connector.
//!
//! "Enable this node" starts `pilus-agent serve --iroh`, the host-side
//! connector that exposes this Luvus session to remote UHP clients. The window
//! owns the child, so closing the app stops the listener; a headless machine
//! runs the agent's own service mode instead.
//!
//! The agent writes its `endpoint id` and enrollment `ticket` to stdout as it
//! comes online; a reader thread folds those lines into [`ConnectorStatus`],
//! which the window polls without blocking on iroh startup.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// Environment override for the connector binary.
pub const AGENT_BIN_ENV: &str = "PILUS_AGENT_BIN";

/// How the connector is started.
#[derive(Clone, Debug, Default)]
pub struct ConnectorOptions {
    /// Advertise the bounded control surface (`--control`).
    pub control: bool,
    /// Advertise the owner-local machine catalog (`--machines`).
    pub machines: bool,
    /// Named session to expose; `None` is the default session.
    pub session: Option<String>,
    /// Persistent identity key path; `None` lets the agent default it.
    pub identity: Option<PathBuf>,
    /// Self-hosted relay URL instead of the n0 public relays.
    pub relay: Option<String>,
}

/// The connector's observable state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConnectorStatus {
    /// Whether the agent process is still running.
    pub running: bool,
    /// The host's stable iroh `EndpointId`, once online.
    pub endpoint_id: Option<String>,
    /// The enrollment ticket a device scans, once online.
    pub ticket: Option<String>,
    /// The short-lived code that admits a new device.
    pub enrollment: Option<String>,
    /// The failure to show when the agent exits.
    pub error: Option<String>,
}

/// A locally running connector child.
pub struct Connector {
    child: Child,
    status: Arc<Mutex<ConnectorStatus>>,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Connector {
    /// Start the connector for this node.
    pub fn start(options: &ConnectorOptions) -> Result<Self> {
        let binary = agent_binary().ok_or_else(|| {
            Error::Connector(
                "pilus-agent not found; set PILUS_AGENT_BIN or install it beside pilus".to_string(),
            )
        })?;
        let mut command = Command::new(binary);
        command.arg("serve").arg("--iroh");
        if options.control {
            command.arg("--control");
        }
        if options.machines {
            command.arg("--machines");
        }
        if let Some(session) = &options.session {
            command.arg("--session").arg(session);
        }
        if let Some(identity) = &options.identity {
            command.arg("--identity").arg(identity);
        }
        if let Some(relay) = &options.relay {
            command.arg("--relay").arg(relay);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // The agent spawns `luvus uhp access`; a dedicated group lets
            // [`Connector::stop`] take both down together instead of orphaning
            // the gateway when the agent is killed.
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Connector("the agent produced no stdout".to_string()))?;
        let status = Arc::new(Mutex::new(ConnectorStatus {
            running: true,
            ..Default::default()
        }));
        let reader_status = Arc::clone(&status);
        thread::Builder::new()
            .name("pilus-connector-stdout".to_string())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                    apply_line(&mut lock(&reader_status), &line);
                    line.clear();
                }
            })
            .ok();

        let stderr = Arc::new(Mutex::new(Vec::new()));
        if let Some(pipe) = child.stderr.take() {
            let tail = Arc::clone(&stderr);
            thread::Builder::new()
                .name("pilus-connector-stderr".to_string())
                .spawn(move || {
                    let mut reader = BufReader::new(pipe);
                    let mut line = String::new();
                    while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                        let text = line.trim().to_string();
                        if !text.is_empty() {
                            let mut guard = lock(&tail);
                            guard.push(text);
                            if guard.len() > 40 {
                                guard.remove(0);
                            }
                        }
                        line.clear();
                    }
                })
                .ok();
        }
        Ok(Self {
            child,
            status,
            stderr,
        })
    }

    /// Read the current state, folding in an exited child.
    pub fn status(&mut self) -> ConnectorStatus {
        if let Ok(Some(exit)) = self.child.try_wait() {
            let mut guard = lock(&self.status);
            if guard.running {
                guard.running = false;
                if !exit.success() && guard.error.is_none() {
                    let tail = lock(&self.stderr);
                    guard.error = Some(
                        tail.last()
                            .cloned()
                            .unwrap_or_else(|| format!("pilus-agent exited with {exit}")),
                    );
                }
            }
        }
        lock(&self.status).clone()
    }

    /// Stop the connector and reap the child.
    pub fn stop(&mut self) {
        #[cfg(unix)]
        {
            // Signal the whole group so the agent's `luvus uhp access` child
            // dies with it; a bare SIGKILL would orphan the gateway.
            let _ = Command::new("/bin/kill")
                .arg("-TERM")
                .arg(format!("-{}", self.child.id()))
                .status();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                _ if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
                _ => thread::sleep(Duration::from_millis(50)),
            }
        }
        lock(&self.status).running = false;
    }
}

impl Drop for Connector {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Where the connector binary lives: `$PILUS_AGENT_BIN`, a sibling of the
/// running `pilus`, then `PATH`.
pub fn agent_binary() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os(AGENT_BIN_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(custom));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(agent_name());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // Beside the `luvus` CLI, where the rest of the tooling usually lives.
    if let Some(luvus) = crate::discovery::luvus_binary()
        && let Some(dir) = luvus.parent()
    {
        let candidate = dir.join(agent_name());
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(agent_name()))
        .find(|candidate| candidate.is_file())
}

fn agent_name() -> &'static str {
    if cfg!(windows) {
        "pilus-agent.exe"
    } else {
        "pilus-agent"
    }
}

/// Fold one agent stdout line into the status.
fn apply_line(status: &mut ConnectorStatus, line: &str) {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("endpoint id:") {
        status.endpoint_id = Some(rest.trim().to_string());
    } else if let Some(rest) = line.strip_prefix("ticket:") {
        status.ticket = Some(rest.trim().to_string());
    } else if let Some(rest) = line.strip_prefix("enroll:") {
        status.enrollment = Some(rest.trim().to_string());
    }
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
    fn folds_agent_output_into_status() {
        let mut status = ConnectorStatus::default();
        apply_line(
            &mut status,
            "endpoint id: ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6\n",
        );
        apply_line(&mut status, "ticket:      endpointad5dexample\n");
        apply_line(&mut status, "enroll:      A1B2-C3D4-E5F6-0718\n");
        apply_line(&mut status, "unrelated log line\n");
        assert_eq!(
            status.endpoint_id.as_deref(),
            Some("ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6")
        );
        assert_eq!(status.ticket.as_deref(), Some("endpointad5dexample"));
        assert_eq!(status.enrollment.as_deref(), Some("A1B2-C3D4-E5F6-0718"));
        assert!(status.error.is_none());
    }
}
