//! Quick open (⌘P): jump to any file in the active workspace by a fuzzy part of
//! its path.
//!
//! The daemon's tree only carries the folders that are open, so the picker reads
//! its own listing of the workspace root (`pilus_client::list_files`: `git
//! ls-files` in a repository, `find` elsewhere) on a worker thread, locally or
//! over the machine's SSH profile. The listing is kept between openings and read
//! again each time the picker opens, so a first keystroke is never waiting on a
//! disk or a network. A pick opens the file as a click on its tree row would.
//!
//! Ranking is a subsequence match over the whole path with a bonus for the file
//! name, so `mod` finds `mod.rs` ahead of `src/gui/model_picker.rs`. It works on
//! bytes and folds ASCII case, so scoring fifty thousand paths allocates
//! nothing; the result is cached until the query or the listing changes.

use std::sync::mpsc::{Receiver, Sender};

use gpui::{AnyElement, Context, MouseButton, ScrollHandle, div, prelude::*, px};
use pilus_client::{FileList, list_files};

use super::palette::Palette;
use super::right_sidebar::SidebarTab;
use super::style::{self, FillExt as _, RadiusExt as _};
use super::{EndpointId, HEADER_HEIGHT, PilusView, icon};

/// Rows the picker lists at once.
const MAX_ROWS: usize = 50;
/// Files remembered as recently opened, for the empty query.
const MAX_RECENT: usize = 20;

/// What the picker knows about the workspace's files.
enum Listing {
    Empty,
    Ready(FileList),
    Failed(String),
}

/// One finished read of a root.
pub(super) struct Listed {
    endpoint: EndpointId,
    root: String,
    result: Result<FileList, String>,
}

/// The picker's state: the query and highlight (a [`Palette`]'s job too), the
/// listing, and the ranked rows.
pub(super) struct QuickOpen {
    query: Palette,
    listing: Listing,
    /// The machine and root `listing` was read for.
    listed: Option<(EndpointId, String)>,
    /// A read is on its way.
    loading: bool,
    /// Bumped when the listing or the recents change, so `ranked` is redone.
    version: u64,
    ranked: Vec<usize>,
    ranked_for: Option<(String, u64)>,
    scroll: ScrollHandle,
    /// Files opened here or in the tab, newest first, as absolute paths.
    recent: Vec<String>,
    tx: Sender<Listed>,
    rx: Receiver<Listed>,
}

impl QuickOpen {
    pub(super) fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            query: Palette::default(),
            listing: Listing::Empty,
            listed: None,
            loading: false,
            version: 0,
            ranked: Vec::new(),
            ranked_for: None,
            scroll: ScrollHandle::new(),
            recent: Vec::new(),
            tx,
            rx,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.query.is_open()
    }

    pub(super) fn close(&mut self) {
        self.query.close();
    }

    pub(super) fn input(&self) -> &super::text_input::TextField {
        self.query.input()
    }

    /// Re-point the highlight when the query changed, and rank again.
    pub(super) fn refresh(&mut self, root: Option<&str>) {
        self.query.refresh_selection();
        self.rank(root);
    }

    /// Remember that `path` (absolute) was opened.
    pub(super) fn note_opened(&mut self, path: &str) {
        self.recent.retain(|held| held != path);
        self.recent.insert(0, path.to_string());
        self.recent.truncate(MAX_RECENT);
        self.version += 1;
    }

    fn files(&self) -> &[String] {
        match &self.listing {
            Listing::Ready(list) => &list.paths,
            _ => &[],
        }
    }

    fn rank(&mut self, root: Option<&str>) {
        let query = self.query.query();
        if self.ranked_for == Some((query.clone(), self.version)) {
            return;
        }
        let recent: Vec<&str> = root
            .map(|root| {
                self.recent
                    .iter()
                    .filter_map(|path| relative(path, root))
                    .collect()
            })
            .unwrap_or_default();
        self.ranked = rank(&query, self.files(), &recent, MAX_ROWS);
        self.ranked_for = Some((query, self.version));
    }

    fn take(&mut self, listed: Listed) {
        self.loading = false;
        self.version += 1;
        match listed.result {
            Ok(list) => {
                self.listing = Listing::Ready(list);
                self.listed = Some((listed.endpoint, listed.root));
            }
            // A failed refresh keeps what the last good read showed.
            Err(error) if matches!(self.listing, Listing::Ready(_)) => {
                tracing::warn!("quick open: {error}");
            }
            Err(error) => {
                self.listing = Listing::Failed(error);
                self.listed = Some((listed.endpoint, listed.root));
            }
        }
    }
}

