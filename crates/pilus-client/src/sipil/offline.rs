//! What this pilus remembers of a sipil daemon, so its agent can be opened and
//! written to while the daemon is away.
//!
//! A daemon is the only place a conversation with its agent lives, and it is
//! often not running: a laptop asleep, a server restarting. Without a copy here
//! its row would lead nowhere until it answered. So every successful connect
//! leaves behind who the primary agent is, and the conversations a person has
//! looked at leave their newest lines. A message written while the daemon is
//! away goes in an outbox instead of being refused, and is handed over, in order,
//! once the daemon answers again.
//!
//! The copy is a snapshot, never a source of truth: the daemon's own transcript
//! replaces it the moment one arrives, and nothing here is sent anywhere.
//! Everything is held in memory and written to the `offline` document by [`flush`]
//! (called on a timer), because a streaming answer changes the snapshot many
//! times a second and a rewrite per change would be wasted.
//!
//! [`flush`]: Offline::flush

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pilus_db::Db;
use serde::{Deserialize, Serialize};

use super::TranscriptItem;
use super::store;

/// The document the snapshots and outboxes are kept in.
const DOC: &str = "offline";
/// The most lines of one conversation kept.
const LINE_LIMIT: usize = 200;
/// The most conversations kept per daemon; the least recently seen go first.
const CONVERSATION_LIMIT: usize = 8;
/// The most messages one daemon's outbox holds, so a daemon that never returns
/// cannot grow the file without bound.
const OUTBOX_LIMIT: usize = 50;
/// How soon after a write the next one may happen.
const WRITE_EVERY: Duration = Duration::from_secs(3);

/// The primary agent of a daemon, as last seen.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RememberedAgent {
    /// Its conversation id, which is what a chat page opens.
    pub id: String,
    /// What to call it.
    pub name: String,
}

/// The newest lines of a conversation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct Snapshot {
    items: Vec<TranscriptItem>,
    /// The daemon had lines before the first of `items`.
    more: bool,
    /// Unix seconds when it was last refreshed, which decides what is pruned.
    seen: u64,
}

/// A message written while its daemon was away.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Queued {
    /// The conversation it is for.
    pub conversation: String,
    /// Sent with the message, so a delivery that was cut off and is retried is
    /// recognised by the daemon instead of said twice.
    pub request: String,
    /// What was written, with any quote already in front.
    pub text: String,
    /// Unix seconds when it was written.
    pub at: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct Remembered {
    /// What its primary agent is called.
    #[serde(default)]
    label: String,
    #[serde(default)]
    agent: Option<RememberedAgent>,
    #[serde(default)]
    conversations: BTreeMap<String, Snapshot>,
    #[serde(default)]
    outbox: Vec<Queued>,
}

#[derive(Default, Deserialize, Serialize)]
struct File {
    #[serde(default)]
    daemons: BTreeMap<String, Remembered>,
}

struct State {
    file: File,
    /// Changed since the last write.
    dirty: bool,
    written: Option<Instant>,
}

/// The snapshots and outboxes of every daemon.
pub struct Offline {
    db: Db,
    state: Mutex<State>,
}

impl Offline {
    /// Read what `db` holds, importing `dir`'s `offline.json` the first time; with
    /// neither it starts empty.
    pub fn open(db: &Db, dir: &Path) -> Self {
        let file = store::load(db, DOC, &dir.join("offline.json"));
        Self {
            db: db.clone(),
            state: Mutex::new(State {
                file,
                dirty: false,
                written: None,
            }),
        }
    }

    fn with<R>(&self, change: impl FnOnce(&mut File) -> R) -> R {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        change(&mut state.file)
    }

    /// Like [`Offline::with`], for a change that must reach the disk.
    fn edit<R>(&self, change: impl FnOnce(&mut File) -> R) -> R {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.dirty = true;
        change(&mut state.file)
    }

    /// The name and primary agent last seen for daemon `id`.
    pub fn agent(&self, id: &str) -> Option<(String, RememberedAgent)> {
        self.with(|file| {
            let daemon = file.daemons.get(id)?;
            Some((daemon.label.clone(), daemon.agent.clone()?))
        })
    }

    /// Note who daemon `id`'s primary agent is, as a connect has just found.
    pub fn remember_agent(&self, id: &str, label: &str, agent: RememberedAgent) {
        let unchanged = self.with(|file| {
            file.daemons.get(id).is_some_and(|daemon| {
                daemon.label == label && daemon.agent.as_ref() == Some(&agent)
            })
        });
        if unchanged {
            return;
        }
        self.edit(|file| {
            let daemon = file.daemons.entry(id.to_string()).or_default();
            daemon.label = label.to_string();
            daemon.agent = Some(agent);
        });
    }

