//! The durable SQLite file behind pilus's own state, `~/.pilus/pilus.db`.
//!
//! This is not the search index. `index.db` (`pilus-index`) is a cache of what
//! the agents wrote: it is dropped when its schema moves and deleted when the
//! person withdraws consent. This file holds the only copy of the conversations,
//! notes and window state, so it is migrated in place ([`schema`]) and nothing
//! deletes it.
//!
//! One [`Db`] is one connection behind a mutex, shared by clone. [`Db::at`] hands
//! every caller in the process the same one for a folder, so a store and the
//! settings never contend through two writers. A store keeps its working copy in
//! memory and writes each change through; the locking order is always the
//! store's own lock first, then this one, never the reverse.
//!
//! A flat file the app used to keep is read by [`Db::import_once`] the first time
//! and renamed by [`retire`], so downgrading is a rename away and nothing is read
//! twice.

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod error;
mod schema;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub use error::{Error, Result};
pub use rusqlite;

/// The file's name inside the folder.
pub const FILE: &str = "pilus.db";

/// A shared handle on the database. Clones are the same connection.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    durable: bool,
}

static OPEN: OnceLock<Mutex<HashMap<PathBuf, Db>>> = OnceLock::new();

impl Db {
    /// Open the database at `path`, creating it (owner-only, since it holds
    /// private conversations) and migrating it.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        create_private(path)?;
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(2))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // A write is on the disk when it returns without waiting for a sync of
        // the log on every commit; a crash can lose the last moments but never
        // corrupts the file.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        schema::migrate(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            durable: true,
        })
    }

    /// A database that lives as long as its handles, for tests and as the
    /// fallback when the file cannot be opened.
    // Opening `:memory:` fails only when the process is out of memory, and the
    // migrations are ours; there is no caller that could do better than stop.
    #[allow(clippy::expect_used)]
    pub fn in_memory() -> Self {
        let mut conn = Connection::open_in_memory().expect("an in-memory database always opens");
        schema::migrate(&mut conn).expect("the migrations run on an empty database");
        Self {
            conn: Arc::new(Mutex::new(conn)),
            durable: false,
        }
    }

    /// The database in `dir`, opened once per process. When it cannot be opened
    /// (a newer pilus wrote it, the disk is read-only) the app still runs on an
    /// in-memory one and says so, as it does for every other state it fails to
    /// save; [`Self::durable`] tells a caller that nothing will outlive the run.
    pub fn at(dir: &Path) -> Self {
        let mut open = OPEN
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        open.entry(dir.to_path_buf())
            .or_insert_with(|| {
                Self::open(&dir.join(FILE)).unwrap_or_else(|error| {
                    tracing::warn!("could not open {}: {error}", dir.join(FILE).display());
                    Self::in_memory()
                })
            })
            .clone()
    }

    /// Whether what is written here reaches a file.
    pub fn durable(&self) -> bool {
        self.durable
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run `work` on the connection.
    pub fn with<R>(&self, work: impl FnOnce(&Connection) -> Result<R>) -> Result<R> {
        work(&self.lock())
    }

    /// Run `work` in one transaction: all of it lands or none does.
    pub fn transaction<R>(&self, work: impl FnOnce(&Transaction) -> Result<R>) -> Result<R> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let out = work(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// The document stored under `name`, if there is one.
    pub fn doc<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let body: Option<String> = self.with(|conn| {
            Ok(conn
                .query_row("SELECT body FROM docs WHERE name = ?1", [name], |row| {
                    row.get(0)
                })
                .optional()?)
        })?;
        Ok(body.map(|body| serde_json::from_str(&body)).transpose()?)
    }

    /// Store `value` as the document `name`, replacing the last one.
    pub fn put_doc<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        self.with(|conn| put_doc_in(conn, name, value))
    }

    /// Whether `tag` has been imported from its flat file already.
    pub fn imported(&self, tag: &str) -> bool {
        self.with(|conn| {
            Ok(conn
                .query_row("SELECT 1 FROM imports WHERE tag = ?1", [tag], |_| Ok(()))
                .optional()?
                .is_some())
        })
        .unwrap_or(false)
    }

    /// Run `import` once for `tag`, in one transaction that also records it, so
    /// a half-read file is never half-imported and a file that could not be
    /// renamed afterwards is not read again. Returns whether it ran.
    pub fn import_once(
        &self,
        tag: &str,
        import: impl FnOnce(&Transaction) -> Result<()>,
    ) -> Result<bool> {
        if self.imported(tag) {
            return Ok(false);
        }
        self.transaction(|tx| {
            import(tx)?;
            tx.execute(
                "INSERT OR IGNORE INTO imports (tag, at) VALUES (?1, ?2)",
                params![tag, now()],
            )?;
            Ok(())
        })?;
        Ok(true)
    }
}

