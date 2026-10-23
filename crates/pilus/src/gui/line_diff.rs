//! Which lines of an edited file differ from Git's copy of it.
//!
//! The Files tab's editor marks its gutter the way an IDE's quick diff does:
//! green beside lines that are new or changed since the index, red where lines
//! were taken out. The index's copy is read on a worker thread; this module
//! compares it with the editor's buffer each time the text changes, on the GUI
//! thread, as the highlighter does.
//!
//! Typing changes one spot, so the shared head and tail are trimmed first and
//! only the middle is traced, with Myers' algorithm. A middle that needs more
//! than [`MAX_EDITS`] steps is marked changed as one block: past that the
//! trace costs more than the gutter is worth.
//!
//! The Changes tab already holds the patch it shows, so its rendered markdown
//! reads the same marks from the patch's hunks ([`from_hunks`]) instead.

use std::ops::Range;

use pilus_client::{DiffHunk, DiffLineKind};

/// The most edit steps traced line by line before the middle is marked
/// changed whole. The trace keeps about `MAX_EDITS²` integers.
const MAX_EDITS: usize = 1000;

/// How the editor's text differs from the base it is compared with. Line
/// numbers count from zero over the editor's text, split on `\n`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct LineChanges {
    /// Lines that are new or changed, as sorted, disjoint ranges.
    pub(super) added: Vec<Range<usize>>,
    /// Lines that removed ones sat just above, sorted. A removal at the end
    /// names the line one past the last.
    pub(super) removed: Vec<usize>,
    /// Lines added and removed, counted as `git diff --numstat` does: a
    /// changed line is one of each.
    pub(super) insertions: usize,
    pub(super) deletions: usize,
}

/// Compare `new` (the editor's text) against `old` (the base).
pub(super) fn diff(old: &str, new: &str) -> LineChanges {
    let old: Vec<&str> = old.split('\n').collect();
    let new: Vec<&str> = new.split('\n').collect();
    let mut changes = LineChanges::default();
    for (old_lines, new_lines) in hunks(&old, &new) {
        changes.insertions += new_lines.len();
        changes.deletions += old_lines.len();
        if !new_lines.is_empty() {
            changes.added.push(new_lines.clone());
        }
        // A block that lost more lines than it gained shows the loss after
        // what replaced it.
        if old_lines.len() > new_lines.len() {
            changes.removed.push(new_lines.end);
        }
    }
    changes
}

/// The marks a patch's hunks describe, for the Changes tab's rendered
/// markdown: the same shape [`diff`] gives, read from the patch the tab
/// already holds instead of from both texts, so the marks match it exactly.
pub(super) fn from_hunks(hunks: &[DiffHunk]) -> LineChanges {
    let mut changes = LineChanges::default();
    for hunk in hunks {
        // Every line before the first one on the new side is a removal, so
        // the cursor starts just above that line.
        let first_new = hunk
            .lines
            .iter()
            .find_map(|line| line.new_line)
            .map_or(0, |line| line.saturating_sub(1));
        let mut run = Run {
            // The new side's lines seen so far, which is also the 0-based
            // index of the line after them.
            cursor: first_new as usize,
            ..Run::default()
        };
        for line in &hunk.lines {
            match line.kind {
                DiffLineKind::Context => {
                    run.close(&mut changes);
                    if let Some(new) = line.new_line {
                        run.cursor = new as usize;
                    }
                }
                DiffLineKind::Addition => {
                    let new = line.new_line.map_or(run.cursor + 1, |new| new as usize);
                    run.first_added.get_or_insert(new.saturating_sub(1));
                    run.added += 1;
                    run.cursor = new;
                }
                DiffLineKind::Deletion => run.removed += 1,
            }
        }
        run.close(&mut changes);
    }
    changes
}

/// One run of added and removed lines between two kept ones, as
/// [`from_hunks`] reads it.
#[derive(Default)]
struct Run {
    cursor: usize,
    first_added: Option<usize>,
    added: usize,
    removed: usize,
}

impl Run {
    /// Record the run, as [`diff`] records a block, and start the next.
    fn close(&mut self, changes: &mut LineChanges) {
        changes.insertions += self.added;
        changes.deletions += self.removed;
        if let Some(first) = self.first_added.take() {
            changes.added.push(first..self.cursor);
        }
        if self.removed > self.added {
            changes.removed.push(self.cursor);
        }
        self.added = 0;
        self.removed = 0;
    }
}

