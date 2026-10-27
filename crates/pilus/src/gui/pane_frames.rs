//! The panes a frame is split into, found from the boxes the daemon draws.
//!
//! The daemon sends no pane geometry: each pane is a closed box of `┌─┐│└┘`
//! cells, colored by the daemon's theme and always square. `render_grid` asks
//! this module for those boxes, hides their glyphs, and draws each as a native
//! rounded border instead, so a split follows the color scheme and the
//! corner-radius setting like the rest of the chrome.
//!
//! A TUI's own boxes use the same glyphs, so a box only counts as a pane when
//! the outermost boxes tile the frame: two or more, touching the right and
//! bottom edges, with no gaps or overlaps. Boxes inside a pane are contained
//! by its frame and ignored; a lone box (a full-screen TUI's border) is not a
//! split and is left as drawn.

use pilus_protocol::FrameData;

use super::box_drawing::{self, Arms};

/// A pane's frame: the cells of its border box, ends inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PaneFrame {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl PaneFrame {
    fn area(&self) -> u32 {
        u32::from(self.right - self.left + 1) * u32::from(self.bottom - self.top + 1)
    }

    fn contains(&self, other: &PaneFrame) -> bool {
        self.left <= other.left
            && self.top <= other.top
            && self.right >= other.right
            && self.bottom >= other.bottom
    }

    fn overlaps(&self, other: &PaneFrame) -> bool {
        self.left <= other.right
            && other.left <= self.right
            && self.top <= other.bottom
            && other.top <= self.bottom
    }

    /// Whether `column`, `row` lies inside the box, border included.
    pub(super) fn holds(&self, column: u16, row: u16) -> bool {
        (self.left..=self.right).contains(&column) && (self.top..=self.bottom).contains(&row)
    }

    fn on_border(&self, column: u16, row: u16) -> bool {
        self.holds(column, row)
            && (column == self.left
                || column == self.right
                || row == self.top
                || row == self.bottom)
    }
}

/// Which way a divider runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DividerAxis {
    /// A vertical line between panes side by side; dragging moves it left
    /// and right.
    Vertical,
    /// A horizontal line between stacked panes; dragging moves it up and down.
    Horizontal,
}

/// The seam where two panes meet, found from their frames. The daemon turns a
/// drag that starts on either pane's border cell at the seam into a resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Divider {
    pub axis: DividerAxis,
    /// The first column (vertical) or row (horizontal) past the seam: where
    /// the second pane's frame begins.
    pub at: u16,
    /// The seam's extent along its own direction, the cells the two frames
    /// share, corners included, ends inclusive. Always at least three cells.
    pub from: u16,
    pub to: u16,
}

impl Divider {
    /// The cell a press at `along` (the pointer's row for a vertical seam,
    /// its column for a horizontal one) hands the daemon: a border cell of
    /// the pane before the seam, clear of the corners.
    pub(super) fn grab(&self, along: u16) -> (u16, u16) {
        let along = along.clamp(self.from + 1, self.to - 1);
        match self.axis {
            DividerAxis::Vertical => (self.at - 1, along),
            DividerAxis::Horizontal => (along, self.at - 1),
        }
    }
}

/// The pane frames of one frame; empty when it is not split.
#[derive(Debug, Default)]
pub(super) struct PaneFrames {
    panes: Vec<PaneFrame>,
}

impl PaneFrames {
    /// Find the panes in the first `rows` rows of `frame`.
    pub(super) fn find(frame: &FrameData, rows: u16) -> Self {
        let width = frame.width;
        let rows = rows.min(frame.height);
        let len = usize::from(width) * usize::from(rows);
        let mut arms: Vec<Option<Arms>> = frame
            .cells
            .iter()
            .take(len)
            .map(|cell| {
                let mut chars = cell.symbol.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                box_drawing::arms(c)
            })
            .collect();
        // A frame short of `width * rows` cells is malformed; treat the
        // missing ones as blank rather than index past them.
        arms.resize(len, None);
        let grid = Grid { width, rows, arms };

        let mut boxes = grid.boxes();
        boxes.sort_by_key(|b| std::cmp::Reverse(b.area()));
        let mut outer: Vec<PaneFrame> = Vec::new();
        for candidate in boxes {
            if !outer.iter().any(|kept| kept.contains(&candidate)) {
                outer.push(candidate);
            }
        }
        if !tiles(&outer, width, rows) {
            return Self::default();
        }
        Self { panes: outer }
    }

