//! The native command palette (⌘⇧P).
//!
//! The palette is a keyboard-first overlay. The view hands it the live command
//! list, it filters that list against the typed query, and the view dispatches
//! whichever command is chosen. Nothing here touches GPUI or the daemon, so the
//! ranking is unit-testable on its own.

/// Rows the panel will draw at once; the list scrolls past this.
const MAX_RESULTS: usize = 40;

/// Something the palette can ask the view to do.
///
/// Commands carry their arguments because the view rebuilds the entry list
/// from live state (sessions, workspaces, tabs) every time it opens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    NewWindow,
    NewTab,
    SplitVertical,
    SplitHorizontal,
    CloseTab,
    CloseWindow,
    CloseAllWindows,
    ToggleSidebar,
    IncreaseFontSize,
    DecreaseFontSize,
    ResetFontSize,
    OpenAppearances,
    OpenAbout,
    CopySelection,
    PasteClipboard,
    SelectAllGrid,
    OpenWorkspacePicker,
    OpenSettings,
    OpenProject(String),
    NewProjectWorktree(String),
    Quit,
    FocusTab(u16),
    SwitchSession(String),
    NewSession,
    FocusWorkspace(String),
    FocusAgent(String),
}

/// One palette row: a command plus the text that is searched and drawn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub command: Command,
    pub title: String,
    /// Heading the row is grouped under.
    pub group: &'static str,
    /// Shortcut hint drawn on the right, e.g. `⌘T`.
    pub shortcut: Option<&'static str>,
}

/// Query and caret state for the overlay.
#[derive(Debug, Default)]
pub struct Palette {
    open: bool,
    query: String,
    /// Caret position, as a character index into `query`.
    cursor: usize,
    /// Whether the whole query is selected (⌘A); the next edit replaces it.
    select_all: bool,
    selected: usize,
}

impl Palette {
    /// Whether the overlay should paint.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The text typed so far.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The caret position, as a character index into [`Palette::query`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether the query is selected, so the field paints it highlighted.
    pub fn is_select_all(&self) -> bool {
        self.select_all
    }

    /// Index of the highlighted row, before clamping to the filtered list.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Show the palette with an empty query and the first row highlighted.
    pub fn open(&mut self) {
        self.open = true;
        self.clear();
    }

    /// Hide the palette and forget the query.
    pub fn close(&mut self) {
        self.open = false;
        self.clear();
    }

    /// Clear the query, caret, and selection.
    pub fn clear(&mut self) {
        self.query.clear();
        self.cursor = 0;
        self.select_all = false;
        self.selected = 0;
    }

    /// Select the whole query; the next edit replaces it.
    pub fn select_all(&mut self) {
        self.select_all = true;
        self.cursor = self.query.chars().count();
    }

    /// Insert typed text at the caret, re-pointing the highlight at the best
    /// match. Replaces the query first when it is selected.
    pub fn insert(&mut self, text: &str) {
        if self.select_all {
            self.query.clear();
            self.cursor = 0;
            self.select_all = false;
        }
        let at = self.byte_at(self.cursor);
        self.query.insert_str(at, text);
        self.cursor += text.chars().count();
        self.selected = 0;
    }

    /// Drop the character before the caret.
    pub fn backspace(&mut self) {
        if self.select_all {
            self.query.clear();
            self.cursor = 0;
            self.select_all = false;
            self.selected = 0;
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let end = self.byte_at(self.cursor);
        let start = self.byte_at(self.cursor - 1);
        self.query.replace_range(start..end, "");
        self.cursor -= 1;
        self.selected = 0;
    }

    /// Drop the character under the caret.
    pub fn delete(&mut self) {
        if self.select_all {
            self.query.clear();
            self.cursor = 0;
            self.select_all = false;
            self.selected = 0;
            return;
        }
        let count = self.query.chars().count();
        if self.cursor >= count {
            return;
        }
        let start = self.byte_at(self.cursor);
        let end = self.byte_at(self.cursor + 1);
        self.query.replace_range(start..end, "");
        self.selected = 0;
    }

    /// Move the caret by `delta` characters, collapsing any selection.
    pub fn move_cursor(&mut self, delta: isize) {
        self.clear_selection();
        let last = self.query.chars().count();
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, last as isize) as usize;
    }

    /// Move the caret to the start, collapsing any selection.
    pub fn cursor_home(&mut self) {
        self.clear_selection();
        self.cursor = 0;
    }

    /// Move the caret to the end, collapsing any selection.
    pub fn cursor_end(&mut self) {
        self.clear_selection();
        self.cursor = self.query.chars().count();
    }

    /// Move the highlight by `delta`, clamped to `len` rows.
    pub fn move_selection(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        let last = len - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, last as isize) as usize;
    }

    /// Clear a select-all without touching the text.
    fn clear_selection(&mut self) {
        self.select_all = false;
    }

    /// The byte offset of character `index`, clamped to the string's end.
    fn byte_at(&self, index: usize) -> usize {
        self.query
            .char_indices()
            .nth(index)
            .map_or(self.query.len(), |(byte, _)| byte)
    }

    /// Commands matching the query, best first.
    pub fn matches<'a>(&self, entries: &'a [Entry]) -> Vec<&'a Entry> {
        let mut scored: Vec<(i32, usize, &Entry)> = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                score(&self.query, &entry.title).map(|score| (score, index, entry))
            })
            .collect();
        // Highest score first; ties keep the list's own order.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.truncate(MAX_RESULTS);
        scored.into_iter().map(|(_, _, entry)| entry).collect()
    }

    /// The row the highlight points at, clamped to the filtered list.
    pub fn highlighted<'a>(&self, matches: &[&'a Entry]) -> Option<&'a Entry> {
        matches
            .get(self.selected.min(matches.len().saturating_sub(1)))
            .copied()
    }
}

