//! Bringing the index up to date with a machine's transcripts.

use std::collections::HashSet;
use std::path::PathBuf;

use pilus_client::SshTarget;

use crate::error::Result;
use crate::model::Session;
use crate::parse::{self, Evidence};
use crate::scan::{self, Probe};
use crate::store::Index;

/// Where a machine's transcripts are read from.
pub enum Source<'a> {
    /// This machine, under the given home directory.
    Local(PathBuf),
    /// A saved machine, reached through `ssh`.
    Remote(&'a SshTarget),
}

/// What a refresh changed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Report {
    /// Transcripts read again because they were new or had grown.
    pub indexed: usize,
    /// Rows dropped because the transcript is gone.
    pub removed: usize,
}

/// Bring `machine`'s rows in line with its transcripts.
///
/// Only a transcript whose size or modified time differs from the row is read
/// again. A machine that cannot be listed leaves its rows as they were: being
/// offline is not the same as having no sessions.
pub fn refresh(index: &mut Index, machine: &str, source: &Source<'_>) -> Result<Report> {
    let probes = match source {
        Source::Local(home) => scan::list_local(home)?,
        Source::Remote(target) => scan::list_remote(target)?,
    };
    let known = index.known(machine)?;

    let listed: HashSet<&str> = probes.iter().map(|probe| probe.path.as_str()).collect();
    let gone: Vec<String> = known
        .keys()
        .filter(|path| !listed.contains(path.as_str()))
        .cloned()
        .collect();

    let changed: Vec<&Probe> = probes
        .iter()
        .filter(|probe| known.get(&probe.path) != Some(&(probe.size, probe.mtime)))
        .collect();
    let paths: Vec<String> = changed.iter().map(|probe| probe.path.clone()).collect();
    let evidence: Vec<(String, Evidence)> = match source {
        Source::Local(_) => paths
            .iter()
            .filter_map(|path| match scan::read_local(path) {
                Ok(evidence) => Some((path.clone(), evidence)),
                Err(error) => {
                    // A transcript can vanish between the listing and the read.
                    tracing::debug!(%path, %error, "session transcript unreadable");
                    None
                }
            })
            .collect(),
        Source::Remote(target) => scan::read_remote(target, &paths)?,
    };

    let sessions: Vec<Session> = evidence
        .iter()
        .filter_map(|(path, evidence)| {
            let probe = changed.iter().find(|probe| &probe.path == path)?;
            let stem = path
                .rsplit('/')
                .next()
                .and_then(|name| name.strip_suffix(".jsonl"))
                .unwrap_or(path);
            let parsed = parse::parse(probe.agent, stem, evidence);
            Some(Session {
                machine: machine.to_string(),
                agent: probe.agent,
                session_id: parsed.session_id,
                cwd: parsed.cwd,
                title: parsed.title,
                branch: parsed.branch,
                started: if parsed.started > 0 {
                    parsed.started
                } else {
                    probe.mtime
                },
                updated: probe.mtime,
                path: path.clone(),
                size: probe.size,
            })
        })
        .collect();

    index.upsert(&sessions)?;
    index.remove(machine, &gone)?;
    Ok(Report {
        indexed: sessions.len(),
        removed: gone.len(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::model::Agent;
    use crate::store::Filter;

    fn home(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pilus-index-refresh-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("home");
        dir
    }

    fn write_claude(home: &std::path::Path, id: &str, prompt: &str) -> PathBuf {
        let dir = home.join(".claude/projects/-work-app");
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(
            &path,
            format!(
                "{{\"type\":\"user\",\"cwd\":\"/work/app\",\"timestamp\":\"2026-10-09T14:15:31.131Z\",\"message\":{{\"role\":\"user\",\"content\":\"{prompt}\"}}}}\n"
            ),
        )
        .expect("write");
        path
    }

    #[test]
    fn a_refresh_indexes_then_follows_edits_and_deletions() {
        let home = home("follow");
        let first = write_claude(&home, "s1", "fix the relay");
        let mut index = Index::in_memory().expect("index");
        let source = Source::Local(home.clone());

        let report = refresh(&mut index, "local", &source).expect("refresh");
        assert_eq!(
            report,
            Report {
                indexed: 1,
                removed: 0
            }
        );
        let found = index
            .search("relay", &Filter::default(), 10)
            .expect("search");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].agent, Agent::Claude);
        assert_eq!(found[0].session_id, "s1");
        assert_eq!(found[0].cwd, "/work/app");
        assert_eq!(found[0].started, 1_791_555_331);

        // Nothing changed, so nothing is read again.
        let report = refresh(&mut index, "local", &source).expect("refresh");
        assert_eq!(report, Report::default());

        // A grown transcript is read again.
        let mut grown = fs::read_to_string(&first).expect("read");
        grown.push_str("{\"type\":\"ai-title\",\"aiTitle\":\"Relay audit\"}\n");
        fs::write(&first, grown).expect("grow");
        let report = refresh(&mut index, "local", &source).expect("refresh");
        assert_eq!(report.indexed, 1);
        let found = index.search("", &Filter::default(), 10).expect("search");
        assert_eq!(found[0].title, "Relay audit");

        fs::remove_file(&first).expect("delete");
        let report = refresh(&mut index, "local", &source).expect("refresh");
        assert_eq!(
            report,
            Report {
                indexed: 0,
                removed: 1
            }
        );
        assert_eq!(index.count(None).expect("count"), 0);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn machines_do_not_touch_each_others_rows() {
        let home = home("machines");
        write_claude(&home, "s1", "one");
        let mut index = Index::in_memory().expect("index");
        refresh(&mut index, "local", &Source::Local(home.clone())).expect("local");
        let empty = self::home("machines-empty");
        refresh(&mut index, "other", &Source::Local(empty.clone())).expect("other");
        assert_eq!(index.count(Some("local")).expect("count"), 1);
        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&empty);
    }
}
