//! Reading one text file from the machine that owns the checkout.
//!
//! The daemon's `files.*` methods list and open files but never return their
//! contents, so a client that wants to draw a document itself reads it where it
//! lives: straight from disk for the local machine, over the saved SSH profile
//! for a remote one. A file is either read as it sits on disk or as the index
//! holds it, which is the version a staged diff describes.
//!
//! Reads are bounded and refuse binary files, so a stray large or compiled file
//! never reaches the renderer.

use std::io::Read;
use std::process::Command;

use crate::git::sh_quote;
use crate::transport::SshTarget;

/// Most bytes of a file a read keeps.
pub const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;

/// A file's text, cut at [`MAX_TEXT_BYTES`] when it was longer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextFile {
    pub text: String,
    pub truncated: bool,
}

/// Which copy of a file to read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Version<'a> {
    /// The file on disk, at an absolute path.
    Disk(&'a str),
    /// The index's copy of `path`, relative to the repository at `repo`.
    Index { repo: &'a str, path: &'a str },
}

/// Read `version` on this machine, or on `target`'s host.
pub fn read_text(target: Option<&SshTarget>, version: Version<'_>) -> Result<TextFile, String> {
    let bytes = match (target, version) {
        (None, Version::Disk(path)) => read_disk(path)?,
        (Some(target), Version::Disk(path)) => run(
            target.shell(&format!(
                "head -c {} -- {}",
                MAX_TEXT_BYTES + 1,
                sh_quote(path)
            )),
            "read the file",
        )?,
        (None, Version::Index { repo, path }) => {
            let mut command = Command::new("git");
            command
                .arg("-C")
                .arg(repo)
                .arg("show")
                .arg(format!(":{path}"));
            run(command, "read the staged file")?
        }
        (Some(target), Version::Index { repo, path }) => run(
            target.shell(&format!(
                "git -C {} show {}",
                sh_quote(repo),
                sh_quote(&format!(":{path}"))
            )),
            "read the staged file",
        )?,
    };
    decode(bytes)
}

fn read_disk(path: &str) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_TEXT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    Ok(bytes)
}

/// Run `command` and return its stdout, or its stderr as the reason.
fn run(mut command: Command, what: &str) -> Result<Vec<u8>, String> {
    let output = command
        .output()
        .map_err(|error| format!("couldn't {what}: {error}"))?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("couldn't {what}")
    } else {
        stderr
    })
}

/// Turn raw bytes into a [`TextFile`]: cut at the bound, refuse binary, and
/// replace invalid UTF-8 rather than failing the whole read.
fn decode(mut bytes: Vec<u8>) -> Result<TextFile, String> {
    let truncated = bytes.len() > MAX_TEXT_BYTES;
    bytes.truncate(MAX_TEXT_BYTES);
    if bytes.contains(&0) {
        return Err("binary file".to_string());
    }
    Ok(TextFile {
        text: String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_decodes_and_marks_a_cut() {
        let file = decode(b"# Title\n".to_vec()).expect("text");
        assert_eq!(file.text, "# Title\n");
        assert!(!file.truncated);
        let long = decode(vec![b'a'; MAX_TEXT_BYTES + 1]).expect("text");
        assert_eq!(long.text.len(), MAX_TEXT_BYTES);
        assert!(long.truncated);
    }

    #[test]
    fn a_nul_byte_means_binary() {
        assert_eq!(decode(b"MZ\0\0".to_vec()), Err("binary file".to_string()));
    }

    #[test]
    fn invalid_utf8_is_replaced_not_rejected() {
        let file = decode(vec![b'o', b'k', 0xff]).expect("lossy");
        assert!(file.text.starts_with("ok"));
    }

    #[test]
    fn disk_reads_stop_at_the_bound() {
        let dir = std::env::temp_dir().join(format!("pilus-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("big.md");
        std::fs::write(&path, vec![b'x'; MAX_TEXT_BYTES + 10]).expect("write");
        let file = read_text(None, Version::Disk(path.to_str().expect("utf8"))).expect("read");
        assert_eq!(file.text.len(), MAX_TEXT_BYTES);
        assert!(file.truncated);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_reports_why() {
        let error = read_text(None, Version::Disk("/definitely/not/here.md")).expect_err("missing");
        assert!(!error.is_empty());
    }

    #[test]
    fn the_index_copy_comes_from_git() {
        let dir = std::env::temp_dir().join(format!("pilus-read-git-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .expect("git")
        };
        git(&["init", "--quiet"]);
        std::fs::write(dir.join("a.md"), "staged\n").expect("write");
        git(&["add", "a.md"]);
        std::fs::write(dir.join("a.md"), "edited after staging\n").expect("write");
        let repo = dir.to_str().expect("utf8");
        let staged = read_text(None, Version::Index { repo, path: "a.md" }).expect("staged");
        assert_eq!(staged.text, "staged\n");
        let disk = read_text(
            None,
            Version::Disk(dir.join("a.md").to_str().expect("utf8")),
        )
        .expect("disk");
        assert_eq!(disk.text, "edited after staging\n");
        std::fs::remove_dir_all(&dir).ok();
    }
}
