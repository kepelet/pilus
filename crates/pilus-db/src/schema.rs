//! The tables, as an append-only list of migrations.
//!
//! Unlike the search index, which is a cache and is dropped and rebuilt when its
//! schema moves, this file holds the only copy of the person's conversations and
//! notes. A step already released is never edited: a change is a new step at the
//! end, and `PRAGMA user_version` says how many have run.

use rusqlite::Connection;

use crate::error::{Error, Result};

/// Every migration, oldest first.
const MIGRATIONS: &[&str] = &[
    // 1: the first stores moved off flat files.
    "
    -- Whole-document state that is small, always read whole and changed through
    -- one edit: contacts, channels, what is remembered of the daemons, and the
    -- window state kept out of settings.json.
    CREATE TABLE docs (
        name    TEXT PRIMARY KEY,
        body    TEXT NOT NULL,
        updated INTEGER NOT NULL
    ) WITHOUT ROWID;

    -- A flat file that has been read into this database, so it is read once
    -- even when it could not be renamed afterwards.
    CREATE TABLE imports (
        tag TEXT PRIMARY KEY,
        at  INTEGER NOT NULL
    ) WITHOUT ROWID;

    CREATE TABLE chat_threads (
        thread       TEXT PRIMARY KEY,
        -- The id of the newest line ever added, which the next one follows even
        -- after older lines were trimmed away.
        next         INTEGER NOT NULL DEFAULT 0,
        read_through INTEGER NOT NULL DEFAULT 0
    ) WITHOUT ROWID;

    CREATE TABLE chat_lines (
        thread     TEXT NOT NULL,
        id         INTEGER NOT NULL,
        mine       INTEGER NOT NULL,
        text       TEXT NOT NULL,
        at         INTEGER NOT NULL,
        who        TEXT,
        agent      TEXT,
        reply      TEXT,
        delegation TEXT,
        PRIMARY KEY (thread, id)
    ) WITHOUT ROWID;

    -- The request id each peer line arrived under, to recognise a retry.
    CREATE TABLE chat_requests (
        thread  TEXT NOT NULL,
        line    INTEGER NOT NULL,
        request TEXT NOT NULL,
        PRIMARY KEY (thread, line)
    ) WITHOUT ROWID;
    CREATE INDEX chat_requests_by_request ON chat_requests (thread, request);

    CREATE TABLE chat_cursors (
        thread  TEXT NOT NULL,
        daemon  TEXT NOT NULL,
        session TEXT NOT NULL,
        through INTEGER NOT NULL,
        PRIMARY KEY (thread, daemon)
    ) WITHOUT ROWID;

    CREATE TABLE md_notes (
        id        INTEGER PRIMARY KEY,
        endpoint  TEXT NOT NULL,
        path      TEXT NOT NULL,
        first_line INTEGER,
        last_line  INTEGER,
        quote     TEXT NOT NULL,
        body      TEXT NOT NULL,
        sent_to   TEXT
    );
    CREATE INDEX md_notes_by_file ON md_notes (endpoint, path);

    CREATE TABLE github_cache (
        key        TEXT PRIMARY KEY,
        fetched_at INTEGER NOT NULL,
        etag       TEXT NOT NULL,
        body       TEXT NOT NULL
    ) WITHOUT ROWID;
    ",
];

/// Bring `conn` up to the newest schema.
pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    let known = MIGRATIONS.len() as i64;
    let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > known {
        return Err(Error::Newer { found, known });
    }
    for (index, step) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        // The version moves with the step, so a crash leaves either both or
        // neither.
        let tx = conn.transaction()?;
        tx.execute_batch(step)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}