/// The blocks that differ, as (old lines, new lines), in order.
fn hunks(old: &[&str], new: &[&str]) -> Vec<(Range<usize>, Range<usize>)> {
    let head = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_mid = &old[head..old.len() - tail];
    let new_mid = &new[head..new.len() - tail];
    if old_mid.is_empty() && new_mid.is_empty() {
        return Vec::new();
    }
    let (removed, inserted) = trace(old_mid, new_mid)
        .unwrap_or_else(|| (vec![true; old_mid.len()], vec![true; new_mid.len()]));
    // Walk both sides together: kept lines pair up in order, so each run of
    // removed and inserted lines between two kept ones is one block.
    let mut blocks = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < old_mid.len() || j < new_mid.len() {
        if i < old_mid.len() && j < new_mid.len() && !removed[i] && !inserted[j] {
            i += 1;
            j += 1;
            continue;
        }
        let (from_i, from_j) = (i, j);
        loop {
            let before = (i, j);
            while i < old_mid.len() && removed[i] {
                i += 1;
            }
            while j < new_mid.len() && inserted[j] {
                j += 1;
            }
            if (i, j) == before {
                break;
            }
        }
        if (i, j) == (from_i, from_j) {
            // Unreachable when the trace is consistent; stop rather than spin.
            break;
        }
        blocks.push((head + from_i..head + i, head + from_j..head + j));
    }
    blocks
}

/// Myers' shortest edit script from `old` to `new`, as which old lines are
/// removed and which new lines are inserted. `None` when it needs more than
/// [`MAX_EDITS`] steps.
fn trace(old: &[&str], new: &[&str]) -> Option<(Vec<bool>, Vec<bool>)> {
    let n = old.len() as isize;
    let m = new.len() as isize;
    let max = (n + m).min(MAX_EDITS as isize);
    // `v[k + offset]`: the furthest x reached on diagonal `k = x - y`, or -1
    // where no path reaches.
    let offset = max + 1;
    let mut v = vec![-1isize; (2 * offset + 1) as usize];
    // Round `d` keeps `v` as round `d - 1` left it, for `k` in `-d..=d`, at
    // `kept[d²..]`, so the path can be walked back.
    let mut kept: Vec<isize> = Vec::new();
    for d in 0..=max {
        kept.extend_from_slice(&v[(offset - d) as usize..=(offset + d) as usize]);
        let mut k = -d;
        while k <= d {
            let at = |k: isize| v[(offset + k) as usize];
            let Some(start) = step(d, k, n, m, at) else {
                v[(offset + k) as usize] = -1;
                k += 2;
                continue;
            };
            let (mut x, mut y) = (start.x, start.x - k);
            while x < n && y < m && old[x as usize] == new[y as usize] {
                x += 1;
                y += 1;
            }
            v[(offset + k) as usize] = x;
            if x == n && y == m {
                return Some(walk_back(&kept, d, n, m, old.len(), new.len()));
            }
            k += 2;
        }
    }
    None
}

/// Where a path on diagonal `k` starts in round `d`, before its run of equal
/// lines.
struct Start {
    x: isize,
    /// The diagonal it stepped from: `k + 1` for an inserted line, `k - 1`
    /// for a removed one.
    from: isize,
}

/// The step onto diagonal `k` in round `d`, from the furthest of its two
/// neighbours that stays inside the grid. `at` reads round `d - 1`.
fn step(d: isize, k: isize, n: isize, m: isize, at: impl Fn(isize) -> isize) -> Option<Start> {
    if d == 0 {
        return Some(Start { x: 0, from: 0 });
    }
    // Down: insert a line of `new`, keeping x.
    let down = (k < d).then(|| at(k + 1)).filter(|&x| x >= 0 && x - k <= m);
    // Right: remove a line of `old`.
    let right = (k > -d)
        .then(|| at(k - 1))
        .filter(|&x| x >= 0 && x < n)
        .map(|x| x + 1);
    match (down, right) {
        (Some(down), Some(right)) if down >= right => Some(Start {
            x: down,
            from: k + 1,
        }),
        (_, Some(right)) => Some(Start {
            x: right,
            from: k - 1,
        }),
        (Some(down), None) => Some(Start {
            x: down,
            from: k + 1,
        }),
        (None, None) => None,
    }
}