    /// Panes known up front, for tests elsewhere that need a split.
    #[cfg(test)]
    pub(super) fn from_panes(panes: Vec<PaneFrame>) -> Self {
        Self { panes }
    }

    pub(super) fn panes(&self) -> &[PaneFrame] {
        &self.panes
    }

    /// The box the panes tile, border cells included; `None` when not split.
    pub(super) fn bounds(&self) -> Option<PaneFrame> {
        let first = self.panes.first()?;
        Some(self.panes.iter().fold(*first, |all, pane| PaneFrame {
            left: all.left.min(pane.left),
            top: all.top.min(pane.top),
            right: all.right.max(pane.right),
            bottom: all.bottom.max(pane.bottom),
        }))
    }

    /// Every seam between two panes. The panes tile, so frames that meet
    /// share an edge exactly, with no cell between.
    pub(super) fn dividers(&self) -> Vec<Divider> {
        let mut seams = Vec::new();
        for (i, a) in self.panes.iter().enumerate() {
            for b in &self.panes[i + 1..] {
                for (first, second) in [(a, b), (b, a)] {
                    if first.right + 1 == second.left {
                        let (from, to) = (a.top.max(b.top), a.bottom.min(b.bottom));
                        if from + 1 < to {
                            seams.push(Divider {
                                axis: DividerAxis::Vertical,
                                at: second.left,
                                from,
                                to,
                            });
                        }
                    } else if first.bottom + 1 == second.top {
                        let (from, to) = (a.left.max(b.left), a.right.min(b.right));
                        if from + 1 < to {
                            seams.push(Divider {
                                axis: DividerAxis::Horizontal,
                                at: second.top,
                                from,
                                to,
                            });
                        }
                    }
                }
            }
        }
        seams
    }

    /// Whether the cell is part of a pane's border box.
    pub(super) fn is_border(&self, column: u16, row: u16) -> bool {
        self.panes.iter().any(|pane| pane.on_border(column, row))
    }

    /// How many seams lie between each pane and the split's left edge, along
    /// the longest run of side-by-side panes leading to it; in `panes()`
    /// order. A pane on the left edge is 0.
    pub(super) fn depths(&self) -> Vec<u16> {
        let mut order: Vec<usize> = (0..self.panes.len()).collect();
        order.sort_by_key(|&index| self.panes[index].left);
        let mut depths = vec![0u16; self.panes.len()];
        for &index in &order {
            let pane = &self.panes[index];
            // A left neighbor starts further left, so its depth is settled.
            depths[index] = self
                .panes
                .iter()
                .zip(&depths)
                .filter(|(other, _)| beside(other, pane))
                .map(|(_, depth)| depth + 1)
                .max()
                .unwrap_or(0);
        }
        depths
    }

    /// The deepest of [`Self::depths`]: how many seams a row crosses at most.
    pub(super) fn max_depth(&self) -> u16 {
        self.depths().into_iter().max().unwrap_or(0)
    }
}

/// Whether `left` meets `right` along a seam, side by side: `right` starts
/// the column after `left` ends and they share a stretch of rows, as a
/// [`Divider`] needs.
pub(super) fn beside(left: &PaneFrame, right: &PaneFrame) -> bool {
    left.right + 1 == right.left && left.top.max(right.top) + 1 < left.bottom.min(right.bottom)
}

/// Whether `panes` are a split filling the frame down to its right and bottom
/// edges: at least two, disjoint, and covering their bounding box exactly.
fn tiles(panes: &[PaneFrame], width: u16, rows: u16) -> bool {
    if panes.len() < 2 || width == 0 || rows == 0 {
        return false;
    }
    let right = panes.iter().map(|p| p.right).max().unwrap_or(0);
    let bottom = panes.iter().map(|p| p.bottom).max().unwrap_or(0);
    if right != width - 1 || bottom != rows - 1 {
        return false;
    }
    let left = panes.iter().map(|p| p.left).min().unwrap_or(0);
    let top = panes.iter().map(|p| p.top).min().unwrap_or(0);
    let disjoint = panes
        .iter()
        .enumerate()
        .all(|(i, a)| panes[i + 1..].iter().all(|b| !a.overlaps(b)));
    let covered: u32 = panes.iter().map(PaneFrame::area).sum();
    let bounds = u32::from(right - left + 1) * u32::from(bottom - top + 1);
    disjoint && covered == bounds
}