/// `path` below `root`, if it is.
fn relative<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    path.strip_prefix(root.trim_end_matches('/'))?
        .strip_prefix('/')
}

/// The part of `path` after its last slash.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Indices into `files` of the best `limit` matches for `query`.
///
/// An empty query lists `recent` (paths relative to the root) first, then the
/// listing's own order.
fn rank(query: &str, files: &[String], recent: &[&str], limit: usize) -> Vec<usize> {
    if query.trim().is_empty() {
        let mut rows: Vec<usize> = recent
            .iter()
            .filter_map(|path| files.binary_search_by(|file| file.as_str().cmp(path)).ok())
            .collect();
        for index in 0..files.len() {
            if rows.len() >= limit {
                break;
            }
            if !rows.contains(&index) {
                rows.push(index);
            }
        }
        rows.truncate(limit);
        return rows;
    }
    let needle: Vec<u8> = query
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .map(|byte| byte.to_ascii_lowercase())
        .collect();
    let mut scored: Vec<(i32, usize)> = files
        .iter()
        .enumerate()
        .filter_map(|(index, path)| score(&needle, path).map(|score| (score, index)))
        .collect();
    // Highest first; equal scores keep the listing's sorted order.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(limit);
    scored.into_iter().map(|(_, index)| index).collect()
}

/// How well `needle` (lowercase, no spaces) matches `path`, or `None` when it
/// is not a subsequence of it.
fn score(needle: &[u8], path: &str) -> Option<i32> {
    let whole = subsequence(needle, path.as_bytes())?;
    let name = file_name(path);
    let mut total = whole;
    if let Some(in_name) = subsequence(needle, name.as_bytes()) {
        total += in_name * 3;
        let lowered = name.to_ascii_lowercase();
        let needle = String::from_utf8_lossy(needle);
        if lowered.starts_with(needle.as_ref()) {
            total += 40;
        } else if lowered.contains(needle.as_ref()) {
            total += 20;
        }
    }
    // Between equals, the shorter path is the likelier target.
    Some(total - (path.len() / 8) as i32)
}

/// Points for matching `needle` through `hay` left to right: a run of adjacent
/// letters and a match at a word start both score above a scattered one.
fn subsequence(needle: &[u8], hay: &[u8]) -> Option<i32> {
    let mut next = 0;
    let mut total = 0;
    let mut previous: Option<usize> = None;
    for (index, byte) in hay.iter().enumerate() {
        if next == needle.len() {
            break;
        }
        if byte.to_ascii_lowercase() != needle[next] {
            continue;
        }
        let mut points = 1;
        if index > 0 && previous == Some(index - 1) {
            points += 4;
        }
        let word_start = index == 0
            || matches!(hay[index - 1], b'/' | b'-' | b'_' | b'.' | b' ')
            || (hay[index - 1].is_ascii_lowercase() && byte.is_ascii_uppercase());
        if word_start {
            points += 3;
        }
        total += points;
        previous = Some(index);
        next += 1;
    }
    (next == needle.len()).then_some(total)
}

impl PilusView {
    /// Open the picker over the Files tab, bringing the tab up first since the
    /// file lands there.
    pub(super) fn quick_open_show(&mut self, cx: &mut Context<Self>) {
        self.palette.close();
        if !self.right.visible {
            self.toggle_right_sidebar(cx);
        }
        self.select_right_tab(SidebarTab::Files, cx);
        self.quick_open.query.open();
        self.quick_open_load(true);
        let root = self.files_panel.tree_root();
        self.quick_open.refresh(root.as_deref());
        cx.notify();
    }

    pub(super) fn quick_open_close(&mut self, cx: &mut Context<Self>) {
        self.quick_open.close();
        cx.notify();
    }

