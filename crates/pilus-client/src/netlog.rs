//! A short memory of what pilus asked of the network, for the Process page.
//!
//! Every place that reaches outside the machine (a call to a sipil node, an SSH
//! command to a saved host, a `gh` request, a download) notes it here with where
//! it went, what it was and how it ended. The log keeps the newest few hundred
//! and a running count per kind, so the page can show both what is happening
//! now and how much of it there has been. It records nothing about content:
//! a target is a short device ID or a host name, and `what` is a method or a
//! subcommand.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How many calls are remembered.
const KEPT: usize = 300;

/// Where a call went.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Kind {
    /// A call to a sipil daemon or another pilus, over iroh.
    Sipil,
    /// An SSH command to a saved machine.
    Ssh,
    /// A request to GitHub through `gh`.
    Github,
    /// A plain HTTPS download.
    Http,
}

impl Kind {
    /// Every kind, in the order the page lists them.
    pub const ALL: [Self; 4] = [Self::Sipil, Self::Ssh, Self::Github, Self::Http];

    /// A word for the page.
    pub fn label(self) -> &'static str {
        match self {
            Self::Sipil => "sipil",
            Self::Ssh => "ssh",
            Self::Github => "github",
            Self::Http => "http",
        }
    }
}

/// One call.
#[derive(Clone, Debug)]
pub struct Entry {
    /// When it started.
    pub at: Instant,
    /// Where it went.
    pub kind: Kind,
    /// Who it went to: a short device ID, or a host.
    pub target: String,
    /// What it asked: a method, a subcommand.
    pub what: String,
    /// How long it took. `None` for a call noted when it was started and not
    /// timed, such as an SSH command that is spawned by someone else.
    pub took: Option<Duration>,
    /// Whether it succeeded. A call that was only started counts as having.
    pub ok: bool,
}

static LOG: Mutex<VecDeque<Entry>> = Mutex::new(VecDeque::new());
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

fn log() -> std::sync::MutexGuard<'static, VecDeque<Entry>> {
    LOG.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn push(entry: Entry) {
    COUNTS[entry.kind as usize].fetch_add(1, Ordering::Relaxed);
    let mut log = log();
    if log.len() >= KEPT {
        log.pop_front();
    }
    log.push_back(entry);
}

/// Note a call that has finished: `started` is when it began.
pub fn record(kind: Kind, target: &str, what: &str, started: Instant, ok: bool) {
    push(Entry {
        at: started,
        kind,
        target: target.to_string(),
        what: what.to_string(),
        took: Some(started.elapsed()),
        ok,
    });
}

/// Note a call at the moment it is started, when its end is not seen here.
pub fn note(kind: Kind, target: &str, what: &str) {
    push(Entry {
        at: Instant::now(),
        kind,
        target: target.to_string(),
        what: what.to_string(),
        took: None,
        ok: true,
    });
}

/// The newest calls, newest first, at most `limit`.
pub fn recent(limit: usize) -> Vec<Entry> {
    log().iter().rev().take(limit).cloned().collect()
}

/// How many calls of `kind` have been made since launch.
pub fn total(kind: Kind) -> u64 {
    COUNTS[kind as usize].load(Ordering::Relaxed)
}

/// How many calls have been made since launch, of every kind.
pub fn total_all() -> u64 {
    Kind::ALL.into_iter().map(total).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test, because the log is process-wide and a second test's flood would
    /// push the first's entries out from under it.
    #[test]
    fn calls_are_counted_by_kind_listed_newest_first_and_only_the_newest_kept() {
        let before = (total(Kind::Http), total_all());
        let started = Instant::now();
        record(Kind::Http, "netlog-test", "download", started, false);
        note(Kind::Http, "netlog-test", "version");
        assert!(total(Kind::Http) >= before.0 + 2);
        assert!(total_all() >= before.1 + 2);
        let mine: Vec<Entry> = recent(KEPT)
            .into_iter()
            .filter(|entry| entry.target == "netlog-test")
            .collect();
        assert_eq!(mine[0].what, "version");
        assert!(mine[0].took.is_none());
        assert_eq!(mine[1].what, "download");
        assert!(!mine[1].ok && mine[1].took.is_some());

        for n in 0..KEPT + 20 {
            note(Kind::Ssh, "netlog-flood", &n.to_string());
        }
        assert_eq!(recent(usize::MAX).len(), KEPT);
    }
}
