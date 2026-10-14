//! The Diff page's pure rules: turning a [`FileDiff`] into display rows,
//! moving a cursor over them, anchoring notes, and filtering the
//! file list.
//!
//! Nothing here touches GPUI or the daemon, so the layout and navigation
//! rules stay unit-testable. `diff.rs` owns the state and paints these rows.
//! A diff is flattened once into *source rows* (hunk headers and lines, in
//! patch order); the cursor and every note anchor address those, so they
//! survive a switch between Split and Stack, which only changes how the same
//! source rows are laid out as *display rows*.

use std::collections::HashMap;

use pilus_client::{
    DiffFile, DiffLayer, DiffLineKind, DiffNote, FileDiff, NoteKind, NoteRange, NoteSide,
    NoteState, StageAction,
};

use super::highlight::{self, Span};

/// Fewest columns each side of a Split row needs to stay readable.
const SPLIT_MIN_COLUMNS: f32 = 56.0;

/// What a source row is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SrcKind {
    Hunk,
    Context,
    Addition,
    Deletion,
}

/// One row of the patch in order: a hunk header or a line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Src {
    pub(super) kind: SrcKind,
    pub(super) old_line: Option<u32>,
    pub(super) new_line: Option<u32>,
    /// The line's text, or a hunk's header.
    pub(super) text: String,
    /// Syntax-colored runs of `text`; empty for headers and plain files.
    pub(super) spans: Vec<Span>,
}

impl Src {
    fn number(&self, side: NoteSide) -> Option<u32> {
        match side {
            NoteSide::Old => self.old_line,
            NoteSide::New => self.new_line,
        }
    }
}

/// A diff flattened to source rows.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Flat {
    pub(super) src: Vec<Src>,
    /// Source index of each hunk header.
    pub(super) hunk_starts: Vec<usize>,
}

pub(super) fn flatten(diff: &FileDiff) -> Flat {
    let mut flat = Flat::default();
    let mut colored = highlight::hunks(&diff.file.path, &diff.hunks).into_iter();
    for hunk in &diff.hunks {
        flat.hunk_starts.push(flat.src.len());
        flat.src.push(Src {
            kind: SrcKind::Hunk,
            old_line: None,
            new_line: None,
            text: hunk.header.clone(),
            spans: Vec::new(),
        });
        let mut spans = colored.next().unwrap_or_default().into_iter();
        for line in &hunk.lines {
            flat.src.push(Src {
                kind: match line.kind {
                    DiffLineKind::Context => SrcKind::Context,
                    DiffLineKind::Addition => SrcKind::Addition,
                    DiffLineKind::Deletion => SrcKind::Deletion,
                },
                old_line: line.old_line,
                new_line: line.new_line,
                // Tabs would collapse to one cell in a proportional layout.
                text: line.text.replace('\t', "    "),
                spans: spans.next().unwrap_or_default(),
            });
        }
    }
    flat
}

/// How a file's rows are laid out.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum LayoutMode {
    #[default]
    Auto,
    Split,
    Stack,
}

impl LayoutMode {
    pub(super) const ALL: [LayoutMode; 3] =
        [LayoutMode::Auto, LayoutMode::Split, LayoutMode::Stack];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Split => "Split",
            Self::Stack => "Stack",
        }
    }

    pub(super) fn next(self) -> Self {
        match self {
            Self::Auto => Self::Split,
            Self::Split => Self::Stack,
            Self::Stack => Self::Auto,
        }
    }

    /// Whether to draw two sides, given the room for code and a column's
    /// width. Auto splits only when both sides stay readable.
    pub(super) fn splits(self, code_width: f32, column: f32) -> bool {
        match self {
            Self::Split => true,
            Self::Stack => false,
            Self::Auto => code_width / 2.0 / column.max(1.0) >= SPLIT_MIN_COLUMNS,
        }
    }
}

/// One drawn row. Indices point into [`Flat::src`] or the notes slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Row {
    Hunk(usize),
    Stack(usize),
    Split {
        left: Option<usize>,
        right: Option<usize>,
    },
    Note(usize),
    Composer,
}