    /// Read the listing of the tab's root when it is not the one held (or
    /// `force`). Waits for the tab's first tree, which names the root.
    fn quick_open_load(&mut self, force: bool) {
        let Some(root) = self.files_panel.tree_root() else {
            return;
        };
        let endpoint = self.active.clone();
        let wanted = (endpoint.clone(), root.clone());
        if self.quick_open.loading || (!force && self.quick_open.listed.as_ref() == Some(&wanted)) {
            return;
        }
        let target = match self.read_target(&endpoint) {
            Ok(target) => target,
            Err(error) => {
                self.quick_open.listing = Listing::Failed(error);
                self.quick_open.listed = Some(wanted);
                self.quick_open.version += 1;
                return;
            }
        };
        self.quick_open.loading = true;
        let tx = self.quick_open.tx.clone();
        std::thread::spawn(move || {
            let result = list_files(target.as_ref(), &root);
            tx.send(Listed {
                endpoint,
                root,
                result,
            })
            .ok();
        });
    }

    /// Take a finished read, and start one the open picker is waiting on.
    pub(super) fn quick_open_tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(listed) = self.quick_open.rx.try_recv() {
            self.quick_open.take(listed);
            changed = true;
        }
        if self.quick_open.is_open() {
            self.quick_open_load(false);
            let root = self.files_panel.tree_root();
            self.quick_open.refresh(root.as_deref());
        }
        changed
    }

    /// The files of the root this window shows, once they have been read.
    fn quick_open_files(&self) -> Option<(&FileList, &str)> {
        let (endpoint, root) = self.quick_open.listed.as_ref()?;
        let current = self.files_panel.tree_root()?;
        if *endpoint != self.active || *root != current {
            return None;
        }
        match &self.quick_open.listing {
            Listing::Ready(list) => Some((list, root)),
            _ => None,
        }
    }

    /// Keys the query field leaves to the window.
    pub(super) fn quick_open_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let control = keystroke.modifiers.control;
        let len = self.quick_open.ranked.len();
        let scroll = |this: &mut Self| {
            let at = this.quick_open.query.selected().min(len.saturating_sub(1));
            this.quick_open.scroll.scroll_to_item(at);
        };
        match keystroke.key.as_str() {
            "escape" | "esc" => self.quick_open.close(),
            "enter" => self.quick_open_pick_selected(cx),
            "up" => {
                self.quick_open.query.move_selection(-1, len);
                scroll(self);
            }
            "down" | "tab" => {
                self.quick_open.query.move_selection(1, len);
                scroll(self);
            }
            "p" | "k" if control => {
                self.quick_open.query.move_selection(-1, len);
                scroll(self);
            }
            "n" | "j" if control => {
                self.quick_open.query.move_selection(1, len);
                scroll(self);
            }
            _ => {}
        }
        cx.notify();
    }

    fn quick_open_pick_selected(&mut self, cx: &mut Context<Self>) {
        let ranked = &self.quick_open.ranked;
        let Some(&index) = ranked.get(
            self.quick_open
                .query
                .selected()
                .min(ranked.len().saturating_sub(1)),
        ) else {
            return;
        };
        self.quick_open_pick(index, cx);
    }

    /// Open the listed file at `index`, if the file list still is the one the
    /// index was ranked from.
    fn quick_open_pick(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((list, root)) = self.quick_open_files() else {
            return;
        };
        let Some(file) = list.paths.get(index) else {
            return;
        };
        let path = format!("{}/{file}", root.trim_end_matches('/'));
        self.quick_open.close();
        self.files_open_path(&path, cx);
        cx.notify();
    }

    /// The overlay: the query field over the ranked files.
    pub(super) fn quick_open_overlay(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.quick_open.is_open() {
            return div().into_any_element();
        }
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let selected = self
            .quick_open
            .query
            .selected()
            .min(self.quick_open.ranked.len().saturating_sub(1));

        let message = |text: String| {
            div()
                .px_3()
                .py_4()
                .text_size(px(fonts.body))
                .text_color(theme.muted)
                .child(text)
        };
        let body: AnyElement = match (&self.quick_open.listing, self.quick_open_files()) {
            (Listing::Failed(error), _) => {
                message(format!("Could not list files: {error}")).into_any_element()
            }
            (_, None) => message("Reading files…".to_string()).into_any_element(),
            (_, Some(_)) if self.quick_open.ranked.is_empty() => {
                message("No matching files".to_string()).into_any_element()
            }
            (_, Some((list, _))) => {
                let rows: Vec<AnyElement> = self
                    .quick_open
                    .ranked
                    .iter()
                    .enumerate()
                    .filter_map(|(row, &index)| {
                        let path = list.paths.get(index)?;
                        Some(self.quick_open_row(row, index, path, row == selected, cx))
                    })
                    .collect();
                let truncated = list.truncated.then(|| {
                    message(format!(
                        "Showing the first {} files",
                        pilus_client::MAX_FILES
                    ))
                    .text_size(px(fonts.label))
                });
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("pilus-quick-open-list")
                            .flex()
                            .flex_col()
                            .p_1()
                            .max_h(px(360.0))
                            .overflow_y_scroll()
                            .track_scroll(&self.quick_open.scroll)
                            .children(rows),
                    )
                    .children(truncated)
                    .into_any_element()
            }
        };

        let input = self.quick_open_input.clone();
        input.update(cx, |input, _| {
            input.set_style(
                theme,
                style::chrome_font(&self.shared_settings),
                px(fonts.body),
            );
        });
        let field = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(44.0))
            .px_3()
            .border_b_1()
            .border_color(theme.border)
            .text_size(px(fonts.body))
            .child(input);

        style::enter(
            "pilus-quick-open-enter",
            div()
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(0.0))
                .occlude()
                .bg(theme.window_bg.opacity(0.55))
                .flex()
                .justify_center()
                .pt(px(HEADER_HEIGHT + 12.0))
                .child(
                    div()
                        .w(px(560.0))
                        .max_h(px(440.0))
                        .flex()
                        .flex_col()
                        .radius_lg()
                        .bg(theme.sidebar_bg)
                        .border_1()
                        .border_color(theme.border)
                        .overflow_hidden()
                        .child(field)
                        .child(body)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _, _, cx| cx.stop_propagation()),
                        ),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.quick_open_close(cx)),
                ),
        )
        .into_any_element()
    }

    /// One ranked file: its name, and the folder it is in.
    fn quick_open_row(
        &self,
        row: usize,
        index: usize,
        path: &str,
        active: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let name = file_name(path).to_string();
        let dir = path
            .strip_suffix(name.as_str())
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();
        // The highlight borrows the scheme's selection colors, as the palette's.
        let (bg, fg, dim) = if active {
            (
                theme.selection_bg,
                theme.selection_fg,
                theme.selection_fg.opacity(0.75),
            )
        } else {
            (theme.sidebar_bg, theme.panel_fg, theme.muted)
        };
        div()
            .id(format!("pilus-quick-open-{row}"))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(30.0))
            .px_3()
            .radius_md()
            .state_fill(
                bg,
                if active {
                    theme.selection_bg
                } else {
                    theme.panel
                },
                style::RADIUS_MD,
            )
            .cursor_pointer()
            .text_size(px(fonts.body))
            .text_color(fg)
            .child(icon(
                include_bytes!("../../assets/icons/file.svg"),
                fonts.body,
                dim,
            ))
            .child(div().flex_shrink_0().child(name))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(fonts.secondary))
                    .text_color(dim)
                    .child(dir),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.quick_open_pick(index, cx)))
            .into_any_element()
    }
}

