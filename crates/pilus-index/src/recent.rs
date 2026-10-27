//! A machine's newest sessions and their conversations, read on demand with no
//! index behind them, for a client that has none (the phone, through
//! `pilus-agent`'s hub).
//!
//! The listing reads evidence for the newest transcripts only, so asking for
//! twenty sessions costs a directory walk and twenty short reads. A transcript
//! is read by path, and only a path that has the shape of one of the agents'
//! own transcripts is ever opened, so a caller cannot use this to read
//! anything else on the machine.

use std::path::Path;

use crate::error::{Error, Result};
use crate::model::{Agent, Session};
use crate::parse::{self, Evidence};
use crate::refresh::Source;
use crate::scan::{self, Probe};
use crate::transcript::{self, Transcript};

/// How many of an agent's newest transcripts are read looking for the one a
/// live agent runs in its folder.
const MATCH_DEPTH: usize = 40;

/// Up to `limit` of the newest sessions with a title, newest first, of `agent`
/// when one is named.
pub fn recent(
    source: &Source<'_>,
    machine: &str,
    agent: Option<Agent>,
    limit: usize,
) -> Result<Vec<Session>> {
    let probes = newest(source, agent)?;
    // A few have no prompt yet and are skipped, so read a little past the limit.
    let wanted = limit.saturating_add(limit / 2 + 2);
    let picked: Vec<Probe> = probes.into_iter().take(wanted).collect();
    let mut sessions: Vec<Session> = read(source, machine, &picked)?
        .into_iter()
        .filter(|session| !session.title.is_empty())
        .collect();
    sessions.truncate(limit);
    Ok(sessions)
}

/// The session `agent` most likely runs on in `cwd`: the one with `session` as
/// its id when that is known, else the newest one recorded in that folder.
pub fn session_for(
    source: &Source<'_>,
    machine: &str,
    agent: Agent,
    cwd: &str,
    session: Option<&str>,
) -> Result<Option<Session>> {
    let probes = newest(source, Some(agent))?;
    let session = session.filter(|id| !id.is_empty());
    if let Some(id) = session
        && let Some(probe) = probes.iter().find(|probe| stem(&probe.path).contains(id))
    {
        let found = read(source, machine, std::slice::from_ref(probe))?;
        if let Some(found) = found.into_iter().next() {
            return Ok(Some(found));
        }
    }
    let trim = |path: &str| path.trim_end_matches('/').to_string();
    let cwd = trim(cwd);
    let picked: Vec<Probe> = probes.into_iter().take(MATCH_DEPTH).collect();
    let sessions = read(source, machine, &picked)?;
    if let Some(id) = session
        && let Some(found) = sessions.iter().find(|found| found.session_id == id)
    {
        return Ok(Some(found.clone()));
    }
    Ok(sessions.into_iter().find(|found| trim(&found.cwd) == cwd))
}

/// The conversation in the last `window` bytes of the transcript at `path`,
/// refused unless the path is shaped like an agent's transcript.
pub fn transcript_at(source: &Source<'_>, path: &str, window: u64) -> Result<Transcript> {
    let agent = transcript_agent(source, path)?;
    match source {
        Source::Local(_) => transcript::read_transcript_local(path, agent, window),
        Source::Remote(target) => transcript::read_transcript_remote(target, path, agent, window),
    }
}

/// The agent a transcript path belongs to, or an error for a path that is not
/// one: it must be absolute, end in `.jsonl`, sit in an agent's session folder
/// (under `home` on this machine), and never step back up a folder.
fn transcript_agent(source: &Source<'_>, path: &str) -> Result<Agent> {
    let refuse = || Error::NotTranscript;
    if !path.starts_with('/')
        || !path.ends_with(".jsonl")
        || path.contains('\n')
        || path.contains('\0')
        || path.split('/').any(|part| part == "..")
    {
        return Err(refuse());
    }
    let agent = Agent::from_path(path).ok_or_else(refuse)?;
    if let Source::Local(home) = source {
        let root = home.join(agent.root());
        if !Path::new(path).starts_with(&root) {
            return Err(refuse());
        }
    }
    Ok(agent)
}