    /// The lines last seen of `conversation` on daemon `id`, oldest first, and
    /// whether the daemon had older ones.
    pub fn transcript(&self, id: &str, conversation: &str) -> Option<(Vec<TranscriptItem>, bool)> {
        self.with(|file| {
            let snapshot = file.daemons.get(id)?.conversations.get(conversation)?;
            Some((snapshot.items.clone(), snapshot.more))
        })
    }

    /// Keep the newest lines of `conversation`. Nothing is written until the next
    /// [`flush`](Self::flush), and a snapshot that has not changed costs nothing.
    pub fn remember_transcript(
        &self,
        id: &str,
        conversation: &str,
        items: &[TranscriptItem],
        more: bool,
    ) {
        // A conversation with nothing in it is not worth a snapshot, and one the
        // daemon has shortened must not be kept longer than the daemon has it.
        if items.is_empty() {
            return;
        }
        let from = items.len().saturating_sub(LINE_LIMIT);
        let items = &items[from..];
        let more = more || from > 0;
        let unchanged = self.with(|file| {
            file.daemons
                .get(id)
                .and_then(|daemon| daemon.conversations.get(conversation))
                .is_some_and(|held| held.items == items && held.more == more)
        });
        if unchanged {
            return;
        }
        self.edit(|file| {
            let daemon = file.daemons.entry(id.to_string()).or_default();
            daemon.conversations.insert(
                conversation.to_string(),
                Snapshot {
                    items: items.to_vec(),
                    more,
                    seen: store::now(),
                },
            );
            while daemon.conversations.len() > CONVERSATION_LIMIT {
                let oldest = daemon
                    .conversations
                    .iter()
                    .min_by_key(|(_, snapshot)| snapshot.seen)
                    .map(|(key, _)| key.clone());
                match oldest {
                    Some(key) => daemon.conversations.remove(&key),
                    None => break,
                };
            }
        });
    }

    /// Hold `text` for `conversation` on daemon `id` until it can be delivered.
    /// `None` when the outbox is full.
    pub fn queue(&self, id: &str, conversation: &str, text: &str, request: &str) -> Option<Queued> {
        let queued = self.edit(|file| {
            let daemon = file.daemons.entry(id.to_string()).or_default();
            if daemon.outbox.len() >= OUTBOX_LIMIT {
                return None;
            }
            let queued = Queued {
                conversation: conversation.to_string(),
                request: request.to_string(),
                text: text.to_string(),
                at: store::now(),
            };
            daemon.outbox.push(queued.clone());
            Some(queued)
        });
        // Something the person typed does not wait for the timer.
        if queued.is_some() {
            self.write_now();
        }
        queued
    }

    /// The messages waiting for daemon `id`, oldest first.
    pub fn outbox(&self, id: &str) -> Vec<Queued> {
        self.with(|file| {
            file.daemons
                .get(id)
                .map(|daemon| daemon.outbox.clone())
                .unwrap_or_default()
        })
    }

    /// The messages waiting for `conversation` on daemon `id`, oldest first.
    pub fn queued(&self, id: &str, conversation: &str) -> Vec<Queued> {
        self.outbox(id)
            .into_iter()
            .filter(|queued| queued.conversation == conversation)
            .collect()
    }

    /// Take a message out of the outbox, because it was delivered or because the
    /// person gave it up. Whether it was there.
    pub fn settle(&self, id: &str, request: &str) -> bool {
        let there = self.with(|file| {
            file.daemons
                .get(id)
                .is_some_and(|daemon| daemon.outbox.iter().any(|queued| queued.request == request))
        });
        if !there {
            return false;
        }
        self.edit(|file| {
            if let Some(daemon) = file.daemons.get_mut(id) {
                daemon.outbox.retain(|queued| queued.request != request);
            }
        });
        self.write_now();
        true
    }

    /// Forget everything about daemon `id`: it is no longer one of ours.
    pub fn forget(&self, id: &str) {
        if self.with(|file| file.daemons.contains_key(id)) {
            self.edit(|file| file.daemons.remove(id));
            self.write_now();
        }
    }

    /// Write what changed, unless a write just happened. Called on a timer, so
    /// the last change of a burst reaches the disk a moment after it.
    pub fn flush(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.dirty || state.written.is_some_and(|at| at.elapsed() < WRITE_EVERY) {
            return;
        }
        if store::save(&self.db, DOC, &state.file).is_ok() {
            state.dirty = false;
            state.written = Some(Instant::now());
        }
    }

