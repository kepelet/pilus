//! Saving a file or folder from the machine that owns the checkout to a path
//! on this one.
//!
//! The mirror of [`crate::upload_path`]: a file is copied (or streamed through
//! `cat` from a remote host), and a folder is packed into a gzipped `tar`
//! archive on the machine that has it, so a tree crosses the connection as one
//! stream instead of a round trip per entry.

use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::entries::parent_of;
use crate::git::sh_quote;
use crate::transport::SshTarget;
use crate::upload::tar_command;

/// The name a folder called `name` is saved under.
pub fn archive_name(name: &str) -> String {
    format!("{name}.tar.gz")
}

/// Save `path`, on this machine or on `target`'s host, to `destination`
/// here: the file itself, or for a folder (`dir`) a `.tar.gz` of it. An
/// existing `destination` is replaced, since the save dialog already asked;
/// a failed save leaves nothing half written behind.
pub fn download_path(
    target: Option<&SshTarget>,
    path: &str,
    dir: bool,
    destination: &Path,
) -> Result<(), String> {
    let result = save(target, path, dir, destination);
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
}

fn save(
    target: Option<&SshTarget>,
    path: &str,
    dir: bool,
    destination: &Path,
) -> Result<(), String> {
    let name = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path);
    if !dir && target.is_none() {
        fs::copy(path, destination).map_err(|error| format!("{name}: {error}"))?;
        return Ok(());
    }
    let mut command = stream_command(target, path, dir);
    let output = File::create(destination).map_err(|error| format!("{name}: {error}"))?;
    let result = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(output))
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("couldn't reach {name}: {error}"))?;
    if result.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&result.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("couldn't download {name}")
    } else {
        stderr
    })
}

/// The command whose stdout is what gets saved.
fn stream_command(target: Option<&SshTarget>, path: &str, dir: bool) -> Command {
    match (target, dir) {
        (Some(target), false) => target.shell(&format!("cat -- {}", sh_quote(path))),
        (Some(target), true) => {
            let (parent, name) = split(path);
            target.shell(&format!(
                "tar -czf - -C {} -- {}",
                sh_quote(parent),
                sh_quote(name)
            ))
        }
        (None, _) => {
            let (parent, name) = split(path);
            let mut command = tar_command();
            command.args(["-czf", "-", "-C", parent, "--", name]);
            command
        }
    }
}

/// `path` as its folder and its own name, for `tar -C folder name`.
fn split(path: &str) -> (&str, &str) {
    let trimmed = path.trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or(trimmed);
    (parent_of(trimmed).unwrap_or("."), name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-download-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn a_folder_is_named_for_its_archive() {
        assert_eq!(archive_name("proj"), "proj.tar.gz");
    }

    #[test]
    fn a_path_splits_into_folder_and_name() {
        assert_eq!(split("/a/b/c"), ("/a/b", "c"));
        assert_eq!(split("/a/b/c/"), ("/a/b", "c"));
        assert_eq!(split("/top"), ("/", "top"));
    }

    #[test]
    fn a_local_file_is_copied() {
        let root = scratch("file");
        let source = root.join("note.txt");
        fs::write(&source, b"hello").expect("write");
        let saved = root.join("saved.txt");
        download_path(None, source.to_str().expect("utf8"), false, &saved).expect("download");
        assert_eq!(fs::read(saved).expect("read"), b"hello");
    }

    #[test]
    fn a_local_folder_is_archived_and_unpacks_to_the_same_tree() {
        let root = scratch("folder");
        let source = root.join("proj");
        fs::create_dir_all(source.join("src")).expect("dir");
        fs::write(source.join("src/main.rs"), b"fn main() {}").expect("write");
        let saved = root.join(archive_name("proj"));
        download_path(None, source.to_str().expect("utf8"), true, &saved).expect("download");

        let out = root.join("out");
        fs::create_dir_all(&out).expect("dir");
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&saved)
            .arg("-C")
            .arg(&out)
            .status()
            .expect("tar");
        assert!(status.success());
        assert_eq!(
            fs::read(out.join("proj/src/main.rs")).expect("read"),
            b"fn main() {}"
        );
    }

    #[test]
    fn a_failed_download_leaves_no_partial_file() {
        let root = scratch("missing");
        let saved = root.join("out.tar.gz");
        let missing = root.join("nope");
        let error = download_path(None, missing.to_str().expect("utf8"), true, &saved)
            .expect_err("missing folder");
        assert!(!error.is_empty());
        assert!(!saved.exists());
    }
}