/// Every transcript of `agent` (or of every agent), newest first.
fn newest(source: &Source<'_>, agent: Option<Agent>) -> Result<Vec<Probe>> {
    let mut probes = match source {
        Source::Local(home) => scan::list_local(home)?,
        Source::Remote(target) => scan::list_remote(target)?,
    };
    probes.retain(|probe| agent.is_none_or(|agent| probe.agent == agent));
    probes.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| a.path.cmp(&b.path)));
    Ok(probes)
}

/// Sessions for `probes`, in their order; one that cannot be read is skipped.
fn read(source: &Source<'_>, machine: &str, probes: &[Probe]) -> Result<Vec<Session>> {
    let evidence: Vec<(String, Evidence)> = match source {
        Source::Local(_) => probes
            .iter()
            .filter_map(|probe| {
                scan::read_local(&probe.path, probe.agent)
                    .ok()
                    .map(|evidence| (probe.path.clone(), evidence))
            })
            .collect(),
        Source::Remote(target) => {
            if probes.is_empty() {
                Vec::new()
            } else {
                let paths: Vec<String> = probes.iter().map(|probe| probe.path.clone()).collect();
                scan::read_remote(target, &paths)?
            }
        }
    };
    Ok(probes
        .iter()
        .filter_map(|probe| {
            let (_, evidence) = evidence.iter().find(|(path, _)| *path == probe.path)?;
            Some(session_of(machine, probe, evidence))
        })
        .collect())
}

fn stem(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .and_then(|name| name.strip_suffix(".jsonl"))
        .unwrap_or(path)
}

/// The row a transcript makes, from its listing and evidence.
pub(crate) fn session_of(machine: &str, probe: &Probe, evidence: &Evidence) -> Session {
    let parsed = parse::parse(probe.agent, stem(&probe.path), evidence);
    Session {
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
        path: probe.path.clone(),
        size: probe.size,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::transcript::Entry;

    fn home(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pilus-index-recent-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("home");
        dir
    }

    fn write_claude(home: &Path, id: &str, cwd: &str, prompt: &str) -> PathBuf {
        let dir = home.join(".claude/projects/-work-app");
        fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(
            &path,
            format!(
                "{{\"type\":\"user\",\"cwd\":\"{cwd}\",\"timestamp\":\"2026-10-09T14:15:31.131Z\",\"message\":{{\"role\":\"user\",\"content\":\"{prompt}\"}}}}\n\
                 {{\"type\":\"assistant\",\"timestamp\":\"2026-10-09T14:15:40.000Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"done\"}}]}}}}\n"
            ),
        )
        .expect("write");
        path
    }

    #[test]
    fn recent_lists_titled_sessions_and_finds_a_live_agents_one() {
        let home = home("list");
        write_claude(&home, "aaa", "/work/app", "fix the build");
        write_claude(&home, "bbb", "/work/other", "write docs");
        let source = Source::Local(home.clone());

        let sessions = recent(&source, "local", None, 10).expect("recent");
        assert_eq!(sessions.len(), 2);
        assert!(
            sessions
                .iter()
                .any(|session| session.title == "fix the build")
        );

        let found = session_for(&source, "local", Agent::Claude, "/work/other/", None)
            .expect("match")
            .expect("found");
        assert_eq!(found.session_id, "bbb");
        let by_id = session_for(&source, "local", Agent::Claude, "/nowhere", Some("aaa"))
            .expect("match")
            .expect("found");
        assert_eq!(by_id.cwd, "/work/app");

        let read = transcript_at(&source, &by_id.path, 1 << 20).expect("transcript");
        assert!(matches!(&read.entries[0], Entry::User { text, .. } if text == "fix the build"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn only_transcript_paths_are_opened() {
        let home = home("refuse");
        let source = Source::Local(home.clone());
        let outside = format!("{}/.ssh/id_ed25519", home.display());
        assert!(transcript_at(&source, &outside, 1024).is_err());
        let climb = format!("{}/.claude/projects/../../secret.jsonl", home.display());
        assert!(transcript_at(&source, &climb, 1024).is_err());
        assert!(transcript_at(&source, "relative/.claude/projects/x.jsonl", 1024).is_err());
        let elsewhere = "/etc/.claude/projects/x/y.jsonl";
        assert!(transcript_at(&source, elsewhere, 1024).is_err());
        let _ = fs::remove_dir_all(&home);
    }
}
