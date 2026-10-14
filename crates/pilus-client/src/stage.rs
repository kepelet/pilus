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

/// What discarding a file throws away.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Discard {
    /// A tracked file's edits (or its deletion): the working tree goes back to
    /// what the index holds, so anything staged survives.
    Edits,
    /// A file Git does not track: it is deleted. Nothing can bring it back.
    Untracked,
}

impl Discard {
    fn args(self) -> &'static [&'static str] {
        match self {
            // `checkout --` rather than `restore`, which older Git lacks.
            Self::Edits => &["--literal-pathspecs", "checkout", "-q", "--"],
            // `-d` because an untracked directory is listed as one entry.
            Self::Untracked => &["--literal-pathspecs", "clean", "-f", "-d", "-q", "--"],
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
    let fallback = format!("couldn't {} the file", action.verb());
    run_git(target, repo, action.args(), paths, &fallback)
}

/// Throw away the working-tree state of `paths`; see [`Discard`]. The caller
/// confirms first, because neither kind can be undone.
pub fn discard(
    target: Option<&SshTarget>,
    repo: &str,
    paths: &[String],
    kind: Discard,
) -> Result<(), String> {
    run_git(
        target,
        repo,
        kind.args(),
        paths,
        "couldn't discard the changes",
    )
}

fn run_git(
    target: Option<&SshTarget>,
    repo: &str,
    args: &[&str],
    paths: &[String],
    fallback: &str,
) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut command = match target {
        None => {
            let mut command = Command::new("git");
            command.arg("-C").arg(repo);
            command.args(args).args(paths);
            command
        }
        Some(target) => target.shell(&remote_command(repo, paths, args)),
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
        fallback.to_string()
    } else {
        stderr
    })
}

/// The line the remote shell runs, every argument quoted.
fn remote_command(repo: &str, paths: &[String], args: &[&str]) -> String {
    let mut words = vec!["git".to_string(), "-C".to_string(), sh_quote(repo)];
    words.extend(args.iter().map(|arg| sh_quote(arg)));
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
    fn discarding_edits_keeps_what_is_staged() {
        let dir = repo("discard-edits");
        let root = dir.to_str().expect("utf8");
        std::fs::write(dir.join("a.txt"), "one\n").expect("write");
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-q", "-m", "first"]);
        std::fs::write(dir.join("a.txt"), "two\n").expect("write");
        git(&dir, &["add", "a.txt"]);
        std::fs::write(dir.join("a.txt"), "three\n").expect("write");
        discard(None, root, &paths(&["a.txt"]), Discard::Edits).expect("discard");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).expect("read"),
            "two\n"
        );
        assert_eq!(git(&dir, &["status", "--porcelain"]), "M  a.txt\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discarding_a_deletion_brings_the_file_back() {
        let dir = repo("discard-delete");
        let root = dir.to_str().expect("utf8");
        std::fs::write(dir.join("a.txt"), "one\n").expect("write");
        git(&dir, &["add", "a.txt"]);
        git(&dir, &["commit", "-q", "-m", "first"]);
        std::fs::remove_file(dir.join("a.txt")).expect("remove");
        discard(None, root, &paths(&["a.txt"]), Discard::Edits).expect("discard");
        assert_eq!(git(&dir, &["status", "--porcelain"]), "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discarding_untracked_deletes_only_the_named_paths() {
        let dir = repo("discard-untracked");
        let root = dir.to_str().expect("utf8");
        std::fs::write(dir.join("a.rs"), "a\n").expect("write");
        std::fs::write(dir.join("*.rs"), "star\n").expect("write");
        std::fs::create_dir_all(dir.join("new")).expect("dir");
        std::fs::write(dir.join("new/b.txt"), "b\n").expect("write");
        discard(None, root, &paths(&["*.rs", "new/"]), Discard::Untracked).expect("discard");
        assert!(dir.join("a.rs").exists(), "a pattern must not match a.rs");
        assert!(!dir.join("*.rs").exists());
        assert!(!dir.join("new").exists());
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
        let line = remote_command(
            "/work/my repo",
            &paths(&["a b.rs"]),
            StageAction::Stage.args(),
        );
        assert_eq!(
            line,
            "git -C '/work/my repo' '--literal-pathspecs' 'add' '-A' '--' 'a b.rs'"
        );
    }
}
