//! Putting a local file or folder into a folder on the machine that owns the
//! checkout.
//!
//! The daemon's `files.*` methods never write, so an upload goes where
//! [`crate::read_text`] reads: straight to disk for the local machine, over the
//! saved SSH profile for a remote one. A file's bytes are piped into `cat`; a
//! folder is streamed as a `tar` archive into `tar -x`, so its tree arrives in
//! one connection.
//!
//! An upload never replaces anything: a file is created exclusively, and a
//! folder is refused when its name is taken, so dropping `config.toml` onto a
//! folder that has one fails instead of overwriting it.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::git::sh_quote;
use crate::transport::SshTarget;

/// Copy the file or folder at `source` into the folder `dir`, keeping its
/// name, on this machine or on `target`'s host. Returns the name it was
/// stored under.
pub fn upload_path(target: Option<&SshTarget>, source: &Path, dir: &str) -> Result<String, String> {
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "this file has no usable name".to_string())?;
    if source.is_dir() {
        upload_folder(target, source, name, dir)?;
    } else {
        upload_file(target, source, name, dir)?;
    }
    Ok(name.to_string())
}

fn upload_file(
    target: Option<&SshTarget>,
    source: &Path,
    name: &str,
    dir: &str,
) -> Result<(), String> {
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
                return Err(remote_failure(name, &output.stderr));
            }
        }
    }
    Ok(())
}

fn upload_folder(
    target: Option<&SshTarget>,
    source: &Path,
    name: &str,
    dir: &str,
) -> Result<(), String> {
    let destination = format!("{}/{name}", dir.trim_end_matches('/'));
    match target {
        None => copy_tree(source, Path::new(&destination)).map_err(|error| exists_or(name, error)),
        Some(target) => {
            let parent = source
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let mut pack = tar_command();
            pack.arg("-cf")
                .arg("-")
                .arg("-C")
                .arg(parent)
                .arg(name)
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let mut pack = pack
                .spawn()
                .map_err(|error| format!("couldn't pack {name}: {error}"))?;
            let stream = pack
                .stdout
                .take()
                .ok_or_else(|| format!("couldn't pack {name}"))?;
            let mut unpack = target.shell(&format!(
                "if [ -e {dest} ]; then echo 'exists' >&2; exit 1; fi; tar -xf - -C {dir}",
                dest = sh_quote(&destination),
                dir = sh_quote(dir),
            ));
            let output = unpack
                .stdin(Stdio::from(stream))
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .map_err(|error| format!("couldn't reach the host: {error}"))?;
            let _ = pack.wait();
            if output.status.success() {
                Ok(())
            } else {
                Err(remote_failure(name, &output.stderr))
            }
        }
    }
}

/// `tar` that leaves macOS's `._` sidecar files out of an archive.
pub(crate) fn tar_command() -> Command {
    let mut command = Command::new("tar");
    command.env("COPYFILE_DISABLE", "1");
    command
}

/// Copy the tree at `from` to `to`, which must not exist. Symlinks are
/// recreated rather than followed, so a link out of the folder cannot pull
/// the rest of the disk in.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let destination = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &destination)?;
        } else {
            fs::copy(entry.path(), &destination)?;
        }
    }
    Ok(())
}

/// Why a command on the host failed, in a line fit for a toast.
fn remote_failure(name: &str, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    if stderr.is_empty() {
        format!("couldn't upload {name}")
    } else if stderr.contains("exist") {
        format!("{name} already exists")
    } else {
        stderr
    }
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
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn a_local_upload_copies_the_file_by_name() {
        let root = scratch("copy");
        let source = root.join("note.txt");
        fs::write(&source, b"hello").expect("write");
        let dest = root.join("into");
        fs::create_dir_all(&dest).expect("dir");
        let name = upload_path(None, &source, dest.to_str().expect("utf8")).expect("upload");
        assert_eq!(name, "note.txt");
        assert_eq!(fs::read(dest.join("note.txt")).expect("read"), b"hello");
    }

    #[test]
    fn an_upload_never_replaces_a_file() {
        let root = scratch("clobber");
        let source = root.join("a.txt");
        fs::write(&source, b"new").expect("write");
        let dest = root.join("into");
        fs::create_dir_all(&dest).expect("dir");
        fs::write(dest.join("a.txt"), b"old").expect("write");
        let error = upload_path(None, &source, dest.to_str().expect("utf8")).expect_err("exists");
        assert_eq!(error, "a.txt already exists");
        assert_eq!(fs::read(dest.join("a.txt")).expect("read"), b"old");
    }

    #[test]
    fn a_folder_is_copied_with_its_tree() {
        let root = scratch("tree");
        let source = root.join("proj");
        fs::create_dir_all(source.join("src")).expect("dir");
        fs::write(source.join("src/main.rs"), b"fn main() {}").expect("write");
        fs::write(source.join("README"), b"hi").expect("write");
        let dest = root.join("into");
        fs::create_dir_all(&dest).expect("dir");
        let name = upload_path(None, &source, dest.to_str().expect("utf8")).expect("upload");
        assert_eq!(name, "proj");
        assert_eq!(
            fs::read(dest.join("proj/src/main.rs")).expect("read"),
            b"fn main() {}"
        );
        assert_eq!(fs::read(dest.join("proj/README")).expect("read"), b"hi");
    }

    #[test]
    fn a_folder_never_merges_into_one_of_the_same_name() {
        let root = scratch("merge");
        let source = root.join("proj");
        fs::create_dir_all(&source).expect("dir");
        fs::write(source.join("new"), b"n").expect("write");
        let dest = root.join("into");
        fs::create_dir_all(dest.join("proj")).expect("dir");
        let error = upload_path(None, &source, dest.to_str().expect("utf8")).expect_err("exists");
        assert_eq!(error, "proj already exists");
        assert!(!dest.join("proj/new").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_a_folder_is_copied_as_a_link() {
        let root = scratch("link");
        let source = root.join("proj");
        fs::create_dir_all(&source).expect("dir");
        std::os::unix::fs::symlink("/etc/hosts", source.join("hosts")).expect("link");
        let dest = root.join("into");
        fs::create_dir_all(&dest).expect("dir");
        upload_path(None, &source, dest.to_str().expect("utf8")).expect("upload");
        let copied = fs::read_link(dest.join("proj/hosts")).expect("still a link");
        assert_eq!(copied, Path::new("/etc/hosts"));
    }
}
