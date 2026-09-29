//! Persistent project definitions, unique to `l3s`.
//!
//! A project names a durable unit of work: a human name, the machine it lives
//! on (`None` is this machine), the repository URL (kept for later work such
//! as PR discovery), an optional branch prefix applied to new worktrees, and
//! the base directory on that machine where worktrees live — or which is
//! worked on directly. Luvus owns workspaces and worktrees; projects only
//! group them, so this file is a plain JSON store under the state root:
//!
//! ```text
//! $L3S_HOME/projects.json
//! ```
//!
//! Failures yield an empty list: projects are sidebar chrome, and a corrupt
//! file must never disturb the window.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// File name of the project store inside the l3s state directory.
const FILE_NAME: &str = "projects.json";

/// One persistent project.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Project {
    /// Stable id, derived from the name at creation.
    pub id: String,
    /// Human name, e.g. `flo`.
    pub name: String,
    /// Saved machine profile id, or `None` for this machine.
    pub machine: Option<String>,
    /// Repository URL, kept for later discovery work.
    pub repository: String,
    /// Optional branch prefix for new worktrees, e.g. `riz/`.
    pub branch_prefix: String,
    /// Base directory on the project's machine.
    pub location: String,
}

impl Project {
    /// The endpoint key this project lives on: `local` or a machine id.
    pub fn endpoint_key(&self) -> &str {
        self.machine.as_deref().unwrap_or("local")
    }

    /// Whether the project lives on this machine.
    pub fn is_local(&self) -> bool {
        self.machine.is_none()
    }

    /// The branch name for a new worktree called `name`.
    pub fn branch_for(&self, name: &str) -> String {
        format!("{}{}", self.branch_prefix, name.trim())
    }

    /// Whether `cwd` on `endpoint_key` belongs to this project: the location
    /// itself or anything beneath it.
    pub fn owns_workspace(&self, endpoint_key: &str, cwd: &str) -> bool {
        if self.endpoint_key() != endpoint_key {
            return false;
        }
        let location = self.location.trim_end_matches('/');
        if location.is_empty() || cwd.is_empty() {
            return false;
        }
        let cwd = cwd.trim_end_matches('/');
        cwd == location || cwd.starts_with(&format!("{location}/"))
    }

    /// Whether `cwd` belongs to this project by directory or by repository.
    ///
    /// Worktrees usually live outside the base directory (e.g. under the
    /// daemon's worktree root), so the directory rule alone misses them.
    /// `common` is the workspace's `git rev-parse --git-common-dir`, which a
    /// worktree reports as its main repo's git dir: matching it against the
    /// project's own `<location>/.git` groups a repo's worktrees no matter
    /// where they were created.
    pub fn owns_workspace_repo(&self, endpoint_key: &str, cwd: &str, common: Option<&str>) -> bool {
        if self.owns_workspace(endpoint_key, cwd) {
            return true;
        }
        if self.endpoint_key() != endpoint_key {
            return false;
        }
        let location = self.location.trim_end_matches('/');
        common.is_some_and(|common| !location.is_empty() && common == format!("{location}/.git"))
    }
}

/// The store file for the l3s state directory `root`.
pub fn projects_file(root: &Path) -> PathBuf {
    root.join(FILE_NAME)
}

/// Load every project under `root`; corrupt or missing files read as empty.
pub fn load_projects(root: &Path) -> Vec<Project> {
    let bytes = std::fs::read(projects_file(root)).unwrap_or_default();
    if bytes.is_empty() {
        return Vec::new();
    }
    parse_projects(&bytes)
}

/// Persist `projects` under `root`, creating the directory as needed.
pub fn save_projects(root: &Path, projects: &[Project]) -> Result<()> {
    std::fs::create_dir_all(root)?;
    let payload = serde_json::to_string_pretty(&serde_json::json!({ "projects": projects }))
        .map_err(|error| Error::Projects(error.to_string()))?;
    std::fs::write(root.join(FILE_NAME), payload)?;
    Ok(())
}

/// Derive a stable id from `name`: lowercase alphanumerics and dashes.
pub fn id_for_name(name: &str) -> String {
    let mut id: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    while id.contains("--") {
        id = id.replace("--", "-");
    }
    id.trim_matches('-').to_string()
}

/// Validate a project before saving; returns the human error to show.
pub fn validate(name: &str, location: &str) -> Option<String> {
    if name.trim().is_empty() {
        return Some("A project name is required.".to_string());
    }
    if id_for_name(name).is_empty() {
        return Some("Use letters or numbers in the project name.".to_string());
    }
    if location.trim().is_empty() {
        return Some("A base directory is required.".to_string());
    }
    None
}

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

/// Resolve `cwd` to its git common dir on this machine.
///
/// A linked worktree reports its main repo's git dir, which is what lets a
/// project own worktrees living outside its base directory. Returns `None`
/// when `cwd` is not inside a git repository.
pub fn common_git_dir(cwd: &str) -> Option<String> {
    // The daemon reports `~`-shorthand dirs, which git itself will not
    // expand; resolve against this machine's home first.
    let cwd = expand_local(cwd);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&cwd)
        .arg("rev-parse")
        .arg("--git-common-dir")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    normalize_common(&cwd, &String::from_utf8_lossy(&output.stdout))
}

/// Resolve `cwd` to its current branch name on this machine.
///
/// Returns `None` outside a git repository or on a detached HEAD.
pub fn current_branch(cwd: &str) -> Option<String> {
    let cwd = expand_local(cwd);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&cwd)
        .arg("branch")
        .arg("--show-current")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

