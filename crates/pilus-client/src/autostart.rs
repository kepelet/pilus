//! Starting a Luvus daemon on demand.
//!
//! `connect` targets a render socket; when nothing listens there the daemon
//! simply isn't running for that session. Rather than surface "no server",
//! start it ourselves with `luvus [--session NAME] server start` (the same
//! command the GUI already used for fresh sessions) and wait for the socket.
//!
//! The child inherits no shell and no `PATH` lookup: the binary is located
//! with [`luvus_binary`], and `LUVUS_HOME` pins it to the same state root the
//! target resolves under, so a debug build's `~/.luvus-dev` doesn't start a
//! daemon the release layout would never see.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::ConnectTarget;
use crate::discovery::{HOME_ENV, client_socket_path_under, config_dir, luvus_binary};
use crate::error::{Error, Result};

/// How long to wait for a freshly started server's socket.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// Pause between socket polls while the server comes up.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Start the daemon for `target` if it isn't running, waiting for its socket.
///
/// Returns immediately when something already answers at the socket. An explicit socket
/// outside the `<root>/[sessions/<name>/]luvus-client.sock` layout names no
/// state root, so there is nothing to start: the original [`Error::NotFound`]
/// is returned unchanged.
pub fn ensure_server(target: &ConnectTarget) -> Result<()> {
    let (root, session) = scope_for(target)?;
    ensure_server_for(&root, session.as_deref())
}

/// Start the daemon for `session` (`None` is the default session) under the
/// state `root`, waiting for its render socket.
///
/// Returns immediately when something already answers at the socket. Fails with
/// [`Error::NotFound`] when no `luvus` binary can be located, and with
/// [`Error::ServerStart`] when the start command fails or the socket never
/// appears.
pub fn ensure_server_for(root: &Path, session: Option<&str>) -> Result<()> {
    let socket = client_socket_path_under(root, session);
    if is_live_socket(&socket) {
        return Ok(());
    }
    let binary = luvus_binary().ok_or_else(|| Error::NotFound {
        path: socket.clone(),
    })?;
    start_and_wait(&binary, root, session, &socket)
}

/// Run `binary server start` for `session` under `root`, then wait for
/// `socket`. Split out so tests can drive the failure paths with a binary
/// that never starts anything.
fn start_and_wait(binary: &Path, root: &Path, session: Option<&str>, socket: &Path) -> Result<()> {
    let mut command = std::process::Command::new(binary);
    if let Some(name) = session {
        command.arg("--session").arg(name);
    }
    let output = command
        .arg("server")
        .arg("start")
        .env(HOME_ENV, root)
        .output()
        .map_err(|error| Error::ServerStart(error.to_string()))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if detail.is_empty() {
            "server start failed".to_string()
        } else {
            detail
        };
        return Err(Error::ServerStart(detail));
    }
    let deadline = Instant::now() + START_TIMEOUT;
    while !is_live_socket(socket) {
        if Instant::now() >= deadline {
            return Err(Error::ServerStart(format!(
                "server did not create {}",
                socket.display()
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    tracing::info!(path = %socket.display(), "started Luvus server");
    Ok(())
}

/// Whether something answers at `path`.
///
/// A stale socket file left behind by a stopped server must not count as
/// live, so this dials rather than checking existence.
fn is_live_socket(path: &Path) -> bool {
    crate::transport::Stream::connect(path).is_ok()
}

/// The state root and session a target's daemon would live under.
///
/// Exported for tests; prefer [`ensure_server`].
fn scope_for(target: &ConnectTarget) -> Result<(PathBuf, Option<String>)> {
    match &target.socket {
        Some(path) => Ok(crate::discovery::split_client_socket(path)
            .ok_or_else(|| Error::NotFound { path: path.clone() })?),
        None => Ok((config_dir(), target.session.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_under_matches_short_layout() {
        let root = Path::new("/tmp/pilus-autostart-test-root");
        assert_eq!(
            client_socket_path_under(root, None),
            root.join("luvus-client.sock")
        );
        assert_eq!(
            client_socket_path_under(root, Some("work")),
            root.join("sessions").join("work").join("luvus-client.sock")
        );
    }

    #[test]
    fn explicit_socket_outside_layout_is_not_started() {
        let target = ConnectTarget::socket("/tmp/definitely-not-a-luvus-state/some.sock");
        let error = ensure_server(&target).expect_err("nothing to start here");
        assert!(matches!(error, Error::NotFound { .. }), "{error:?}");
    }

    #[test]
    fn scope_for_session_target_uses_config_dir() {
        let target = ConnectTarget::session("work");
        let (root, session) = scope_for(&target).expect("session scope");
        assert_eq!(root, config_dir());
        assert_eq!(session.as_deref(), Some("work"));
    }

    #[test]
    fn failed_start_surfaces_a_start_error() {
        // `/usr/bin/false` exits 1 without printing, exercising the
        // empty-stderr fallback.
        let error = start_and_wait(
            Path::new("/usr/bin/false"),
            Path::new("/tmp/pilus-autostart-no-daemon"),
            None,
            Path::new("/tmp/pilus-autostart-no-daemon/luvus-client.sock"),
        )
        .expect_err("false always fails");
        assert!(matches!(error, Error::ServerStart(_)), "{error:?}");
    }

    #[test]
    fn missing_binary_surfaces_a_start_error() {
        let error = start_and_wait(
            Path::new("/tmp/pilus-autostart-no-such-binary"),
            Path::new("/tmp/pilus-autostart-no-daemon"),
            None,
            Path::new("/tmp/pilus-autostart-no-daemon/luvus-client.sock"),
        )
        .expect_err("spawning nothing always fails");
        assert!(matches!(error, Error::ServerStart(_)), "{error:?}");
    }
}
