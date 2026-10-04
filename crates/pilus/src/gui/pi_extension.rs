//! Installing the pi session-report extension.
//!
//! Pi writes its session file with `appendFileSync` and holds no open file
//! descriptor, so nothing outside the process can see which session a pane
//! is in. The bundled extension reports `pane.report_session` to the daemon
//! on every `session_start`; pilus drops it into pi's extensions dir on each
//! machine it serves, locally and over SSH, so every pi pane started after
//! the install can be painted natively.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use pilus_client::SshTarget;

/// The file name inside pi's `extensions` dir.
const FILE_NAME: &str = "pilus-session.ts";
/// The extension source, embedded so an install never reads the disk.
const SOURCE: &str = include_str!("../../assets/pi/pilus-session.ts");

/// The POSIX script [`install_remote`] runs; `shell` pins `/bin/sh`, so
/// `${VAR:-default}` expands on every login shell. `cat >/dev/null` on the
/// early exit drains our piped stdin so ssh does not see a broken pipe.
const REMOTE_COMMAND: &str = "dir=\"${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}\"; \
    [ -d \"$dir\" ] || { cat >/dev/null; exit 0; }; \
    mkdir -p \"$dir/extensions\" && \
    cat > \"$dir/extensions/pilus-session.ts.tmp\" && \
    mv \"$dir/extensions/pilus-session.ts.tmp\" \"$dir/extensions/pilus-session.ts\"";

/// Pi's agent directory, shared with the RPC socket layout in
/// `pilus-session` so both agree where the extension lives.
fn agent_dir() -> PathBuf {
    pilus_session::agent_dir()
}

/// Install the extension into `agent_dir`'s `extensions` dir.
///
/// Returns whether a file was written. A missing agent dir means pi is not
/// installed there, so nothing is created; an already-current file is left
/// alone. The write goes through a temp file and a rename so pi, which loads
/// extensions at startup, never reads a half-written file.
fn install_into(agent_dir: &Path) -> io::Result<bool> {
    if !agent_dir.is_dir() {
        return Ok(false);
    }
    let extensions = agent_dir.join("extensions");
    let destination = extensions.join(FILE_NAME);
    if std::fs::read(&destination)
        .map(|bytes| bytes == SOURCE.as_bytes())
        .unwrap_or(false)
    {
        return Ok(false);
    }
    std::fs::create_dir_all(&extensions)?;
    let temp = extensions.join(format!("{FILE_NAME}.tmp"));
    std::fs::write(&temp, SOURCE)?;
    std::fs::rename(&temp, &destination)?;
    Ok(true)
}

/// Install the extension into this machine's pi agent dir.
pub fn install_local() -> io::Result<bool> {
    install_into(&agent_dir())
}

/// Install the extension on `target`, streaming the source over SSH stdin.
///
/// A host without a pi agent dir exits 0 without creating anything, so an
/// SSH box that has never run pi stays untouched.
pub fn install_remote(target: &SshTarget) -> Result<(), String> {
    let mut child = target
        .shell(REMOTE_COMMAND)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not reach {}: {error}", target.destination))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(SOURCE.as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("extension install on {} failed", target.destination)
    } else {
        stderr
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pilus-pi-ext-{name}-{}", std::process::id()))
    }

    #[test]
    fn missing_agent_dir_installs_nothing() {
        let root = temp_root("missing");
        assert!(!install_into(&root).expect("install"));
        assert!(!root.exists(), "no dirs may be created");
    }

    #[test]
    fn installs_and_then_reports_clean() {
        let root = temp_root("fresh");
        std::fs::create_dir_all(&root).expect("agent dir");
        assert!(install_into(&root).expect("install"));
        let written = root.join("extensions").join(FILE_NAME);
        assert_eq!(std::fs::read(&written).expect("read"), SOURCE.as_bytes());
        // A repeat call sees the current file and writes nothing.
        assert!(!install_into(&root).expect("reinstall"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_content_is_rewritten() {
        let root = temp_root("stale");
        let extensions = root.join("extensions");
        std::fs::create_dir_all(&extensions).expect("extensions dir");
        let file = extensions.join(FILE_NAME);
        std::fs::write(&file, "// an older build\n").expect("write stale");
        assert!(install_into(&root).expect("install"));
        assert_eq!(std::fs::read(&file).expect("read"), SOURCE.as_bytes());
        let _ = std::fs::remove_dir_all(&root);
    }
}