impl Row {
    /// Whether the cursor can rest on this row.
    fn is_source(self) -> bool {
        !matches!(self, Self::Note(_) | Self::Composer)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Display {
    pub(super) rows: Vec<Row>,
    /// The display row each source row is drawn in.
    pub(super) src_row: Vec<usize>,
}

/// Lay `flat` out, with each note drawn under the last line it covers and
/// the note composer under `composer_after`. A note whose lines are not in
/// the loaded diff (its code moved) trails the file.
pub(super) fn build(
    flat: &Flat,
    split: bool,
    notes: &[&DiffNote],
    composer_after: Option<usize>,
) -> Display {
    let mut after: HashMap<usize, Vec<Row>> = HashMap::new();
    for (index, note) in notes.iter().enumerate() {
        let at = flat
            .src
            .iter()
            .position(|src| {
                src.kind != SrcKind::Hunk && src.number(note.side) == Some(note.end_line)
            })
            .unwrap_or(usize::MAX);
        after.entry(at).or_default().push(Row::Note(index));
    }
    if let Some(at) = composer_after {
        after.entry(at).or_default().push(Row::Composer);
    }

    let mut display = Display {
        rows: Vec::with_capacity(flat.src.len()),
        src_row: vec![0; flat.src.len()],
    };
    let mut emit = |display: &mut Display, row: Row, covers: &[usize]| {
        for &src in covers {
            display.src_row[src] = display.rows.len();
        }
        display.rows.push(row);
        for &src in covers {
            if let Some(extra) = after.remove(&src) {
                display.rows.extend(extra);
            }
        }
    };

    let mut deletions: Vec<usize> = Vec::new();
    let mut additions: Vec<usize> = Vec::new();
    for (index, src) in flat.src.iter().enumerate() {
        if !split {
            let row = if src.kind == SrcKind::Hunk {
                Row::Hunk(index)
            } else {
                Row::Stack(index)
            };
            emit(&mut display, row, &[index]);
            continue;
        }
        match src.kind {
            SrcKind::Deletion => deletions.push(index),
            SrcKind::Addition => additions.push(index),
            kind => {
                flush_pairs(&mut display, &mut emit, &mut deletions, &mut additions);
                if kind == SrcKind::Hunk {
                    emit(&mut display, Row::Hunk(index), &[index]);
                } else {
                    let row = Row::Split {
                        left: Some(index),
                        right: Some(index),
                    };
                    emit(&mut display, row, &[index]);
                }
            }
        }
    }
    flush_pairs(&mut display, &mut emit, &mut deletions, &mut additions);
    if let Some(rest) = after.remove(&usize::MAX) {
        display.rows.extend(rest);
    }
    display
}

/// Emit a run of deletions and additions side by side, first to first.
fn flush_pairs(
    display: &mut Display,
    emit: &mut impl FnMut(&mut Display, Row, &[usize]),
    deletions: &mut Vec<usize>,
    additions: &mut Vec<usize>,
) {
    for at in 0..deletions.len().max(additions.len()) {
        let (left, right) = (deletions.get(at).copied(), additions.get(at).copied());
        let covers: Vec<usize> = left.into_iter().chain(right).collect();
        emit(display, Row::Split { left, right }, &covers);
    }
    deletions.clear();
    additions.clear();
}

/// Where the cursor rests: a source row and the side a note would anchor to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Cursor {
    pub(super) src: usize,
    pub(super) side: NoteSide,
}

impl Cursor {
    pub(super) fn start() -> Self {
        Self {
            src: 0,
            side: NoteSide::New,
        }
    }
}

/// The side a source row naturally belongs to.
pub(super) fn natural_side(src: &Src) -> NoteSide {
    if src.kind == SrcKind::Deletion {
        NoteSide::Old
    } else {
        NoteSide::New
    }
}

/// The source row drawn at `row` for `side`, falling back to the other side
/// when this one is empty.
fn src_at(row: Row, side: NoteSide) -> Option<usize> {
    match row {
        Row::Hunk(src) | Row::Stack(src) => Some(src),
        Row::Split { left, right } => match side {
            NoteSide::Old => left.or(right),
            NoteSide::New => right.or(left),
        },
        Row::Note(_) | Row::Composer => None,
    }
}

/// Move `delta` display rows, skipping note cards and the composer.
pub(super) fn step(display: &Display, cursor: Cursor, delta: isize) -> Cursor {
    let Some(&here) = display.src_row.get(cursor.src) else {
        return cursor;
    };
    let mut row = here as isize;
    let mut remaining = delta.unsigned_abs();
    let direction = delta.signum();
    let mut landed = here;
    while remaining > 0 {
        row += direction;
        let Some(candidate) = usize::try_from(row)
            .ok()
            .and_then(|at| display.rows.get(at))
        else {
            break;
        };
        if candidate.is_source() {
            landed = row as usize;
            remaining -= 1;
        }
    }
    Cursor {
        src: src_at(display.rows[landed], cursor.side).unwrap_or(cursor.src),
        side: cursor.side,
    }
}

/// Jump to the first or last source row.
pub(super) fn edge(display: &Display, cursor: Cursor, last: bool) -> Cursor {
    let rows = display
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.is_source());
    let mut rows = rows;
    let pick = if last { rows.next_back() } else { rows.next() };
    pick.and_then(|(_, row)| src_at(*row, cursor.side))
        .map_or(cursor, |src| Cursor { src, ..cursor })
}