/// Follow the path that reached `(n, m)` in round `last` back to the start,
/// marking each line it removed or inserted.
fn walk_back(
    kept: &[isize],
    last: isize,
    n: isize,
    m: isize,
    old_len: usize,
    new_len: usize,
) -> (Vec<bool>, Vec<bool>) {
    let mut removed = vec![false; old_len];
    let mut inserted = vec![false; new_len];
    let (mut x, mut y) = (n, m);
    for d in (1..=last).rev() {
        let base = (d * d) as usize;
        let at = |k: isize| kept[base + (k + d) as usize];
        let k = x - y;
        let Some(start) = step(d, k, n, m, at) else {
            break;
        };
        let from_x = at(start.from);
        let from_y = from_x - start.from;
        if start.from == k + 1 {
            if let Some(line) = inserted.get_mut(from_y as usize) {
                *line = true;
            }
        } else if let Some(line) = removed.get_mut(from_x as usize) {
            *line = true;
        }
        x = from_x;
        y = from_y;
    }
    (removed, inserted)
}

#[cfg(test)]
mod tests {
    use pilus_client::DiffLine;

    use super::*;

    /// A one-hunk patch from `old` to `new` with every kept line as context,
    /// numbered as Git numbers them.
    fn patch(old: &str, new: &str) -> Vec<DiffHunk> {
        let old: Vec<&str> = old.split('\n').collect();
        let new: Vec<&str> = new.split('\n').collect();
        let mut lines = Vec::new();
        let (mut i, mut j) = (0, 0);
        let line = |kind, old_line: Option<usize>, new_line: Option<usize>| DiffLine {
            kind,
            old_line: old_line.map(|n| n as u32 + 1),
            new_line: new_line.map(|n| n as u32 + 1),
            text: String::new(),
        };
        for (gone, fresh) in hunks(&old, &new) {
            while i < gone.start {
                lines.push(line(DiffLineKind::Context, Some(i), Some(j)));
                i += 1;
                j += 1;
            }
            for n in gone {
                lines.push(line(DiffLineKind::Deletion, Some(n), None));
                i += 1;
            }
            for n in fresh {
                lines.push(line(DiffLineKind::Addition, None, Some(n)));
                j += 1;
            }
        }
        while i < old.len() {
            lines.push(line(DiffLineKind::Context, Some(i), Some(j)));
            i += 1;
            j += 1;
        }
        vec![DiffHunk {
            header: String::new(),
            lines,
        }]
    }

    #[test]
    fn a_patch_marks_the_lines_the_texts_do() {
        let cases = [
            ("a\nb\nc", "a\nb\nnew\nc"),
            ("a\nb\nc", "a\nc"),
            ("a\nb\nc", "b\nc"),
            ("a\nb\nc", "a\nb"),
            ("a\nx\ny\nz\nb", "a\nX\nb"),
            ("a\nb\nc\nd\ne\nf\ng", "a\nc\nX\nd\nf\nY\nZ\ng"),
        ];
        for (old, new) in cases {
            assert_eq!(
                from_hunks(&patch(old, new)),
                diff(old, new),
                "{old:?} -> {new:?}"
            );
        }
    }

    #[test]
    fn a_removal_that_opens_a_hunk_sits_above_its_first_new_line() {
        // `@@ -10,3 +10,2 @@`: line 10 removed, 11 and 12 kept as 10 and 11.
        let line = |kind, old_line, new_line| DiffLine {
            kind,
            old_line,
            new_line,
            text: String::new(),
        };
        let hunk = DiffHunk {
            header: "@@ -10,3 +10,2 @@".to_string(),
            lines: vec![
                line(DiffLineKind::Deletion, Some(10), None),
                line(DiffLineKind::Context, Some(11), Some(10)),
                line(DiffLineKind::Context, Some(12), Some(11)),
            ],
        };
        let changes = from_hunks(&[hunk]);
        assert_eq!(changes.removed, vec![9]);
        assert_eq!((changes.insertions, changes.deletions), (0, 1));
    }

    #[test]
    fn the_same_text_has_no_changes() {
        assert_eq!(diff("a\nb\n", "a\nb\n"), LineChanges::default());
    }

    #[test]
    fn an_inserted_line_is_added() {
        let changes = diff("a\nb\nc", "a\nb\nnew\nc");
        assert_eq!(changes.added, vec![2..3]);
        assert!(changes.removed.is_empty());
        assert_eq!((changes.insertions, changes.deletions), (1, 0));
    }

    #[test]
    fn a_removed_line_marks_the_line_below() {
        let changes = diff("a\nb\nc", "a\nc");
        assert!(changes.added.is_empty());
        assert_eq!(changes.removed, vec![1]);
        assert_eq!((changes.insertions, changes.deletions), (0, 1));
    }

