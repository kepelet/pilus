//! The conversations with contacts and in channels, kept on this machine.
//!
//! Each side keeps its own copy: a message is stored here when this pilus sends
//! it and the peer acknowledged it, and when a contact's arrives. There is no
//! server to hold one for an absent peer, so a send to a pilus that is not
//! running fails and stays in the composer to retry.
//!
//! They live in `pilus.db` (`chat_threads`, `chat_lines`, `chat_requests`,
//! `chat_cursors`) and are read into memory when the store opens, because the
//! window asks for a thread's lines on every poll. Each change (a line said, a
//! thread read, an agent's cursor moved, a thread forgotten) is applied to
//! memory and written to the tables in one transaction, so what is in memory is
//! always what a reopen gives, and a send costs a few small row writes instead of
//! rewriting a log. A thread's 500-line limit is enforced in the same
//! transaction.
//!
//! Before `pilus.db` they were `chats.jsonl`, one JSON record per line. The
//! first open replays such a file into the tables ([`State::replay`]) and sets it
//! aside as `chats.jsonl.migrated`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use pilus_db::rusqlite::{self, Transaction, params};
use pilus_db::{Db, Error as DbError};
use serde::{Deserialize, Serialize};

use super::store;
use super::{Quote, Subagent};

/// The most lines a thread keeps; older ones are dropped as new ones arrive.
const THREAD_LIMIT: usize = 500;
/// How many request ids a thread remembers to recognise a retried delivery.
const SEEN_LIMIT: usize = 64;
/// The longest message taken from a peer, in bytes.
pub const MESSAGE_LIMIT: usize = 16 * 1024;
/// Marks `chats.jsonl` as read into the database.
const IMPORT: &str = "chats";

/// One line of a conversation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Line {
    /// Grows with each line in the thread, so a new line has a larger one.
    pub id: u64,
    /// Whether this pilus said it.
    pub mine: bool,
    /// What was said.
    pub text: String,
    /// Unix seconds.
    pub at: u64,
    /// For a line in a channel that is not this pilus's own: the device that
    /// said it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub who: Option<String>,
    /// The name of the sipil agent that said it, when an agent did, whether it
    /// is this pilus's own or a contact's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The line this one answers, quoted by whoever answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<Quote>,
    /// For a note that one of this pilus's agents handed work to another: where
    /// that conversation is. Kept here only. It is not said to anyone else,
    /// since no one else could open it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Delegation>,
}

/// An agent of this pilus handing work to a subagent while it answers in a
/// conversation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Delegation {
    /// The tool call, which keeps one call from being noted twice.
    pub call: String,
    /// The daemon the subagent runs on.
    pub daemon: String,
    /// The subagent's conversation, to open.
    pub conversation: String,
    /// Who was asked: `researcher`, or `subagent`.
    pub agent: String,
    /// What it was asked.
    pub task: String,
}

impl Delegation {
    /// The note as plain text, for anything that cannot draw it as a link.
    pub fn text(&self) -> String {
        format!("talking with {} about “{}”", self.agent, self.task)
    }

    /// This as the subagent a transcript line carries.
    pub fn subagent(&self) -> Subagent {
        Subagent {
            conversation: Some(self.conversation.clone()),
            daemon: Some(self.daemon.clone()),
            agent: self.agent.clone(),
            task: self.task.clone(),
        }
    }
}

/// How far a sipil agent has read a conversation, and the daemon session it
/// answers in.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Cursor {
    /// The daemon session this conversation uses for that agent.
    pub session: String,
    /// The newest line the agent has been shown.
    pub through: u64,
}

#[derive(Default)]
struct Thread {
    /// The id of the newest line ever added, which the next one follows.
    next: u64,
    /// Lines with an id up to this have been read here.
    read_through: u64,
    lines: Vec<Line>,
    /// The request id each peer line was taken under, as `(line id, request id)`,
    /// newest last.
    requests: Vec<(u64, String)>,
    /// Each of this pilus's sipil agents (by daemon ID) that has been mentioned
    /// here.
    agents: BTreeMap<String, Cursor>,
}

