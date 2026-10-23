//! Listing every file under a workspace root, for the quick-open picker.
//!
//! The daemon's `files.tree` only carries the folders the TUI has open, so a
//! fuzzy finder needs a listing of its own. It is read where the checkout
//! lives, like [`crate::read_text`]: one POSIX script run locally or over the
//! saved SSH profile. Inside a Git work tree `git ls-files` answers, which
//! honours `.gitignore` and skips `.git`; elsewhere `find` prunes the folders
//! that are almost never what a person is looking for.
//!
//! The listing is bounded ([`MAX_FILES`]) so a root that is really a home folder
//! costs a truncated answer, not the machine's whole disk.

use std::process::Command;

use crate::git::sh_quote;
use crate::transport::SshTarget;

/// Most paths a listing keeps.
pub const MAX_FILES: usize = 50_000;

/// Folders `find` skips when the root is not a Git work tree.
const PRUNED: &[&str] = &[".git", "node_modules", "target", ".venv", "__pycache__"];

/// The files under `root` as paths relative to it, sorted, and whether the
/// listing was cut at [`MAX_FILES`].
pub fn list_files(target: Option<&SshTarget>, root: &str) -> Result<FileList, String> {
    let script = list_script(root);
    let mut command = match target {
        None => {
            let mut command = Command::new("sh");
            command.arg("-c").arg(&script);
            command
        }
        Some(target) => target.shell(&script),
    };
    let output = command
        .output()
        .map_err(|error| format!("couldn't list files: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "couldn't list files".to_string()
        } else {
            stderr
        });
    }
    Ok(parse(&output.stdout))
}

/// A bounded listing of a root's files.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileList {
    pub paths: Vec<String>,
    pub truncated: bool,
}

/// The host script: NUL-separated paths relative to `root`.
fn list_script(root: &str) -> String {
    let pruned = PRUNED
        .iter()
        .map(|name| format!("-name {}", sh_quote(name)))
        .collect::<Vec<_>>()
        .join(" -o ");
    format!(
        "cd {root} || exit 1; \
         if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then \
         git ls-files -z --cached --others --exclude-standard; \
         else find . -type d \\( {pruned} \\) -prune -o -type f -print0; fi",
        root = sh_quote(root),
    )
}

fn parse(bytes: &[u8]) -> FileList {
    let mut paths: Vec<String> = bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .map(|path| path.strip_prefix("./").map(str::to_string).unwrap_or(path))
        .collect();
    // `git ls-files --cached` still names a file deleted from disk but not yet
    // from the index; opening it would only fail, which the reader reports.
    paths.sort_unstable();
    paths.dedup();
    let truncated = paths.len() > MAX_FILES;
    paths.truncate(MAX_FILES);
    FileList { paths, truncated }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_split_on_nul_and_lose_the_find_prefix() {
        let list = parse(b"./src/main.rs\0./README.md\0");
        assert_eq!(list.paths, ["README.md", "src/main.rs"]);
        assert!(!list.truncated);
    }

    #[test]
    fn a_name_with_a_newline_stays_one_path() {
        let list = parse(b"a\nb.txt\0");
        assert_eq!(list.paths, ["a\nb.txt"]);
    }

    #[test]
    fn a_long_listing_is_cut_and_says_so() {
        let mut bytes = Vec::new();
        for n in 0..=MAX_FILES {
            bytes.extend_from_slice(format!("f{n:06}").as_bytes());
            bytes.push(0);
        }
        let list = parse(&bytes);
        assert_eq!(list.paths.len(), MAX_FILES);
        assert!(list.truncated);
    }

    #[test]
    fn the_root_is_quoted_for_the_shell() {
        assert!(list_script("/a b/it's").starts_with("cd '/a b/it'\\''s' ||"));
    }

    #[test]
    fn a_folder_that_is_not_a_repository_lists_by_find() {
        let dir = std::env::temp_dir().join(format!("pilus-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("node_modules/x")).expect("mkdir");
        std::fs::create_dir_all(dir.join("src")).expect("mkdir");
        std::fs::write(dir.join("node_modules/x/skip.js"), "").expect("write");
        std::fs::write(dir.join("src/lib.rs"), "").expect("write");
        std::fs::write(dir.join("top.txt"), "").expect("write");
        let list = list_files(None, dir.to_str().expect("utf-8")).expect("list");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(list.paths, ["src/lib.rs", "top.txt"]);
    }
}
