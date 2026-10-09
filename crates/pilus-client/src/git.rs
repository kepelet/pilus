//! Local and remote `git` helpers used by the window.
//!
//! These shell out to `git` on this machine or over an SSH bridge to answer
//! questions the daemon does not: a checkout's current branch, whether a
//! worktree has uncommitted changes, and the branches a directory offers.
//! None of it keeps state; callers cache what they need.

use std::path::Path;

/// Expand a leading `~` against this machine's home directory.
pub fn expand_local(path: &str) -> String {
    let home = || std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let trimmed = path.trim();
    if trimmed == "~" {
        return home();
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return format!("{}/{}", home().trim_end_matches('/'), rest);
    }
    trimmed.to_string()
}

/// Initialize a git repository at `path`, creating the directory if needed.
///
/// A fresh session gets its own repository so the agent has an isolated tree
/// to commit into. Failures return the human error to show.
pub fn init_repository(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("couldn't create {}: {error}", path.display()))?;
    let output = std::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(path)
        .output()
        .map_err(|error| format!("couldn't run git: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        "git init failed".to_string()
    } else {
        stderr
    })
}

/// Run `git -C <path> <args>` on this machine, or on `target`'s host.
///
/// Returns stdout; a failure carries git's own stderr so the window can show
/// why. The remote path is quoted for the remote shell.
fn git_in(
    target: Option<&crate::transport::SshTarget>,
    path: &str,
    args: &[&str],
) -> Result<String, String> {
    let output = match target {
        None => std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output(),
        Some(target) => target
            .shell(&format!("git -C {} {}", sh_quote(path), args.join(" ")))
            .output(),
    }
    .map_err(|error| format!("couldn't run git: {error}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("git {} failed", args.first().copied().unwrap_or(""))
    } else {
        stderr
    })
}

/// Whether the checkout at `path` holds modified or untracked files.
///
/// The daemon refuses to remove such a worktree whatever `force` says, and
/// its answer arrives only after deleting would have begun, so the window asks
/// git first. An error means the state is unknown, which callers must treat
/// as unsafe rather than as clean.
pub fn worktree_is_dirty(
    target: Option<&crate::transport::SshTarget>,
    path: &str,
) -> Result<bool, String> {
    git_in(target, path, &["status", "--porcelain"]).map(|out| !out.trim().is_empty())
}

/// Throw away every uncommitted change in the checkout at `path`, tracked and
/// untracked, so the daemon's own removal no longer refuses it.
///
/// Ignored files stay: git does not count them against a removal. Only call
/// this after the user confirmed discarding the work.
pub fn discard_worktree_changes(
    target: Option<&crate::transport::SshTarget>,
    path: &str,
) -> Result<(), String> {
    git_in(target, path, &["reset", "--hard", "--quiet"])?;
    git_in(target, path, &["clean", "-ffdq"])?;
    Ok(())
}

/// One remote batch's answers: the machine's `$HOME` plus the git common dir
/// of each requested directory.
///
/// The home lets later comparisons expand the daemon's `~`-shorthand dirs
/// on that machine, the same shorthand it uses for local ones.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RemoteRepos {
    /// The remote `$HOME`, empty when it could not be read.
    pub home: String,
    /// Git common dirs in the order requested; `None` is not a repo.
    pub facts: Vec<(String, Option<String>)>,
}

