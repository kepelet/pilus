//! Locating the `pilus-agent` binary.
//!
//! Remote access is the background service `pilus-agent service` installs; see
//! [`crate::agent_service`] for how the window drives and reads it. This module
//! only answers "where is the agent binary", for the window and for the
//! service installer.

use std::path::PathBuf;

/// Environment override for the connector binary.
pub const AGENT_BIN_ENV: &str = "PILUS_AGENT_BIN";

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
