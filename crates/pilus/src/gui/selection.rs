//! Native cell selection over the daemon's rendered grid.
//!
//! A selection is an anchor and a moving end, both in cell coordinates. The
//! daemon knows nothing about it: this client tracks the range, tints the cells
//! while painting, and produces the copied text on demand.
//!
//! In a split tab the grid holds every pane side by side, so a plain
//! reading-order range would run across a seam into the neighbor. A selection
//! can carry `bounds`, the pane's inside, and then each row is cut to those
//! columns: what is highlighted and copied belongs to one pane only.

use pilus_protocol::FrameData;

/// A cell position in the frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell {
    /// Column, left to right.
    pub column: u16,
    /// Row, top to bottom.
    pub row: u16,
}

/// A rectangle of cells, ends inclusive, that a selection never leaves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellRect {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl CellRect {
    /// The cell inside the rectangle nearest to `cell`.
    pub fn clamp(&self, cell: Cell) -> Cell {
        Cell {
            column: cell.column.clamp(self.left, self.right),
            row: cell.row.clamp(self.top, self.bottom),
        }
    }

    fn holds(&self, column: u16, row: u16) -> bool {
        (self.left..=self.right).contains(&column) && (self.top..=self.bottom).contains(&row)
    }
}

/// An anchor and a moving end, both inclusive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    /// Where the drag started.
    pub anchor: Cell,
    /// Where the drag currently is.
    pub cursor: Cell,
    /// The pane the selection is held to; `None` spans the whole grid.
    pub bounds: Option<CellRect>,
}

impl Selection {
    /// A collapsed selection at `cell`, held inside `bounds`.
    pub fn within(cell: Cell, bounds: Option<CellRect>) -> Self {
        let cell = bounds.map_or(cell, |bounds| bounds.clamp(cell));
        Self {
            anchor: cell,
            cursor: cell,
            bounds,
        }
    }

    /// Every cell of `bounds`, the pane's Select All.
    pub fn all(bounds: CellRect) -> Self {
        Self {
            anchor: Cell {
                column: bounds.left,
                row: bounds.top,
            },
            cursor: Cell {
                column: bounds.right,
                row: bounds.bottom,
            },
            bounds: Some(bounds),
        }
    }

    /// Move the end to `cell`, kept inside the bounds. Returns whether it
    /// moved, so a drag past the pane's edge does not repaint on every cell.
    pub fn move_cursor(&mut self, cell: Cell) -> bool {
        let cell = self.bounds.map_or(cell, |bounds| bounds.clamp(cell));
        let moved = self.cursor != cell;
        self.cursor = cell;
        moved
    }

    /// Whether the anchor and cursor coincide (a click, not a drag).
    pub fn is_collapsed(&self) -> bool {
        self.anchor == self.cursor
    }

    /// Whether `cell` falls inside the selection.
    pub fn contains(&self, column: u16, row: u16) -> bool {
        if self.bounds.is_some_and(|bounds| !bounds.holds(column, row)) {
            return false;
        }
        let (start, end) = self.ordered();
        let after_start = row > start.row || (row == start.row && column >= start.column);
        let before_end = row < end.row || (row == end.row && column <= end.column);
        after_start && before_end
    }

    /// The columns selected on `row`, ends inclusive, in a grid `width` wide.
    /// A row holds one unbroken span, so the painter can draw it as a single
    /// rounded shape and compare it with its neighbors' spans.
    pub fn columns(&self, row: u16, width: u16) -> Option<(u16, u16)> {
        let (start, end) = self.ordered();
        if width == 0 || row < start.row || row > end.row {
            return None;
        }
        let (left, right) = match self.bounds {
            Some(bounds) if !(bounds.top..=bounds.bottom).contains(&row) => return None,
            Some(bounds) => (bounds.left, bounds.right),
            None => (0, width - 1),
        };
        let first = if row == start.row { start.column } else { left }.max(left);
        let last = if row == end.row { end.column } else { right }
            .min(right)
            .min(width - 1);
        (first <= last).then_some((first, last))
    }

