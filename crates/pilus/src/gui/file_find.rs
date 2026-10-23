//! Find in the file the Files tab has open for editing (⌘F).
//!
//! The bar sits above the editor and owns one single-line field. Matches are
//! byte ranges into the editor's text, found again whenever the query or the
//! text changes (the field's `revision` says which) and painted under the text
//! by the editor itself; the current match is the editor's selection, so the
//! editor's caret-following scroll brings it into view. The search is smart-case:
//! a query without a capital letter ignores case.
//!
//! Matching is plain text and nothing here touches GPUI, so it is
//! unit-testable on its own; the bar and its keys are at the end.

use std::ops::Range;

use gpui::{Context, Div, Focusable as _, KeyDownEvent, Window, div, prelude::*, px};

use super::style;
use super::{PilusView, files_tab::EditMode};

/// Most matches kept, so a one-letter query over a large file stays cheap to
/// paint and to step through.
const MAX_MATCHES: usize = 5_000;

/// Longest selection that seeds the bar when it opens.
const MAX_SEED: usize = 200;

/// The find bar's state. The query itself lives in the bar's field.
#[derive(Default)]
pub(super) struct FileFind {
    open: bool,
    query: String,
    matches: Vec<Range<usize>>,
    /// The search stopped at [`MAX_MATCHES`].
    capped: bool,
    /// The match the editor has selected, as an index into `matches`.
    current: Option<usize>,
    /// The text revision and query `matches` were found for.
    seen: Option<(u64, String)>,
    /// The query changed: select its first match at or after the caret.
    jump: bool,
}

impl FileFind {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// Forget the query's matches, as a new file replaces the old one.
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    /// What the count beside the field says.
    fn count_label(&self) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        let total = self.matches.len();
        let more = if self.capped { "+" } else { "" };
        match (self.current, total) {
            (_, 0) => "No results".to_string(),
            (Some(at), _) => format!("{} of {total}{more}", at + 1),
            (None, _) => format!("{total}{more} results"),
        }
    }
}

/// Every match of `query` in `text`, as byte ranges, and whether the search
/// stopped at [`MAX_MATCHES`].
///
/// Case is folded for ASCII only, which keeps every offset valid in the
/// original text; a query with a capital letter is matched exactly.
fn find_all(text: &str, query: &str) -> (Vec<Range<usize>>, bool) {
    if query.is_empty() {
        return (Vec::new(), false);
    }
    let exact = query.chars().any(char::is_uppercase);
    let (haystack, needle);
    let (haystack, needle) = if exact {
        (text, query)
    } else {
        haystack = text.to_ascii_lowercase();
        needle = query.to_ascii_lowercase();
        (haystack.as_str(), needle.as_str())
    };
    let mut found: Vec<Range<usize>> = haystack
        .match_indices(needle)
        .take(MAX_MATCHES + 1)
        .map(|(at, hit)| at..at + hit.len())
        .collect();
    let capped = found.len() > MAX_MATCHES;
    found.truncate(MAX_MATCHES);
    (found, capped)
}

/// The first match that starts at or after `caret`, wrapping to the first.
fn first_from(matches: &[Range<usize>], caret: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let at = matches.partition_point(|hit| hit.start < caret);
    Some(if at == matches.len() { 0 } else { at })
}

/// The match after (or before) `current`, wrapping at either end. With none
/// current, forward starts at `caret` and backward at the match before it.
fn step(
    matches: &[Range<usize>],
    current: Option<usize>,
    caret: usize,
    forward: bool,
) -> Option<usize> {
    let total = matches.len();
    if total == 0 {
        return None;
    }
    Some(match (current, forward) {
        (Some(at), true) => (at + 1) % total,
        (Some(at), false) => (at + total - 1) % total,
        (None, true) => first_from(matches, caret)?,
        (None, false) => {
            let before = matches.partition_point(|hit| hit.start < caret);
            before.checked_sub(1).unwrap_or(total - 1)
        }
    })
}

