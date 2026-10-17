//! Text selection over a rendered markdown document.
//!
//! GPUI draws text but does not select it, so the document does its own. Every
//! run of selectable text is a *leaf* (a paragraph, a heading, a table cell, a
//! code block), numbered in reading order within its block. A selection is two
//! [`TextPoint`]s, a leaf and a byte offset into that leaf's plain text, so it
//! survives the list scrolling its rows out of existence, and the copied text
//! comes from the parsed [`Document`] rather than from whatever is on screen.
//!
//! Painting reports back: each text segment the renderer lays out registers
//! its [`TextLayout`] in [`Hits`] once it is measured, and a pointer position
//! is turned into a [`TextPoint`] against those. The model here is pure so it
//! can be tested without a window.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

use gpui::TextLayout;
use gpui::{Bounds, Hsla, Pixels, Point, TextRun};

/// One run of selectable text: the block it sits in, and its place in that
/// block's reading order.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct LeafId {
    pub(super) block: usize,
    pub(super) leaf: usize,
}

/// A place in the document's text: a byte offset into one leaf.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct TextPoint {
    pub(super) leaf: LeafId,
    pub(super) offset: usize,
}

/// An anchor and a moving end. Either may come first in the text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Selection {
    pub(super) anchor: TextPoint,
    pub(super) head: TextPoint,
}

impl Selection {
    pub(super) fn collapsed(at: TextPoint) -> Self {
        Self {
            anchor: at,
            head: at,
        }
    }

    pub(super) fn is_collapsed(&self) -> bool {
        self.anchor == self.head
    }

    /// The two ends in reading order.
    pub(super) fn ordered(&self) -> (TextPoint, TextPoint) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// The part of `leaf`, `len` bytes long, that the selection covers.
    pub(super) fn range_in(&self, leaf: LeafId, len: usize) -> Option<Range<usize>> {
        let (start, end) = self.ordered();
        if leaf < start.leaf || leaf > end.leaf {
            return None;
        }
        let from = if leaf == start.leaf { start.offset } else { 0 };
        let to = if leaf == end.leaf { end.offset } else { len };
        let (from, to) = (from.min(len), to.min(len));
        (from < to).then_some(from..to)
    }
}

/// The word around byte `offset` of `text`, or the whitespace run if it falls
/// on one. A double click selects this.
pub(super) fn word_range(text: &str, offset: usize) -> Range<usize> {
    let offset = offset.min(text.len());
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let class = |c: char| (c.is_whitespace(), word(c));
    let before = text[..offset].chars().next_back();
    let after = text[offset..].chars().next();
    let Some(probe) = after.or(before) else {
        return offset..offset;
    };
    let mut start = offset;
    for c in text[..offset].chars().rev() {
        if class(c) != class(probe) {
            break;
        }
        start -= c.len_utf8();
    }
    let mut end = offset;
    for c in text[offset..].chars() {
        if class(c) != class(probe) {
            break;
        }
        end += c.len_utf8();
    }
    start..end
}

/// Where selectable text reported itself during the last paint.
struct Hit {
    leaf: LeafId,
    /// Where this segment's text begins within the leaf's plain text: a
    /// paragraph with inline code is several segments of one leaf.
    start: usize,
    layout: TextLayout,
    bounds: Bounds<Pixels>,
}

/// The measured text segments on screen, rebuilt every frame.
#[derive(Clone, Default)]
pub(super) struct Hits(Rc<RefCell<Vec<Hit>>>);

impl Hits {
    /// Forget last frame's segments; the rows being drawn report again.
    pub(super) fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    /// Record a segment. Called while painting, after its text was measured.
    pub(super) fn report(&self, leaf: LeafId, start: usize, layout: TextLayout) {
        let bounds = layout.bounds();
        self.0.borrow_mut().push(Hit {
            leaf,
            start,
            layout,
            bounds,
        });
    }

