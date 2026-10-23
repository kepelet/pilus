//! Listing a machine's folders, for choosing one to open as a workspace.
//!
//! The window's Open Workspace picker and the hub's `hub.folders` (which a phone
//! uses for the same job) read directories the same way: the local filesystem
//! directly, an SSH machine through one `ssh` call that runs a small POSIX
//! script. Keeping that here means the two cannot disagree about what a folder
//! is, whether `~` expands, or how a name with a space survives.
//!
//! Every call blocks on the filesystem or a process, so callers run it off their
//! UI or request thread.

use std::path::Path;

use crate::transport::SshTarget;

/// More folders than this in one directory are cut off: a phone cannot use them
/// and a reply should stay small.
pub const MAX_FOLDERS: usize = 1000;

/// One child directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Folder {
    /// Base name.
    pub name: String,
    /// Absolute path.
    pub path: String,
    /// Whether it holds a `.git`, so it is likely a project worth opening.
    pub git: bool,
}

/// A directory and its child directories.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Listing {
    /// The directory's resolved absolute path (`~` expanded).
    pub root: String,
    /// Its child directories, sorted without regard to case.
    pub folders: Vec<Folder>,
}

/// The parent of an absolute POSIX path, or `None` at the root.
pub fn parent_of(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(index) => Some(trimmed[..index].to_string()),
        None => None,
    }
}

/// List `path` on this machine. `~` and relative paths are resolved first.
pub fn list_local(path: &str) -> Result<Listing, String> {
    let root = expand_path(path);
    let read = std::fs::read_dir(&root).map_err(|error| error.to_string())?;
    let mut folders = Vec::new();
    for entry in read.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Symlinks to directories are followed so a linked project is usable.
        let is_dir = file_type.is_dir() || (file_type.is_symlink() && entry.path().is_dir());
        if !is_dir {
            continue;
        }
        let path = entry.path();
        folders.push(Folder {
            name: entry.file_name().to_string_lossy().into_owned(),
            git: path.join(".git").exists(),
            path: path.to_string_lossy().into_owned(),
        });
    }
    Ok(finish(root, folders))
}

/// List `path` on an SSH machine, in one round trip.
pub fn list_remote(target: &SshTarget, path: &str) -> Result<Listing, String> {
    // `cd` resolves `~`; `$PWD` reports the absolute directory. Each child is a
    // NUL-terminated `g<name>` (a git repository) or `d<name>`, so spaces and
    // newlines in names survive and the root is explicit. The globs follow
    // symlinks to directories, like the local listing does.
    let remote = format!(
        "cd {} && printf '%s\\0' \"$PWD\" && for d in */ .[!.]*/; do \
         [ -d \"$d\" ] || continue; d=${{d%/}}; \
         if [ -e \"$d/.git\" ]; then printf 'g%s\\0' \"$d\"; else printf 'd%s\\0' \"$d\"; fi; done",
        remote_cd_arg(path)
    );
    let output = target
        .shell(&remote)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "remote directory listing failed".to_string()
        } else {
            stderr
        });
    }
    parse_remote(&output.stdout)
}

fn parse_remote(stdout: &[u8]) -> Result<Listing, String> {
    let mut fields = stdout.split(|byte| *byte == 0);
    let root = fields
        .next()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default();
    if root.is_empty() {
        return Err("remote shell did not report a directory".to_string());
    }
    let mut folders = Vec::new();
    for raw in fields {
        let text = String::from_utf8_lossy(raw);
        let Some(kind) = text.chars().next() else {
            continue;
        };
        let name = text[kind.len_utf8()..].to_string();
        if name.is_empty() || !matches!(kind, 'g' | 'd') {
            continue;
        }
        folders.push(Folder {
            path: format!("{}/{}", root.trim_end_matches('/'), name),
            git: kind == 'g',
            name,
        });
    }
    Ok(finish(root, folders))
}

fn finish(root: String, mut folders: Vec<Folder>) -> Listing {
    folders.sort_by_key(|folder| folder.name.to_lowercase());
    folders.truncate(MAX_FOLDERS);
    Listing { root, folders }
}

/// Expand a leading `~` against this machine's home, and anchor a relative path
/// at the current directory.
pub fn expand_path(path: &str) -> String {
    let home = || std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    if path == "~" {
        return home();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("{}/{}", home().trim_end_matches('/'), rest);
    }
    if path.starts_with('/') {
        return path.to_string();
    }
    match std::env::current_dir() {
        Ok(dir) => dir.join(Path::new(path)).to_string_lossy().into_owned(),
        Err(_) => path.to_string(),
    }
}