struct Grid {
    width: u16,
    rows: u16,
    arms: Vec<Option<Arms>>,
}

impl Grid {
    fn at(&self, column: u16, row: u16) -> Option<Arms> {
        if column >= self.width || row >= self.rows {
            return None;
        }
        self.arms[usize::from(row) * usize::from(self.width) + usize::from(column)]
    }

    /// Every closed box, at least three cells each way, whose four edges join
    /// end to end.
    fn boxes(&self) -> Vec<PaneFrame> {
        let mut boxes = Vec::new();
        for top in 0..self.rows {
            for left in 0..self.width {
                if let Some(frame) = self.box_from(left, top) {
                    boxes.push(frame);
                }
            }
        }
        boxes
    }

    /// The box whose top-left corner is `left`, `top`, if one closes.
    fn box_from(&self, left: u16, top: u16) -> Option<PaneFrame> {
        let corner = self.at(left, top)?;
        if !(corner.right && corner.down) {
            return None;
        }
        let right = self.walk(
            left,
            |at| (self.at(at, top), self.at(at + 1, top)),
            |a| a.right,
            |a| a.left,
        );
        let bottom = self.walk(
            top,
            |at| (self.at(left, at), self.at(left, at + 1)),
            |a| a.down,
            |a| a.up,
        );
        if right < left + 2 || bottom < top + 2 {
            return None;
        }
        let top_right = self.at(right, top)?;
        let bottom_left = self.at(left, bottom)?;
        let bottom_right = self.at(right, bottom)?;
        let closes = top_right.down
            && bottom_left.right
            && bottom_right.up
            && bottom_right.left
            && (top..bottom).all(|row| {
                matches!((self.at(right, row), self.at(right, row + 1)), (Some(a), Some(b)) if a.down && b.up)
            })
            && (left..right).all(|column| {
                matches!((self.at(column, bottom), self.at(column + 1, bottom)), (Some(a), Some(b)) if a.right && b.left)
            });
        closes.then_some(PaneFrame {
            left,
            top,
            right,
            bottom,
        })
    }