impl PilusView {
    /// Open the bar over the file being edited, switching a markdown preview to
    /// its source first. A selection that fits on a line becomes the query.
    pub(super) fn find_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.files_visible() || self.files_panel.doc_view().is_none() {
            return;
        }
        // Find in File from the terminal moves the keyboard to the file.
        self.right.focused = true;
        self.files_begin_edit(cx);
        if !self.md_editing() {
            return;
        }
        let seed = self
            .md_editor_input
            .read(cx)
            .selected_text()
            .filter(|text| !text.contains('\n') && text.len() <= MAX_SEED);
        self.file_find.open = true;
        self.find_input.update(cx, |input, cx| {
            if let Some(seed) = seed {
                input.set_value(seed, cx);
            }
            input.select_all(cx);
        });
        window.focus(&self.find_input.focus_handle(cx), cx);
        self.file_find.jump = true;
        self.find_refresh(cx);
        cx.notify();
    }

    /// Close the bar, drop the highlights and hand the keyboard to the editor.
    pub(super) fn find_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.file_find.open = false;
        self.file_find.matches.clear();
        self.file_find.current = None;
        self.file_find.seen = None;
        self.md_editor_input
            .update(cx, |input, cx| input.set_marks(Vec::new(), cx));
        window.focus(&self.md_editor_input.focus_handle(cx), cx);
        cx.notify();
    }

    /// Find the matches again if the query or the text changed since the last
    /// call, and select the first one after the caret when the query did.
    pub(super) fn find_refresh(&mut self, cx: &mut Context<Self>) {
        if !self.file_find.open {
            return;
        }
        let query = self.find_input.read(cx).value();
        let editor = self.md_editor_input.read(cx);
        let revision = editor.revision();
        if self.file_find.seen == Some((revision, query.clone())) {
            return;
        }
        if self.file_find.query != query {
            self.file_find.jump = true;
        }
        let caret = editor.caret_byte();
        let (matches, capped) = editor.with_text(|text| find_all(text, &query));
        let find = &mut self.file_find;
        find.seen = Some((revision, query.clone()));
        find.query = query;
        find.matches = matches;
        find.capped = capped;
        find.current = None;
        let target = std::mem::take(&mut find.jump)
            .then(|| first_from(&find.matches, caret))
            .flatten();
        let marks = find.matches.clone();
        self.md_editor_input
            .update(cx, |input, cx| input.set_marks(marks, cx));
        if let Some(at) = target {
            self.find_select(at, cx);
        }
    }

    /// Select match `at` in the editor.
    fn find_select(&mut self, at: usize, cx: &mut Context<Self>) {
        let Some(range) = self.file_find.matches.get(at).cloned() else {
            return;
        };
        self.file_find.current = Some(at);
        self.md_editor_input
            .update(cx, |input, cx| input.select_range(range, cx));
        // The selection is the editor's own text, so the revision is unchanged.
    }

    /// Go to the next match, or the previous one.
    pub(super) fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.find_refresh(cx);
        let caret = self.md_editor_input.read(cx).caret_byte();
        let find = &self.file_find;
        if let Some(at) = step(&find.matches, find.current, caret, forward) {
            self.find_select(at, cx);
            cx.notify();
        }
    }

    /// The bar's keys, which the field leaves to the window. True when one was
    /// the bar's.
    pub(super) fn find_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.file_find.open {
            return false;
        }
        let keystroke = &event.keystroke;
        let shift = keystroke.modifiers.shift;
        match keystroke.key.as_str() {
            "escape" => self.find_close(window, cx),
            "enter" if self.find_input.read(cx).is_focused(window) => {
                self.find_step(!shift, cx);
            }
            "g" if keystroke.modifiers.platform => self.find_step(!shift, cx),
            _ => return false,
        }
        true
    }

    /// The bar above the editor, when there is one to show.
    pub(super) fn find_bar(&self, cx: &mut Context<Self>) -> Option<Div> {
        let editing = self
            .files_panel
            .edit_mode()
            .is_some_and(|mode: EditMode| mode.edits());
        if !self.file_find.open || !editing {
            return None;
        }
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let height = style::scaled(&self.shared_settings, style::CONTROL_HEIGHT_SMALL);
        let focused = self.find_input.read(cx).was_focused();
        let field = style::search_field(
            "pilus-files-find",
            focused,
            theme,
            height,
            fonts.secondary,
            self.find_input.clone(),
        )
        .flex_1()
        .min_w(px(0.0))
        .text_size(px(fonts.body));
        let count = div()
            .flex_shrink_0()
            .px_1()
            .text_size(px(fonts.secondary))
            .text_color(
                if self.file_find.query.is_empty() || !self.file_find.matches.is_empty() {
                    theme.muted
                } else {
                    theme.alert
                },
            )
            .child(self.file_find.count_label());
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(theme.border)
                .child(field)
                .child(count)
                .child(
                    self.diff_icon_button(
                        "pilus-files-find-prev",
                        include_bytes!("../../assets/icons/arrow-up.svg"),
                        "Previous match (⇧⌘G)".to_string(),
                        false,
                        false,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.find_step(false, cx))),
                )
                .child(
                    self.diff_icon_button(
                        "pilus-files-find-next",
                        include_bytes!("../../assets/icons/arrow-down.svg"),
                        "Next match (⌘G)".to_string(),
                        false,
                        false,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.find_step(true, cx))),
                )
                .child(
                    self.diff_icon_button(
                        "pilus-files-find-close",
                        include_bytes!("../../assets/icons/x.svg"),
                        "Close (Esc)".to_string(),
                        false,
                        false,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.find_close(window, cx))),
                ),
        )
    }
}

