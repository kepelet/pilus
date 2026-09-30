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
/// Environment override for the CLI binary used for session lifecycle.
pub const BIN_ENV: &str = "LUVUS_BIN";
/// Environment variable naming the active session.
pub const SESSION_ENV: &str = "LUVUS_SESSION";
/// The session used when none is named.
pub const DEFAULT_SESSION: &str = "default";
/// Longest accepted session name, in bytes.
const MAX_SESSION_NAME_LEN: usize = 64;

/// Environment override for l3s's own state directory.
pub const L3S_HOME_ENV: &str = "L3S_HOME";

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

/// The directory l3s owns for its stores, independent of the Luvus state root.
///
/// Projects, client settings, and personal workspaces live here so they
/// survive Luvus session changes and never mix with the daemon's own files.
pub fn l3s_config_dir() -> PathBuf {
    if let Some(value) = std::env::var_os(L3S_HOME_ENV)
        && !value.is_empty()
    {
        return PathBuf::from(value);
    }
    home_dir().join(".l3s")
}

/// The directory holding personal (project-less) workspaces.
pub fn l3s_workspaces_dir() -> PathBuf {
    l3s_config_dir().join("workspaces")
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

/// The binary render socket for `session` (`None` is the default session)
/// under an explicit state `root`, rather than this process's config dir.
///
/// The aliasing rules match [`client_socket_path_for`]; the two agree
/// whenever `root` is the config dir.
pub fn client_socket_path_under(root: &Path, session: Option<&str>) -> PathBuf {
    let dir = match session {
        Some(name) => root.join("sessions").join(name),
        None => root.to_path_buf(),
    };
    alias_for(dir.join("luvus-client.sock"), "luvus", "client")
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
pub(crate) fn current_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, has no side effects, and is
    // async-signal-safe; it always succeeds.
    unsafe { libc::geteuid() }
}

/// One Luvus session visible from a state root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionInfo {
    /// Session name; `"default"` for the default session.
    pub name: String,
    /// Whether its socket exists on disk, i.e. the server looks alive.
    pub running: bool,
}

/// Split an explicit client socket into its state root and session.
///
/// `<root>/luvus-client.sock` is the default session, while
/// `<root>/sessions/<name>/luvus-client.sock` is a named one. Returns `None`
/// when the path doesn't match either layout.
pub fn split_client_socket(path: &Path) -> Option<(PathBuf, Option<String>)> {
    let dir = path.parent()?;
    if dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| validate_session(name).is_ok())
        && dir
            .parent()
            .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "sessions"))
    {
        let name = dir.file_name()?.to_str()?.to_string();
        let root = dir.parent()?.parent()?.to_path_buf();
        return Some((root, Some(name)));
    }
    if dir.join("sessions").is_dir() || dir.join("luvus.sock").exists() {
        return Some((dir.to_path_buf(), None));
    }
    None
}

/// Every session under `root`: `default` first, then named ones sorted.
///
/// `running` mirrors `luvus session list` closely enough for a sidebar: the
/// session's socket exists on disk.
pub fn list_session_infos(root: &Path) -> Vec<SessionInfo> {
    let mut infos = vec![SessionInfo {
        name: DEFAULT_SESSION.to_string(),
        running: root.join("luvus-client.sock").exists() || root.join("luvus.sock").exists(),
    }];
    let mut named: Vec<String> = std::fs::read_dir(root.join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| validate_session(name).is_ok())
        .collect();
    named.sort();
    for name in named {
        let dir = root.join("sessions").join(&name);
        infos.push(SessionInfo {
            name,
            running: dir.join("luvus-client.sock").exists() || dir.join("luvus.sock").exists(),
        });
    }
    infos
}

/// Locate the `luvus` CLI for session lifecycle commands.
///
/// `$LUVUS_BIN` wins, then `PATH`, then the usual install spots. The GUI
/// bundle doesn't inherit a login shell's `PATH`, hence the fallbacks.
pub fn luvus_binary() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os(BIN_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(custom));
    }
    if let Some(dirs) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&dirs) {
            let candidate = dir.join("luvus");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        let home = PathBuf::from(home);
        for suffix in [".local/bin/luvus", ".cargo/bin/luvus"] {
            let candidate = home.join(suffix);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    ["/opt/homebrew/bin/luvus", "/usr/local/bin/luvus"]
        .into_iter()
        .map(PathBuf::from)
        .find(|candidate| candidate.is_file())
}

/// Generate a fresh `adjective-noun` session name, docker-style.
///
/// Skips names already present in `existing`, then falls back to a numeric
/// suffix, so creation never blocks on a collision.
pub fn generate_session_name(existing: &[String]) -> String {
    const ADJECTIVES: [&str; 20] = [
        "brave", "clever", "cosmic", "daring", "eager", "elite", "fancy", "gentle", "happy",
        "jolly", "kind", "lively", "lucky", "nimble", "noble", "proud", "quick", "quiet", "swift",
        "witty",
    ];
    const NOUNS: [&str; 20] = [
        "badger", "comet", "dancer", "energy", "falcon", "fan", "forest", "glider", "harbor",
        "kite", "lantern", "meadow", "nomad", "otter", "pilot", "quill", "rider", "sailor",
        "signal", "voyager",
    ];
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
        .wrapping_add(u128::from(std::process::id()));
    // A tiny LCG is plenty for picking words; no rng crate needed.
    let mut state = nanos | 1;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };
    for _ in 0..100 {
        let candidate = format!(
            "{}-{}",
            ADJECTIVES[next() % ADJECTIVES.len()],
            NOUNS[next() % NOUNS.len()]
        );
        if !existing.iter().any(|name| name == &candidate) {
            return candidate;
        }
    }
    let mut counter = 2u32;
    loop {
        let candidate = format!("session-{counter}");
        if !existing.iter().any(|name| name == &candidate) {
            return candidate;
        }
        counter = counter.saturating_add(1);
    }
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

    #[test]
    fn generated_session_names_avoid_collisions() {
        let existing = vec!["brave-falcon".to_string()];
        for _ in 0..50 {
            let name = generate_session_name(&existing);
            assert!(name.contains('-'));
            assert!(!existing.contains(&name));
        }
    }
}
