//! Putting a local file into a folder on the machine that owns the checkout.
//!
//! The daemon's `files.*` methods never write, so an upload goes where
//! [`crate::read_text`] reads: straight to disk for the local machine, over the
//! saved SSH profile for a remote one, as the file's bytes piped into `cat`.
//!
//! An upload never replaces a file: the destination is created exclusively, so
//! dropping `config.toml` onto a folder that has one fails instead of
//! overwriting it.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::process::Stdio;

use crate::git::sh_quote;
use crate::transport::SshTarget;

/// Copy the file at `source` into the folder `dir`, keeping its name, on this
/// machine or on `target`'s host. Returns the name it was stored under.
pub fn upload_file(target: Option<&SshTarget>, source: &Path, dir: &str) -> Result<String, String> {
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "this file has no usable name".to_string())?;
    if source.is_dir() {
        return Err(format!("{name} is a folder; only files can be uploaded"));
    }
    let mut input = File::open(source).map_err(|error| format!("{name}: {error}"))?;
    let destination = format!("{}/{name}", dir.trim_end_matches('/'));
    match target {
        None => {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .map_err(|error| exists_or(name, error))?;
            io::copy(&mut input, &mut output).map_err(|error| format!("{name}: {error}"))?;
        }
        Some(target) => {
            // `set -C` makes the redirect refuse an existing file.
            let mut command = target.shell(&format!("set -C; cat > {}", sh_quote(&destination)));
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|error| format!("couldn't reach the host: {error}"))?;
            if let Some(mut stdin) = child.stdin.take() {
                // A refused redirect closes the pipe early; its reason is on
                // stderr, which is the more useful error.
                let _ = io::copy(&mut input, &mut stdin);
            }
            let output = child
                .wait_with_output()
                .map_err(|error| format!("{name}: {error}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(if stderr.is_empty() {
                    format!("couldn't upload {name}")
                } else if stderr.contains("exist") {
                    format!("{name} already exists")
                } else {
                    stderr
                });
            }
        }
    }
    Ok(name.to_string())
}

fn exists_or(name: &str, error: io::Error) -> String {
    if error.kind() == io::ErrorKind::AlreadyExists {
        format!("{name} already exists")
    } else {
        format!("{name}: {error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-upload-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn a_local_upload_copies_the_file_by_name() {
        let root = scratch("copy");
        let source = root.join("note.txt");
        std::fs::write(&source, b"hello").expect("write");
        let dest = root.join("into");
        std::fs::create_dir_all(&dest).expect("dir");
        let name = upload_file(None, &source, dest.to_str().expect("utf8")).expect("upload");
        assert_eq!(name, "note.txt");
        assert_eq!(
            std::fs::read(dest.join("note.txt")).expect("read"),
            b"hello"
        );
    }

    #[test]
    fn an_upload_never_replaces_a_file() {
        let root = scratch("clobber");
        let source = root.join("a.txt");
        std::fs::write(&source, b"new").expect("write");
        let dest = root.join("into");
        std::fs::create_dir_all(&dest).expect("dir");
        std::fs::write(dest.join("a.txt"), b"old").expect("write");
        let error = upload_file(None, &source, dest.to_str().expect("utf8")).expect_err("exists");
        assert_eq!(error, "a.txt already exists");
        assert_eq!(std::fs::read(dest.join("a.txt")).expect("read"), b"old");
    }

    #[test]
    fn a_folder_is_refused() {
        let root = scratch("dir");
        let error = upload_file(None, &root, "/tmp").expect_err("folder");
        assert!(error.contains("is a folder"));
    }
}
