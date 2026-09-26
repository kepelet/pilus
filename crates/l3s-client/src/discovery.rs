//! Locating a Luvus daemon on this machine.
//!
//! The layout mirrors the daemon's own state directory:
//!
//! ```text
//! $LUVUS_HOME/                       default session
//!   luvus-client.sock                  the render socket l3s attaches to
//!   luvus.sock                         the JSON control socket (CLI/UHP)
//!   sessions/<name>/                 a named session
//! ```
//!
//! `$LUVUS_HOME` overrides the root; otherwise `~/.luvus` is used, or
//! `~/.luvus-dev` for a debug build.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Environment override for the state root.
pub const HOME_ENV: &str = "LUVUS_HOME";
/// Environment variable naming the active session.
pub const SESSION_ENV: &str = "LUVUS_SESSION";
/// The session used when none is named.
pub const DEFAULT_SESSION: &str = "default";
/// Longest accepted session name, in bytes.
const MAX_SESSION_NAME_LEN: usize = 64;

/// The state root for this process.
pub fn config_dir() -> PathBuf {
    if let Some(value) = std::env::var_os(HOME_ENV)
        && !value.is_empty()
    {
        return PathBuf::from(value);
    }
    let name = if cfg!(debug_assertions) {
        ".luvus-dev"
    } else {
        ".luvus"
    };
    home_dir().join(name)
}

/// The runtime directory for a session (`None` is the default session).
pub fn session_dir_for(session: Option<&str>) -> PathBuf {
    match session {
        Some(name) => config_dir().join("sessions").join(name),
        None => config_dir(),
    }
}

/// The binary render socket a client attaches to for a session.
pub fn client_socket_path_for(session: Option<&str>) -> PathBuf {
    socket_path_for(session, "luvus-client.sock", "client")
}

/// The JSON control socket (the CLI and UHP surface).
pub fn api_socket_path_for(session: Option<&str>) -> PathBuf {
    socket_path_for(session, "luvus.sock", "api")
}

fn socket_path_for(session: Option<&str>, file_name: &str, role: &str) -> PathBuf {
    let logical = session_dir_for(session).join(file_name);
    alias_for(logical, "luvus", role)
}

/// Validate a session name the way the daemon does.
pub fn validate_session(name: &str) -> Result<()> {
    if name == DEFAULT_SESSION {
        return Ok(());
    }
    if name.is_empty() || name.len() > MAX_SESSION_NAME_LEN {
        return Err(Error::InvalidSession(name.to_string()));
    }
    if matches!(name, "." | "..")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(Error::InvalidSession(name.to_string()));
    }
    Ok(())
}

/// The active session named by `$LUVUS_SESSION`, if any.
pub fn active_session() -> Option<String> {
    std::env::var_os(SESSION_ENV)
        .and_then(|value| value.into_string().ok())
        .filter(|name| name != DEFAULT_SESSION)
        .filter(|name| validate_session(name).is_ok())
}

/// Named sessions with a socket that currently exists on disk.
pub fn list_sessions() -> Vec<String> {
    let root = config_dir().join("sessions");
    let mut found: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| validate_session(name).is_ok())
        .collect();
    found.sort();
    found
}

fn home_dir() -> PathBuf {
    #[cfg(unix)]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_default()
    }
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_default()
    }
}

/// macOS caps `sun_path` at 103 bytes plus a NUL, so a long application home
/// moves the socket to a stable, owner-scoped alias under the temporary
/// directory. The hash and resulting filename match the daemon's.
#[cfg(unix)]
fn alias_for(logical: PathBuf, namespace: &str, role: &str) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;

    const SUN_PATH_BUDGET: usize = 100;
    if logical.as_os_str().as_bytes().len() < SUN_PATH_BUDGET {
        return logical;
    }
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in logical.as_os_str().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let root = if cfg!(target_os = "macos") {
        "/private/tmp"
    } else {
        "/tmp"
    };
    PathBuf::from(format!(
        "{root}/{namespace}-{}/{:016x}-{role}.sock",
        current_uid(),
        hash
    ))
}

#[cfg(not(unix))]
fn alias_for(logical: PathBuf, _namespace: &str, _role: &str) -> PathBuf {
    logical
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn current_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, has no side effects, and is
    // async-signal-safe; it always succeeds.
    unsafe { libc::geteuid() }
}

/// Where a target's socket lives, for diagnostics.
pub fn describe(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_named_socket_layout() {
        let default = client_socket_path_for(None);
        assert!(default.ends_with("luvus-client.sock"));
        let named = client_socket_path_for(Some("work"));
        assert!(named.to_string_lossy().contains("sessions/work"));
    }

    #[test]
    fn rejects_unsafe_session_names() {
        assert!(validate_session("good-name_1").is_ok());
        assert!(validate_session("").is_err());
        assert!(validate_session("../escape").is_err());
        assert!(validate_session("has space").is_err());
    }
}
