//! The native command palette (⌘⇧P).
//!
//! The palette is a keyboard-first overlay. The view hands it the live command
//! list, it filters that list against the typed query, and the view dispatches
//! whichever command is chosen. Nothing here touches GPUI or the daemon, so the
//! ranking is unit-testable on its own.

use super::text_input::TextField;

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
    OpenLocalTerminal,
    SplitVertical,
    SplitHorizontal,
    CloseTab,
    CloseWindow,
    CloseAllWindows,
    ToggleSidebar,
    ToggleRightSidebar,
    TogglePaneToolbar,
    ToggleEnergyMode,
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
    OpenHome,
    OpenSearch,
    OpenOverview,
    OpenStats,
    OpenProcess,
    OpenDatabase,
    OpenUiPlayground,
    Quit,
    FocusTab(u16),
    SwitchSession(String),
    NewSession,
    RenameSession,
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

/// Query and highlight state for the overlay.
#[derive(Debug, Default)]
pub struct Palette {
    open: bool,
    input: TextField,
    selected: usize,
    /// Query text the highlight last reset against, so caret-blink repaints
    /// don't re-home it.
    reset_query: String,
}

impl Palette {
    /// Whether the overlay should paint.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The text typed so far.
    pub fn query(&self) -> String {
        self.input.value()
    }

    /// The query buffer, for the window's input entity.
    pub fn input(&self) -> &TextField {
        &self.input
    }

    /// The query buffer, for the shot harness to type a query into.
    #[cfg(feature = "visual-test")]
    pub fn input_mut(&mut self) -> &mut TextField {
        &mut self.input
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

    /// Clear the query and re-point the highlight at the first row.
    pub fn clear(&mut self) {
        self.input.clear();
        self.selected = 0;
        self.reset_query.clear();
    }

    /// Re-point the highlight at the first row when the query changed since
    /// the last call.
    ///
    /// The field notifies on every repaint, including the caret blink, so an
    /// unconditional reset would yank the highlight back to the top while the
    /// user walks the list with the arrow keys.
    pub fn refresh_selection(&mut self) {
        if self.input.value() != self.reset_query {
            self.reset_query = self.input.value();
            self.selected = 0;
        }
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

    /// Commands matching the query, best first.
    pub fn matches<'a>(&self, entries: &'a [Entry]) -> Vec<&'a Entry> {
        let query = self.query();
        let mut scored: Vec<(i32, usize, &Entry)> = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                score(&query, &entry.title).map(|score| (score, index, entry))
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

    fn seeded(query: &str) -> Palette {
        let mut palette = Palette::default();
        palette.input.set_value(query.to_string());
        palette
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
        let palette = seeded("nt");
        assert_eq!(titles(&palette.matches(&entries)), ["New Tab"]);
    }

    #[test]
    fn contiguous_and_prefix_matches_rank_first() {
        let entries = vec![entry("Set Local"), entry("Select All"), entry("Renew")];
        let palette = seeded("sel");
        // "Select All" matches s-e-l contiguously; "Set Local" skips a word.
        assert_eq!(titles(&palette.matches(&entries))[0], "Select All");
    }

    #[test]
    fn prefix_beats_an_incidental_run() {
        let entries = vec![entry("Renew"), entry("New Tab")];
        let palette = seeded("new");
        assert_eq!(titles(&palette.matches(&entries)), ["New Tab", "Renew"]);
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
        let mut palette = seeded("q");
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
    fn clear_drops_the_whole_query() {
        let mut palette = seeded("new");
        palette.move_selection(2, 5);
        palette.clear();
        assert_eq!(palette.query(), "");
        assert_eq!(palette.selected(), 0);
    }

    #[test]
    fn refresh_selection_ignores_repaints_without_an_edit() {
        let mut palette = seeded("new");
        palette.refresh_selection();
        palette.move_selection(3, 5);
        assert_eq!(palette.selected(), 3);
        // A caret-blink repaint notifies the field without changing the query,
        // so the highlight must stay put.
        palette.refresh_selection();
        assert_eq!(palette.selected(), 3);
    }

    #[test]
    fn refresh_selection_rehomes_the_highlight_after_an_edit() {
        let mut palette = seeded("new");
        palette.refresh_selection();
        palette.move_selection(3, 5);
        palette.input.set_value("newt".to_string());
        palette.refresh_selection();
        assert_eq!(palette.selected(), 0);
    }
}