/// The hunk header before or after the cursor, if there is one.
pub(super) fn hunk_jump(flat: &Flat, from: usize, forward: bool) -> Option<usize> {
    if forward {
        flat.hunk_starts.iter().copied().find(|&start| start > from)
    } else {
        flat.hunk_starts
            .iter()
            .rev()
            .copied()
            .find(|&start| start < from)
    }
}

/// Select `side` in a Split row. Stack rows keep their natural side.
pub(super) fn with_side(display: &Display, cursor: Cursor, side: NoteSide) -> Cursor {
    let Some(&row) = display
        .src_row
        .get(cursor.src)
        .and_then(|at| display.rows.get(*at))
    else {
        return cursor;
    };
    match row {
        Row::Split { .. } => Cursor {
            src: src_at(row, side).unwrap_or(cursor.src),
            side,
        },
        _ => cursor,
    }
}

/// The note range covering the source rows between `anchor` and `cursor`,
/// on one side. Rows with no number on that side (an addition, when the
/// side is old) do not extend the range.
pub(super) fn note_range(flat: &Flat, anchor: usize, cursor: Cursor) -> Option<NoteRange> {
    let (low, high) = (anchor.min(cursor.src), anchor.max(cursor.src));
    let side = flat
        .src
        .get(cursor.src)
        .filter(|src| src.kind != SrcKind::Hunk)
        .map(|src| match cursor.side {
            // A lone deletion or addition has only one side to offer.
            NoteSide::Old if src.old_line.is_none() => NoteSide::New,
            NoteSide::New if src.new_line.is_none() => NoteSide::Old,
            side => side,
        })?;
    let mut numbers = flat.src[low..=high]
        .iter()
        .filter(|src| src.kind != SrcKind::Hunk)
        .filter_map(|src| src.number(side));
    let first = numbers.next()?;
    let (start, end) = numbers.fold((first, first), |(low, high), n| (low.min(n), high.max(n)));
    Some(NoteRange { side, start, end })
}

/// The entry after (or before) `from` in sorted `positions`, wrapping at the
/// ends. Note jumps use it to cycle through the notes of a file.
pub(super) fn next_match(positions: &[usize], from: usize, forward: bool) -> Option<usize> {
    if forward {
        positions
            .iter()
            .copied()
            .find(|&at| at > from)
            .or_else(|| positions.first().copied())
    } else {
        positions
            .iter()
            .rev()
            .copied()
            .find(|&at| at < from)
            .or_else(|| positions.last().copied())
    }
}

