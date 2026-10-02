//! Wire protocol for `pilus`, a native desktop client of a Luvus daemon.
//!
//! A Luvus server renders its whole TUI surface to a client as a grid of
//! styled cells. This crate owns the shared vocabulary for that link: the
//! client and server message enums, the frame/cell model, and the
//! length-prefixed bincode framing that carries them.
//!
//! Encoding is positional, so **the declaration order of every enum variant
//! and struct field is part of the wire format**. Add new variants at the end
//! and bump [`PROTOCOL_VERSION`]; never reorder or remove an existing one.

#![deny(missing_docs)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod frame;
mod message;
mod theme;

pub use frame::{CellData, DiffRun, FrameData, FrameDiff, FrameHyperlink};
pub use message::{ClientMessage, ServerMessage, SurfaceInterest};
pub use theme::{
    MachineFormTheme, ShellDockBlock, ShellDockLayout, ShellDockRect, ShellResize, ShellSidebars,
    ShellWorkspace, Side, SideConfig, SidebarsConfig, SoundCue, SoundSignal, SoundStyle,
    TerminalColors,
};

use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The binary client/server protocol revision `pilus` speaks.
///
/// This tracks the **released** daemon: Luvus `v0.14.2` ships protocol 18.
/// Protocol 19 (on `main`) adds a `FrameData.hyperlinks` field, which is
/// positional and therefore not wire-compatible with 18. When attaching to a
/// newer daemon, bump this and add the field back to `FrameData` in the same
/// position the daemon sends it.
///
/// The server decodes `ClientMessage::Hello` before any version check and
/// answers a mismatch with a `Welcome` carrying an error, so this value is the
/// single place a client declares its compatibility.
pub const PROTOCOL_VERSION: u32 = 18;

/// Hard ceiling on a single framed message. A peer that announces a larger
/// frame is rejected before any allocation proportional to it happens.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// Errors raised while framing a message.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The underlying transport failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A message could not be encoded or decoded.
    #[error("protocol codec error: {0}")]
    Codec(#[from] bincode::error::EncodeError),
    /// A message could not be decoded.
    #[error("protocol decode error: {0}")]
    Decode(#[from] bincode::error::DecodeError),
}

/// Encode and write one length-prefixed message, flushing the writer.
///
/// The four-byte little-endian length prefix is what lets the peer read a
/// complete frame without knowing the payload type in advance.
pub fn write_message<W: Write>(writer: &mut W, message: &impl Serialize) -> Result<(), FrameError> {
    write_message_counted(writer, message).map(|_| ())
}

/// Like [`write_message`], but also returns the exact wire length including the
/// four-byte prefix.
pub fn write_message_counted<W: Write>(
    writer: &mut W,
    message: &impl Serialize,
) -> Result<usize, FrameError> {
    let bytes = bincode::serde::encode_to_vec(message, bincode::config::standard())?;
    if bytes.len() > MAX_FRAME {
        return Err(FrameError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME",
        )));
    }
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(bytes.len().saturating_add(4))
}

/// Read exactly one length-prefixed message of type `M`.
///
/// Returns an `InvalidData` IO error when the announced length exceeds
/// [`MAX_FRAME`] so a malformed peer cannot force a huge allocation.
pub fn read_message<R: Read, M: DeserializeOwned>(reader: &mut R) -> Result<M, FrameError> {
    let mut length = [0u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    if length > MAX_FRAME {
        return Err(FrameError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        )));
    }
    let mut buffer = vec![0u8; length];
    reader.read_exact(&mut buffer)?;
    let config = bincode::config::standard().with_limit::<MAX_FRAME>();
    let (message, _) = bincode::serde::decode_from_slice(&buffer, config)?;
    Ok(message)
}

/// Total display width of `symbol` in terminal cells.
///
/// Zero-width combining marks return `0`; wide CJK/emoji glyphs return `2`.
pub fn symbol_width(symbol: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(symbol)
}