/// Resolve many `cwds` to their git common dirs on a remote machine.
///
/// One SSH invocation carries the whole batch as NUL-separated answers, so
/// a long workspace list costs one round trip instead of one per row.
/// `~`-shorthand dirs are translated to `$HOME` in-script, since only the
/// remote shell knows that home. Directories outside any repository answer
/// empty and map to `None`.
pub fn common_git_dirs_remote(
    target: &crate::transport::SshTarget,
    cwds: &[String],
) -> RemoteRepos {
    let empty = || RemoteRepos::default();
    if cwds.is_empty() {
        return empty();
    }
    let mut script = String::from("printf '%s\\0' \"$HOME\"; for d in");
    for cwd in cwds {
        script.push(' ');
        script.push_str(&remote_dir_arg(cwd));
    }
    script.push_str("; do git -C \"$d\" rev-parse --git-common-dir 2>/dev/null | tr -d '\\n'; printf '\\0'; done");
    let Ok(output) = target.shell(&script).output() else {
        return empty();
    };
    if !output.status.success() {
        return empty();
    }
    let answers: Vec<&[u8]> = output.stdout.split(|byte| *byte == 0).collect();
    let home = answers
        .first()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .unwrap_or_default();
    let facts = cwds
        .iter()
        .enumerate()
        .map(|(index, cwd)| {
            // Absolute answers compare directly against the location; a
            // relative `.git` resolves against the expanded request dir.
            let expanded = expand_remote(cwd, &home);
            let raw = answers
                .get(index + 1)
                .map(|bytes| String::from_utf8_lossy(bytes))
                .unwrap_or_default();
            (cwd.clone(), normalize_common(&expanded, &raw))
        })
        .collect();
    RemoteRepos { home, facts }
}

/// Expand a leading `~` against a known home directory.
pub fn expand_remote(path: &str, home: &str) -> String {
    let trimmed = path.trim();
    if home.is_empty() {
        return trimmed.to_string();
    }
    if trimmed == "~" {
        return home.trim_end_matches('/').to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return format!("{}/{}", home.trim_end_matches('/'), rest);
    }
    trimmed.to_string()
}

/// Build the remote `cd`/`-C` argument, letting the remote shell expand `~`.
fn remote_dir_arg(path: &str) -> String {
    if path == "~" {
        return "\"$HOME\"".to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("\"$HOME\"/{}", sh_quote(rest));
    }
    sh_quote(path)
}

/// Normalize one `git rev-parse --git-common-dir` answer.
///
/// Git may print a relative dir (`.git`); resolve it against the queried
/// `cwd` so comparisons stay absolute. Empty answers mean "not a repo".
fn normalize_common(cwd: &str, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.starts_with('/') {
        return Some(raw.trim_end_matches('/').to_string());
    }
    Some(format!("{}/{}", cwd.trim_end_matches('/'), raw))
}

/// Quote `value` for a POSIX shell as a single argument.
pub(crate) fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_remote_resolves_shorthand() {
        assert_eq!(expand_remote("~/flo", "/Users/me"), "/Users/me/flo");
        assert_eq!(expand_remote("~", "/Users/me"), "/Users/me");
        assert_eq!(expand_remote("/abs/path", "/Users/me"), "/abs/path");
        assert_eq!(expand_remote("~/flo", ""), "~/flo");
    }

    #[test]
    fn normalize_common_resolves_relative_answers() {
        assert_eq!(
            normalize_common("/a/b", ".git"),
            Some("/a/b/.git".to_string())
        );
        assert_eq!(
            normalize_common("/a/b", "/x/y/.git\n"),
            Some("/x/y/.git".to_string())
        );
        assert_eq!(normalize_common("/a/b", "  \n "), None);
    }

    #[test]
    fn dirty_checkout_is_detected_and_discarded() {
        let dir = std::env::temp_dir().join(format!("pilus-dirty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        init_repository(&dir).expect("init");
        let path = dir.to_string_lossy().into_owned();
        let git = |args: &[&str]| {
            git_in(None, &path, args).expect("git");
        };
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("tracked.txt"), "one").expect("write");
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", "init"]);
        assert_eq!(worktree_is_dirty(None, &path), Ok(false));

        std::fs::write(dir.join("tracked.txt"), "two").expect("edit");
        std::fs::write(dir.join("untracked.txt"), "x").expect("write");
        assert_eq!(worktree_is_dirty(None, &path), Ok(true));

        discard_worktree_changes(None, &path).expect("discard");
        assert_eq!(worktree_is_dirty(None, &path), Ok(false));
        assert_eq!(
            std::fs::read_to_string(dir.join("tracked.txt")).expect("read"),
            "one"
        );
        assert!(!dir.join("untracked.txt").exists());

        let missing = dir.join("missing").to_string_lossy().into_owned();
        assert!(worktree_is_dirty(None, &missing).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