/// Build the remote `cd` argument, letting the remote shell expand `~`.
fn remote_cd_arg(path: &str) -> String {
    if path == "~" {
        return "\"$HOME\"".to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("\"$HOME\"/{}", sh_quote(rest));
    }
    sh_quote(path)
}

/// Quote `value` for a POSIX shell as a single argument.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn parent_of_handles_roots_and_nesting() {
        assert_eq!(parent_of("/Users/alex"), Some("/Users".to_string()));
        assert_eq!(parent_of("/Users"), Some("/".to_string()));
        assert_eq!(parent_of("/"), None);
        assert_eq!(parent_of(""), None);
        assert_eq!(parent_of("/Users/alex/"), Some("/Users".to_string()));
    }

    #[test]
    fn sh_quote_escapes_single_quotes() {
        assert_eq!(sh_quote("/a b"), "'/a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn remote_cd_expands_tilde() {
        assert_eq!(remote_cd_arg("~"), "\"$HOME\"");
        assert_eq!(remote_cd_arg("~/flo"), "\"$HOME\"/'flo'");
        assert_eq!(remote_cd_arg("/a b"), "'/a b'");
    }

    #[test]
    fn expand_path_resolves_tilde_and_keeps_absolute_paths() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
        assert_eq!(expand_path("~"), home);
        assert_eq!(
            expand_path("~/flo"),
            format!("{}/flo", home.trim_end_matches('/'))
        );
        assert_eq!(expand_path("/already/absolute"), "/already/absolute");
    }

    #[test]
    fn local_listing_returns_sorted_folders_and_marks_repositories() {
        let base = std::env::temp_dir().join(format!("pilus-folders-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("zeta/.git")).expect("zeta");
        std::fs::create_dir_all(base.join("Alpha")).expect("alpha");
        std::fs::create_dir_all(base.join("with space")).expect("space");
        std::fs::write(base.join("file.txt"), b"x").expect("file");
        let listed = list_local(base.to_str().expect("utf8")).expect("list");
        assert_eq!(listed.root, base.to_string_lossy());
        let names: Vec<_> = listed.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "with space", "zeta"]);
        let git: Vec<_> = listed.folders.iter().map(|f| f.git).collect();
        assert_eq!(git, [false, false, true]);
        assert!(list_local("/definitely/not/here").is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remote_output_is_parsed_with_spaces_and_repository_marks() {
        let out = b"/home/me\0gproject\0dnotes and more\0d.config\0\0xbogus\0g\0";
        let listed = parse_remote(out).expect("parse");
        assert_eq!(listed.root, "/home/me");
        let seen: Vec<_> = listed
            .folders
            .iter()
            .map(|f| (f.name.as_str(), f.path.as_str(), f.git))
            .collect();
        assert_eq!(
            seen,
            [
                (".config", "/home/me/.config", false),
                ("notes and more", "/home/me/notes and more", false),
                ("project", "/home/me/project", true),
            ]
        );
        assert!(parse_remote(b"").is_err());
    }

    /// The remote script runs under a real POSIX shell here; only the `ssh` hop
    /// is skipped, so quoting, globs, and the marks are exercised for real.
    #[test]
    fn the_remote_script_lists_what_the_local_listing_does() {
        let base = std::env::temp_dir().join(format!("pilus-folders-sh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.git")).expect("repo");
        std::fs::create_dir_all(base.join("it's here")).expect("quote");
        std::fs::create_dir_all(base.join(".hidden")).expect("hidden");
        std::fs::write(base.join("file.txt"), b"x").expect("file");
        let script = format!(
            "cd {} && printf '%s\\0' \"$PWD\" && for d in */ .[!.]*/; do \
             [ -d \"$d\" ] || continue; d=${{d%/}}; \
             if [ -e \"$d/.git\" ]; then printf 'g%s\\0' \"$d\"; else printf 'd%s\\0' \"$d\"; fi; done",
            sh_quote(base.to_str().expect("utf8"))
        );
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("sh");
        assert!(output.status.success());
        let remote = parse_remote(&output.stdout).expect("parse");
        let local = list_local(base.to_str().expect("utf8")).expect("local");
        let names = |l: &Listing| -> Vec<(String, bool)> {
            l.folders.iter().map(|f| (f.name.clone(), f.git)).collect()
        };
        assert_eq!(names(&remote), names(&local));
        assert_eq!(names(&remote).len(), 3);
        let _ = std::fs::remove_dir_all(&base);
    }
}
