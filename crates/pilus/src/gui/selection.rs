//! Native cell selection over the daemon's rendered grid.
//!
//! A selection is an anchor and a moving end, both in cell coordinates. The
//! daemon knows nothing about it: this client tracks the range, tints the cells
//! while painting, and produces the copied text on demand.

use pilus_protocol::FrameData;

/// A cell position in the frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell {
    /// Column, left to right.
    pub column: u16,
    /// Row, top to bottom.
    pub row: u16,
}

/// An anchor and a moving end, both inclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    /// Where the drag started.
    pub anchor: Cell,
    /// Where the drag currently is.
    pub cursor: Cell,
}

impl Selection {
    /// A collapsed selection at `cell`.
    pub fn new(cell: Cell) -> Self {
        Self {
            anchor: cell,
            cursor: cell,
        }
    }

    /// Whether the anchor and cursor coincide (a click, not a drag).
    pub fn is_collapsed(&self) -> bool {
        self.anchor == self.cursor
    }

    /// Whether `cell` falls inside the selection.
    pub fn contains(&self, column: u16, row: u16) -> bool {
        let (start, end) = self.ordered();
        let after_start = row > start.row || (row == start.row && column >= start.column);
        let before_end = row < end.row || (row == end.row && column <= end.column);
        after_start && before_end
    }

    /// The selected text in a frame, with trailing blanks trimmed per line.
    pub fn text(&self, frame: &FrameData) -> String {
        if frame.width == 0 || frame.height == 0 {
            return String::new();
        }
        let (start, end) = self.ordered();
        let last_row = end.row.min(frame.height - 1);
        let mut output = String::new();
        for row in start.row..=last_row {
            let first = if row == start.row { start.column } else { 0 };
            let last = if row == end.row {
                end.column
            } else {
                frame.width - 1
            };
            let last = last.min(frame.width - 1);
            let mut line = String::new();
            for column in first..=last {
                if let Some(cell) = frame.cell(column, row) {
                    if cell.symbol.is_empty() {
                        line.push(' ');
                    } else {
                        line.push_str(&cell.symbol);
                    }
                }
            }
            output.push_str(line.trim_end());
            if row != last_row {
                output.push('\n');
            }
        }
        output
    }

    /// The selection with anchor and cursor in reading order.
    fn ordered(&self) -> (Cell, Cell) {
        if (self.anchor.row, self.anchor.column) <= (self.cursor.row, self.cursor.column) {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilus_protocol::{CellData, FrameData};

    fn cell(symbol: &str) -> CellData {
        CellData {
            symbol: symbol.to_string(),
            fg: 0,
            bg: 0,
            mods: 0,
        }
    }

    #[test]
    fn text_trims_trailing_blanks_and_joins_lines() {
        let frame = FrameData {
            width: 4,
            height: 2,
            cells: vec![
                cell("a"),
                cell("b"),
                cell(" "),
                cell(" "),
                cell("c"),
                cell(" "),
                cell(" "),
                cell(" "),
            ],
            cursor: None,
            cursor_visible: false,
        };
        let selection = Selection {
            anchor: Cell { column: 0, row: 0 },
            cursor: Cell { column: 1, row: 1 },
        };
        assert_eq!(selection.text(&frame), "ab\nc");
    }

    #[test]
    fn contains_spans_lines_in_order() {
        let selection = Selection {
            anchor: Cell { column: 2, row: 0 },
            cursor: Cell { column: 1, row: 1 },
        };
        assert!(selection.contains(2, 0));
        assert!(selection.contains(0, 1));
        assert!(selection.contains(1, 1));
        assert!(!selection.contains(1, 0));
        assert!(!selection.contains(2, 1));
    }

    #[test]
    fn reversed_drag_normalizes() {
        let reversed = Selection {
            anchor: Cell { column: 3, row: 1 },
            cursor: Cell { column: 1, row: 0 },
        };
        assert!(reversed.contains(1, 0));
        assert!(reversed.contains(3, 1));
    }
}
