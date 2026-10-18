//! What can go wrong while indexing.

/// An indexing failure, worded for a status line.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The SQLite file could not be opened or written.
    #[error("the index database failed: {0}")]
    Database(#[from] rusqlite::Error),
    /// A transcript directory could not be read.
    #[error("couldn't read the sessions: {0}")]
    Io(#[from] std::io::Error),
    /// A remote machine did not answer, or answered with a failure.
    #[error("{0}")]
    Remote(String),
}

/// A result carrying an indexing [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