/// [`Db::put_doc`] on a connection or transaction already in hand. Inside
/// [`Db::transaction`] the handle itself must not be used again: its lock is held.
pub fn put_doc_in<T: Serialize>(conn: &Connection, name: &str, value: &T) -> Result<()> {
    let body = serde_json::to_string(value)?;
    conn.execute(
        "INSERT INTO docs (name, body, updated) VALUES (?1, ?2, ?3)
         ON CONFLICT (name) DO UPDATE SET body = excluded.body, updated = excluded.updated",
        params![name, body, now()],
    )?;
    Ok(())
}

/// Put an imported flat file aside as `<name>.migrated` rather than deleting it:
/// it is the copy an older pilus would read, and the person's own data.
pub fn retire(path: &Path) {
    let mut name = path.as_os_str().to_owned();
    name.push(".migrated");
    if let Err(error) = std::fs::rename(path, PathBuf::from(name)) {
        tracing::warn!("could not set {} aside: {error}", path.display());
    }
}

/// Seconds since the Unix epoch.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Create `path` owner-only if it is not there, so SQLite (which gives its
/// `-wal` and `-shm` files the main file's mode) never creates it world-readable.
fn create_private(path: &Path) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-db-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_document_round_trips_and_is_replaced() {
        let db = Db::in_memory();
        assert_eq!(db.doc::<Vec<u32>>("a").unwrap(), None);
        db.put_doc("a", &vec![1, 2]).unwrap();
        db.put_doc("a", &vec![3]).unwrap();
        assert_eq!(db.doc::<Vec<u32>>("a").unwrap(), Some(vec![3]));
    }

    #[test]
    fn an_import_runs_once_and_not_at_all_when_it_fails() {
        let db = Db::in_memory();
        let failed = db.import_once("x", |tx| {
            tx.execute("INSERT INTO docs VALUES ('half', '1', 0)", [])?;
            Err(Error::Io(std::io::Error::other("boom")))
        });
        assert!(failed.is_err());
        assert_eq!(db.doc::<u8>("half").unwrap(), None, "rolled back");
        assert!(!db.imported("x"));
        assert!(db.import_once("x", |_| Ok(())).unwrap());
        assert!(!db.import_once("x", |_| Ok(())).unwrap());
    }

    #[test]
    fn the_file_is_private_and_survives_a_reopen() {
        let dir = scratch("reopen");
        let db = Db::open(&dir.join(FILE)).unwrap();
        db.put_doc("k", &"v").unwrap();
        drop(db);
        let again = Db::open(&dir.join(FILE)).unwrap();
        assert_eq!(again.doc::<String>("k").unwrap().as_deref(), Some("v"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_from_a_newer_pilus_is_left_alone() {
        let dir = scratch("newer");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = Connection::open(dir.join(FILE)).unwrap();
        conn.pragma_update(None, "user_version", 999).unwrap();
        drop(conn);
        assert!(matches!(
            Db::open(&dir.join(FILE)),
            Err(Error::Newer { found: 999, .. })
        ));
        // The app keeps running, on a database that is not the file.
        assert!(!Db::at(&dir).durable());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn at_shares_one_connection_per_folder() {
        let dir = scratch("shared");
        let a = Db::at(&dir);
        let b = Db::at(&dir);
        a.put_doc("k", &1u8).unwrap();
        assert_eq!(b.doc::<u8>("k").unwrap(), Some(1));
        assert!(a.durable());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retire_keeps_the_old_file_under_a_new_name() {
        let dir = scratch("retire");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("old.json");
        std::fs::write(&file, "{}").unwrap();
        retire(&file);
        assert!(!file.exists());
        assert!(dir.join("old.json.migrated").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
