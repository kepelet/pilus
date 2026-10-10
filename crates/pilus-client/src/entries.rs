//! Creating, renaming, and deleting files and folders in the tree.
//!
//! The daemon's `files.*` methods never write, so these go where
//! [`crate::upload_file`] goes: straight to disk for the local machine, over
//! the saved SSH profile for a remote one. Paths are plain `/`-separated
//! strings because a remote path is never a local `Path`.
//!
//! Create and rename never replace anything: an entry that already has the
//! name makes the call fail, as an upload does. Delete is the one destructive
//! call and takes only an absolute path with something below `/` in it.

use std::fs::{self, OpenOptions};
use std::process::Stdio;

use crate::git::sh_quote;
use crate::transport::SshTarget;

/// The folder a path sits in, or `None` for a path with no parent segment.
pub fn parent_of(path: &str) -> Option<&str> {
    let (parent, _) = path.trim_end_matches('/').rsplit_once('/')?;
    Some(if parent.is_empty() { "/" } else { parent })
}

/// Why `name` cannot be one entry's name, if it cannot.
///
/// A separator would make a create or rename land somewhere other than where
/// the user pointed, and `.`/`..` name folders that already exist.
pub fn name_problem(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("a name is required")
    } else if name == "." || name == ".." {
        Some("that name is reserved")
    } else if name.contains('/') || name.contains('\0') {
        Some("a name cannot contain a slash")
    } else {
        None
    }
}

fn join(dir: &str, name: &str) -> String {
    format!("{}/{name}", dir.trim_end_matches('/'))
}

/// Create an empty file, or a folder when `folder`, named `name` inside `dir`.
/// Returns the new entry's path.
pub fn create_entry(
    target: Option<&SshTarget>,
    dir: &str,
    name: &str,
    folder: bool,
) -> Result<String, String> {
    if let Some(problem) = name_problem(name) {
        return Err(problem.to_string());
    }
    let path = join(dir, name);
    match target {
        None => {
            let made = if folder {
                fs::create_dir(&path)
            } else {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map(drop)
            };
            made.map_err(|error| failed(name, &error.to_string()))?;
        }
        Some(target) => {
            // `set -C` makes the redirect refuse an existing file, as `mkdir`
            // refuses an existing folder.
            let quoted = sh_quote(&path);
            let script = if folder {
                format!("mkdir {quoted}")
            } else {
                format!("set -C; : > {quoted}")
            };
            remote(target, &script, name)?;
        }
    }
    Ok(path)
}

/// Rename the entry at `path` to `new_name` within its folder. Returns the new
/// path.
pub fn rename_entry(
    target: Option<&SshTarget>,
    path: &str,
    new_name: &str,
) -> Result<String, String> {
    if let Some(problem) = name_problem(new_name) {
        return Err(problem.to_string());
    }
    let parent = parent_of(path).ok_or_else(|| "this entry has no folder".to_string())?;
    let destination = join(parent, new_name);
    if destination == path {
        return Ok(destination);
    }
    match target {
        None => {
            // `rename` would replace a file; the check keeps it from, short of
            // a race with another writer.
            if fs::symlink_metadata(&destination).is_ok() {
                return Err(format!("{new_name} already exists"));
            }
            fs::rename(path, &destination).map_err(|error| failed(new_name, &error.to_string()))?;
        }
        Some(target) => {
            let (from, to) = (sh_quote(path), sh_quote(&destination));
            let script = format!(
                "if [ -e {to} ] || [ -L {to} ]; then echo exists >&2; exit 1; fi; mv -- {from} {to}"
            );
            remote(target, &script, new_name)?;
        }
    }
    Ok(destination)
}

/// Delete the file, or the folder and everything in it, at `path`. A link is
/// removed, never followed.
pub fn delete_entry(target: Option<&SshTarget>, path: &str) -> Result<(), String> {
    // A path this short is a root or a top-level system folder; no tree row
    // asks to delete one, so a call that does is a bug worth refusing.
    if !path.starts_with('/') || path.trim_end_matches('/').matches('/').count() < 2 {
        return Err("refusing to delete a top-level folder".to_string());
    }
    let name = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path);
    match target {
        None => {
            let meta =
                fs::symlink_metadata(path).map_err(|error| failed(name, &error.to_string()))?;
            let removed = if meta.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            removed.map_err(|error| failed(name, &error.to_string()))
        }
        Some(target) => remote(target, &format!("rm -rf -- {}", sh_quote(path)), name),
    }
}

