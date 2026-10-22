//! The native Database page: a read-only look at what `pilus.db` holds, opened
//! from the command palette.
//!
//! It exists so the person can see what pilus keeps about them (conversations,
//! contacts, notes, cached GitHub lists, window state) without a SQLite client.
//! Nothing here writes: the page asks `pilus_db::browse` for a summary and one
//! window of rows at a time, and only ever runs `SELECT`s. It reads when it
//! opens, when a table or page is chosen and on Refresh, never on a timer, so an
//! open page costs nothing while idle. The first half holds the pure rules,
//! unit-tested without GPUI; the second half holds the page's state and renders.

use std::path::{Path, PathBuf};

use gpui::{Context, Div, FontWeight, KeyDownEvent, ScrollHandle, div, prelude::*, px};
use pilus_db::Db;
use pilus_db::browse::{Cell, Rows, Summary};

use super::page_menu::PageAction;
use super::style::{self, ButtonKind, RadiusExt};
use super::{PilusView, icon};

/// Rows shown at once.
const PAGE_ROWS: u64 = 50;

// -- Pure rules ----------------------------------------------------------

/// The offset one page on, or `None` at the end of the table.
fn forward(offset: u64, total: u64) -> Option<u64> {
    let next = offset.saturating_add(PAGE_ROWS);
    (next < total).then_some(next)
}

/// The offset one page back, or `None` at the start.
fn back(offset: u64) -> Option<u64> {
    (offset > 0).then(|| offset.saturating_sub(PAGE_ROWS))
}

/// Which rows are on screen, as the pager says it.
fn shown(offset: u64, on_page: usize, total: u64) -> String {
    if total == 0 {
        return "empty".to_string();
    }
    let first = offset + 1;
    let last = offset + on_page as u64;
    if first > last {
        return format!("no rows from {first} · {total} in all");
    }
    format!("rows {first}–{last} of {total}")
}

/// Each column's width in design pixels, from what its header and shown rows
/// need: `Some` for a column that takes just that, `None` for the one that takes
/// the rest, which is the column with the longest text (the last on a tie), so
/// the words in a table are not squeezed by the ids beside them.
fn column_widths(rows: &Rows) -> Vec<Option<f32>> {
    const CHAR: f32 = 7.5;
    const PADDING: f32 = 20.0;
    const MIN: f32 = 64.0;
    const MAX: f32 = 240.0;
    let longest: Vec<usize> = rows
        .columns
        .iter()
        .enumerate()
        .map(|(at, name)| {
            rows.rows
                .iter()
                .filter_map(|row| row.get(at))
                .map(|cell| cell.line().chars().count())
                .max()
                .unwrap_or(0)
                .max(name.chars().count())
        })
        .collect();
    let widest = longest
        .iter()
        .enumerate()
        .max_by_key(|(_, len)| **len)
        .map(|(at, _)| at);
    longest
        .iter()
        .enumerate()
        .map(|(at, len)| {
            (Some(at) != widest).then(|| (*len as f32 * CHAR + PADDING).clamp(MIN, MAX))
        })
        .collect()
}