/// Shortest the file list shrinks to when its divider is dragged.
pub(super) const MIN_LIST_HEIGHT: f32 = 64.0;
/// Most of the window the list may take, so the diff keeps room to read.
const MAX_LIST_SHARE: f32 = 0.6;

/// `wanted` kept between the minimum and the window's share.
pub(super) fn clamp_list_height(wanted: f32, window_h: f32) -> f32 {
    wanted.clamp(
        MIN_LIST_HEIGHT,
        (window_h * MAX_LIST_SHARE).max(MIN_LIST_HEIGHT),
    )
}

/// Which files the list shows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum FileFilter {
    #[default]
    All,
    /// Not yet marked viewed.
    Unviewed,
    /// Changed since last viewed.
    Changed,
    /// Carrying unresolved notes.
    Notes,
}

impl FileFilter {
    pub(super) const ALL: [FileFilter; 4] = [
        FileFilter::All,
        FileFilter::Unviewed,
        FileFilter::Changed,
        FileFilter::Notes,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Unviewed => "Unviewed",
            Self::Changed => "Changed",
            Self::Notes => "Notes",
        }
    }

    pub(super) fn next(self) -> Self {
        match self {
            Self::All => Self::Unviewed,
            Self::Unviewed => Self::Changed,
            Self::Changed => Self::Notes,
            Self::Notes => Self::All,
        }
    }

    pub(super) fn admits(self, file: &DiffFile, viewed: bool) -> bool {
        match self {
            Self::All => true,
            Self::Unviewed => !viewed,
            Self::Changed => file.modified_since_review,
            Self::Notes => file.notes > 0,
        }
    }
}

/// Whether `file`'s path answers a search: every word of `query` appears in
/// it, in any order and ignoring case. A renamed file answers to both its
/// names, so a search for where it came from still finds it. An empty query
/// admits everything.
pub(super) fn matches_query(file: &DiffFile, query: &str) -> bool {
    let path = file.path.to_lowercase();
    let old = file.renamed().then(|| file.old_path.to_lowercase());
    query.split_whitespace().all(|word| {
        let word = word.to_lowercase();
        path.contains(&word) || old.as_ref().is_some_and(|old| old.contains(&word))
    })
}

/// The files `filter` and `query` admit, in list order: grouped by layer,
/// conflicts first, each group keeping the daemon's order.
pub(super) fn visible_files(
    files: &[DiffFile],
    filter: FileFilter,
    query: &str,
    viewed: impl Fn(&DiffFile) -> bool,
) -> Vec<usize> {
    let mut order = Vec::new();
    for layer in DiffLayer::ALL {
        order.extend(
            files
                .iter()
                .enumerate()
                .filter(|(_, file)| {
                    file.layer == layer
                        && filter.admits(file, viewed(file))
                        && matches_query(file, query)
                })
                .map(|(index, _)| index),
        );
    }
    order
}

/// The layer `path` sits in once `action` has run, for a selection to follow
/// the file it was on. Staging lands in the index; unstaging leaves the file
/// edited or, when it was new, untracked.
pub(super) fn layer_after(
    files: &[DiffFile],
    path: &str,
    action: StageAction,
) -> Option<DiffLayer> {
    let candidates: &[DiffLayer] = match action {
        StageAction::Stage => &[DiffLayer::Staged],
        StageAction::Unstage => &[DiffLayer::Worktree, DiffLayer::Untracked],
    };
    candidates
        .iter()
        .copied()
        .find(|layer| files.iter().any(|f| f.layer == *layer && f.path == path))
}

/// Notes still pointing at live code and awaiting a decision.
pub(super) fn is_open(note: &DiffNote) -> bool {
    note.state == NoteState::Open
}

