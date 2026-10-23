//! Branch state and history from the daemon's UHP `git.*` methods.
//!
//! The daemon runs Git for its active workspace and pilus only draws the
//! answer, so the Git tab follows whichever workspace the sidebar focuses, as
//! the Diff does. Everything here is read-only. Parsing is lenient, as in
//! [`crate::diff`]: a field an older daemon omits reads as empty rather than
//! failing the whole snapshot, and a row without its identifying field is
//! dropped.

use serde_json::{Value, json};

use crate::control::ControlClient;
use crate::error::{Error, Result};

/// Where the active workspace's branch stands. What changed in the working
/// tree is the Diff's to show, so only the branch's position is read here.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GitStatus {
    pub branch: String,
    /// The branch it tracks; `None` when it tracks nothing.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub stashes: u32,
}

impl GitStatus {
    fn from_json(value: &Value) -> Self {
        Self {
            branch: text(value, "branch"),
            upstream: value
                .get("upstream")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_string),
            ahead: count(value, "ahead"),
            behind: count(value, "behind"),
            // The entries' shape is the daemon's to change; only their number
            // is drawn.
            stashes: array(value, "stashes").len().min(u32::MAX as usize) as u32,
        }
    }
}

/// A branch or tag a commit carries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitRef {
    pub name: String,
    pub kind: CommitRefKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitRefKind {
    /// The checked-out branch (`HEAD -> name`), or a detached `HEAD`.
    Head,
    Branch,
    Tag,
}

/// One commit of the history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCommit {
    /// The abbreviated hash.
    pub sha: String,
    pub subject: String,
    pub author: String,
    /// When it landed, as the daemon words it (`4 hours ago`).
    pub when: String,
    pub refs: Vec<CommitRef>,
}

impl GitCommit {
    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            sha: value.get("sha")?.as_str()?.to_string(),
            subject: text(value, "subject"),
            author: text(value, "author"),
            when: text(value, "when"),
            refs: parse_refs(&text(value, "refs")),
        })
    }
}

/// Read git's decoration, such as `(HEAD -> main, origin/main, tag: v1)`.
fn parse_refs(decoration: &str) -> Vec<CommitRef> {
    let inner = decoration
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')');
    inner
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            if let Some(name) = part.strip_prefix("HEAD -> ") {
                (name, CommitRefKind::Head)
            } else if let Some(name) = part.strip_prefix("tag: ") {
                (name, CommitRefKind::Tag)
            } else if part == "HEAD" {
                (part, CommitRefKind::Head)
            } else {
                (part, CommitRefKind::Branch)
            }
        })
        .map(|(name, kind)| CommitRef {
            name: name.to_string(),
            kind,
        })
        .collect()
}

fn text(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn count(value: &Value, field: &str) -> u32 {
    value
        .get(field)
        .and_then(Value::as_u64)
        .map_or(0, |n| n.min(u64::from(u32::MAX)) as u32)
}

fn array<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// The daemon reads the count from `n`. Any other name, such as `limit`, is
/// ignored without an error and the reply stops at the daemon's default of 30,
/// which leaves the History tab unable to page past them.
fn log_params(limit: u32) -> Value {
    json!({ "n": limit })
}

impl ControlClient {
    /// Branch, tracking, and working-tree state of the active workspace.
    pub fn git_status(&mut self) -> Result<GitStatus> {
        let result = self.request("git.status", json!({}))?;
        if result.get("branch").is_none() {
            return Err(Error::Control("git.status: unrecognized reply".to_string()));
        }
        Ok(GitStatus::from_json(&result))
    }

    /// The newest `limit` commits of the active workspace, newest first.
    pub fn git_log(&mut self, limit: u32) -> Result<Vec<GitCommit>> {
        let result = self.request("git.log", log_params(limit))?;
        Ok(array(&result, "commits")
            .iter()
            .filter_map(GitCommit::from_json)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reads_the_daemons_shape() {
        let status = GitStatus::from_json(&json!({
            "ahead": 2, "behind": 1, "branch": "feat", "upstream": "origin/feat",
            "staged": [{ "code": "A", "path": "new.rs" }],
            "unstaged": [{ "code": "M", "path": "a.rs" }],
            "untracked": ["scratch.txt"],
            "stashes": [{}, "stash@{1}"],
            "type": "git_status",
        }));
        assert_eq!((status.ahead, status.behind), (2, 1));
        assert_eq!(status.branch, "feat");
        assert_eq!(status.upstream.as_deref(), Some("origin/feat"));
        assert_eq!(status.stashes, 2);
    }

    #[test]
    fn missing_fields_read_as_empty() {
        let status = GitStatus::from_json(&json!({ "branch": "main", "upstream": null }));
        assert_eq!(status.upstream, None);
        assert_eq!((status.ahead, status.behind, status.stashes), (0, 0, 0));
    }

    #[test]
    fn decoration_names_the_head_branches_and_tags() {
        let refs = parse_refs("(HEAD -> git-integration, origin/main, tag: v1.2, main)");
        let kinds: Vec<_> = refs.iter().map(|r| (r.name.as_str(), r.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("git-integration", CommitRefKind::Head),
                ("origin/main", CommitRefKind::Branch),
                ("v1.2", CommitRefKind::Tag),
                ("main", CommitRefKind::Branch),
            ]
        );
        assert!(parse_refs("").is_empty());
        assert_eq!(parse_refs("(HEAD)")[0].kind, CommitRefKind::Head);
    }

    #[test]
    fn commit_needs_a_hash_and_tolerates_the_rest() {
        let commit = GitCommit::from_json(&json!({
            "sha": "3e1a128", "subject": "docs: cover the hub", "refs": "",
        }))
        .expect("commit");
        assert_eq!(commit.sha, "3e1a128");
        assert!(commit.author.is_empty() && commit.refs.is_empty());
        assert!(GitCommit::from_json(&json!({ "subject": "no hash" })).is_none());
    }

    #[test]
    fn log_asks_for_its_count_as_n() {
        assert_eq!(log_params(150), json!({ "n": 150 }));
    }
}
