//! The platform transport under a connection.
//!
//! Luvus listens on a Unix-domain socket whose path is derived from the state
//! directory. This module turns that path into a connected stream that both
//! reads and writes, and that can be borrowed immutably for either direction.

use std::io::{Read, Write};
use std::path::Path;

use crate::error::{Error, Result};

/// A connected client stream.
pub struct Stream(interprocess::local_socket::Stream);

impl Stream {
    /// Connect to the socket at `path`.
    ///
    /// Returns [`Error::NotFound`] when nothing is listening, so the caller can
    /// suggest starting the daemon instead of surfacing a raw IO error.
    pub fn connect(path: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            use interprocess::local_socket::prelude::*;

            if !is_socket(path)? {
                return Err(Error::NotFound {
                    path: path.to_path_buf(),
                });
            }
            let name = path
                .to_fs_name::<interprocess::local_socket::GenericFilePath>()
                .map_err(|error| Error::Io(std::io::Error::other(error.to_string())))?;
            let stream = interprocess::local_socket::Stream::connect(name).map_err(|error| {
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) {
                    Error::NotFound {
                        path: path.to_path_buf(),
                    }
                } else {
                    Error::Io(error)
                }
            })?;
            Ok(Self(stream))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "named-pipe transport is not implemented yet",
            )))
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl Read for &Stream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        (&self.0).read(buffer)
    }
}

impl Write for &Stream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        (&self.0).write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        (&self.0).flush()
    }
}

#[cfg(unix)]
fn is_socket(path: &Path) -> Result<bool> {
    use std::os::unix::fs::FileTypeExt;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_socket()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::Io(error)),
    }
}