/// List `cwd`'s local branch names on this machine, most recent first.
///
/// Returns an empty list outside a git repository. The first entry is not
/// necessarily the checked-out branch; callers that need that use
/// [`current_branch`].
#[must_use]
pub fn list_branches(cwd: &str) -> Vec<String> {
    let cwd = expand_local(cwd);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&cwd)
        .arg("for-each-ref")
        .arg("--sort=-committerdate")
        .arg("--format=%(refname:short)")
        .arg("refs/heads")
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolve many `cwds` to their current branch names on a remote machine.
///
/// One SSH invocation carries the whole batch as NUL-separated answers.
/// `~`-shorthand dirs are translated to `$HOME` in-script. Empty answers
/// (not a repo, detached HEAD) map to `None`.
pub fn current_branches_remote(
    target: &crate::transport::SshTarget,
    cwds: &[String],
) -> Vec<(String, Option<String>)> {
    if cwds.is_empty() {
        return Vec::new();
    }
    let mut script = String::from("for d in");
    for cwd in cwds {
        script.push(' ');
        script.push_str(&remote_dir_arg(cwd));
    }
    script.push_str(
        "; do git -C \"$d\" branch --show-current 2>/dev/null | tr -d '\\n'; printf '\\0'; done",
    );
    let Ok(output) = target.shell(&script).output() else {
        return cwds.iter().map(|cwd| (cwd.clone(), None)).collect();
    };
    if !output.status.success() {
        return cwds.iter().map(|cwd| (cwd.clone(), None)).collect();
    }
    let answers: Vec<&[u8]> = output.stdout.split(|byte| *byte == 0).collect();
    cwds.iter()
        .enumerate()
        .map(|(index, cwd)| {
            let branch = answers
                .get(index)
                .map(|bytes| String::from_utf8_lossy(bytes).trim().to_string())
                .unwrap_or_default();
            (cwd.clone(), (!branch.is_empty()).then_some(branch))
        })
        .collect()
}
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
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn parse_projects(bytes: &[u8]) -> Vec<Project> {
    let value: serde_json::Value = serde_json::from_slice(bytes).unwrap_or(serde_json::Value::Null);
    let mut projects: Vec<Project> = value
        .get("projects")
        .and_then(|projects| projects.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|item| serde_json::from_value(item.clone()).ok())
        .collect();
    projects.sort_by_key(|project| project.name.to_lowercase());
    projects
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_derives_from_name() {
        assert_eq!(id_for_name("flo"), "flo");
        assert_eq!(id_for_name(" My Cool Project! "), "my-cool-project");
        assert_eq!(id_for_name("---"), "");
    }

    #[test]
    fn validation_requires_name_and_location() {
        assert!(validate("", "/tmp/x").is_some());
        assert!(validate("flo", "").is_some());
        assert!(validate("flo", "~/work").is_none());
    }

    #[test]
    fn ownership_matches_location_and_children() {
        let project = Project {
            id: "flo".to_string(),
            name: "flo".to_string(),
            machine: None,
            repository: String::new(),
            branch_prefix: String::new(),
            location: "/Users/me/work/flo".to_string(),
        };
        assert!(project.owns_workspace("local", "/Users/me/work/flo"));
        assert!(project.owns_workspace("local", "/Users/me/work/flo/auth"));
        assert!(!project.owns_workspace("local", "/Users/me/work/flo2"));
        assert!(!project.owns_workspace("mac-mini", "/Users/me/work/flo"));
    }

    #[test]
    fn branch_prefix_is_applied() {
        let project = Project {
            branch_prefix: "riz/".to_string(),
            ..Project::default()
        };
        assert_eq!(project.branch_for("auth"), "riz/auth");
    }

    #[test]
    fn repo_rule_groups_worktrees_outside_the_base() {
        let project = Project {
            id: "l3s".to_string(),
            name: "l3s".to_string(),
            machine: None,
            repository: String::new(),
            branch_prefix: String::new(),
            location: "/Users/me/.superset/projects/l3s".to_string(),
        };
        let common = "/Users/me/.superset/projects/l3s/.git";
        assert!(project.owns_workspace_repo(
            "local",
            "/Users/me/.luvus/worktrees/l3s/lively-glider",
            Some(common),
        ));
        assert!(project.owns_workspace_repo("local", "/Users/me/.superset/projects/l3s", None));
        assert!(!project.owns_workspace_repo(
            "local",
            "/Users/me/other",
            Some("/Users/me/other/.git"),
        ));
        assert!(!project.owns_workspace_repo(
            "mac-mini",
            "/Users/me/.luvus/worktrees/l3s/lively-glider",
            Some(common),
        ));
    }

    #[test]
    fn current_branch_reports_and_misses() {
        // This checkout is a git repository with a checked-out branch.
        let here = env!("CARGO_MANIFEST_DIR");
        assert!(common_git_dir(here).is_some());
        assert!(current_branch(here).is_some());
        assert_eq!(current_branch("/tmp"), None);
    }

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
    fn garbage_yields_no_projects() {
        assert!(parse_projects(b"not json").is_empty());
        assert!(parse_projects(b"{}").is_empty());
    }

    #[test]
    fn round_trip_through_disk() {
        let dir = std::env::temp_dir().join(format!("l3s-projects-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let projects = vec![Project {
            id: "flo".to_string(),
            name: "flo".to_string(),
            machine: Some("mac-mini".to_string()),
            repository: "git@github.com:me/flo.git".to_string(),
            branch_prefix: String::new(),
            location: "/Users/me/work/flo".to_string(),
        }];
        save_projects(&dir, &projects).expect("save");
        assert_eq!(load_projects(&dir), projects);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