    /// Write at once, for a change that must not wait for the timer.
    fn write_now(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if store::save(&self.db, DOC, &state.file).is_ok() {
            state.dirty = false;
            state.written = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::sipil::ItemRole;

    fn line(id: u64) -> TranscriptItem {
        TranscriptItem {
            id,
            role: ItemRole::User,
            text: format!("m{id}"),
            tools: Vec::new(),
            calls: Vec::new(),
            call_id: None,
            is_error: false,
            reply: None,
            error: None,
            at: None,
            speaker: None,
        }
    }

    fn lines(range: std::ops::RangeInclusive<u64>) -> Vec<TranscriptItem> {
        range.map(line).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-offline-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The store on the database that lives in `dir`, as a launch would open it.
    fn open(dir: &Path) -> Offline {
        Offline::open(&Db::at(dir), dir)
    }

    fn agent() -> RememberedAgent {
        RememberedAgent {
            id: "c1".to_string(),
            name: "Researcher".to_string(),
        }
    }

    #[test]
    fn what_a_daemon_was_survives_a_restart() {
        let dir = scratch("restart");
        let offline = open(&dir);
        offline.remember_agent("d", "Researcher", agent());
        offline.remember_transcript("d", "c1", &lines(1..=3), true);
        offline.queue("d", "c1", "hello", "r1");
        offline.flush();

        let again = open(&dir);
        assert_eq!(again.agent("d"), Some(("Researcher".to_string(), agent())));
        assert_eq!(again.transcript("d", "c1"), Some((lines(1..=3), true)));
        assert_eq!(again.queued("d", "c1").len(), 1);
        assert!(again.agent("other").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_snapshot_keeps_the_newest_lines_and_says_older_ones_exist() {
        let offline = open(&scratch("limit"));
        offline.remember_transcript("d", "c1", &lines(1..=250), false);
        let (held, more) = offline.transcript("d", "c1").expect("kept");
        assert_eq!(held.len(), LINE_LIMIT);
        assert_eq!(held.first().map(|item| item.id), Some(51));
        assert!(more, "the lines cut off are older lines");
    }

    #[test]
    fn the_least_recently_seen_conversations_are_dropped() {
        let offline = open(&scratch("prune"));
        for n in 0..=CONVERSATION_LIMIT {
            offline.remember_transcript("d", &format!("c{n}"), &lines(1..=1), false);
            // `seen` is in seconds; make each later than the one before.
            offline.edit(|file| {
                if let Some(snapshot) = file
                    .daemons
                    .get_mut("d")
                    .and_then(|daemon| daemon.conversations.get_mut(&format!("c{n}")))
                {
                    snapshot.seen = n as u64;
                }
            });
        }
        assert!(offline.transcript("d", "c0").is_none());
        assert!(
            offline
                .transcript("d", &format!("c{CONVERSATION_LIMIT}"))
                .is_some()
        );
    }

    #[test]
    fn messages_wait_in_order_and_leave_one_by_one() {
        let offline = open(&scratch("outbox"));
        offline.queue("d", "c1", "first", "r1");
        offline.queue("d", "c2", "elsewhere", "r2");
        offline.queue("d", "c1", "second", "r3");
        let texts: Vec<_> = offline
            .queued("d", "c1")
            .into_iter()
            .map(|queued| queued.text)
            .collect();
        assert_eq!(texts, ["first", "second"]);
        assert_eq!(offline.outbox("d").len(), 3);

        assert!(offline.settle("d", "r1"));
        assert!(!offline.settle("d", "r1"), "a message leaves once");
        assert_eq!(offline.queued("d", "c1").len(), 1);
    }

    #[test]
    fn a_full_outbox_refuses_rather_than_growing() {
        let offline = open(&scratch("full"));
        for n in 0..OUTBOX_LIMIT {
            assert!(offline.queue("d", "c1", "x", &format!("r{n}")).is_some());
        }
        assert!(offline.queue("d", "c1", "one too many", "late").is_none());
    }

    #[test]
    fn a_forgotten_daemon_leaves_nothing_behind() {
        let dir = scratch("forget");
        let offline = open(&dir);
        offline.remember_agent("d", "Researcher", agent());
        offline.queue("d", "c1", "hello", "r1");
        offline.forget("d");
        assert!(offline.agent("d").is_none());
        assert!(offline.outbox("d").is_empty());
        assert!(open(&dir).agent("d").is_none(), "and on disk");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_wait_for_the_timer_and_an_unchanged_snapshot_is_not_a_change() {
        let dir = scratch("timer");
        let offline = open(&dir);
        offline.remember_transcript("d", "c1", &lines(1..=2), false);
        offline.flush();
        assert!(!offline.state.lock().expect("lock").dirty);

        offline.remember_transcript("d", "c1", &lines(1..=2), false);
        assert!(
            !offline.state.lock().expect("lock").dirty,
            "the same lines are not a change"
        );
        offline.remember_transcript("d", "c1", &lines(1..=3), false);
        offline.flush();
        assert!(
            offline.state.lock().expect("lock").dirty,
            "a second write inside the window waits"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