#[cfg(feature = "visual-test")]
impl PilusView {
    /// The picker open on `query` over a canned listing of the sample tree's
    /// root, so a capture never lists a real disk.
    pub(super) fn quick_open_show_sample(&mut self, query: &str, cx: &mut Context<Self>) {
        let root = self.files_panel.tree_root().unwrap_or_default();
        let paths = [
            "AGENTS.md",
            "Cargo.toml",
            "README.md",
            "crates/pilus-client/src/control.rs",
            "crates/pilus-client/src/files.rs",
            "crates/pilus-client/src/lib.rs",
            "crates/pilus-client/src/walk.rs",
            "crates/pilus/src/gui/files_tab.rs",
            "crates/pilus/src/gui/mod.rs",
            "crates/pilus/src/gui/model_picker.rs",
            "crates/pilus/src/gui/quick_open.rs",
            "docs/files.md",
        ];
        self.quick_open.listing = Listing::Ready(FileList {
            paths: paths.iter().map(|path| path.to_string()).collect(),
            truncated: false,
        });
        self.quick_open.listed = Some((self.active.clone(), root.clone()));
        self.quick_open.version += 1;
        self.quick_open.query.open();
        self.quick_open
            .query
            .input_mut()
            .set_value(query.to_string());
        self.quick_open.refresh(Some(&root));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> Vec<String> {
        let mut files: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        files.sort();
        files
    }

    fn ranked<'a>(query: &str, files: &'a [String]) -> Vec<&'a str> {
        rank(query, files, &[], 10)
            .into_iter()
            .map(|index| files[index].as_str())
            .collect()
    }

    #[test]
    fn a_query_keeps_only_subsequences() {
        let files = files(&["src/main.rs", "README.md", "src/gui/mod.rs"]);
        assert_eq!(ranked("mnr", &files), ["src/main.rs"]);
        assert!(ranked("zzz", &files).is_empty());
    }

    #[test]
    fn the_file_name_outranks_a_scattered_path() {
        let files = files(&["src/gui/model_picker.rs", "src/gui/mod.rs"]);
        assert_eq!(ranked("mod", &files)[0], "src/gui/mod.rs");
    }

    #[test]
    fn case_and_spaces_in_the_query_do_not_matter() {
        let files = files(&["src/FileTab.rs", "src/other.rs"]);
        assert_eq!(ranked("file tab", &files), ["src/FileTab.rs"]);
    }

    #[test]
    fn a_folder_part_narrows_the_match() {
        let files = files(&["a/lib.rs", "b/lib.rs"]);
        assert_eq!(ranked("b/lib", &files), ["b/lib.rs"]);
    }

    #[test]
    fn the_shorter_path_wins_a_tie() {
        let files = files(&["deep/er/than/lib.rs", "lib.rs"]);
        assert_eq!(ranked("lib", &files)[0], "lib.rs");
    }

    #[test]
    fn an_empty_query_lists_recent_files_first_then_the_rest() {
        let files = files(&["a.rs", "b.rs", "c.rs", "d.rs"]);
        let rows = rank("", &files, &["c.rs", "gone.rs", "b.rs"], 3);
        let names: Vec<&str> = rows.iter().map(|&index| files[index].as_str()).collect();
        assert_eq!(names, ["c.rs", "b.rs", "a.rs"]);
    }

    #[test]
    fn rows_are_capped() {
        let names: Vec<String> = (0..100).map(|n| format!("f{n:03}.rs")).collect();
        assert_eq!(rank("f", &names, &[], MAX_ROWS).len(), MAX_ROWS);
        assert_eq!(rank("", &names, &[], MAX_ROWS).len(), MAX_ROWS);
    }

    #[test]
    fn a_path_is_made_relative_to_its_root() {
        assert_eq!(relative("/w/src/a.rs", "/w"), Some("src/a.rs"));
        assert_eq!(relative("/w/src/a.rs", "/w/"), Some("src/a.rs"));
        assert_eq!(relative("/other/a.rs", "/w"), None);
        // A sibling that shares the root's prefix is not below it.
        assert_eq!(relative("/work/a.rs", "/w"), None);
    }

    #[test]
    fn a_new_listing_is_ranked_again() {
        let mut open = QuickOpen::new();
        let endpoint_listing = |paths: &[&str]| FileList {
            paths: files(paths),
            truncated: false,
        };
        open.listing = Listing::Ready(endpoint_listing(&["a.rs"]));
        open.rank(None);
        assert_eq!(open.ranked, [0]);
        open.listing = Listing::Ready(endpoint_listing(&["a.rs", "b.rs"]));
        open.version += 1;
        open.rank(None);
        assert_eq!(open.ranked, [0, 1]);
    }
}