impl Thread {
    fn unread(&self) -> u64 {
        self.lines
            .iter()
            .filter(|line| !line.mine && line.id > self.read_through)
            .count() as u64
    }

    fn apply(&mut self, entry: Entry) {
        match entry {
            Entry::Line { line, request } => {
                self.next = self.next.max(line.id);
                if let Some(request) = request {
                    self.requests.push((line.id, request));
                    let excess = self.requests.len().saturating_sub(SEEN_LIMIT);
                    self.requests.drain(..excess);
                }
                self.lines.push(line);
                let excess = self.lines.len().saturating_sub(THREAD_LIMIT);
                self.lines.drain(..excess);
            }
            Entry::Read { through } => self.read_through = self.read_through.max(through),
            Entry::Cursor {
                daemon,
                session,
                through,
            } => {
                self.agents.insert(daemon, Cursor { session, through });
            }
            // A forgotten thread is removed whole by `State::apply`.
            Entry::Forget => {}
        }
    }
}

/// One record of the old `chats.jsonl`, and the unit of change today.
#[derive(Deserialize)]
struct Record {
    /// The peer's device ID, or `#<id>` for a channel.
    thread: String,
    #[serde(flatten)]
    entry: Entry,
}

// Each entry lives only long enough to be applied and written, so a boxed line
// would cost an allocation to save nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Entry {
    /// A line was said. `request` is the id the peer sent it under.
    Line {
        line: Line,
        #[serde(default)]
        request: Option<String>,
    },
    /// Every line up to `through` was read.
    Read { through: u64 },
    /// An agent of ours has been shown the thread up to `through`.
    Cursor {
        daemon: String,
        session: String,
        through: u64,
    },
    /// The thread is gone.
    Forget,
}

#[derive(Default)]
struct State {
    threads: BTreeMap<String, Thread>,
}

impl State {
    fn apply(&mut self, record: Record) {
        match record.entry {
            Entry::Forget => {
                self.threads.remove(&record.thread);
            }
            entry => self.threads.entry(record.thread).or_default().apply(entry),
        }
    }

    /// Replay the old log at `path`. A record that does not parse is skipped, so
    /// one damaged line never takes the rest with it.
    fn replay(path: &Path) -> Self {
        let bytes = std::fs::read(path).unwrap_or_default();
        let mut state = Self::default();
        for line in bytes.split(|byte| *byte == b'\n') {
            if let Ok(record) = serde_json::from_slice::<Record>(line) {
                state.apply(record);
            }
        }
        state
    }