/// The score of `query` against `title`, or `None` when `query` is not a
/// subsequence of it. Adjacent matches and matches at word starts score
/// highest, so `nt` finds "New Tab" ahead of an incidental `n…t` elsewhere.
fn score(query: &str, title: &str) -> Option<i32> {
    let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return Some(0);
    }
    let hay: Vec<char> = title.chars().flat_map(char::to_lowercase).collect();
    let mut next = 0usize;
    let mut total = 0i32;
    let mut previous: Option<usize> = None;
    for (index, ch) in hay.iter().enumerate() {
        if next == needle.len() {
            break;
        }
        if *ch != needle[next] {
            continue;
        }
        let mut points = 1;
        if previous == Some(index.wrapping_sub(1)) && index > 0 {
            points += 4;
        }
        if index == 0 || is_boundary(hay[index - 1]) {
            points += 3;
        }
        total += points;
        previous = Some(index);
        next += 1;
    }
    (next == needle.len()).then_some(total)
}

/// Whether `ch` separates words, so a match right after it is a word start.
fn is_boundary(ch: char) -> bool {
    ch == ' ' || ch == '-' || ch == '_' || ch == '/' || ch == '.'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str) -> Entry {
        Entry {
            command: Command::NewTab,
            title: title.to_string(),
            group: "Test",
            shortcut: None,
        }
    }

    fn titles<'a>(matches: &[&'a Entry]) -> Vec<&'a str> {
        matches.iter().map(|entry| entry.title.as_str()).collect()
    }

    #[test]
    fn empty_query_lists_every_entry() {
        let entries = vec![entry("New Tab"), entry("Quit")];
        let palette = Palette::default();
        assert_eq!(titles(&palette.matches(&entries)), ["New Tab", "Quit"]);
    }

    #[test]
    fn query_keeps_only_subsequences() {
        let entries = vec![entry("New Tab"), entry("Open Appearances"), entry("Quit")];
        let mut palette = Palette::default();
        palette.insert("nt");
        assert_eq!(titles(&palette.matches(&entries)), ["New Tab"]);
    }

    #[test]
    fn contiguous_and_prefix_matches_rank_first() {
        let entries = vec![entry("Set Local"), entry("Select All"), entry("Renew")];
        let mut palette = Palette::default();
        palette.insert("sel");
        // "Select All" matches s-e-l contiguously; "Set Local" skips a word.
        assert_eq!(titles(&palette.matches(&entries))[0], "Select All");
    }

    #[test]
    fn prefix_beats_an_incidental_run() {
        let entries = vec![entry("Renew"), entry("New Tab")];
        let mut palette = Palette::default();
        palette.insert("new");
        assert_eq!(titles(&palette.matches(&entries)), ["New Tab", "Renew"]);
    }

    #[test]
    fn typing_resets_and_backspace_pops() {
        let mut palette = Palette::default();
        palette.insert("new");
        palette.move_selection(1, 3);
        assert_eq!(palette.selected(), 1);
        palette.insert("x");
        assert_eq!(palette.selected(), 0);
        assert_eq!(palette.query(), "newx");
        palette.backspace();
        assert_eq!(palette.query(), "new");
    }

    #[test]
    fn selection_clamps_to_the_list() {
        let mut palette = Palette::default();
        palette.move_selection(5, 3);
        assert_eq!(palette.selected(), 2);
        palette.move_selection(-9, 3);
        assert_eq!(palette.selected(), 0);
        palette.move_selection(1, 0);
        assert_eq!(palette.selected(), 0);
    }

    #[test]
    fn highlighted_is_clamped_to_matches() {
        let entries = vec![entry("New Tab"), entry("Quit")];
        let mut palette = Palette::default();
        palette.insert("q");
        let matches = palette.matches(&entries);
        assert_eq!(
            palette.highlighted(&matches).map(|e| e.title.as_str()),
            Some("Quit")
        );
        palette.move_selection(9, matches.len());
        assert_eq!(
            palette.highlighted(&matches).map(|e| e.title.as_str()),
            Some("Quit")
        );
    }

    #[test]
    fn edits_land_at_the_caret() {
        let mut palette = Palette::default();
        palette.insert("nt");
        palette.move_cursor(-1);
        palette.insert("a");
        assert_eq!(palette.query(), "nat");
        assert_eq!(palette.cursor(), 2);
        palette.cursor_home();
        palette.delete();
        assert_eq!(palette.query(), "at");
        palette.cursor_end();
        palette.backspace();
        assert_eq!(palette.query(), "a");
    }

    #[test]
    fn select_all_is_replaced_by_the_next_edit() {
        let mut palette = Palette::default();
        palette.insert("new");
        palette.select_all();
        assert!(palette.is_select_all());
        palette.insert("q");
        assert!(!palette.is_select_all());
        assert_eq!(palette.query(), "q");
        palette.select_all();
        palette.backspace();
        assert_eq!(palette.query(), "");
    }

    #[test]
    fn clear_drops_the_whole_query() {
        let mut palette = Palette::default();
        palette.insert("new");
        palette.move_selection(2, 5);
        palette.clear();
        assert_eq!(palette.query(), "");
        assert_eq!(palette.cursor(), 0);
        assert_eq!(palette.selected(), 0);
        assert!(!palette.is_select_all());
    }

    #[test]
    fn caret_steps_over_multibyte_characters() {
        let mut palette = Palette::default();
        palette.insert("aé");
        assert_eq!(palette.cursor(), 2);
        palette.move_cursor(-1);
        palette.insert("x");
        assert_eq!(palette.query(), "axé");
        palette.backspace();
        assert_eq!(palette.query(), "aé");
    }
}
