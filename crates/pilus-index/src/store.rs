//! The SQLite file behind the index.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::error::Result;
use crate::model::{Agent, Session};
use crate::usage::Bucket;

/// Bump when the tables change: an index that does not match is dropped and
/// rebuilt from the transcripts, which is cheaper than migrating a cache.
const SCHEMA: i64 = 3;

/// What a search narrows to.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Filter {
    /// One machine (`local` or a profile id), or all of them.
    pub machine: Option<String>,
    /// One agent, or both.
    pub agent: Option<Agent>,
}

/// An open index file.
///
/// A connection is not shared between threads: the indexer writes through its
/// own and the window reads through another, which WAL lets run side by side.
pub struct Index {
    pub(crate) conn: Connection,
}

impl Index {
    /// Open the index at `path`, creating it (and its folder) when absent.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            pilus_db::private_dir(parent)?;
        }
        // Titles and machine names of private sessions: owner-only, like pilus.db.
        pilus_db::create_private(path)?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_millis(500))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::prepare(conn)
    }

    /// An index that lives only as long as the value, for tests.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(conn: Connection) -> Result<Self> {
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != SCHEMA {
            conn.execute_batch("DROP TABLE IF EXISTS sessions; DROP TABLE IF EXISTS usage;")?;
            conn.execute_batch(
                "CREATE TABLE sessions (
                     machine    TEXT NOT NULL,
                     path       TEXT NOT NULL,
                     agent      TEXT NOT NULL,
                     session_id TEXT NOT NULL,
                     cwd        TEXT NOT NULL,
                     title      TEXT NOT NULL,
                     branch     TEXT NOT NULL,
                     started    INTEGER NOT NULL,
                     updated    INTEGER NOT NULL,
                     size       INTEGER NOT NULL,
                     PRIMARY KEY (machine, path)
                 );
                 CREATE INDEX sessions_updated ON sessions (updated DESC);
                 CREATE TABLE usage (
                     machine     TEXT NOT NULL,
                     path        TEXT NOT NULL,
                     hour        INTEGER NOT NULL,
                     model       TEXT NOT NULL,
                     input       INTEGER NOT NULL,
                     output      INTEGER NOT NULL,
                     cache_read  INTEGER NOT NULL,
                     cache_write INTEGER NOT NULL,
                     turns       INTEGER NOT NULL,
                     cost_micros INTEGER NOT NULL,
                     recorded    INTEGER NOT NULL,
                     PRIMARY KEY (machine, path, hour, model)
                 );
                 CREATE INDEX usage_hour ON usage (hour);",
            )?;
            conn.pragma_update(None, "user_version", SCHEMA)?;
        }
        Ok(Self { conn })
    }

    /// The `(size, updated)` last indexed for each transcript on `machine`.
    pub fn known(&self, machine: &str) -> Result<HashMap<String, (u64, i64)>> {
        let mut statement = self
            .conn
            .prepare("SELECT path, size, updated FROM sessions WHERE machine = ?1")?;
        let rows = statement.query_map([machine], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (row.get::<_, i64>(1)?.unsigned_abs(), row.get::<_, i64>(2)?),
            ))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Insert or replace `sessions`, all or none.
    pub fn upsert(&mut self, sessions: &[Session]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for session in sessions {
            insert_session(&tx, session)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// [`Self::upsert`] together with each transcript's usage, replacing what
    /// the transcript had before. One transaction, so a session never exists
    /// without the usage its file size says was read.
    pub fn upsert_with_usage(&mut self, files: &[(Session, Vec<Bucket>)]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (session, buckets) in files {
            insert_session(&tx, session)?;
            tx.execute(
                "DELETE FROM usage WHERE machine = ?1 AND path = ?2",
                params![session.machine, session.path],
            )?;
            let mut statement = tx.prepare_cached(
                "INSERT INTO usage
                     (machine, path, hour, model, input, output, cache_read, cache_write, turns,
                      cost_micros, recorded)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for bucket in buckets {
                statement.execute(params![
                    session.machine,
                    session.path,
                    bucket.hour,
                    bucket.model,
                    clamp(bucket.input),
                    clamp(bucket.output),
                    clamp(bucket.cache_read),
                    clamp(bucket.cache_write),
                    clamp(bucket.turns),
                    clamp(bucket.cost_micros),
                    clamp(bucket.recorded),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop the rows for transcripts that no longer exist on `machine`.
    pub fn remove(&mut self, machine: &str, paths: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut sessions =
                tx.prepare("DELETE FROM sessions WHERE machine = ?1 AND path = ?2")?;
            let mut usage = tx.prepare("DELETE FROM usage WHERE machine = ?1 AND path = ?2")?;
            for path in paths {
                sessions.execute(params![machine, path])?;
                usage.execute(params![machine, path])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Forget everything indexed for `machine`, for one the person took out of
    /// the sources.
    pub fn forget(&mut self, machine: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE machine = ?1", [machine])?;
        self.conn
            .execute("DELETE FROM usage WHERE machine = ?1", [machine])?;
        Ok(())
    }

    /// Sessions matching every word of `query` in their title, directory,
    /// branch, agent or id, newest first. A session nobody wrote to is not
    /// listed.
    pub fn search(&self, query: &str, filter: &Filter, limit: usize) -> Result<Vec<Session>> {
        let mut sql = String::from(
            "SELECT machine, path, agent, session_id, cwd, title, branch, started, updated, size
             FROM sessions WHERE title <> ''",
        );
        let mut args: Vec<String> = Vec::new();
        if let Some(machine) = &filter.machine {
            args.push(machine.clone());
            sql.push_str(&format!(" AND machine = ?{}", args.len()));
        }
        if let Some(agent) = filter.agent {
            args.push(agent.kind().to_string());
            sql.push_str(&format!(" AND agent = ?{}", args.len()));
        }
        for word in query.split_whitespace() {
            args.push(like(word));
            let at = args.len();
            sql.push_str(&format!(
                " AND (title LIKE ?{at} ESCAPE '\\' OR cwd LIKE ?{at} ESCAPE '\\' \
                  OR branch LIKE ?{at} ESCAPE '\\' OR agent LIKE ?{at} ESCAPE '\\' \
                  OR session_id LIKE ?{at} ESCAPE '\\')"
            ));
        }
        sql.push_str(&format!(" ORDER BY updated DESC LIMIT {limit}"));
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(args.iter()), |row| {
            Ok((
                row.get::<_, String>(2)?,
                Session {
                    machine: row.get(0)?,
                    agent: Agent::Pi,
                    session_id: row.get(3)?,
                    cwd: row.get(4)?,
                    title: row.get(5)?,
                    branch: row.get(6)?,
                    started: row.get(7)?,
                    updated: row.get(8)?,
                    path: row.get(1)?,
                    size: row.get::<_, i64>(9)?.unsigned_abs(),
                },
            ))
        })?;
        let mut found = Vec::new();
        for row in rows {
            let (kind, mut session) = row?;
            // A row from an agent this build does not read is not an error;
            // it just is not shown.
            if let Some(agent) = Agent::from_kind(&kind) {
                session.agent = agent;
                found.push(session);
            }
        }
        Ok(found)
    }

    /// How many listed sessions `machine` has, or all machines together.
    pub fn count(&self, machine: Option<&str>) -> Result<u64> {
        let count: Option<i64> = match machine {
            Some(machine) => self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sessions WHERE title <> '' AND machine = ?1",
                    [machine],
                    |row| row.get(0),
                )
                .optional()?,
            None => self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sessions WHERE title <> ''",
                    [],
                    |row| row.get(0),
                )
                .optional()?,
        };
        Ok(count.unwrap_or(0).unsigned_abs())
    }
}

/// A count as SQLite stores it: its integers are signed 64-bit.
pub(crate) fn clamp(count: u64) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

fn insert_session(tx: &rusqlite::Transaction<'_>, session: &Session) -> Result<()> {
    tx.prepare_cached(
        "INSERT OR REPLACE INTO sessions
             (machine, path, agent, session_id, cwd, title, branch, started, updated, size)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?
    .execute(params![
        session.machine,
        session.path,
        session.agent.kind(),
        session.session_id,
        session.cwd,
        session.title,
        session.branch,
        session.started,
        session.updated,
        clamp(session.size),
    ])?;
    Ok(())
}

/// `word` as a `LIKE` pattern that matches it literally anywhere.
fn like(word: &str) -> String {
    let mut pattern = String::from("%");
    for character in word.chars() {
        if matches!(character, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

/// Delete the index file and its journal. Withdrawing consent calls this, so
/// nothing indexed outlives the permission to index.
pub fn wipe(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let _ = fs::remove_file(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(machine: &str, agent: Agent, id: &str, title: &str, updated: i64) -> Session {
        Session {
            machine: machine.to_string(),
            agent,
            session_id: id.to_string(),
            cwd: "/work/app".to_string(),
            title: title.to_string(),
            branch: "main".to_string(),
            started: updated - 10,
            updated,
            path: format!("/{machine}/{id}.jsonl"),
            size: 100,
        }
    }

    fn seeded() -> Index {
        let mut index = Index::in_memory().expect("index");
        index
            .upsert(&[
                session("local", Agent::Claude, "a", "Fix the relay", 30),
                session("local", Agent::Pi, "b", "Refactor sidebar", 20),
                session("box", Agent::Claude, "c", "relay audit", 10),
                session("local", Agent::Pi, "d", "", 40),
            ])
            .expect("upsert");
        index
    }

    #[test]
    fn search_is_newest_first_and_hides_untitled_sessions() {
        let index = seeded();
        let all = index.search("", &Filter::default(), 50).expect("search");
        let ids: Vec<_> = all.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(index.count(None).expect("count"), 3);
    }

    #[test]
    fn every_word_must_match_in_any_column() {
        let index = seeded();
        let hits = index
            .search("relay claude", &Filter::default(), 50)
            .expect("search");
        assert_eq!(hits.len(), 2);
        let hits = index
            .search("relay /work", &Filter::default(), 50)
            .expect("search");
        assert_eq!(hits.len(), 2);
        let hits = index
            .search("relay sidebar", &Filter::default(), 50)
            .expect("search");
        assert!(hits.is_empty());
    }

    #[test]
    fn filters_narrow_by_machine_and_agent() {
        let index = seeded();
        let filter = Filter {
            machine: Some("box".to_string()),
            agent: None,
        };
        assert_eq!(index.search("", &filter, 50).expect("search").len(), 1);
        let filter = Filter {
            machine: None,
            agent: Some(Agent::Pi),
        };
        assert_eq!(index.search("", &filter, 50).expect("search").len(), 1);
    }

    #[test]
    fn wildcards_in_a_query_are_literal() {
        let mut index = seeded();
        index
            .upsert(&[session("local", Agent::Pi, "e", "100% done", 50)])
            .expect("upsert");
        let hits = index
            .search("100%", &Filter::default(), 50)
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert!(
            index
                .search("%", &Filter::default(), 50)
                .expect("search")
                .len()
                == 1
        );
    }

    #[test]
    fn known_remove_and_forget_track_a_machine() {
        let mut index = seeded();
        let known = index.known("local").expect("known");
        assert_eq!(known.get("/local/a.jsonl"), Some(&(100, 30)));
        index
            .remove("local", &["/local/a.jsonl".to_string()])
            .expect("remove");
        assert_eq!(index.known("local").expect("known").len(), 2);
        index.forget("local").expect("forget");
        assert!(index.known("local").expect("known").is_empty());
        assert_eq!(index.count(Some("box")).expect("count"), 1);
    }

    #[test]
    fn a_stale_schema_is_rebuilt_empty() {
        let dir = std::env::temp_dir().join(format!("pilus-index-schema-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("index.db");
        {
            let mut index = Index::open(&path).expect("open");
            index
                .upsert(&[session("local", Agent::Pi, "a", "kept?", 1)])
                .expect("upsert");
            index
                .conn
                .pragma_update(None, "user_version", SCHEMA - 1)
                .expect("downgrade");
        }
        let index = Index::open(&path).expect("reopen");
        assert_eq!(index.count(None).expect("count"), 0);
        wipe(&path);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
