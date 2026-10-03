//! The cell-grid frame model and its invariants.
//!
//! A frame is a row-major grid of [`CellData`]. The server sends a full
//! [`FrameData`] on attach and resize, then [`FrameDiff`]s that touch only the
//! cells that changed. A client applies diffs to its stored frame and repaints.

use std::cell::Cell;

use serde::{Deserialize, Serialize};

thread_local! {
    /// Protocol revision the current reader thread negotiated. `FrameData` is
    /// positional, so the decoder must know whether the daemon sent the
    /// protocol-20 `hyperlinks` field. One connection per reader thread keeps
    /// this isolated even when endpoints on different revisions are bridged at
    /// once.
    static DECODE_VERSION: Cell<u32> = const { Cell::new(crate::PROTOCOL_VERSION) };
}

/// Set the protocol revision [`FrameData`]'s deserializer uses on this thread.
///
/// A connection's reader thread calls this once, right after the handshake
/// agrees on a revision. Threads that never call it decode the newest shape.
pub fn set_decode_version(version: u32) {
    DECODE_VERSION.with(|cell| cell.set(version));
}

/// One rendered terminal cell.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct CellData {
    /// The grapheme drawn in this cell, possibly empty for the trailing half
    /// of a wide glyph.
    pub symbol: String,
    /// Foreground color packed as `0x00RRGGBB`, or a palette index the host
    /// resolves.
    pub fg: u32,
    /// Background color packed as `0x00RRGGBB`.
    pub bg: u32,
    /// Attribute bits (bold, italic, underline, reverse, and so on).
    pub mods: u16,
}

impl CellData {
    /// A blank cell with the given colors.
    pub fn blank(fg: u32, bg: u32) -> Self {
        Self {
            symbol: " ".to_string(),
            fg,
            bg,
            mods: 0,
        }
    }

    /// Whether this cell carries no glyph and no styling.
    pub fn is_empty(&self) -> bool {
        self.symbol.is_empty() || (self.symbol == " " && self.mods == 0)
    }
}

/// A sparse `OSC 8` hyperlink run over the flat cell index space.
#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
pub struct FrameHyperlink {
    /// First cell index, inclusive.
    pub start: u32,
    /// Last cell index, exclusive.
    pub end: u32,
    /// Target URI.
    pub uri: String,
}

/// A maximal run of adjacent changed cells sharing one style.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct DiffRun {
    /// Flat index of the first cell.
    pub start: u32,
    /// Foreground color for the run.
    pub fg: u32,
    /// Background color for the run.
    pub bg: u32,
    /// Attribute bits for the run.
    pub mods: u16,
    /// One grapheme per cell, kept separate so wide glyphs survive the trip.
    pub symbols: Vec<String>,
}

/// A complete `width * height` grid of cells plus the caret.
///
/// Protocol 20 shape. The `hyperlinks` field sits between `cells` and `cursor`,
/// where the daemon sends it; its position is positional wire state, so it
/// must stay there. The deserializer is hand-written so the same type can
/// decode a protocol-18 daemon's frame (without `hyperlinks`) when a connection
/// negotiates down; see [`set_decode_version`].
#[derive(Serialize, Clone, PartialEq, Debug)]
pub struct FrameData {
    /// Grid width in cells.
    pub width: u16,
    /// Grid height in cells.
    pub height: u16,
    /// Row-major cells, exactly `width * height` long.
    pub cells: Vec<CellData>,
    /// Sparse `OSC 8` link spans, empty for most frames.
    pub hyperlinks: Vec<FrameHyperlink>,
    /// Caret position as `(column, row)`, or `None` when hidden.
    pub cursor: Option<(u16, u16)>,
    /// When `cursor` is present, whether the host caret should be shown.
    #[serde(default)]
    pub cursor_visible: bool,
}

/// The protocol-20 field order, `hyperlinks` included.
#[derive(Deserialize)]
struct FrameDataV20 {
    width: u16,
    height: u16,
    cells: Vec<CellData>,
    hyperlinks: Vec<FrameHyperlink>,
    cursor: Option<(u16, u16)>,
    #[serde(default)]
    cursor_visible: bool,
}

/// The protocol-18 field order, without `hyperlinks`.
#[derive(Deserialize)]
struct FrameDataV18 {
    width: u16,
    height: u16,
    cells: Vec<CellData>,
    cursor: Option<(u16, u16)>,
    #[serde(default)]
    cursor_visible: bool,
}