    /// Everything in the tables.
    fn read(db: &Db) -> Result<Self, DbError> {
        db.with(|conn| {
            let mut state = Self::default();
            let mut threads =
                conn.prepare("SELECT thread, next, read_through FROM chat_threads")?;
            for row in threads.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })? {
                let (name, next, read_through) = row?;
                let thread = state.threads.entry(name).or_default();
                thread.next = next.unsigned_abs();
                thread.read_through = read_through.unsigned_abs();
            }
            let mut lines = conn.prepare(
                "SELECT thread, id, mine, text, at, who, agent, reply, delegation
                 FROM chat_lines ORDER BY thread, id",
            )?;
            for row in lines.query_map([], |row| {
                let json = |index: usize| -> rusqlite::Result<Option<String>> { row.get(index) };
                Ok((
                    row.get::<_, String>(0)?,
                    Line {
                        id: row.get::<_, i64>(1)?.unsigned_abs(),
                        mine: row.get(2)?,
                        text: row.get(3)?,
                        at: row.get::<_, i64>(4)?.unsigned_abs(),
                        who: row.get(5)?,
                        agent: row.get(6)?,
                        reply: json(7)?.and_then(|text| serde_json::from_str(&text).ok()),
                        delegation: json(8)?.and_then(|text| serde_json::from_str(&text).ok()),
                    },
                ))
            })? {
                let (name, line) = row?;
                if let Some(thread) = state.threads.get_mut(&name) {
                    thread.lines.push(line);
                }
            }
            let mut requests = conn
                .prepare("SELECT thread, line, request FROM chat_requests ORDER BY thread, line")?;
            for row in requests.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })? {
                let (name, line, request) = row?;
                if let Some(thread) = state.threads.get_mut(&name) {
                    thread.requests.push((line.unsigned_abs(), request));
                }
            }
            let mut cursors =
                conn.prepare("SELECT thread, daemon, session, through FROM chat_cursors")?;
            for row in cursors.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })? {
                let (name, daemon, session, through) = row?;
                if let Some(thread) = state.threads.get_mut(&name) {
                    thread.agents.insert(
                        daemon,
                        Cursor {
                            session,
                            through: through.unsigned_abs(),
                        },
                    );
                }
            }
            Ok(state)
        })
    }

    /// Write every thread, for the import. A row that is already there wins, so
    /// an import never overwrites what the tables hold.
    fn write_all(&self, tx: &Transaction) -> Result<(), DbError> {
        for (name, thread) in &self.threads {
            tx.execute(
                "INSERT OR IGNORE INTO chat_threads (thread, next, read_through) VALUES (?1, ?2, ?3)",
                params![name, thread.next as i64, thread.read_through as i64],
            )?;
            for line in &thread.lines {
                insert_line(tx, name, line)?;
            }
            for (id, request) in &thread.requests {
                tx.execute(
                    "INSERT OR IGNORE INTO chat_requests (thread, line, request) VALUES (?1, ?2, ?3)",
                    params![name, *id as i64, request],
                )?;
            }
            for (daemon, cursor) in &thread.agents {
                tx.execute(
                    "INSERT OR IGNORE INTO chat_cursors (thread, daemon, session, through)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![name, daemon, cursor.session, cursor.through as i64],
                )?;
            }
        }
        Ok(())
    }
}

fn insert_line(tx: &Transaction, thread: &str, line: &Line) -> Result<(), DbError> {
    tx.execute(
        "INSERT OR REPLACE INTO chat_lines
             (thread, id, mine, text, at, who, agent, reply, delegation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            thread,
            line.id as i64,
            line.mine,
            line.text,
            line.at as i64,
            line.who,
            line.agent,
            line.reply
                .as_ref()
                .and_then(|q| serde_json::to_string(q).ok()),
            line.delegation
                .as_ref()
                .and_then(|d| serde_json::to_string(d).ok()),
        ],
    )?;
    Ok(())
}

/// Write one change to the tables, the same change `Thread::apply` makes in
/// memory, including the limits on how much a thread keeps.
fn write(tx: &Transaction, thread: &str, entry: &Entry) -> Result<(), DbError> {
    if matches!(entry, Entry::Forget) {
        for table in [
            "chat_lines",
            "chat_requests",
            "chat_cursors",
            "chat_threads",
        ] {
            tx.execute(&format!("DELETE FROM {table} WHERE thread = ?1"), [thread])?;
        }
        return Ok(());
    }
    tx.execute(
        "INSERT OR IGNORE INTO chat_threads (thread) VALUES (?1)",
        [thread],
    )?;
    match entry {
        Entry::Line { line, request } => {
            tx.execute(
                "UPDATE chat_threads SET next = MAX(next, ?2) WHERE thread = ?1",
                params![thread, line.id as i64],
            )?;
            insert_line(tx, thread, line)?;
            // Keep the newest `THREAD_LIMIT` lines: everything older than the
            // one that many from the end. With fewer there is no such row, the
            // comparison is NULL, and nothing goes.
            tx.execute(
                "DELETE FROM chat_lines WHERE thread = ?1 AND id <
                 (SELECT id FROM chat_lines WHERE thread = ?1 ORDER BY id DESC LIMIT 1 OFFSET ?2)",
                params![thread, THREAD_LIMIT as i64 - 1],
            )?;
            if let Some(request) = request {
                tx.execute(
                    "INSERT OR REPLACE INTO chat_requests (thread, line, request) VALUES (?1, ?2, ?3)",
                    params![thread, line.id as i64, request],
                )?;
                tx.execute(
                    "DELETE FROM chat_requests WHERE thread = ?1 AND line <
                     (SELECT line FROM chat_requests WHERE thread = ?1 ORDER BY line DESC LIMIT 1 OFFSET ?2)",
                    params![thread, SEEN_LIMIT as i64 - 1],
                )?;
            }
        }
        Entry::Read { through } => {
            tx.execute(
                "UPDATE chat_threads SET read_through = MAX(read_through, ?2) WHERE thread = ?1",
                params![thread, *through as i64],
            )?;
        }
        Entry::Cursor {
            daemon,
            session,
            through,
        } => {
            tx.execute(
                "INSERT INTO chat_cursors (thread, daemon, session, through) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (thread, daemon) DO UPDATE
                 SET session = excluded.session, through = excluded.through",
                params![thread, daemon, session, *through as i64],
            )?;
        }
        Entry::Forget => {}
    }
    Ok(())
}