    /// The text point under `position`; with `nearest`, the closest one when
    /// the pointer is over no text, which is what a drag past the text wants.
    pub(super) fn point_at(&self, position: Point<Pixels>, nearest: bool) -> Option<TextPoint> {
        let hits = self.0.borrow();
        let over = hits.iter().filter(|hit| hit.bounds.contains(&position));
        let mut fallback = None;
        for hit in over {
            match hit.layout.index_for_position(position) {
                Ok(index) => return Some(hit.point(index)),
                Err(index) => {
                    fallback.get_or_insert_with(|| hit.point(index));
                }
            }
        }
        if fallback.is_some() || !nearest {
            return fallback;
        }
        let closest = hits.iter().min_by(|a, b| {
            distance(&a.bounds, position).total_cmp(&distance(&b.bounds, position))
        })?;
        // Clamped into the segment, the lookup lands on its nearest edge.
        let inside = Point {
            x: position
                .x
                .clamp(closest.bounds.left(), closest.bounds.right()),
            y: position
                .y
                .clamp(closest.bounds.top(), closest.bounds.bottom()),
        };
        let index = match closest.layout.index_for_position(inside) {
            Ok(index) | Err(index) => index,
        };
        Some(closest.point(index))
    }
}

impl Hit {
    fn point(&self, index: usize) -> TextPoint {
        TextPoint {
            leaf: self.leaf,
            offset: self.start + index,
        }
    }
}

/// Squared distance from `point` to the rectangle, zero inside it.
fn distance(bounds: &Bounds<Pixels>, point: Point<Pixels>) -> f32 {
    let dx = (bounds.left() - point.x)
        .max(point.x - bounds.right())
        .max(Pixels::ZERO);
    let dy = (bounds.top() - point.y)
        .max(point.y - bounds.bottom())
        .max(Pixels::ZERO);
    f32::from(dx).powi(2) + f32::from(dy).powi(2)
}

/// What painting one block needs to take part in selection: which block it is,
/// the selection to tint, and where to report text.
pub(super) struct Leaves {
    block: usize,
    next: Cell<usize>,
    selection: Option<Selection>,
    tint: Hsla,
    hits: Hits,
}

impl Leaves {
    pub(super) fn new(block: usize, selection: Option<Selection>, tint: Hsla, hits: Hits) -> Self {
        Self {
            block,
            next: Cell::new(0),
            selection,
            tint,
            hits,
        }
    }

    /// For text nothing selects: numbers its leaves and reports them nowhere.
    pub(super) fn inert(block: usize) -> Self {
        Self::new(block, None, Hsla::default(), Hits::default())
    }

    /// The next leaf in the block, in the order the parse numbered them.
    pub(super) fn next_leaf(&self) -> LeafId {
        let leaf = self.next.get();
        self.next.set(leaf + 1);
        LeafId {
            block: self.block,
            leaf,
        }
    }

    pub(super) fn hits(&self) -> &Hits {
        &self.hits
    }

    /// `runs` for the segment of `leaf` that starts at byte `start` and is
    /// `len` bytes long, with the selected part tinted.
    pub(super) fn tinted(
        &self,
        leaf: LeafId,
        start: usize,
        len: usize,
        runs: Vec<TextRun>,
    ) -> Vec<TextRun> {
        let Some(range) = self
            .selection
            .and_then(|selection| selection.range_in(leaf, usize::MAX))
        else {
            return runs;
        };
        let from = range.start.saturating_sub(start).min(len);
        let to = range.end.saturating_sub(start).min(len);
        if from >= to {
            return runs;
        }
        highlight(runs, from..to, self.tint)
    }
}