impl<'de> Deserialize<'de> for FrameData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if DECODE_VERSION.with(Cell::get) >= 19 {
            let FrameDataV20 {
                width,
                height,
                cells,
                hyperlinks,
                cursor,
                cursor_visible,
            } = FrameDataV20::deserialize(deserializer)?;
            Ok(Self {
                width,
                height,
                cells,
                hyperlinks,
                cursor,
                cursor_visible,
            })
        } else {
            let FrameDataV18 {
                width,
                height,
                cells,
                cursor,
                cursor_visible,
            } = FrameDataV18::deserialize(deserializer)?;
            Ok(Self {
                width,
                height,
                cells,
                hyperlinks: Vec::new(),
                cursor,
                cursor_visible,
            })
        }
    }
}

/// A sparse update to a [`FrameData`] of identical dimensions.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct FrameDiff {
    /// Grid width; must match the frame being updated.
    pub width: u16,
    /// Grid height; must match the frame being updated.
    pub height: u16,
    /// Changed runs.
    pub runs: Vec<DiffRun>,
    /// New caret position.
    pub cursor: Option<(u16, u16)>,
    /// Whether the caret should be shown.
    #[serde(default)]
    pub cursor_visible: bool,
}

/// A frame invariant was violated.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameInvariant {
    /// The cell count did not match `width * height`.
    #[error("cell count {got} does not match {width}x{height}")]
    CellCount {
        /// Declared width.
        width: u16,
        /// Declared height.
        height: u16,
        /// Actual cell count.
        got: usize,
    },
    /// The caret was placed outside the grid.
    #[error("cursor ({x}, {y}) is outside {width}x{height}")]
    Cursor {
        /// Caret column.
        x: u16,
        /// Caret row.
        y: u16,
        /// Grid width.
        width: u16,
        /// Grid height.
        height: u16,
    },
    /// A diff targeted a frame with different dimensions.
    #[error("diff is {dw}x{dh} but frame is {fw}x{fh}")]
    DimensionMismatch {
        /// Diff width.
        dw: u16,
        /// Diff height.
        dh: u16,
        /// Frame width.
        fw: u16,
        /// Frame height.
        fh: u16,
    },
    /// A diff run overran the grid.
    #[error("diff run at {start} with {len} cells overruns the grid")]
    RunBounds {
        /// Run start index.
        start: u32,
        /// Run length in cells.
        len: usize,
    },
}

impl FrameData {
    /// An all-blank frame of the given size.
    pub fn blank(width: u16, height: u16, fg: u32, bg: u32) -> Self {
        Self {
            width,
            height,
            cells: vec![CellData::blank(fg, bg); usize::from(width) * usize::from(height)],
            hyperlinks: Vec::new(),
            cursor: None,
            cursor_visible: false,
        }
    }

    /// Number of cells the dimensions imply.
    pub fn cell_count(&self) -> usize {
        usize::from(self.width) * usize::from(self.height)
    }

    /// The cell at `(column, row)`, if in bounds.
    pub fn cell(&self, column: u16, row: u16) -> Option<&CellData> {
        if column >= self.width || row >= self.height {
            return None;
        }
        let index = usize::from(row) * usize::from(self.width) + usize::from(column);
        self.cells.get(index)
    }

    /// Validate the frame's structural invariants.
    pub fn validate(&self) -> Result<(), FrameInvariant> {
        if self.cells.len() != self.cell_count() {
            return Err(FrameInvariant::CellCount {
                width: self.width,
                height: self.height,
                got: self.cells.len(),
            });
        }
        let _ = self.cells.len();
        if let Some((x, y)) = self.cursor
            && self.cursor_visible
            && (x >= self.width || y >= self.height)
        {
            return Err(FrameInvariant::Cursor {
                x,
                y,
                width: self.width,
                height: self.height,
            });
        }
        Ok(())
    }