/// A byte count as `812 B`, `14.2 KB` or `3.1 MB`.
fn size_label(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    // Byte counts are far below f64's exact range, and this is for display.
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64;
    if value >= KB * KB {
        format!("{:.1} MB", value / (KB * KB))
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// The path with the home folder written as `~`.
fn tilde(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

// -- The page ------------------------------------------------------------

/// The page's state.
pub(super) struct DatabasePage {
    open: bool,
    /// Where the rows are read from; `None` until the page opens.
    source: Option<Db>,
    path: PathBuf,
    summary: Option<Summary>,
    error: Option<String>,
    selected: usize,
    offset: u64,
    rows: Option<Rows>,
    scroll: ScrollHandle,
    /// The rows are made up for a screenshot, not read from the real file.
    sample: bool,
    /// Whether the page menu (Refresh, Close) is dropped.
    menu: bool,
}

impl Default for DatabasePage {
    fn default() -> Self {
        Self {
            open: false,
            source: None,
            path: PathBuf::new(),
            summary: None,
            error: None,
            selected: 0,
            offset: 0,
            rows: None,
            scroll: ScrollHandle::new(),
            sample: false,
            menu: false,
        }
    }
}

impl DatabasePage {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.menu = false;
        // What was on screen can name private conversations; keep none of it
        // while the page is shut.
        self.summary = None;
        self.rows = None;
        self.source = None;
        self.sample = false;
    }

    fn start(&mut self, source: Db, path: PathBuf) {
        self.open = true;
        self.source = Some(source);
        self.path = path;
        self.selected = 0;
        self.offset = 0;
        self.read();
    }

    /// Read the summary and the selected table's rows again.
    fn read(&mut self) {
        let Some(db) = &self.source else { return };
        self.error = None;
        match db.summary() {
            Ok(summary) => {
                self.selected = self.selected.min(summary.tables.len().saturating_sub(1));
                self.summary = Some(summary);
            }
            Err(error) => {
                self.error = Some(error.to_string());
                self.summary = None;
                self.rows = None;
                return;
            }
        }
        let Some(table) = self.table_name() else {
            self.rows = None;
            return;
        };
        match db.rows(&table, self.offset, PAGE_ROWS) {
            Ok(rows) => self.rows = Some(rows),
            Err(error) => {
                self.error = Some(error.to_string());
                self.rows = None;
            }
        }
    }

    fn table_name(&self) -> Option<String> {
        self.summary
            .as_ref()
            .and_then(|summary| summary.tables.get(self.selected))
            .map(|table| table.name.clone())
    }

    fn table_count(&self) -> usize {
        self.summary
            .as_ref()
            .map_or(0, |summary| summary.tables.len())
    }

    fn select(&mut self, index: usize) {
        if index < self.table_count() && index != self.selected {
            self.selected = index;
            self.offset = 0;
            self.read();
        }
    }

    fn step(&mut self, ahead: bool) {
        let total = self.rows.as_ref().map_or(0, |rows| rows.total);
        let moved = if ahead {
            forward(self.offset, total)
        } else {
            back(self.offset)
        };
        if let Some(offset) = moved {
            self.offset = offset;
            self.read();
        }
    }
}

impl PilusView {
    /// Open the page on the database under the pilus folder.
    pub(super) fn open_database(&mut self, cx: &mut Context<Self>) {
        self.leave_settings_page();
        let db = Db::at(&self.pilus_root);
        let path = self.pilus_root.join(pilus_db::FILE);
        self.database.start(db, path);
        cx.notify();
    }

    fn close_database(&mut self, cx: &mut Context<Self>) {
        self.database.close();
        cx.notify();
    }

    pub(super) fn database_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if event.keystroke.modifiers.platform || event.keystroke.modifiers.control {
            return;
        }
        match key {
            "escape" | "q" => self.close_database(cx),
            "r" => self.database.read(),
            "up" => self
                .database
                .select(self.database.selected.saturating_sub(1)),
            "down" => self.database.select(self.database.selected + 1),
            "left" => self.database.step(false),
            "right" => self.database.step(true),
            _ => return,
        }
        cx.notify();
    }

    pub(super) fn database_page(&mut self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let durable = self.database.source.as_ref().is_some_and(Db::durable);
        let subtitle = if self.database.sample {
            format!("{} · read-only · sample rows", tilde(&self.database.path))
        } else if durable {
            format!("{} · read-only", tilde(&self.database.path))
        } else {
            "pilus.db could not be opened, so this run keeps its state in memory only".to_string()
        };
        let menu = self.page_menu(
            "database",
            self.database.menu,
            vec![
                PageAction {
                    label: "Refresh",
                    run: |this, _| this.database.read(),
                },
                PageAction {
                    label: "Close",
                    run: |this, _| this.database.close(),
                },
            ],
            |this| &mut this.database.menu,
            cx,
        );
        let bar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .flex_wrap()
            .gap_3()
            .px_4()
            .py_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(fonts.title))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.fg)
                            .child("Database"),
                    )
                    .child(
                        div()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.muted)
                            .child(subtitle),
                    ),
            )
            .child(menu);
        let body = match (&self.database.summary, &self.database.error) {
            (Some(_), _) => self.database_body(cx),
            (None, error) => div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(style::empty_state(
                    theme,
                    fonts,
                    icon(
                        include_bytes!("../../assets/icons/rows-2.svg"),
                        24.0,
                        theme.muted,
                    ),
                    "Nothing to show",
                    error
                        .clone()
                        .unwrap_or_else(|| "The database has not been read yet.".to_string()),
                ))
                .into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(bar)
            .child(body)
    }

    fn database_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let page = &self.database;
        let Some(summary) = &page.summary else {
            return div().into_any_element();
        };
        let total: u64 = summary.tables.iter().map(|table| table.rows).sum();

        // Flush tiles with no rules, as on the Insights page: the page's cards
        // carry the borders, and a frame around the headline figures would
        // crowd them.
        let tiles = div()
            .flex()
            .flex_row()
            .px_4()
            .child(
                style::stat_tile(
                    theme,
                    fonts,
                    "TABLES",
                    summary.tables.len().to_string(),
                    theme.accent,
                    "in the schema",
                )
                .border_r_0(),
            )
            .child(
                style::stat_tile(
                    theme,
                    fonts,
                    "ROWS",
                    total.to_string(),
                    theme.panel_fg,
                    "across all tables",
                )
                .border_r_0(),
            )
            .child(
                style::stat_tile(
                    theme,
                    fonts,
                    "SIZE",
                    size_label(summary.bytes),
                    theme.panel_fg,
                    "the file, without its log",
                )
                .border_r_0(),
            )
            .child(
                style::stat_tile(
                    theme,
                    fonts,
                    "SCHEMA",
                    format!("v{}", summary.schema),
                    theme.panel_fg,
                    "migrations applied",
                )
                .border_r_0(),
            );

        let mut chooser = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .justify_center()
            .gap_1()
            .px_4();
        for (index, table) in summary.tables.iter().enumerate() {
            chooser = chooser.child(
                style::tab(
                    ("pilus-database-table", index),
                    format!("{}  {}", table.name, table.rows),
                    index == page.selected,
                    theme,
                    fonts.body,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.database.select(index);
                    cx.notify();
                })),
            );
        }

        let column = div()
            .flex()
            .flex_col()
            .gap_4()
            .w_full()
            .max_w(px(style::scaled(&self.shared_settings, 1240.0)))
            .pb_6()
            .child(tiles)
            .child(chooser)
            .child(div().px_4().child(self.database_rows(cx)));

        div()
            .id("pilus-database-scroll")
            .flex()
            .flex_col()
            .items_center()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.database.scroll)
            .child(column)
            .into_any_element()
    }

    /// The selected table: its columns, one window of rows, and the pager.
    fn database_rows(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let Some(rows) = &self.database.rows else {
            return div()
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(self.database.error.clone().unwrap_or_default());
        };
        let last = rows.columns.len().saturating_sub(1);
        let widths = column_widths(rows);
        let cell = |at: usize| {
            let width = widths.get(at).copied().flatten();
            style::table_cell(&self.shared_settings, theme, width, at == last)
        };
        let row_height = style::scaled(&self.shared_settings, 26.0);

        let mut head = div()
            .flex()
            .flex_row()
            .h(px(row_height))
            .border_b_1()
            .border_color(theme.border);
        for (at, name) in rows.columns.iter().enumerate() {
            head = head.child(
                cell(at)
                    .overflow_hidden()
                    .text_size(px(fonts.label))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.muted)
                    .child(name.to_uppercase()),
            );
        }
        let mut grid = div()
            .flex()
            .flex_col()
            .border_1()
            .border_color(theme.border)
            .radius_md()
            .overflow_hidden()
            .child(head);
        if rows.rows.is_empty() {
            grid = grid.child(
                div()
                    .p_3()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child("This table has no rows."),
            );
        }
        for (at, values) in rows.rows.iter().enumerate() {
            let mut line = div()
                .id(("pilus-database-row", at))
                .flex()
                .flex_row()
                .h(px(row_height))
                .when(at > 0, |line| line.border_t_1().border_color(theme.border));
            for (column, value) in values.iter().enumerate() {
                line = line.child(
                    cell(column).child(
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(fonts.body))
                            .text_color(match value {
                                Cell::Null | Cell::Blob(_) => theme.muted,
                                _ => theme.panel_fg,
                            })
                            .child(value.line()),
                    ),
                );
            }
            grid = grid.child(line);
        }

        let pager = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(shown(rows.offset, rows.rows.len(), rows.total)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(
                        style::button_enabled(
                            "pilus-database-prev",
                            "Previous",
                            ButtonKind::Secondary,
                            theme,
                            fonts.label,
                            back(rows.offset).is_some(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.database.step(false);
                            cx.notify();
                        })),
                    )
                    .child(
                        style::button_enabled(
                            "pilus-database-next",
                            "Next",
                            ButtonKind::Secondary,
                            theme,
                            fonts.label,
                            forward(rows.offset, rows.total).is_some(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.database.step(true);
                            cx.notify();
                        })),
                    ),
            );
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(grid)
            .child(pager)
            .child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(format!(
                        "Read-only: this page only reads. Text is cut at {} characters. \
                         ↑ ↓ choose a table, ← → turn the page, R refreshes, Esc closes.",
                        pilus_db::browse::CELL_LIMIT
                    )),
            )
    }
}