    /// Follow a line from `start` while each cell reaches the next, and return
    /// the last position.
    fn walk(
        &self,
        start: u16,
        pair: impl Fn(u16) -> (Option<Arms>, Option<Arms>),
        toward: impl Fn(Arms) -> bool,
        back: impl Fn(Arms) -> bool,
    ) -> u16 {
        let mut at = start;
        while let (Some(here), Some(next)) = pair(at) {
            if !(toward(here) && back(next)) {
                break;
            }
            at += 1;
        }
        at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame holding `rows`, one string per row, space-padded to the widest.
    fn frame(rows: &[&str]) -> FrameData {
        let width = rows.iter().map(|r| r.chars().count()).max().unwrap_or(0);
        let mut frame = FrameData::blank(width as u16, rows.len() as u16, 0, 0);
        for (row, line) in rows.iter().enumerate() {
            for (column, c) in line.chars().enumerate() {
                frame.cells[row * width + column].symbol = c.to_string();
            }
        }
        frame
    }

    fn panes(rows: &[&str]) -> Vec<PaneFrame> {
        let frame = frame(rows);
        PaneFrames::find(&frame, frame.height).panes
    }

    fn pane(left: u16, top: u16, right: u16, bottom: u16) -> PaneFrame {
        PaneFrame {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn side_by_side_boxes_are_two_panes() {
        let found = panes(&["┌───┐┌───┐", "│   ││   │", "│   ││   │", "└───┘└───┘"]);
        assert_eq!(found.len(), 2);
        assert!(found.contains(&pane(0, 0, 4, 3)) && found.contains(&pane(5, 0, 9, 3)));
    }

    #[test]
    fn a_split_beside_a_sidebar_rule_still_tiles() {
        // The sidebar's `│` and the tab row sit outside the boxes.
        let found = panes(&[
            "x │         ",
            "x │┌──┐┌───┐",
            "x ││  ││   │",
            "x ││  ││   │",
            "x ││  │└───┘",
            "x ││  │┌───┐",
            "x ││  ││   │",
            "x ││  ││   │",
            "x │└──┘└───┘",
        ]);
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn rounded_boxes_are_panes_too() {
        assert_eq!(
            panes(&["╭──╮╭──╮", "│  ││  │", "│  ││  │", "╰──╯╰──╯"]).len(),
            2
        );
    }

    #[test]
    fn a_box_inside_a_pane_is_not_a_pane() {
        let found = panes(&[
            "┌──────┐┌───┐",
            "│┌──┐  ││   │",
            "││  │  ││   │",
            "│└──┘  ││   │",
            "└──────┘└───┘",
        ]);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn a_lone_box_is_not_a_split() {
        assert!(panes(&["┌──┐", "│  │", "│  │", "└──┘"]).is_empty());
    }

    #[test]
    fn a_box_floating_in_unframed_text_is_left_alone() {
        assert!(panes(&["abcdefgh", "a┌──┐ gh", "a│  │ gh", "a└──┘ gh", "abcdefgh"]).is_empty());
    }

    #[test]
    fn boxes_that_leave_a_gap_are_not_a_split() {
        // A popup ate the second pane's right edge, so the boxes no longer
        // tile; the daemon's own glyphs stay rather than half a layout.
        assert!(panes(&["┌───┐┌────", "│   ││    ", "│   ││    ", "└───┘└────"]).is_empty());
    }

    #[test]
    fn bounds_span_every_pane() {
        let frame = frame(&["┌───┐┌───┐", "│   ││   │", "│   ││   │", "└───┘└───┘"]);
        let found = PaneFrames::find(&frame, frame.height);
        assert_eq!(found.bounds(), Some(pane(0, 0, 9, 3)));
        assert_eq!(PaneFrames::default().bounds(), None);
    }

    #[test]
    fn side_by_side_panes_share_a_vertical_divider() {
        let frame = frame(&["┌───┐┌───┐", "│   ││   │", "│   ││   │", "└───┘└───┘"]);
        let seams = PaneFrames::find(&frame, frame.height).dividers();
        assert_eq!(
            seams,
            vec![Divider {
                axis: DividerAxis::Vertical,
                at: 5,
                from: 0,
                to: 3
            }]
        );
        // A press anywhere along it lands on the left pane's border, off
        // the corners.
        assert_eq!(seams[0].grab(0), (4, 1));
        assert_eq!(seams[0].grab(2), (4, 2));
        assert_eq!(seams[0].grab(9), (4, 2));
    }

    #[test]
    fn stacked_panes_share_a_horizontal_divider() {
        let frame = frame(&["┌────┐", "│    │", "└────┘", "┌────┐", "│    │", "└────┘"]);
        let seams = PaneFrames::find(&frame, frame.height).dividers();
        assert_eq!(
            seams,
            vec![Divider {
                axis: DividerAxis::Horizontal,
                at: 3,
                from: 0,
                to: 5
            }]
        );
        assert_eq!(seams[0].grab(3), (3, 2));
    }

    #[test]
    fn a_tee_split_has_a_seam_per_meeting_edge() {
        let seams = PaneFrames::find(
            &frame(&[
                "┌──┐┌───┐",
                "│  ││   │",
                "│  ││   │",
                "│  │└───┘",
                "│  │┌───┐",
                "│  ││   │",
                "│  ││   │",
                "└──┘└───┘",
            ]),
            8,
        )
        .dividers();
        let vertical = seams
            .iter()
            .filter(|d| d.axis == DividerAxis::Vertical)
            .count();
        let horizontal = seams
            .iter()
            .filter(|d| d.axis == DividerAxis::Horizontal)
            .count();
        // The left pane meets both right panes; the right panes meet each other.
        assert_eq!((vertical, horizontal), (2, 1));
    }

    #[test]
    fn border_cells_are_the_box_edge_only() {
        let frame = frame(&["┌───┐┌───┐", "│   ││   │", "│   ││   │", "└───┘└───┘"]);
        let found = PaneFrames::find(&frame, frame.height);
        assert!(found.is_border(0, 1) && found.is_border(4, 3) && found.is_border(5, 0));
        assert!(!found.is_border(2, 1));
    }

    #[test]
    fn depth_counts_the_seams_to_the_left_edge() {
        let frame = |left, top, right, bottom| PaneFrame {
            left,
            top,
            right,
            bottom,
        };
        // [A | B] over C, with D down the whole right side.
        let panes = PaneFrames {
            panes: vec![
                frame(0, 0, 9, 9),
                frame(10, 0, 19, 9),
                frame(0, 10, 19, 19),
                frame(20, 0, 29, 19),
            ],
        };
        assert_eq!(panes.depths(), vec![0, 1, 0, 2]);
        assert_eq!(panes.max_depth(), 2);
    }
}