/// Read `path`, the old log, into the tables the first time, then set it aside.
fn import_legacy(db: &Db, path: &Path) {
    if !path.is_file() || db.imported(IMPORT) {
        return;
    }
    let old = State::replay(path);
    match db.import_once(IMPORT, |tx| old.write_all(tx)) {
        Ok(_) => pilus_db::retire(path),
        Err(error) => tracing::warn!("could not import {}: {error}", path.display()),
    }
}

/// Every conversation, keyed by the peer's device ID (`#<id>` for a channel).
pub struct Chats {
    db: Db,
    inner: Mutex<State>,
    version: AtomicU64,
}

impl Chats {
    /// Open the conversations in `db`, first importing `dir`'s `chats.jsonl` if
    /// the old log is still there.
    pub fn open(db: &Db, dir: &Path) -> Self {
        import_legacy(db, &dir.join("chats.jsonl"));
        let state = State::read(db).unwrap_or_else(|error| {
            tracing::warn!("could not read the chats: {error}");
            State::default()
        });
        Self {
            db: db.clone(),
            inner: Mutex::new(state),
            version: AtomicU64::new(0),
        }
    }

    /// Counts every change, so a window can repaint only when it moved.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// The newest lines of the thread with `peer`, oldest first.
    pub fn lines(&self, peer: &str) -> Vec<Line> {
        self.lock()
            .threads
            .get(peer)
            .map(|thread| thread.lines.clone())
            .unwrap_or_default()
    }

    /// How many of `peer`'s lines have not been read here yet.
    pub fn unread(&self, peer: &str) -> u64 {
        self.lock().threads.get(peer).map_or(0, Thread::unread)
    }

    /// Unread lines across all threads.
    pub fn unread_total(&self) -> u64 {
        self.lock().threads.values().map(Thread::unread).sum()
    }

    /// The last line of each thread, for ordering and previews.
    pub fn last(&self, peer: &str) -> Option<Line> {
        self.lock()
            .threads
            .get(peer)
            .and_then(|thread| thread.lines.last().cloned())
    }

    /// Mark everything from `peer` as read.
    pub fn mark_read(&self, peer: &str) {
        let mut state = self.lock();
        let through = state
            .threads
            .get(peer)
            .filter(|thread| thread.unread() > 0)
            .map(|thread| thread.next);
        if let Some(through) = through {
            self.commit(&mut state, peer, Entry::Read { through });
        }
    }

    /// Record a line this pilus sent, or that one of its agents did.
    pub fn push_mine(&self, thread: &str, text: &str, agent: Option<&str>) {
        self.push_mine_replying(thread, text, agent, None);
    }

    /// [`Chats::push_mine`] for a line that answers another.
    pub fn push_mine_replying(
        &self,
        thread: &str,
        text: &str,
        agent: Option<&str>,
        reply: Option<&Quote>,
    ) {
        self.push(thread, text, true, None, None, agent, reply, None);
    }