    #[test]
    fn a_removal_at_the_end_names_the_line_past_the_last() {
        let changes = diff("a\nb\nc", "a\nb");
        assert_eq!(changes.removed, vec![2]);
    }

    #[test]
    fn a_changed_line_counts_once_each_way() {
        let changes = diff("a\nb\nc", "a\nB\nc");
        assert_eq!(changes.added, vec![1..2]);
        assert!(changes.removed.is_empty());
        assert_eq!((changes.insertions, changes.deletions), (1, 1));
    }

    #[test]
    fn a_block_that_shrank_shows_its_loss_after_the_new_lines() {
        let changes = diff("a\nx\ny\nz\nb", "a\nX\nb");
        assert_eq!(changes.added, vec![1..2]);
        assert_eq!(changes.removed, vec![2]);
    }

    #[test]
    fn changes_far_apart_stay_separate_blocks() {
        let old: Vec<String> = (0..200).map(|n| format!("line {n}")).collect();
        let mut new = old.clone();
        new[10] = "ten".to_string();
        new.remove(150);
        new.insert(180, "added".to_string());
        let changes = diff(&old.join("\n"), &new.join("\n"));
        assert_eq!(changes.added, vec![10..11, 180..181]);
        assert_eq!(changes.removed, vec![150]);
        assert_eq!((changes.insertions, changes.deletions), (2, 2));
    }

    #[test]
    fn a_new_file_against_an_empty_base_is_all_added() {
        let changes = diff("", "a\nb");
        assert_eq!(changes.added, vec![0..2]);
        assert_eq!(changes.deletions, 1, "the base's one empty line");
    }

    #[test]
    fn an_untraceable_middle_is_one_block() {
        let old: Vec<String> = (0..1500).map(|n| format!("old {n}")).collect();
        let new: Vec<String> = (0..1500).map(|n| format!("new {n}")).collect();
        let changes = diff(
            &format!("head\n{}\ntail", old.join("\n")),
            &format!("head\n{}\ntail", new.join("\n")),
        );
        assert_eq!(changes.added, vec![1..1501]);
        assert_eq!((changes.insertions, changes.deletions), (1500, 1500));
    }

    /// The longest common subsequence's length, by the textbook table.
    fn lcs(a: &[&str], b: &[&str]) -> usize {
        let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in 0..a.len() {
            for j in 0..b.len() {
                table[i + 1][j + 1] = if a[i] == b[j] {
                    table[i][j] + 1
                } else {
                    table[i][j + 1].max(table[i + 1][j])
                };
            }
        }
        table[a.len()][b.len()]
    }

    #[test]
    fn the_trace_is_shortest_and_keeps_only_equal_lines() {
        // A fixed LCG, so a failure replays.
        let mut seed: u64 = 0x5eed;
        let mut next = move |bound: u64| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % bound
        };
        let words = ["a", "b", "c", "d"];
        for _ in 0..2000 {
            let (old_len, new_len) = (next(9), next(9));
            let old: Vec<&str> = (0..old_len)
                .map(|_| words[next(words.len() as u64) as usize])
                .collect();
            let new: Vec<&str> = (0..new_len)
                .map(|_| words[next(words.len() as u64) as usize])
                .collect();
            let Some((removed, inserted)) = trace(&old, &new) else {
                panic!("small inputs always trace");
            };
            let kept_old: Vec<&str> = old
                .iter()
                .zip(&removed)
                .filter(|(_, gone)| !**gone)
                .map(|(line, _)| *line)
                .collect();
            let kept_new: Vec<&str> = new
                .iter()
                .zip(&inserted)
                .filter(|(_, fresh)| !**fresh)
                .map(|(line, _)| *line)
                .collect();
            assert_eq!(kept_old, kept_new, "{old:?} -> {new:?}");
            assert_eq!(kept_old.len(), lcs(&old, &new), "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn the_trace_matches_a_shortest_script() {
        // Interleaved keeps, removals and insertions inside one middle.
        let changes = diff("a\nb\nc\nd\ne\nf\ng", "a\nc\nX\nd\nf\nY\nZ\ng");
        assert_eq!((changes.insertions, changes.deletions), (3, 2));
        assert_eq!(changes.added, vec![2..3, 5..7]);
        assert_eq!(changes.removed, vec![1, 4]);
    }
}