#[cfg(feature = "visual-test")]
impl PilusView {
    /// The bar open on `query` over the file in the editor, as ⌘F leaves it.
    pub(super) fn find_show_sample(&mut self, query: &str, cx: &mut Context<Self>) {
        self.file_find.open = true;
        self.find_input
            .update(cx, |input, cx| input.set_value(query.to_string(), cx));
        self.file_find.jump = true;
        self.find_refresh(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(text: &str, query: &str) -> Vec<(usize, usize)> {
        find_all(text, query)
            .0
            .into_iter()
            .map(|hit| (hit.start, hit.end))
            .collect()
    }

    #[test]
    fn a_lowercase_query_ignores_case() {
        assert_eq!(spans("Foo foo FOO", "foo"), [(0, 3), (4, 7), (8, 11)]);
    }

    #[test]
    fn a_query_with_a_capital_is_exact() {
        assert_eq!(spans("Foo foo FOO", "Foo"), [(0, 3)]);
    }

    #[test]
    fn offsets_stay_valid_around_multibyte_text() {
        let text = "é😀 abc é😀 ABC";
        for hit in find_all(text, "abc").0 {
            assert_eq!(text[hit].to_ascii_lowercase(), "abc");
        }
        assert_eq!(find_all(text, "abc").0.len(), 2);
    }

    #[test]
    fn matches_do_not_overlap() {
        assert_eq!(spans("aaaa", "aa"), [(0, 2), (2, 4)]);
    }

    #[test]
    fn an_empty_query_finds_nothing() {
        assert_eq!(find_all("abc", ""), (Vec::new(), false));
    }

    #[test]
    fn a_flood_of_matches_is_cut_and_says_so() {
        let (found, capped) = find_all(&"a".repeat(MAX_MATCHES + 10), "a");
        assert_eq!(found.len(), MAX_MATCHES);
        assert!(capped);
    }

    #[test]
    fn the_first_match_is_the_one_at_or_after_the_caret() {
        let found = find_all("x ab ab ab", "ab").0;
        assert_eq!(first_from(&found, 0), Some(0));
        assert_eq!(first_from(&found, 5), Some(1));
        assert_eq!(first_from(&found, 7), Some(2));
        // Past the last match it wraps to the first.
        assert_eq!(first_from(&found, 10), Some(0));
        assert_eq!(first_from(&[], 0), None);
    }

    #[test]
    fn stepping_wraps_at_both_ends() {
        let found = find_all("ab ab ab", "ab").0;
        assert_eq!(step(&found, Some(2), 0, true), Some(0));
        assert_eq!(step(&found, Some(0), 0, false), Some(2));
        assert_eq!(step(&found, Some(0), 0, true), Some(1));
    }

    #[test]
    fn stepping_from_no_match_starts_at_the_caret() {
        let found = find_all("ab ab ab", "ab").0;
        assert_eq!(step(&found, None, 4, true), Some(2));
        assert_eq!(step(&found, None, 4, false), Some(1));
        assert_eq!(step(&found, None, 0, false), Some(2));
        assert_eq!(step(&[], None, 0, true), None);
    }

    #[test]
    fn the_count_says_where_you_are() {
        let mut find = FileFind::default();
        assert_eq!(find.count_label(), "");
        find.query = "ab".to_string();
        assert_eq!(find.count_label(), "No results");
        find.matches = find_all("ab ab", "ab").0;
        assert_eq!(find.count_label(), "2 results");
        find.current = Some(1);
        assert_eq!(find.count_label(), "2 of 2");
        find.capped = true;
        assert_eq!(find.count_label(), "2 of 2+");
    }
}
