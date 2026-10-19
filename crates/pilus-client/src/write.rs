//! Saving a text file back where [`crate::read_text`] read it.
//!
//! The daemon's `files.*` methods never write, so a save goes straight to disk
//! for the local machine, or is piped over the saved SSH profile for a remote
//! one. A save replaces the contents of a file that exists; it never creates
//! one, so a file deleted or renamed behind the editor's back makes the save
//! fail instead of resurrecting it under the old name.
//!
//! A remote save lands in a temporary file beside the target first and is
//! copied over it only once every byte has arrived, so a connection that drops
//! halfway leaves the file as it was rather than cut short.

use std::fs;
use std::io::Write;
use std::process::Stdio;

use crate::git::sh_quote;
use crate::transport::SshTarget;

/// Replace the contents of the file at `path` with `text`, on this machine or
/// on `target`'s host.
pub fn write_text(target: Option<&SshTarget>, path: &str, text: &str) -> Result<(), String> {
    match target {
        None => {
            if !fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
                return Err("the file is gone".to_string());
            }
            fs::write(path, text).map_err(|error| error.to_string())
        }
        Some(target) => {
            let mut child = target
                .shell(&remote_save_script(path, text.len()))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| format!("couldn't reach the host: {error}"))?;
            if let Some(mut stdin) = child.stdin.take() {
                // A refused redirect closes the pipe early; the reason is on
                // stderr, which is the more useful error.
                let _ = stdin.write_all(text.as_bytes());
            }
            let output = child
                .wait_with_output()
                .map_err(|error| error.to_string())?;
            if output.status.success() {
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if stderr.is_empty() {
                "couldn't write the file".to_string()
            } else {
                stderr
            })
        }
    }
}

/// The host script for a remote save of `len` bytes read from stdin.
///
/// Copying the staged bytes over the target (rather than renaming the staged
/// file onto it) keeps the target's mode, owner, hard links and symlink.
fn remote_save_script(path: &str, len: usize) -> String {
    let quoted = sh_quote(path);
    format!(
        "f={quoted}\n\
         [ -f \"$f\" ] || {{ echo 'the file is gone' >&2; exit 1; }}\n\
         t=$(mktemp \"${{f%/*}}/.pilus-save.XXXXXX\" 2>/dev/null || mktemp) || exit 1\n\
         trap 'rm -f \"$t\"' EXIT\n\
         cat > \"$t\" || exit 1\n\
         [ \"$(wc -c < \"$t\" | tr -d ' ')\" = {len} ] || {{ echo 'the connection dropped before the whole file arrived; the file was not changed' >&2; exit 1; }}\n\
         cat \"$t\" > \"$f\"\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-write-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn a_save_replaces_the_whole_file() {
        let dir = scratch("replace");
        let path = dir.join("note.md");
        fs::write(&path, "a much longer first draft\n").expect("seed");
        write_text(None, path.to_str().expect("utf8"), "# short\n").expect("save");
        assert_eq!(fs::read_to_string(&path).expect("read"), "# short\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_never_creates_a_file() {
        let dir = scratch("missing");
        let path = dir.join("gone.md");
        let error = write_text(None, path.to_str().expect("utf8"), "x").expect_err("refused");
        assert_eq!(error, "the file is gone");
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_save_refuses_a_folder() {
        let dir = scratch("folder");
        assert!(write_text(None, dir.to_str().expect("utf8"), "x").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Run the remote save script on this machine, feeding it `sent`.
    fn run_remote_save(path: &std::path::Path, len: usize, sent: &str) -> std::process::Output {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(remote_save_script(path.to_str().expect("utf8"), len))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(sent.as_bytes())
            .expect("send");
        child.wait_with_output().expect("wait")
    }

    #[test]
    fn a_remote_save_replaces_the_file_only_when_every_byte_arrived() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("remote");
        let path = dir.join("it's a note.md");
        fs::write(&path, "the original\n").expect("seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("mode");

        let text = "# new\nwith ünïcode\n";
        let cut = run_remote_save(&path, text.len(), &text[..5]);
        assert!(!cut.status.success());
        assert!(String::from_utf8_lossy(&cut.stderr).contains("connection dropped"));
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "the original\n",
            "a cut-off save changes nothing"
        );

        let whole = run_remote_save(&path, text.len(), text);
        assert!(
            whole.status.success(),
            "{}",
            String::from_utf8_lossy(&whole.stderr)
        );
        assert_eq!(fs::read_to_string(&path).expect("read"), text);
        assert_eq!(
            fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
            0o640,
            "the mode is kept"
        );
        let left: Vec<_> = fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left.len(), 1, "no staged file is left: {left:?}");

        let gone = run_remote_save(&dir.join("gone.md"), 1, "x");
        assert!(String::from_utf8_lossy(&gone.stderr).contains("the file is gone"));
        let _ = fs::remove_dir_all(&dir);
    }
}
