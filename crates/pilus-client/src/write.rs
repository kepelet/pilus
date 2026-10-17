//! Saving a text file back where [`crate::read_text`] read it.
//!
//! The daemon's `files.*` methods never write, so a save goes straight to disk
//! for the local machine, or is piped into `cat` over the saved SSH profile
//! for a remote one. A save replaces the contents of a file that exists; it
//! never creates one, so a file deleted or renamed behind the editor's back
//! makes the save fail instead of resurrecting it under the old name.

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
            let quoted = sh_quote(path);
            let mut child = target
                .shell(&format!(
                    "[ -f {quoted} ] || {{ echo 'the file is gone' >&2; exit 1; }}; cat > {quoted}"
                ))
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
}
