//! What can go wrong opening or using the database.

/// Why a database call failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SQLite refused.
    #[error("database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The folder or file could not be prepared.
    #[error("database file: {0}")]
    Io(#[from] std::io::Error),
    /// A stored document did not match the type asked for.
    #[error("stored document: {0}")]
    Json(#[from] serde_json::Error),
    /// A table that is not in the database was asked for.
    #[error("no table named {0}")]
    NoTable(String),
    /// The file was written by a newer pilus. Writing to it with older
    /// migrations could corrupt what that version stored, so it is left alone.
    #[error("pilus.db is from a newer pilus (schema {found}, this one knows {known})")]
    Newer {
        /// The schema version in the file.
        found: i64,
        /// The newest version this build can migrate to.
        known: i64,
    },
}

/// Shorthand for results in this crate.
pub type Result<T> = std::result::Result<T, Error>;

impl From<Error> for std::io::Error {
    fn from(error: Error) -> Self {
        match error {
            Error::Io(io) => io,
            other => Self::other(other),
        }
    }
}
