//! Small JSON files the sipil side keeps beside the settings.
//!
//! Each store is read once and rewritten whole on every change, through a
//! temporary file and a rename, so a crash mid-write leaves the old file rather
//! than half of a new one. They are the app's own state, so a file that cannot
//! be read starts the store empty instead of failing the window; the unreadable
//! file is left where it is for the user to look at, and the next change
//! replaces it. The conversations are the exception: `chat.rs` appends to a JSON
//! Lines log with [`append_private`] and only rewrites it, with [`replace`], to
//! compact it.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Read `path` as `T`; a missing or unreadable file is `T::default()`.
pub(super) fn load<T: DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Write `value` to `path` atomically and owner-only.
pub(super) fn save<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    replace(path, &bytes)
}

/// Put `bytes` at `path` atomically and owner-only, through a temporary file.
pub(super) fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("json.tmp");
    write_private(&temp, bytes)?;
    std::fs::rename(&temp, path)
}

/// Add `bytes` to the end of `path`, creating it owner-only. One `write` on an
/// append-mode file, so a record is not interleaved with another writer's.
pub(super) fn append_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// Create `path` owner-only (`0600` on Unix) holding `bytes`, replacing any file.
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