/// The kind after `kind` in picker order, wrapping.
pub(super) fn next_kind(kind: NoteKind) -> NoteKind {
    let at = NoteKind::ALL.iter().position(|k| *k == kind).unwrap_or(0);
    NoteKind::ALL[(at + 1) % NoteKind::ALL.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilus_client::{DiffHunk, DiffLayer, DiffLine};

    fn line(kind: DiffLineKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_line: old,
            new_line: new,
            text: text.to_string(),
        }
    }

    fn file() -> DiffFile {
        DiffFile {
            path: "a.rs".into(),
            old_path: "a.rs".into(),
            layer: DiffLayer::Worktree,
            status: "M".into(),
            additions: None,
            deletions: None,
            binary: false,
            notes: 0,
            viewed: false,
            modified_since_review: false,
            fingerprint: String::new(),
        }
    }

    /// One hunk: context, two deletions, one addition, context; then a
    /// second hunk with a lone context line.
    fn diff() -> FileDiff {
        use DiffLineKind::{Addition, Context, Deletion};
        FileDiff {
            file: file(),
            truncated: false,
            omitted_lines: 0,
            hunks: vec![
                DiffHunk {
                    header: "@@ -1,4 +1,3 @@".into(),
                    lines: vec![
                        line(Context, Some(1), Some(1), "keep"),
                        line(Deletion, Some(2), None, "old one"),
                        line(Deletion, Some(3), None, "old two"),
                        line(Addition, None, Some(2), "new one"),
                        line(Context, Some(4), Some(3), "tail\tTab"),
                    ],
                },
                DiffHunk {
                    header: "@@ -20 +19 @@".into(),
                    lines: vec![line(Context, Some(20), Some(19), "far")],
                },
            ],
        }
    }

    fn note(side: NoteSide, start: u32, end: u32) -> DiffNote {
        DiffNote {
            id: "n".into(),
            path: "a.rs".into(),
            layer: DiffLayer::Worktree,
            side,
            start_line: start,
            end_line: end,
            kind: NoteKind::Question,
            state: NoteState::Open,
            body: "b".into(),
            deliveries: 0,
        }
    }

    #[test]
    fn flatten_orders_hunks_and_lines_and_expands_tabs() {
        let flat = flatten(&diff());
        assert_eq!(flat.hunk_starts, vec![0, 6]);
        assert_eq!(flat.src.len(), 8);
        assert_eq!(flat.src[5].text, "tail    Tab");
    }

    #[test]
    fn stack_draws_one_row_per_source_row() {
        let flat = flatten(&diff());
        let display = build(&flat, false, &[], None);
        assert_eq!(display.rows.len(), flat.src.len());
        assert_eq!(display.rows[0], Row::Hunk(0));
        assert_eq!(display.rows[2], Row::Stack(2));
    }

    #[test]
    fn split_pairs_deletions_with_additions_first_to_first() {
        let flat = flatten(&diff());
        let display = build(&flat, true, &[], None);
        assert_eq!(
            display.rows[2],
            Row::Split {
                left: Some(2),
                right: Some(4)
            }
        );
        assert_eq!(
            display.rows[3],
            Row::Split {
                left: Some(3),
                right: None
            }
        );
        // Two deletions and one addition fold into two rows.
        assert_eq!(display.rows.len(), flat.src.len() - 1);
        assert_eq!(display.src_row[4], display.src_row[2]);
    }

    #[test]
    fn notes_sit_under_their_last_line_and_detached_ones_trail() {
        let flat = flatten(&diff());
        let anchored = note(NoteSide::Old, 2, 3);
        let detached = note(NoteSide::New, 99, 99);
        let display = build(&flat, false, &[&anchored, &detached], Some(1));
        let at = |row: Row| display.rows.iter().position(|r| *r == row).expect("row");
        assert_eq!(at(Row::Note(0)), at(Row::Stack(3)) + 1);
        assert_eq!(at(Row::Composer), at(Row::Stack(1)) + 1);
        assert_eq!(*display.rows.last().expect("rows"), Row::Note(1));
        // Source rows still map to their own display row after insertions.
        assert_eq!(display.rows[display.src_row[6]], Row::Hunk(6));
    }

    #[test]
    fn cursor_steps_over_note_cards_and_clamps() {
        let flat = flatten(&diff());
        let n = note(NoteSide::New, 1, 1);
        let display = build(&flat, false, &[&n], None);
        let on = |src| Cursor {
            src,
            side: NoteSide::New,
        };
        // Source 1 is the context line the note hangs under.
        assert_eq!(step(&display, on(1), 1).src, 2);
        assert_eq!(step(&display, on(2), -1).src, 1);
        assert_eq!(step(&display, on(0), -1).src, 0);
        assert_eq!(step(&display, on(7), 1).src, 7);
        assert_eq!(edge(&display, on(3), true).src, 7);
        assert_eq!(edge(&display, on(3), false).src, 0);
    }

    #[test]
    fn split_cursor_keeps_its_side_across_rows() {
        let flat = flatten(&diff());
        let display = build(&flat, true, &[], None);
        let left = with_side(
            &display,
            Cursor {
                src: 2,
                side: NoteSide::New,
            },
            NoteSide::Old,
        );
        assert_eq!(
            left,
            Cursor {
                src: 2,
                side: NoteSide::Old
            }
        );
        // The second row has no addition, so the new side falls back.
        assert_eq!(
            step(
                &display,
                Cursor {
                    src: 4,
                    side: NoteSide::New
                },
                1
            )
            .src,
            3
        );
        assert_eq!(
            with_side(
                &display,
                Cursor {
                    src: 2,
                    side: NoteSide::Old
                },
                NoteSide::New
            )
            .src,
            4
        );
    }

    #[test]
    fn hunk_jumps_move_between_headers() {
        let flat = flatten(&diff());
        assert_eq!(hunk_jump(&flat, 0, true), Some(6));
        assert_eq!(hunk_jump(&flat, 3, false), Some(0));
        assert_eq!(hunk_jump(&flat, 6, true), None);
        assert_eq!(hunk_jump(&flat, 0, false), None);
    }

    #[test]
    fn note_range_stays_on_one_side() {
        let flat = flatten(&diff());
        let old = Cursor {
            src: 3,
            side: NoteSide::Old,
        };
        assert_eq!(
            note_range(&flat, 2, old),
            Some(NoteRange {
                side: NoteSide::Old,
                start: 2,
                end: 3
            })
        );
        // The addition has no old number, so the old range ignores it.
        let range = note_range(
            &flat,
            2,
            Cursor {
                src: 4,
                side: NoteSide::Old,
            },
        );
        assert_eq!(
            range,
            Some(NoteRange {
                side: NoteSide::New,
                start: 2,
                end: 2
            })
        );
        let new = Cursor {
            src: 5,
            side: NoteSide::New,
        };
        assert_eq!(
            note_range(&flat, 1, new),
            Some(NoteRange {
                side: NoteSide::New,
                start: 1,
                end: 3
            })
        );
        assert_eq!(
            note_range(
                &flat,
                0,
                Cursor {
                    src: 0,
                    side: NoteSide::New
                }
            ),
            None
        );
    }

    #[test]
    fn jumps_wrap_at_the_ends() {
        let notes = [2, 3];
        assert_eq!(next_match(&notes, 2, true), Some(3));
        assert_eq!(next_match(&notes, 3, true), Some(2));
        assert_eq!(next_match(&notes, 2, false), Some(3));
        assert_eq!(next_match(&notes, 3, false), Some(2));
        assert_eq!(next_match(&[], 0, true), None);
    }

    #[test]
    fn the_list_height_stays_between_the_minimum_and_the_window_share() {
        assert_eq!(clamp_list_height(10.0, 1000.0), MIN_LIST_HEIGHT);
        assert_eq!(clamp_list_height(300.0, 1000.0), 300.0);
        assert!((clamp_list_height(5000.0, 1000.0) - 600.0).abs() < 0.01);
        // A window too short for any share still keeps the minimum.
        assert_eq!(clamp_list_height(200.0, 50.0), MIN_LIST_HEIGHT);
    }

    #[test]
    fn layout_mode_auto_needs_room_for_both_sides() {
        assert!(LayoutMode::Auto.splits(900.0, 7.0));
        assert!(!LayoutMode::Auto.splits(500.0, 7.0));
        assert!(LayoutMode::Split.splits(100.0, 7.0));
        assert!(!LayoutMode::Stack.splits(2000.0, 7.0));
        assert_eq!(LayoutMode::Stack.next(), LayoutMode::Auto);
    }

    #[test]
    fn file_list_groups_by_layer_and_filters() {
        let mut staged = file();
        staged.layer = DiffLayer::Staged;
        let mut conflict = file();
        conflict.layer = DiffLayer::Conflict;
        let mut noted = file();
        noted.notes = 1;
        let files = vec![staged, noted, conflict];
        let all = visible_files(&files, FileFilter::All, "", |_| false);
        assert_eq!(all, vec![2, 0, 1]);
        assert_eq!(
            visible_files(&files, FileFilter::Notes, "", |_| false),
            vec![1]
        );
        assert!(visible_files(&files, FileFilter::Unviewed, "", |_| true).is_empty());
        assert_eq!(FileFilter::Notes.next(), FileFilter::All);
    }

    #[test]
    fn a_search_narrows_by_every_word_of_the_path() {
        let named = |path: &str| {
            let mut f = file();
            f.path = path.into();
            f.old_path = path.into();
            f
        };
        let files = vec![
            named("crates/pilus/src/gui/diff.rs"),
            named("crates/pilus/src/gui/Style.rs"),
            named("README.md"),
        ];
        let find = |query: &str| visible_files(&files, FileFilter::All, query, |_| false);
        assert_eq!(find(""), vec![0, 1, 2]);
        assert_eq!(find("   "), vec![0, 1, 2]);
        assert_eq!(find("STYLE"), vec![1]);
        assert_eq!(find("gui diff"), vec![0]);
        assert_eq!(find("diff gui"), vec![0]);
        assert!(find("missing").is_empty());
    }

    #[test]
    fn a_search_also_matches_where_a_renamed_file_came_from() {
        let mut moved = file();
        moved.path = "new_name.rs".into();
        moved.old_path = "old_name.rs".into();
        assert!(matches_query(&moved, "old_name"));
        assert!(matches_query(&moved, "new_name"));
        assert!(!matches_query(&moved, "other"));
    }

    #[test]
    fn a_selection_follows_a_file_to_its_new_layer() {
        let at = |layer: DiffLayer, path: &str| {
            let mut f = file();
            f.layer = layer;
            f.path = path.into();
            f
        };
        let staged = vec![at(DiffLayer::Staged, "a.rs")];
        assert_eq!(
            layer_after(&staged, "a.rs", StageAction::Stage),
            Some(DiffLayer::Staged)
        );
        let edited = vec![at(DiffLayer::Worktree, "a.rs")];
        assert_eq!(
            layer_after(&edited, "a.rs", StageAction::Unstage),
            Some(DiffLayer::Worktree)
        );
        let new = vec![at(DiffLayer::Untracked, "a.rs")];
        assert_eq!(
            layer_after(&new, "a.rs", StageAction::Unstage),
            Some(DiffLayer::Untracked)
        );
        // The file is gone from the list, as after a commit elsewhere.
        assert_eq!(layer_after(&new, "b.rs", StageAction::Stage), None);
    }

    #[test]
    fn a_search_composes_with_the_filter() {
        let mut noted = file();
        noted.path = "a.rs".into();
        noted.notes = 1;
        let mut plain = file();
        plain.path = "b.rs".into();
        let files = vec![noted, plain];
        assert_eq!(
            visible_files(&files, FileFilter::Notes, "rs", |_| false),
            vec![0]
        );
        assert!(visible_files(&files, FileFilter::Notes, "b.rs", |_| false).is_empty());
    }
}