    /// Apply a diff in place, updating the caret.
    ///
    /// Fails without mutating the frame when the diff's dimensions disagree or
    /// a run overruns the grid, so the caller can request a full frame instead
    /// of painting a torn surface.
    pub fn apply_diff(&mut self, diff: &FrameDiff) -> Result<(), FrameInvariant> {
        if diff.width != self.width || diff.height != self.height {
            return Err(FrameInvariant::DimensionMismatch {
                dw: diff.width,
                dh: diff.height,
                fw: self.width,
                fh: self.height,
            });
        }
        let limit = self.cells.len();
        for run in &diff.runs {
            let start = run.start as usize;
            if start + run.symbols.len() > limit {
                return Err(FrameInvariant::RunBounds {
                    start: run.start,
                    len: run.symbols.len(),
                });
            }
        }
        for run in &diff.runs {
            let start = run.start as usize;
            for (offset, symbol) in run.symbols.iter().enumerate() {
                let cell = &mut self.cells[start + offset];
                cell.symbol.clone_from(symbol);
                cell.fg = run.fg;
                cell.bg = run.bg;
                cell.mods = run.mods;
            }
        }
        self.cursor = diff.cursor;
        self.cursor_visible = diff.cursor_visible;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FALLBACK_PROTOCOL_VERSION, PROTOCOL_VERSION};

    fn frame() -> FrameData {
        FrameData::blank(3, 2, 0x00ff_ffff, 0x0000_0000)
    }

    /// The protocol-18 shape, used to prove the fallback decode path.
    #[derive(Serialize)]
    struct FrameV18 {
        width: u16,
        height: u16,
        cells: Vec<CellData>,
        cursor: Option<(u16, u16)>,
        cursor_visible: bool,
    }

    #[test]
    fn decodes_both_frame_shapes_by_negotiated_revision() {
        let config = bincode::config::standard();
        let v18 = FrameV18 {
            width: 2,
            height: 1,
            cells: vec![CellData::blank(1, 0), CellData::blank(2, 0)],
            cursor: Some((1, 0)),
            cursor_visible: true,
        };
        let bytes = bincode::serde::encode_to_vec(&v18, config).expect("encode v18");
        set_decode_version(FALLBACK_PROTOCOL_VERSION);
        let (decoded, _): (FrameData, usize) =
            bincode::serde::decode_from_slice(&bytes, config).expect("decode v18");
        assert!(decoded.hyperlinks.is_empty());
        assert_eq!(decoded.cursor, Some((1, 0)));
        assert!(decoded.cursor_visible);

        // The newest revision round-trips a non-empty span.
        set_decode_version(PROTOCOL_VERSION);
        let v20 = FrameData {
            width: 1,
            height: 1,
            cells: vec![CellData::blank(1, 0)],
            hyperlinks: vec![FrameHyperlink {
                start: 0,
                end: 1,
                uri: "https://example.com".into(),
            }],
            cursor: None,
            cursor_visible: false,
        };
        let bytes = bincode::serde::encode_to_vec(&v20, config).expect("encode v20");
        let (decoded, _): (FrameData, usize) =
            bincode::serde::decode_from_slice(&bytes, config).expect("decode v20");
        assert_eq!(decoded.hyperlinks.len(), 1);
    }

    #[test]
    fn blank_frame_validates() {
        let frame = frame();
        assert_eq!(frame.cells.len(), 6);
        assert!(frame.validate().is_ok());
    }

    #[test]
    fn diff_applies_changed_run() {
        let mut frame = frame();
        let diff = FrameDiff {
            width: 3,
            height: 2,
            runs: vec![DiffRun {
                start: 4,
                fg: 0x00ff_0000,
                bg: 0x0000_0000,
                mods: 1,
                symbols: vec!["h".into(), "i".into()],
            }],
            cursor: Some((2, 1)),
            cursor_visible: true,
        };
        frame.apply_diff(&diff).expect("diff applies");
        assert_eq!(frame.cells[4].symbol, "h");
        assert_eq!(frame.cells[5].fg, 0x00ff_0000);
        assert_eq!(frame.cursor, Some((2, 1)));
    }

    #[test]
    fn diff_rejects_dimension_mismatch_without_mutating() {
        let mut frame = frame();
        let before = frame.clone();
        let diff = FrameDiff {
            width: 4,
            height: 2,
            runs: Vec::new(),
            cursor: None,
            cursor_visible: false,
        };
        assert!(frame.apply_diff(&diff).is_err());
        assert_eq!(frame, before);
    }

    #[test]
    fn diff_rejects_overrun_without_mutating() {
        let mut frame = frame();
        let before = frame.clone();
        let diff = FrameDiff {
            width: 3,
            height: 2,
            runs: vec![DiffRun {
                start: 5,
                fg: 0,
                bg: 0,
                mods: 0,
                symbols: vec!["a".into(), "b".into()],
            }],
            cursor: None,
            cursor_visible: false,
        };
        assert!(frame.apply_diff(&diff).is_err());
        assert_eq!(frame, before);
    }
}
