//! Small helpers the sipil stores share on top of `pilus.db`.
//!
//! Contacts, channels and what is remembered of the daemons are each one document
//! in the `docs` table: read once at launch, held in memory, and replaced whole
//! on every change inside a transaction, so a crash leaves the old document
//! rather than half of a new one. They are the app's own state, so one that
//! cannot be read starts the store empty instead of failing the window.
//!
//! Before `pilus.db` they were JSON files beside the identity key. The first
//! open reads such a file into the database and sets it aside as
//! `<name>.migrated` ([`pilus_db::retire`]). The conversations are not documents
//! but tables, and `chat.rs` does its own import of `chats.jsonl`.

use std::path::Path;

use pilus_db::Db;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The document `name`, else the flat file `legacy` imported into it, else
/// `T::default()`. A flat file that does not parse is left where it is for the
/// person to look at, and the next change replaces nothing of it.
pub(super) fn load<T: DeserializeOwned + Serialize + Default>(
    db: &Db,
    name: &str,
    legacy: &Path,
) -> T {
    match db.doc::<T>(name) {
        Ok(Some(value)) => return value,
        Ok(None) => {}
        Err(error) => {
            tracing::warn!("could not read {name} from the database: {error}");
            return T::default();
        }
    }
    let Some(value) = read_file::<T>(legacy) else {
        return T::default();
    };
    match db.put_doc(name, &value) {
        Ok(()) => pilus_db::retire(legacy),
        Err(error) => tracing::warn!("could not import {}: {error}", legacy.display()),
    }
    value
}

/// Replace the document `name` with `value`.
pub(super) fn save<T: Serialize>(db: &Db, name: &str, value: &T) -> std::io::Result<()> {
    db.put_doc(name, value).map_err(std::io::Error::from)
}

fn read_file<T: DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Create `path` owner-only (`0600` on Unix) holding `bytes`, replacing any file.
/// The identity key is written this way.
pub(super) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()
}

/// Seconds since the Unix epoch.
pub(super) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