/// `runs` with `range` given a background, splitting the runs that straddle
/// its ends.
pub(super) fn highlight(runs: Vec<TextRun>, range: Range<usize>, tint: Hsla) -> Vec<TextRun> {
    let mut out = Vec::with_capacity(runs.len() + 2);
    let mut at = 0;
    for run in runs {
        let end = at + run.len;
        let inside_start = range.start.clamp(at, end);
        let inside_end = range.end.clamp(at, end);
        if inside_start >= inside_end {
            out.push(run);
        } else {
            for (from, to, selected) in [
                (at, inside_start, false),
                (inside_start, inside_end, true),
                (inside_end, end, false),
            ] {
                if from < to {
                    let mut piece = run.clone();
                    piece.len = to - from;
                    if selected {
                        piece.background_color = Some(tint);
                    }
                    out.push(piece);
                }
            }
        }
        at = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use gpui::{black, font};

    use super::*;

    fn leaf(block: usize, leaf: usize) -> LeafId {
        LeafId { block, leaf }
    }

    fn at(block: usize, index: usize, offset: usize) -> TextPoint {
        TextPoint {
            leaf: leaf(block, index),
            offset,
        }
    }

    fn run(len: usize) -> TextRun {
        TextRun {
            len,
            font: font("Test"),
            color: black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }
    }

    #[test]
    fn a_selection_covers_the_tail_of_its_first_leaf_and_the_head_of_its_last() {
        let selection = Selection {
            anchor: at(0, 0, 3),
            head: at(1, 1, 2),
        };
        assert_eq!(selection.range_in(leaf(0, 0), 10), Some(3..10));
        assert_eq!(selection.range_in(leaf(0, 1), 10), Some(0..10));
        assert_eq!(selection.range_in(leaf(1, 0), 10), Some(0..10));
        assert_eq!(selection.range_in(leaf(1, 1), 10), Some(0..2));
        assert_eq!(selection.range_in(leaf(1, 2), 10), None);
    }

    #[test]
    fn dragging_backwards_selects_the_same_text() {
        let forward = Selection {
            anchor: at(0, 0, 1),
            head: at(0, 2, 4),
        };
        let backward = Selection {
            anchor: forward.head,
            head: forward.anchor,
        };
        for index in 0..3 {
            assert_eq!(
                forward.range_in(leaf(0, index), 8),
                backward.range_in(leaf(0, index), 8)
            );
        }
    }

    #[test]
    fn a_collapsed_selection_covers_nothing() {
        let selection = Selection::collapsed(at(0, 0, 4));
        assert!(selection.is_collapsed());
        assert_eq!(selection.range_in(leaf(0, 0), 10), None);
    }

    #[test]
    fn an_offset_past_the_text_is_clamped() {
        let selection = Selection {
            anchor: at(0, 0, 2),
            head: at(0, 0, 99),
        };
        assert_eq!(selection.range_in(leaf(0, 0), 5), Some(2..5));
    }

    #[test]
    fn highlight_splits_the_runs_it_cuts_through() {
        let tint = black();
        let runs = highlight(vec![run(4), run(4)], 2..6, tint);
        let shape: Vec<(usize, bool)> = runs
            .iter()
            .map(|run| (run.len, run.background_color.is_some()))
            .collect();
        assert_eq!(shape, vec![(2, false), (2, true), (2, true), (2, false)]);
        assert_eq!(runs.iter().map(|run| run.len).sum::<usize>(), 8);
    }

    #[test]
    fn highlight_leaves_runs_outside_the_range_alone() {
        let runs = highlight(vec![run(3), run(3)], 3..6, black());
        let shape: Vec<(usize, bool)> = runs
            .iter()
            .map(|run| (run.len, run.background_color.is_some()))
            .collect();
        assert_eq!(shape, vec![(3, false), (3, true)]);
    }

    #[test]
    fn a_segment_tints_only_its_own_part_of_the_leaf() {
        let leaves = Leaves::new(
            0,
            Some(Selection {
                anchor: at(0, 0, 3),
                head: at(0, 0, 8),
            }),
            black(),
            Hits::default(),
        );
        // A segment covering bytes 5..10 of the leaf: bytes 5..8 are selected.
        let runs = leaves.tinted(leaf(0, 0), 5, 5, vec![run(5)]);
        let shape: Vec<(usize, bool)> = runs
            .iter()
            .map(|run| (run.len, run.background_color.is_some()))
            .collect();
        assert_eq!(shape, vec![(3, true), (2, false)]);
        // Another leaf is untouched.
        let other = leaves.tinted(leaf(0, 1), 0, 5, vec![run(5)]);
        assert!(other.iter().all(|run| run.background_color.is_none()));
    }

    #[test]
    fn a_word_is_the_run_of_letters_around_the_offset() {
        let text = "read the diff_view, now";
        assert_eq!(word_range(text, 6), 5..8);
        // On the edge between a word and a space, the character after wins.
        assert_eq!(word_range(text, 8), 8..9);
        assert_eq!(word_range(text, 12), 9..18);
        assert_eq!(word_range(text, 0), 0..4);
        // The end of the text belongs to the last word.
        assert_eq!(word_range(text, text.len()), 20..23);
        assert_eq!(word_range("", 0), 0..0);
        assert_eq!(word_range("naïve café", 4), 0..6);
    }

    #[test]
    fn leaves_are_numbered_in_the_order_they_are_asked_for() {
        let leaves = Leaves::new(4, None, black(), Hits::default());
        assert_eq!(leaves.next_leaf(), leaf(4, 0));
        assert_eq!(leaves.next_leaf(), leaf(4, 1));
    }
}