/// Shot-harness entry point: the page over a database made up for the capture,
/// so a screenshot never shows (or opens) the real one.
#[cfg(feature = "visual-test")]
impl PilusView {
    pub(super) fn database_show_sample(&mut self, cx: &mut Context<Self>) {
        use pilus_client::sipil::{Chats, Directory};

        self.leave_settings_page();
        let db = Db::in_memory();
        let nowhere = Path::new("/nonexistent");
        let rina = "a".repeat(64);
        let contacts = Directory::open(&db, nowhere);
        contacts.add(&rina, "Rina", None).ok();
        let chats = Chats::open(&db, nowhere);
        for (n, text) in [
            "ship it on friday?",
            "yes, once the migration lands",
            "I'll cut the release branch after lunch",
        ]
        .into_iter()
        .enumerate()
        {
            if n % 2 == 0 {
                chats.push_theirs(&rina, text, &format!("req-{n}"), None, None);
            } else {
                chats.push_mine(&rina, text, None);
            }
        }
        db.put_doc("github_backoff", &0u64).ok();
        self.database.start(db, PathBuf::from("~/.pilus/pilus.db"));
        self.database.sample = true;
        // Open on the table that has the most to show.
        if let Some(at) = self
            .database
            .summary
            .as_ref()
            .and_then(|s| s.tables.iter().position(|t| t.name == "chat_lines"))
        {
            self.database.select(at);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paging_stops_at_both_ends() {
        assert_eq!(forward(0, 120), Some(50));
        assert_eq!(forward(50, 120), Some(100));
        assert_eq!(forward(100, 120), None);
        assert_eq!(forward(0, 50), None, "exactly one page has no next");
        assert_eq!(forward(0, 0), None);
        assert_eq!(back(100), Some(50));
        assert_eq!(back(30), Some(0));
        assert_eq!(back(0), None);
    }

    #[test]
    fn the_pager_says_which_rows_are_showing() {
        assert_eq!(shown(0, 0, 0), "empty");
        assert_eq!(shown(0, 50, 120), "rows 1–50 of 120");
        assert_eq!(shown(100, 20, 120), "rows 101–120 of 120");
        assert_eq!(shown(150, 0, 120), "no rows from 151 · 120 in all");
    }

    #[test]
    fn sizes_read_in_the_unit_that_keeps_them_short() {
        assert_eq!(size_label(812), "812 B");
        assert_eq!(size_label(14_540), "14.2 KB");
        assert_eq!(size_label(3_250_000), "3.1 MB");
    }

    fn rows_of(columns: &[&str], rows: Vec<Vec<Cell>>) -> Rows {
        Rows {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            total: rows.len() as u64,
            offset: 0,
            rows,
        }
    }

    fn text(value: &str) -> Cell {
        Cell::Text {
            text: value.to_string(),
            cut: false,
        }
    }

    #[test]
    fn the_longest_column_takes_the_rest_and_short_ones_stay_narrow() {
        let rows = rows_of(
            &["thread", "id", "text"],
            vec![vec![
                text("abcdefgh"),
                Cell::Integer(1),
                text("ship it on friday once the migration lands"),
            ]],
        );
        let widths = column_widths(&rows);
        assert_eq!(widths[2], None, "the words get the slack");
        let id = widths[1].unwrap();
        let thread = widths[0].unwrap();
        assert!(id < thread, "a narrow id column is narrower than a name");
        assert!(id >= 64.0, "but never too small to read");
        // A long id-like value is capped, so it cannot push the words off.
        let wide = rows_of(&["k", "v"], vec![vec![text(&"x".repeat(200)), text("y")]]);
        assert_eq!(column_widths(&wide), [None, Some(64.0)]);
        let capped = rows_of(
            &["k", "v"],
            vec![vec![text(&"x".repeat(60)), text(&"y".repeat(90))]],
        );
        assert_eq!(column_widths(&capped), [Some(240.0), None]);
    }

    #[test]
    fn an_empty_table_still_lays_out_its_headers() {
        let rows = rows_of(&["name", "body"], Vec::new());
        assert_eq!(column_widths(&rows).len(), 2);
        assert_eq!(
            column_widths(&rows_of(&[], Vec::new())),
            Vec::<Option<f32>>::new()
        );
    }

    fn page() -> DatabasePage {
        let db = Db::in_memory();
        for n in 0..120 {
            db.put_doc(&format!("d{n:03}"), &n).unwrap();
        }
        let mut page = DatabasePage::default();
        page.start(db, PathBuf::from("/x/pilus.db"));
        page
    }

    #[test]
    fn choosing_a_table_starts_at_its_first_row() {
        let mut page = page();
        let docs = page
            .summary
            .as_ref()
            .unwrap()
            .tables
            .iter()
            .position(|t| t.name == "docs")
            .unwrap();
        page.select(docs);
        page.step(true);
        assert_eq!(page.rows.as_ref().unwrap().offset, 50);
        page.select(docs + 1);
        assert_eq!(page.rows.as_ref().unwrap().offset, 0);
        assert_eq!(page.rows.as_ref().unwrap().total, 0);
    }

    #[test]
    fn stepping_walks_a_table_and_never_past_it() {
        let mut page = page();
        let docs = page
            .summary
            .as_ref()
            .unwrap()
            .tables
            .iter()
            .position(|t| t.name == "docs")
            .unwrap();
        page.select(docs);
        for _ in 0..5 {
            page.step(true);
        }
        let rows = page.rows.as_ref().unwrap();
        assert_eq!((rows.offset, rows.rows.len()), (100, 20));
        page.step(false);
        page.step(false);
        page.step(false);
        assert_eq!(page.rows.as_ref().unwrap().offset, 0);
    }

    #[test]
    fn closing_lets_go_of_what_was_read() {
        let mut page = page();
        assert!(page.is_open() && page.rows.is_some());
        page.close();
        assert!(!page.is_open());
        assert!(page.summary.is_none() && page.rows.is_none() && page.source.is_none());
    }

    #[test]
    fn a_selection_past_the_last_table_is_ignored() {
        let mut page = page();
        let count = page.table_count();
        page.select(count);
        assert_eq!(page.selected, 0);
    }
}