    /// The selected text in a frame, with trailing blanks trimmed per line.
    pub fn text(&self, frame: &FrameData) -> String {
        if frame.width == 0 || frame.height == 0 {
            return String::new();
        }
        let (start, end) = self.ordered();
        let (left, right) = self
            .bounds
            .map_or((0, frame.width - 1), |bounds| (bounds.left, bounds.right));
        let last_row = end.row.min(frame.height - 1);
        let mut output = String::new();
        for row in start.row..=last_row {
            let first = if row == start.row { start.column } else { left };
            let last = if row == end.row { end.column } else { right };
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
            hyperlinks: Vec::new(),
            cursor: None,
            cursor_visible: false,
        };
        let selection = Selection {
            anchor: Cell { column: 0, row: 0 },
            cursor: Cell { column: 1, row: 1 },
            bounds: None,
        };
        assert_eq!(selection.text(&frame), "ab\nc");
    }

    #[test]
    fn contains_spans_lines_in_order() {
        let selection = Selection {
            anchor: Cell { column: 2, row: 0 },
            cursor: Cell { column: 1, row: 1 },
            bounds: None,
        };
        assert!(selection.contains(2, 0));
        assert!(selection.contains(0, 1));
        assert!(selection.contains(1, 1));
        assert!(!selection.contains(1, 0));
        assert!(!selection.contains(2, 1));
    }

    #[test]
    fn columns_agree_with_contains() {
        let bounded = Selection {
            anchor: Cell { column: 5, row: 1 },
            cursor: Cell { column: 3, row: 3 },
            bounds: Some(CellRect {
                left: 2,
                top: 1,
                right: 6,
                bottom: 3,
            }),
        };
        let free = Selection {
            bounds: None,
            ..bounded
        };
        for selection in [bounded, free] {
            for row in 0..5 {
                let expected: Vec<u16> = (0..10).filter(|c| selection.contains(*c, row)).collect();
                let got: Vec<u16> = selection
                    .columns(row, 10)
                    .map_or_else(Vec::new, |(a, b)| (a..=b).collect());
                assert_eq!(got, expected, "row {row}");
            }
        }
    }

    #[test]
    fn reversed_drag_normalizes() {
        let reversed = Selection {
            anchor: Cell { column: 3, row: 1 },
            cursor: Cell { column: 1, row: 0 },
            bounds: None,
        };
        assert!(reversed.contains(1, 0));
        assert!(reversed.contains(3, 1));
    }

    fn row(text: &str) -> Vec<CellData> {
        text.chars().map(|c| cell(&c.to_string())).collect()
    }

    /// Two panes side by side, `L` on the left and `R` on the right.
    fn split_frame() -> FrameData {
        let cells = ["LLL|RRR", "LLL|RRR", "LLL|RRR"]
            .iter()
            .flat_map(|line| row(line))
            .collect();
        FrameData {
            width: 7,
            height: 3,
            cells,
            hyperlinks: Vec::new(),
            cursor: None,
            cursor_visible: false,
        }
    }

    const LEFT_PANE: CellRect = CellRect {
        left: 0,
        top: 0,
        right: 2,
        bottom: 2,
    };

    #[test]
    fn select_all_in_a_pane_copies_only_that_pane() {
        let selection = Selection::all(LEFT_PANE);
        assert_eq!(selection.text(&split_frame()), "LLL\nLLL\nLLL");
        assert!(!selection.contains(4, 1));
        assert!(!selection.contains(3, 0));
    }

    #[test]
    fn a_drag_stays_inside_its_pane() {
        let mut selection = Selection::within(Cell { column: 1, row: 0 }, Some(LEFT_PANE));
        // The pointer wanders into the right pane, two rows down.
        assert!(selection.move_cursor(Cell { column: 5, row: 1 }));
        assert_eq!(selection.cursor, Cell { column: 2, row: 1 });
        assert_eq!(selection.text(&split_frame()), "LL\nLLL");
        // Further right is still the same clamped cell: nothing to repaint.
        assert!(!selection.move_cursor(Cell { column: 6, row: 1 }));
    }
}
