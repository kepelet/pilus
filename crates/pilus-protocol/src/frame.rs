//! The cell-grid frame model and its invariants.
//!
//! A frame is a row-major grid of [`CellData`]. The server sends a full
//! [`FrameData`] on attach and resize, then [`FrameDiff`]s that touch only the
//! cells that changed. A client applies diffs to its stored frame and repaints.

use serde::{Deserialize, Serialize};

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
/// Protocol 18 shape. Protocol 19 inserts a `Vec<FrameHyperlink>` between
/// `cells` and `cursor`; adding that field here without bumping
/// [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION) would corrupt every frame.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct FrameData {
    /// Grid width in cells.
    pub width: u16,
    /// Grid height in cells.
    pub height: u16,
    /// Row-major cells, exactly `width * height` long.
    pub cells: Vec<CellData>,
    /// Caret position as `(column, row)`, or `None` when hidden.
    pub cursor: Option<(u16, u16)>,
    /// When `cursor` is present, whether the host caret should be shown.
    #[serde(default)]
    pub cursor_visible: bool,
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

    fn frame() -> FrameData {
        FrameData::blank(3, 2, 0x00ff_ffff, 0x0000_0000)
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
