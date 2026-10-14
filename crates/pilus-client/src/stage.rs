//! Staging and unstaging files in the checkout, with Git itself.
//!
//! The daemon's `diff.*` methods describe the working tree but have no call
//! that changes the index, so the Diff page asks Git directly, on this machine
//! or over the saved SSH target, for the repository the daemon reports. Paths
//! come from a daemon, so every one is taken literally: `--literal-pathspecs`
//! keeps a file named `*.rs` or `:(top)x` from being read as a pattern, and
//! the `--` keeps a name that starts with `-` from being read as an option.

use std::process::{Command, Stdio};

use crate::git::sh_quote;
use crate::transport::SshTarget;

/// Which way a file moves between the index and the working tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageAction {
    /// Add the file's working-tree state, a deletion included, to the index.
    Stage,
    /// Put the index's entry back to what `HEAD` holds, leaving the working
    /// tree alone.
    Unstage,
}

impl StageAction {
    /// The Git arguments after `-C <repo>`, before the paths.
    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Stage => &["--literal-pathspecs", "add", "-A", "--"],
            // `reset -- <paths>` rather than `restore --staged`: it also
            // works in a repository whose first commit is still to come.
            Self::Unstage => &["--literal-pathspecs", "reset", "-q", "--"],
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Stage => "stage",
            Self::Unstage => "unstage",
        }
    }
}

/// Stage or unstage `paths` (relative to the repository at `repo`) on this
/// machine, or on `target`'s host. Failures carry Git's own message.
pub fn set_staged(
    target: Option<&SshTarget>,
    repo: &str,
    paths: &[String],
    action: StageAction,
) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut command = match target {
        None => {
            let mut command = Command::new("git");
            command.arg("-C").arg(repo);
            command.args(action.args()).args(paths);
            command
        }
        Some(target) => target.shell(&remote_command(repo, paths, action)),
    };
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("couldn't run git: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("couldn't {} the file", action.verb())
    } else {
        stderr
    })
}

/// The line the remote shell runs, every argument quoted.
fn remote_command(repo: &str, paths: &[String], action: StageAction) -> String {
    let mut words = vec!["git".to_string(), "-C".to_string(), sh_quote(repo)];
    words.extend(action.args().iter().map(|arg| sh_quote(arg)));
    words.extend(paths.iter().map(|path| sh_quote(path)));
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-stage-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("dir");
        git(&dir, &["init", "--quiet"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "t"]);
        dir
    }

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn paths(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn staging_then_unstaging_moves_a_file_through_the_index() {
        let dir = repo("round-trip");
        let root = dir.to_str().expect("utf8");
        std::fs::write(dir.join("a.txt"), "one\n").expect("write");
        set_staged(None, root, &paths(&["a.txt"]), StageAction::Stage).expect("stage");
        assert_eq!(git(&dir, &["status", "--porcelain"]), "A  a.txt\n");
        // No commit exists yet; unstaging must still work.
        set_staged(None, root, &paths(&["a.txt"]), StageAction::Unstage).expect("unstage");
        assert_eq!(git(&dir, &["status", "--porcelain"]), "?? a.txt\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_deletion_stages_and_unstages() {
        let dir = repo("delete");
        let root = dir.to_str().expect("utf8");
        std::fs::write(dir.join("a.txt"), "one\n").expect("write");
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-q", "-m", "first"]);
        std::fs::remove_file(dir.join("a.txt")).expect("remove");
        set_staged(None, root, &paths(&["a.txt"]), StageAction::Stage).expect("stage");
        assert_eq!(git(&dir, &["status", "--porcelain"]), "D  a.txt\n");
        set_staged(None, root, &paths(&["a.txt"]), StageAction::Unstage).expect("unstage");
        assert_eq!(git(&dir, &["status", "--porcelain"]), " D a.txt\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_path_is_never_a_pattern_or_an_option() {
        let dir = repo("literal");
        let root = dir.to_str().expect("utf8");
        std::fs::write(dir.join("a.rs"), "a\n").expect("write");
        std::fs::write(dir.join("*.rs"), "star\n").expect("write");
        std::fs::write(dir.join("-n"), "dash\n").expect("write");
        set_staged(None, root, &paths(&["*.rs", "-n"]), StageAction::Stage).expect("stage");
        let status = git(&dir, &["status", "--porcelain"]);
        assert!(
            status.contains("A  \"*.rs\"") || status.contains("A  *.rs"),
            "{status}"
        );
        assert!(status.contains("A  -n"), "{status}");
        assert!(status.contains("?? a.rs"), "{status}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn git_failures_surface_the_reason() {
        let error = set_staged(
            None,
            "/definitely/not/a/repo",
            &paths(&["a"]),
            StageAction::Stage,
        )
        .expect_err("not a repo");
        assert!(!error.is_empty());
    }

    #[test]
    fn the_remote_line_quotes_every_word() {
        let line = remote_command("/work/my repo", &paths(&["a b.rs"]), StageAction::Stage);
        assert_eq!(
            line,
            "git -C '/work/my repo' '--literal-pathspecs' 'add' '-A' '--' 'a b.rs'"
        );
    }
}