    /// Note that `agent`, one of ours, has handed work to a subagent while it
    /// answers in `thread`. Once per call: noting it again changes nothing.
    pub fn push_delegation(&self, thread: &str, agent: &str, delegation: Delegation) {
        {
            let state = self.lock();
            let noted = state.threads.get(thread).is_some_and(|thread| {
                thread.lines.iter().any(|line| {
                    line.delegation
                        .as_ref()
                        .is_some_and(|d| d.call == delegation.call)
                })
            });
            if noted {
                return;
            }
        }
        let text = delegation.text();
        self.push(
            thread,
            &text,
            true,
            None,
            None,
            Some(agent),
            None,
            Some(delegation),
        );
    }

    /// Record a line from someone else. In a channel `who` is the device that
    /// said it. Returns false when `request_id` was already taken, which is a
    /// retried delivery rather than a second message.
    pub fn push_theirs(
        &self,
        thread: &str,
        text: &str,
        request_id: &str,
        who: Option<&str>,
        agent: Option<&str>,
    ) -> bool {
        self.push_theirs_replying(thread, text, request_id, who, agent, None)
    }

    /// [`Chats::push_theirs`] for a line that answers another.
    pub fn push_theirs_replying(
        &self,
        thread: &str,
        text: &str,
        request_id: &str,
        who: Option<&str>,
        agent: Option<&str>,
        reply: Option<&Quote>,
    ) -> bool {
        self.push(
            thread,
            text,
            false,
            Some(request_id),
            who,
            agent,
            reply,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &self,
        thread: &str,
        text: &str,
        mine: bool,
        request_id: Option<&str>,
        who: Option<&str>,
        agent: Option<&str>,
        reply: Option<&Quote>,
        delegation: Option<Delegation>,
    ) -> bool {
        let mut state = self.lock();
        let current = state.threads.get(thread);
        let request = request_id.filter(|id| !id.is_empty());
        if let Some(request) = request
            && current.is_some_and(|thread| thread.requests.iter().any(|(_, seen)| seen == request))
        {
            return false;
        }
        let id = current.map_or(0, |thread| thread.next) + 1;
        let entry = Entry::Line {
            line: Line {
                id,
                mine,
                text: text.to_string(),
                at: store::now(),
                who: who.map(str::to_string),
                agent: agent.map(str::to_string),
                reply: reply.cloned(),
                delegation,
            },
            request: request.map(str::to_string),
        };
        self.commit(&mut state, thread, entry);
        true
    }

    /// Where the agent of daemon `daemon` stands in `thread`.
    pub fn cursor(&self, thread: &str, daemon: &str) -> Option<Cursor> {
        self.lock()
            .threads
            .get(thread)
            .and_then(|thread| thread.agents.get(daemon).cloned())
    }

    /// Record where the agent of daemon `daemon` stands in `thread`.
    pub fn set_cursor(&self, thread: &str, daemon: &str, cursor: Cursor) {
        let mut state = self.lock();
        let entry = Entry::Cursor {
            daemon: daemon.to_string(),
            session: cursor.session,
            through: cursor.through,
        };
        self.commit(&mut state, thread, entry);
    }

    /// Drop a conversation: used when a channel is left.
    pub fn forget(&self, thread: &str) {
        let mut state = self.lock();
        if state.threads.contains_key(thread) {
            self.commit(&mut state, thread, Entry::Forget);
        }
    }

    /// Write `entry` to the tables and apply it to memory, so what is in memory
    /// is always what a reopen gives.
    fn commit(&self, state: &mut State, thread: &str, entry: Entry) {
        let saved = self.db.transaction(|tx| write(tx, thread, &entry));
        state.apply(Record {
            thread: thread.to_string(),
            entry,
        });
        self.version.fetch_add(1, Ordering::Relaxed);
        // A chat that could not be saved is still in memory for this run; there
        // is nobody to tell that the window would not already show as lost data.
        if let Err(error) = saved {
            tracing::warn!("could not save chats: {error}");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-chats-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The store on a database of its own, as a launch would open it.
    fn reopen(dir: &Path) -> Chats {
        Chats::open(&Db::open(&dir.join(pilus_db::FILE)).unwrap(), dir)
    }

    fn memory() -> Chats {
        Chats::open(&Db::in_memory(), Path::new("/nonexistent"))
    }

    #[test]
    fn a_retried_delivery_is_one_message() {
        let chats = memory();
        assert!(chats.push_theirs("p", "hi", "r1", None, None));
        assert!(!chats.push_theirs("p", "hi", "r1", None, None));
        assert!(chats.push_theirs("p", "hi", "r2", None, None));
        assert_eq!(chats.lines("p").len(), 2);
        assert_eq!(chats.unread("p"), 2);
    }

    #[test]
    fn reading_clears_unread_and_my_own_lines_never_count() {
        let chats = memory();
        chats.push_mine("p", "yo", None);
        assert_eq!(chats.unread_total(), 0);
        chats.push_theirs("p", "hey", "r", None, None);
        assert_eq!(chats.unread_total(), 1);
        chats.mark_read("p");
        assert_eq!(chats.unread_total(), 0);
    }

    #[test]
    fn a_thread_keeps_only_its_newest_lines_and_ids_keep_growing() {
        let chats = memory();
        for n in 0..(THREAD_LIMIT + 3) {
            chats.push_mine("p", &n.to_string(), None);
        }
        let lines = chats.lines("p");
        assert_eq!(lines.len(), THREAD_LIMIT);
        assert_eq!(lines[0].text, "3");
        assert!(lines.windows(2).all(|pair| pair[0].id < pair[1].id));
    }

    #[test]
    fn a_reply_keeps_its_quote_and_a_delegation_is_noted_once() {
        let chats = memory();
        let quote = Quote::of("Rina", "ship it\n  on   friday");
        assert_eq!(quote.text, "ship it on friday");
        chats.push_theirs_replying("p", "agreed", "r1", None, None, Some(&quote));
        chats.push_mine_replying("p", "great", None, Some(&quote));
        let lines = chats.lines("p");
        assert_eq!(lines[0].reply.as_ref(), Some(&quote));
        assert_eq!(lines[1].reply.as_ref(), Some(&quote));
        assert_eq!(chats.lines("p")[0].delegation, None);

        let note = Delegation {
            call: "c1".to_string(),
            daemon: "d".to_string(),
            conversation: "9".to_string(),
            agent: "researcher".to_string(),
            task: "How does X work?".to_string(),
        };
        chats.push_delegation("p", "CoS", note.clone());
        chats.push_delegation("p", "CoS", note.clone());
        let lines = chats.lines("p");
        assert_eq!(lines.len(), 3, "the same call is not noted twice");
        let line = lines.last().unwrap();
        assert!(line.mine && line.agent.as_deref() == Some("CoS"));
        assert_eq!(line.delegation.as_ref(), Some(&note));
        assert_eq!(chats.unread("p"), 1, "a note of ours is never unread");
    }

    #[test]
    fn threads_survive_a_restart_with_every_field() {
        let dir = scratch("restart");
        let quote = Quote::of("Rina", "ship it");
        let note = Delegation {
            call: "c1".to_string(),
            daemon: "d".to_string(),
            conversation: "9".to_string(),
            agent: "researcher".to_string(),
            task: "How?".to_string(),
        };
        let first = reopen(&dir);
        first.push_theirs_replying(
            "#c",
            "still here",
            "r",
            Some("dev"),
            Some("Bot"),
            Some(&quote),
        );
        first.push_delegation("#c", "CoS", note.clone());
        let before = first.lines("#c");
        drop(first);

        let again = reopen(&dir);
        assert_eq!(again.lines("#c"), before);
        assert_eq!(again.lines("#c")[0].reply.as_ref(), Some(&quote));
        assert_eq!(again.lines("#c")[1].delegation.as_ref(), Some(&note));
        assert!(
            !again.push_theirs("#c", "still here", "r", None, None),
            "dedupe survives too"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reading_cursors_and_forgetting_survive_a_restart() {
        let dir = scratch("replay");
        let chats = reopen(&dir);
        chats.push_theirs("p", "one", "r1", None, None);
        chats.push_theirs("p", "two", "r2", None, None);
        chats.mark_read("p");
        chats.push_theirs("p", "three", "r3", None, None);
        chats.set_cursor(
            "p",
            "d",
            Cursor {
                session: "s".to_string(),
                through: 2,
            },
        );
        chats.set_cursor(
            "p",
            "d",
            Cursor {
                session: "s2".to_string(),
                through: 3,
            },
        );
        chats.push_mine("gone", "bye", None);
        chats.forget("gone");
        drop(chats);

        let again = reopen(&dir);
        assert_eq!(again.lines("p").len(), 3);
        assert_eq!(again.unread("p"), 1);
        let cursor = again.cursor("p", "d").unwrap();
        assert_eq!((cursor.session.as_str(), cursor.through), ("s2", 3));
        assert!(again.lines("gone").is_empty());
        assert!(!again.push_theirs("p", "two", "r2", None, None));
        assert_eq!(again.lines("p").last().unwrap().id, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_a_thread_trims_stays_trimmed_after_a_restart() {
        let dir = scratch("trim");
        let chats = reopen(&dir);
        let total = THREAD_LIMIT + SEEN_LIMIT + 10;
        for n in 0..total {
            chats.push_theirs("p", &n.to_string(), &format!("r{n}"), None, None);
        }
        drop(chats);

        let again = reopen(&dir);
        let lines = again.lines("p");
        assert_eq!(lines.len(), THREAD_LIMIT);
        assert_eq!(lines.last().unwrap().text, (total - 1).to_string());
        assert_eq!(again.unread("p"), THREAD_LIMIT as u64);
        assert!(!again.push_theirs("p", "x", &format!("r{}", total - 1), None, None));
        assert!(
            again.push_theirs("p", "x", "r0", None, None),
            "a request older than the last {SEEN_LIMIT} is forgotten"
        );
        assert_eq!(
            again.lines("p").last().unwrap().id,
            total as u64 + 1,
            "ids keep growing past the trimmed ones"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_old_log_is_imported_once_and_set_aside() {
        let dir = scratch("legacy");
        let log = [
            r#"{"thread":"p","op":"line","line":{"id":1,"mine":false,"text":"kept","at":5},"request":"r1"}"#,
            "not json",
            r#"{"thread":"p","op":"cursor","daemon":"d","session":"s","through":1}"#,
            r#"{"thread":"gone","op":"line","line":{"id":1,"mine":true,"text":"x","at":6}}"#,
            r#"{"thread":"gone","op":"forget"}"#,
            r#"{"thread":"p","op":"line","line":{"id":2,"mine":true,"text":"after","at":7}}"#,
            r#"{"thread":"p","op":"read","through":1}"#,
            r#"{"thread":"p","op":"li"#,
        ]
        .join("\n");
        std::fs::write(dir.join("chats.jsonl"), log).unwrap();

        let chats = reopen(&dir);
        let texts: Vec<_> = chats.lines("p").into_iter().map(|l| l.text).collect();
        assert_eq!(
            texts,
            ["kept", "after"],
            "damaged records cost only themselves"
        );
        assert_eq!(chats.unread("p"), 0);
        assert_eq!(chats.cursor("p", "d").unwrap().session, "s");
        assert!(chats.lines("gone").is_empty());
        assert!(!dir.join("chats.jsonl").exists());
        assert!(dir.join("chats.jsonl.migrated").is_file());
        drop(chats);

        // A log put back is not read a second time over what has changed since.
        std::fs::write(dir.join("chats.jsonl"), "").unwrap();
        let again = reopen(&dir);
        assert_eq!(again.lines("p").len(), 2);
        assert!(!again.push_theirs("p", "kept", "r1", None, None));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