fn failed(name: &str, error: &str) -> String {
    if error.contains("exist") {
        format!("{name} already exists")
    } else {
        format!("{name}: {error}")
    }
}

/// Run `script` on the host, reporting its stderr as the reason on failure.
fn remote(target: &SshTarget, script: &str, name: &str) -> Result<(), String> {
    let output = target
        .shell(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("couldn't reach the host: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("couldn't change {name}")
    } else {
        failed(name, &stderr)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-entries-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    fn text(path: &std::path::Path) -> &str {
        path.to_str().expect("utf8")
    }

    #[test]
    fn a_parent_is_the_folder_above() {
        assert_eq!(parent_of("/a/b/c.rs"), Some("/a/b"));
        assert_eq!(parent_of("/a/b/"), Some("/a"));
        assert_eq!(parent_of("/a"), Some("/"));
        assert_eq!(parent_of("a"), None);
    }

    #[test]
    fn a_name_cannot_leave_its_folder() {
        assert!(name_problem("notes.md").is_none());
        assert!(name_problem(".gitignore").is_none());
        for bad in ["", ".", "..", "a/b", "/etc"] {
            assert!(name_problem(bad).is_some(), "{bad:?}");
        }
    }

    #[test]
    fn creating_makes_an_empty_file_or_a_folder_and_never_replaces() {
        let root = scratch("create");
        let file = create_entry(None, text(&root), "a.txt", false).expect("file");
        assert_eq!(fs::read(&file).expect("read"), b"");
        let folder = create_entry(None, text(&root), "sub", true).expect("folder");
        assert!(std::path::Path::new(&folder).is_dir());
        fs::write(&file, b"keep").expect("write");
        let error = create_entry(None, text(&root), "a.txt", false).expect_err("exists");
        assert_eq!(error, "a.txt already exists");
        assert_eq!(fs::read(&file).expect("read"), b"keep");
        let error = create_entry(None, text(&root), "sub", true).expect_err("exists");
        assert_eq!(error, "sub already exists");
    }

    #[test]
    fn renaming_moves_within_the_folder_and_never_replaces() {
        let root = scratch("rename");
        let from = root.join("a.txt");
        fs::write(&from, b"a").expect("write");
        fs::write(root.join("b.txt"), b"b").expect("write");
        let error = rename_entry(None, text(&from), "b.txt").expect_err("exists");
        assert_eq!(error, "b.txt already exists");
        assert_eq!(fs::read(root.join("b.txt")).expect("read"), b"b");
        let to = rename_entry(None, text(&from), "c.txt").expect("rename");
        assert_eq!(to, text(&root.join("c.txt")));
        assert!(!from.exists());
        assert_eq!(fs::read(&to).expect("read"), b"a");
        // The same name is a no-op, not an "already exists".
        assert!(rename_entry(None, &to, "c.txt").is_ok());
        assert!(rename_entry(None, &to, "x/y").is_err());
    }

    #[test]
    fn deleting_removes_a_folder_with_its_contents() {
        let root = scratch("delete");
        let folder = root.join("sub");
        fs::create_dir_all(folder.join("deep")).expect("dir");
        fs::write(folder.join("deep/f.txt"), b"x").expect("write");
        delete_entry(None, text(&folder)).expect("delete");
        assert!(!folder.exists());
        let file = root.join("a.txt");
        fs::write(&file, b"x").expect("write");
        delete_entry(None, text(&file)).expect("delete");
        assert!(!file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_link_leaves_what_it_points_at() {
        let root = scratch("link");
        let real = root.join("real");
        fs::create_dir_all(&real).expect("dir");
        fs::write(real.join("f.txt"), b"x").expect("write");
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        delete_entry(None, text(&link)).expect("delete");
        assert!(!link.exists());
        assert!(real.join("f.txt").exists());
    }

    #[test]
    fn deleting_a_top_level_folder_is_refused() {
        for path in ["/", "/usr", "/usr/", "relative/dir", ""] {
            assert!(delete_entry(None, path).is_err(), "{path:?}");
        }
    }
}
