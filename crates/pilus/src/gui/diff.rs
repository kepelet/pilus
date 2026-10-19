//! The native Diff page: the daemon's read-only review of the active
//! workspace's Git changes, with local review notes and per-file staging.
//!
//! The daemon owns the scan, the bounded patch parser, and the notes
//! (`diff.*` over UHP); this page polls its snapshot on a background thread,
//! lays the selected file out through `diff_model`, and sends every note edit
//! back off the GUI thread. Nothing here stages, discards, or commits: the
//! page never edits the checkout. The one write it makes is the index: the
//! daemon has no call for that, so staging and unstaging run Git itself
//! (`pilus_client::set_staged`), off the GUI thread, then ask the daemon for a
//! fresh scan. The daemon answers for whichever workspace it has active, so
//! the page follows the sidebar's selection rather than carrying a workspace
//! of its own.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use gpui::{
    Anchor, AnyElement, Context, Div, Entity, FontStyle, FontWeight, HighlightStyle, KeyDownEvent,
    ListAlignment, ListState, MouseButton, MouseDownEvent, SharedString, Stateful, StyledText,
    anchored, deferred, div, list, point, prelude::*, px,
};
use pilus_client::{
    Agent, ControlClient, DiffFile, DiffLayer, DiffNote, DiffSnapshot, Discard, FileDiff, NoteKind,
    NoteRange, NoteSelection, NoteSide, NoteState, StageAction, TextFile, Version, discard,
    read_text, set_staged,
};

use super::diff_model::{
    self as model, Cursor, Display, FileFilter, Flat, FolderKey, LayoutMode, ListView, Row, Src,
    SrcKind, TreeRow,
};
use super::highlight::{self, Token};
use super::md_view::{self, Composing, MdDoc, Page};
use super::style::{self, ButtonKind, FillExt, RadiusExt};
use super::text_input::TextInput;
use super::theme::Theme;
use super::toast::ToastKind;
use super::{EndpointId, PilusView, agent_icon, icon};
use crate::settings::Settings;

/// How often the open page asks the daemon for a fresh snapshot.
const POLL: Duration = Duration::from_millis(1500);
/// Rows a half-page key moves.
const HALF_PAGE: isize = 12;
const LIST_WIDTH: f32 = 280.0;
/// Height the file list starts at above the diff, before the user drags it.
const DEFAULT_LIST_HEIGHT: f32 = 168.0;
/// The strip between the list and the diff that a drag grabs.
const DIVIDER_HEIGHT: f32 = 7.0;
/// Below this page width the file list steps aside for the diff.
const LIST_MIN_PAGE: f32 = 640.0;
const GUTTER: f32 = 40.0;
/// Opacity of the wash behind an added or removed line.
const CHANGE_WASH: f32 = 0.14;

type FileKey = (DiffLayer, String);

/// What a background call reports back to the page.
pub(super) enum DiffMsg {
    Snapshot {
        endpoint: EndpointId,
        result: Result<(DiffSnapshot, Vec<DiffNote>), String>,
    },
    File {
        key: FileKey,
        fingerprint: String,
        result: Result<FileDiff, String>,
    },
    /// A markdown file's text, read for the rendered view.
    Text {
        key: FileKey,
        fingerprint: String,
        result: Result<TextFile, String>,
    },
    /// A note operation finished; the text becomes a toast.
    Done(Result<String, String>),
    /// Git finished moving a file into or out of the index.
    Staged(Result<(), String>),
    Discarded(Result<(), String>),
}

/// A changed markdown file's rendered form, for the version of it the
/// fingerprint names.
struct MdText {
    key: FileKey,
    fingerprint: String,
    state: Result<MdDoc, String>,
}

/// A file's diff, flattened and laid out.
struct Loaded {
    key: FileKey,
    fingerprint: String,
    diff: FileDiff,
    flat: Flat,
    display: Display,
    built: Option<BuildKey>,
    /// The list is rebuilt from scratch rather than spliced, so a new file
    /// opens at its top.
    fresh: bool,
}

/// What a built layout depends on; a change rebuilds it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BuildKey {
    split: bool,
    notes_gen: u64,
    composer_after: Option<usize>,
}

/// The note being written or edited.
struct Composer {
    range: NoteRange,
    /// Source row the composer sits under.
    after: usize,
    kind: NoteKind,
    editing: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SendScope {
    Note,
    File,
    Review,
}

impl SendScope {
    fn label(self) -> &'static str {
        match self {
            Self::Note => "This note",
            Self::File => "This file",
            Self::Review => "All files",
        }
    }
}

/// The header dropdowns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiffMenu {
    Layout,
    Filter,
}

/// How long after an outside press a press on the same button is read as the
/// click that closed it, not a request to open it again.
const MENU_DISMISS_WINDOW: Duration = Duration::from_millis(250);

/// The agent picker for sending notes.
struct Sending {
    scope: SendScope,
    pick: usize,
    /// The note a card's Send button named, which the picker sends instead of
    /// the one under the cursor.
    note: Option<String>,
}

/// The page's own state.
pub(super) struct DiffPanel {
    endpoint: Option<EndpointId>,
    snapshot: Option<DiffSnapshot>,
    error: Option<String>,
    notes: Vec<DiffNote>,
    notes_gen: u64,
    /// The current file's notes, in daemon order.
    file_notes: Vec<DiffNote>,
    selected: Option<FileKey>,
    loaded: Option<Loaded>,
    loading: Option<(FileKey, String)>,
    filter: FileFilter,
    /// Whether the list shows files flat or nested under their folders.
    view: ListView,
    /// Folders folded shut in the tree, by layer.
    folded: HashSet<FolderKey>,
    /// The folder whose chevron was just turned, so only it animates.
    folder_toggled: Option<FolderKey>,
    /// What the search field holds, mirrored here so list logic needs no
    /// view context.
    query: String,
    /// The search field holds the keyboard.
    searching: bool,
    /// A file just staged or unstaged, and which way: the open file moves
    /// layer with it, so the next snapshot reopens it there.
    follow: Option<(FileKey, StageAction)>,
    mode: LayoutMode,
    /// Show a markdown file rendered rather than as a patch.
    rendered: bool,
    md: Option<MdText>,
    md_loading: Option<(FileKey, String)>,
    cursor: Cursor,
    /// Set while a range is being selected for a note.
    anchor: Option<usize>,
    composer: Option<Composer>,
    sending: Option<Sending>,
    /// The open header dropdown, and the one an outside press just closed.
    menu: Option<DiffMenu>,
    menu_dismissed: Option<(DiffMenu, Instant)>,
    /// Height of the file list above the diff, in logical pixels.
    list_h: f32,
    /// A divider drag in progress: pointer y and list height at its start.
    divider: Option<(f32, f32)>,
    /// Width of the file list beside the diff, in logical pixels; `None`
    /// until dragged, which takes the default at the Interface size.
    list_w: Option<f32>,
    /// A side divider drag in progress: pointer x and list width at its start.
    side_divider: Option<(f32, f32)>,
    /// Width of the page as last drawn, which bounds the list's width.
    page_w: f32,
    /// Fingerprints of versions marked viewed in this window.
    viewed: HashSet<String>,
    /// Files folded down to their header, as History folds a commit's files.
    collapsed: HashSet<FileKey>,
    /// Set by a fold toggle, so only that click turns the chevron.
    toggled: bool,
    confirm_delete: Option<String>,
    status: String,
    list: ListState,
    reveal: bool,
    /// Frames left to scroll the comment form into view. A form just
    /// inserted into the list has no measured height yet, so the first
    /// reveal aims at an estimate and the next one at the real size.
    reveal_composer: u8,
    polling: bool,
    last_poll: Option<Instant>,
    needs_refresh: bool,
    tx: Sender<DiffMsg>,
    rx: Receiver<DiffMsg>,
}

impl DiffPanel {
    pub(super) fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            endpoint: None,
            snapshot: None,
            error: None,
            notes: Vec::new(),
            notes_gen: 0,
            file_notes: Vec::new(),
            selected: None,
            loaded: None,
            loading: None,
            filter: FileFilter::default(),
            view: ListView::default(),
            folded: HashSet::new(),
            folder_toggled: None,
            query: String::new(),
            searching: false,
            follow: None,
            mode: LayoutMode::default(),
            rendered: true,
            md: None,
            md_loading: None,
            cursor: Cursor::start(),
            anchor: None,
            composer: None,
            sending: None,
            menu: None,
            menu_dismissed: None,
            list_h: DEFAULT_LIST_HEIGHT,
            divider: None,
            list_w: None,
            side_divider: None,
            page_w: 0.0,
            viewed: HashSet::new(),
            collapsed: HashSet::new(),
            toggled: false,
            confirm_delete: None,
            status: String::new(),
            list: ListState::new(0, ListAlignment::Top, px(600.0)),
            reveal: false,
            reveal_composer: 0,
            polling: false,
            last_poll: None,
            needs_refresh: true,
            tx,
            rx,
        }
    }

    /// Ask for a fresh scan the next time the page polls, such as when its
    /// tab comes back on screen.
    pub(super) fn mark_stale(&mut self) {
        self.needs_refresh = true;
        self.last_poll = None;
    }

    /// Start the file list at a saved height; anything too small to use (an
    /// unset 0) keeps the default.
    pub(super) fn set_list_height(&mut self, height: f32) {
        if height >= model::MIN_LIST_HEIGHT {
            self.list_h = height;
        }
    }

    /// Start the wide file list at a saved width; an unset 0 keeps the default.
    pub(super) fn set_list_width(&mut self, width: f32) {
        if width >= model::MIN_LIST_WIDTH {
            self.list_w = Some(width);
        }
    }

    /// The width the file list takes beside the diff.
    fn list_width(&self, settings: &Rc<RefCell<Settings>>) -> f32 {
        self.list_w.map_or_else(
            || style::scaled(settings, LIST_WIDTH),
            |wanted| model::clamp_list_width(wanted, self.page_w),
        )
    }

    pub(super) fn composing(&self) -> bool {
        self.composer.is_some()
    }

    /// Whether the search field holds the keyboard.
    pub(super) fn searching(&self) -> bool {
        self.searching
    }

    /// Mirror the search field's text. Returns whether it changed.
    pub(super) fn set_query(&mut self, query: String) -> bool {
        let changed = self.query != query;
        self.query = query;
        changed
    }

    /// Drop everything read from the daemon, keeping the user's view
    /// settings.
    fn reset(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        *self = Self {
            filter: self.filter,
            view: self.view,
            folded: std::mem::take(&mut self.folded),
            // The field keeps its text across machines, so the mirror does.
            query: std::mem::take(&mut self.query),
            mode: self.mode,
            rendered: self.rendered,
            viewed: std::mem::take(&mut self.viewed),
            list_h: self.list_h,
            list_w: self.list_w,
            tx,
            rx,
            ..Self::new()
        };
    }

    fn files(&self) -> &[DiffFile] {
        self.snapshot.as_ref().map_or(&[], |s| s.files.as_slice())
    }

    fn is_viewed(&self, file: &DiffFile) -> bool {
        file.viewed || self.viewed.contains(&file.fingerprint)
    }

    /// Indices into [`Self::files`] the list shows, in list order.
    fn visible(&self) -> Vec<usize> {
        model::visible_files(self.files(), self.filter, &self.query, |file| {
            self.is_viewed(file)
        })
    }

    /// A layer's rows in the tree. A search opens every folder so no match
    /// hides inside a shut one.
    fn tree(&self, layer: DiffLayer, visible: &[usize]) -> Vec<TreeRow> {
        model::tree_rows(
            self.files(),
            visible,
            layer,
            &self.folded,
            !self.query.trim().is_empty(),
        )
    }

    /// Indices into [`Self::files`] in the order the list draws them, which is
    /// the order the step keys walk. A file inside a shut folder is not drawn,
    /// so it is not listed.
    fn listed(&self) -> Vec<usize> {
        let visible = self.visible();
        if self.view == ListView::Flat {
            return visible;
        }
        DiffLayer::ALL
            .into_iter()
            .flat_map(|layer| self.tree(layer, &visible))
            .filter_map(|row| match row {
                TreeRow::File { index, .. } => Some(index),
                TreeRow::Folder { .. } => None,
            })
            .collect()
    }

    fn selected_file(&self) -> Option<&DiffFile> {
        let (layer, path) = self.selected.as_ref()?;
        self.files()
            .iter()
            .find(|file| file.layer == *layer && file.path == *path)
    }

    /// Whether the open file is shown as a rendered document: a markdown file
    /// that still exists, with the preview on.
    fn previewing(&self) -> bool {
        self.rendered && self.selected_file().is_some_and(previewable)
    }

    /// The rendered document of the open file, once its text has arrived.
    pub(super) fn md_view(&self) -> Option<&MdDoc> {
        let md = self.md.as_ref()?;
        (self.selected.as_ref() == Some(&md.key))
            .then(|| md.state.as_ref().ok())
            .flatten()
    }

    pub(super) fn md_view_mut(&mut self) -> Option<&mut MdDoc> {
        let md = self.md.as_mut()?;
        if self.selected.as_ref() != Some(&md.key) {
            return None;
        }
        md.state.as_mut().ok()
    }

    /// The open file, by layer and path.
    pub(super) fn selected_key(&self) -> Option<(DiffLayer, String)> {
        self.selected.clone()
    }

    pub(super) fn file_note(&self, index: usize) -> Option<&DiffNote> {
        self.file_notes.get(index)
    }

    /// The note an open agent picker would send for the Note scope: the one
    /// a card named, else the one under the cursor.
    fn note_for_send(&self) -> Option<&DiffNote> {
        match self
            .sending
            .as_ref()
            .and_then(|sending| sending.note.as_deref())
        {
            Some(id) => self.file_notes.iter().find(|note| note.id == id),
            None => self.note_at_cursor(),
        }
    }

    /// The note under the cursor, if one covers the cursor's line.
    fn note_at_cursor(&self) -> Option<&DiffNote> {
        let src = self.loaded.as_ref()?.flat.src.get(self.cursor.src)?;
        self.file_notes.iter().find(|note| {
            src.old_line
                .is_some_and(|line| note.covers(NoteSide::Old, line))
                || src
                    .new_line
                    .is_some_and(|line| note.covers(NoteSide::New, line))
        })
    }

    fn refresh_file_notes(&mut self) {
        self.file_notes = match &self.selected {
            Some((layer, path)) => self
                .notes
                .iter()
                .filter(|note| note.layer == *layer && note.path == *path)
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        self.notes_gen += 1;
    }

    fn open_notes_in_review(&self) -> usize {
        self.notes
            .iter()
            .filter(|note| model::is_open(note))
            .count()
    }
}

/// The text fields the page edits.
pub(super) struct DiffInputs {
    note: Entity<TextInput>,
    pub(super) search: Entity<TextInput>,
}

impl DiffInputs {
    pub(super) fn new(
        font: gpui::Font,
        size: f32,
        theme: Theme,
        cx: &mut Context<PilusView>,
    ) -> Self {
        let search_font = font.clone();
        let note = cx.new(move |cx| {
            TextInput::new(
                String::new(),
                "Leave a note for this change…",
                font,
                px(size),
                true,
                theme,
                cx,
            )
            .submit_on_enter()
        });
        let search = cx.new(move |cx| {
            TextInput::new(
                String::new(),
                "Filter files…",
                search_font,
                px(size),
                false,
                theme,
                cx,
            )
        });
        Self { note, search }
    }

    pub(super) fn focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        gpui::Focusable::focus_handle(&self.note, cx)
    }

    pub(super) fn search_focus_handle(&self, cx: &gpui::App) -> gpui::FocusHandle {
        gpui::Focusable::focus_handle(&self.search, cx)
    }
}

/// Where a note's side reads: `L` for the old file, `R` for the new.
fn range_label(note: &DiffNote) -> String {
    let side = match note.side {
        NoteSide::Old => 'L',
        NoteSide::New => 'R',
    };
    if note.end_line == note.start_line {
        format!("{side}{}", note.start_line)
    } else {
        format!("{side}{}–{}", note.start_line, note.end_line)
    }
}

/// The letter and color a changed file's status reads as. The daemon reports
/// an untracked file's status as `U`, which Git itself reserves for a
/// conflict; the layer tells them apart.
pub(super) fn status_glyph(theme: Theme, layer: DiffLayer, status: &str) -> (&str, gpui::Rgba) {
    match (layer, status) {
        (DiffLayer::Untracked, _) => ("?", theme.ok),
        (DiffLayer::Conflict, _) | (_, "D") => (status, theme.alert),
        (_, "A") => ("A", theme.ok),
        _ => (status, theme.branch),
    }
}

/// Whether a changed file has a rendered form: markdown text that exists on
/// disk or in the index.
fn previewable(file: &DiffFile) -> bool {
    md_view::is_markdown(&file.path) && !file.binary && file.status != "D"
}

pub(super) fn split_path(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => ("", path),
    }
}

impl PilusView {
    /// Poll the daemon and fold finished calls into the page. Returns
    /// whether anything changed.
    pub(super) fn diff_tick(&mut self) -> bool {
        let mut changed = false;
        while let Ok(message) = self.diff_panel.rx.try_recv() {
            self.diff_receive(message);
            changed = true;
        }
        if !self.diff_visible() {
            return changed;
        }
        if self.diff_panel.endpoint.as_ref() != Some(&self.active) {
            // A different machine answers for different repositories.
            self.diff_panel.reset();
            self.diff_panel.endpoint = Some(self.active.clone());
            changed = true;
        }
        let panel = &mut self.diff_panel;
        if panel.polling
            || panel
                .last_poll
                .is_some_and(|at| at.elapsed() < super::activity::scaled(POLL))
        {
            return changed;
        }
        panel.polling = true;
        panel.last_poll = Some(Instant::now());
        let refresh = std::mem::take(&mut panel.needs_refresh);
        let tx = panel.tx.clone();
        let endpoint = self.active.clone();
        let client = self.control_client_for_endpoint(&endpoint);
        std::thread::spawn(move || {
            let result = match client {
                None => Err("Could not reach the daemon".to_string()),
                Some(mut client) => (|| {
                    if refresh {
                        // A stale first list would show last session's files.
                        client.diff_refresh().ok();
                    }
                    let snapshot = client.diff_list()?;
                    let notes = client.diff_notes(None)?;
                    Ok((snapshot, notes))
                })()
                .map_err(|error: pilus_client::Error| error.to_string()),
            };
            tx.send(DiffMsg::Snapshot { endpoint, result }).ok();
        });
        changed
    }

    fn diff_receive(&mut self, message: DiffMsg) {
        match message {
            DiffMsg::Snapshot { endpoint, result } => {
                self.diff_panel.polling = false;
                if self
                    .diff_panel
                    .endpoint
                    .as_ref()
                    .is_some_and(|e| *e != endpoint)
                {
                    return;
                }
                match result {
                    Ok((snapshot, notes)) => self.diff_apply_snapshot(snapshot, notes),
                    Err(error) => self.diff_panel.error = Some(error),
                }
            }
            DiffMsg::File {
                key,
                fingerprint,
                result,
            } => {
                let panel = &mut self.diff_panel;
                if panel.loading.as_ref() == Some(&(key.clone(), fingerprint.clone())) {
                    panel.loading = None;
                }
                if panel.selected.as_ref() != Some(&key) {
                    return;
                }
                match result {
                    Ok(diff) => self.diff_install(key, fingerprint, diff),
                    Err(error) => {
                        panel.loaded = None;
                        self.toast(
                            ToastKind::Error,
                            format!("Could not load {}: {error}", key.1),
                        );
                    }
                }
            }
            DiffMsg::Text {
                key,
                fingerprint,
                result,
            } => {
                let panel = &mut self.diff_panel;
                if panel.md_loading.as_ref() == Some(&(key.clone(), fingerprint.clone())) {
                    panel.md_loading = None;
                }
                // An answer for a file that is no longer open, or an older
                // version of it, would paint the wrong document.
                let current = panel.selected_file().is_some_and(|file| {
                    (file.layer, file.path.as_str()) == (key.0, key.1.as_str())
                        && file.fingerprint == fingerprint
                });
                if !current {
                    return;
                }
                let state = result.map(|file| MdDoc::new(&file, &key.1));
                panel.md = Some(MdText {
                    key,
                    fingerprint,
                    state,
                });
            }
            DiffMsg::Done(result) => {
                self.diff_panel.status.clear();
                match result {
                    Ok(text) => self.toast(ToastKind::Success, text),
                    Err(error) => self.toast(ToastKind::Error, error),
                }
                // Pick the daemon's new truth up now rather than next poll.
                self.diff_panel.last_poll = None;
            }
            DiffMsg::Discarded(result) => {
                let panel = &mut self.diff_panel;
                panel.status.clear();
                if let Err(error) = result {
                    self.toast(ToastKind::Error, error);
                }
                // The file is gone from the list, or Git said why not.
                let panel = &mut self.diff_panel;
                panel.needs_refresh = true;
                panel.last_poll = None;
            }
            DiffMsg::Staged(result) => {
                let panel = &mut self.diff_panel;
                panel.status.clear();
                if let Err(error) = result {
                    panel.follow = None;
                    self.toast(ToastKind::Error, error);
                }
                // The index moved (or Git said why not); either way the
                // list should show what is true now, so ask for a new scan
                // rather than wait out the poll.
                let panel = &mut self.diff_panel;
                panel.needs_refresh = true;
                panel.last_poll = None;
            }
        }
    }

    fn diff_apply_snapshot(&mut self, snapshot: DiffSnapshot, notes: Vec<DiffNote>) {
        let panel = &mut self.diff_panel;
        panel.error = None;
        let changed_notes = panel.notes != notes;
        panel.notes = notes;
        panel.snapshot = Some(snapshot);
        if changed_notes {
            panel.refresh_file_notes();
        }
        // Keep the open file while it exists; a file just staged or unstaged
        // is reopened in its new layer; otherwise start at the first.
        let present = panel.selected_file().is_some();
        if !present {
            let followed = panel.follow.take().and_then(|((_, path), action)| {
                model::layer_after(panel.files(), &path, action).map(|layer| (layer, path))
            });
            let first = || {
                panel
                    .listed()
                    .first()
                    .map(|&at| panel.files()[at].key())
                    .map(|(layer, path)| (layer, path.to_string()))
            };
            panel.selected = followed.or_else(first);
            panel.loaded = None;
            panel.md = None;
            panel.md_loading = None;
            panel.refresh_file_notes();
        }
        self.diff_fetch_if_stale();
    }

    /// Request the selected file's diff when the loaded one is missing or
    /// out of date.
    fn diff_fetch_if_stale(&mut self) {
        self.diff_fetch_text_if_stale();
        let panel = &self.diff_panel;
        let Some(file) = panel.selected_file() else {
            return;
        };
        let want = ((file.layer, file.path.clone()), file.fingerprint.clone());
        let current = panel
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.key == want.0 && loaded.fingerprint == want.1);
        if current || panel.loading.as_ref() == Some(&want) {
            return;
        }
        self.diff_panel.loading = Some(want.clone());
        let tx = self.diff_panel.tx.clone();
        let client = self.control_client_for_endpoint(&self.active);
        std::thread::spawn(move || {
            let (key, fingerprint) = want;
            let result = match client {
                None => Err("Could not reach the daemon".to_string()),
                Some(mut client) => client
                    .diff_get(&key.1, key.0)
                    .map_err(|error| error.to_string()),
            };
            tx.send(DiffMsg::File {
                key,
                fingerprint,
                result,
            })
            .ok();
        });
    }

    /// Read the open markdown file's text when the preview wants a version of
    /// it that is not loaded or on its way. The patch carries only the hunks,
    /// so the document is read where the file lives.
    fn diff_fetch_text_if_stale(&mut self) {
        let panel = &self.diff_panel;
        if !panel.previewing() {
            return;
        }
        let (Some(file), Some(snapshot)) = (panel.selected_file(), panel.snapshot.as_ref()) else {
            return;
        };
        let want = ((file.layer, file.path.clone()), file.fingerprint.clone());
        let current = panel
            .md
            .as_ref()
            .is_some_and(|md| md.key == want.0 && md.fingerprint == want.1);
        if current || panel.md_loading.as_ref() == Some(&want) {
            return;
        }
        let (repo, layer) = (snapshot.repo.clone(), file.layer);
        let target = match self.read_target(&self.active) {
            Ok(target) => target,
            Err(error) => {
                self.diff_panel.md = Some(MdText {
                    key: want.0,
                    fingerprint: want.1,
                    state: Err(error),
                });
                return;
            }
        };
        self.diff_panel.md_loading = Some(want.clone());
        let tx = self.diff_panel.tx.clone();
        std::thread::spawn(move || {
            let (key, fingerprint) = want;
            let absolute = format!("{}/{}", repo.trim_end_matches('/'), key.1);
            let version = match layer {
                // A staged file's diff describes the index, which the working
                // copy may have moved on from.
                DiffLayer::Staged => Version::Index {
                    repo: &repo,
                    path: &key.1,
                },
                _ => Version::Disk(&absolute),
            };
            let result = read_text(target.as_ref(), version);
            tx.send(DiffMsg::Text {
                key,
                fingerprint,
                result,
            })
            .ok();
        });
    }

    fn diff_install(&mut self, key: FileKey, fingerprint: String, diff: FileDiff) {
        let panel = &mut self.diff_panel;
        let same_file = panel
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.key == key);
        let flat = model::flatten(&diff);
        if !same_file {
            // Land on the first real line, not the hunk header above it.
            panel.cursor = Cursor {
                src: usize::from(flat.src.len() > 1),
                side: NoteSide::New,
            };
            panel.anchor = None;
            panel.composer = None;
        } else {
            panel.cursor.src = panel.cursor.src.min(flat.src.len().saturating_sub(1));
        }
        panel.loaded = Some(Loaded {
            key,
            fingerprint,
            diff,
            flat,
            display: Display::default(),
            built: None,
            fresh: !same_file,
        });
    }

    /// Point the Diff at one changed file, as the Files tab does when a click
    /// lands on a file Git reports changed. The selection holds across the
    /// next snapshot because the file is in it.
    pub(super) fn diff_reveal(&mut self, layer: DiffLayer, path: &str, cx: &mut Context<Self>) {
        self.diff_select((layer, path.to_string()), cx);
    }

    fn diff_select(&mut self, key: FileKey, cx: &mut Context<Self>) {
        if self.diff_panel.selected.as_ref() == Some(&key) {
            return;
        }
        let panel = &mut self.diff_panel;
        panel.selected = Some(key);
        panel.follow = None;
        panel.toggled = false;
        panel.loaded = None;
        panel.loading = None;
        panel.md = None;
        panel.md_loading = None;
        panel.composer = None;
        panel.anchor = None;
        panel.confirm_delete = None;
        panel.refresh_file_notes();
        self.diff_fetch_if_stale();
        cx.notify();
    }

    /// Select the file `delta` rows from the current one in list order.
    fn diff_step_file(&mut self, delta: isize, cx: &mut Context<Self>) {
        let panel = &self.diff_panel;
        let visible = panel.listed();
        if visible.is_empty() {
            return;
        }
        let at = panel.selected_file().and_then(|file| {
            visible
                .iter()
                .position(|&i| panel.files()[i].key() == file.key())
        });
        let next = match at {
            Some(at) => (at as isize + delta).clamp(0, visible.len() as isize - 1) as usize,
            None => 0,
        };
        let file = &panel.files()[visible[next]];
        let key = (file.layer, file.path.clone());
        self.diff_pick(key, cx);
    }

    fn diff_toggle_view(&mut self, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        panel.view = panel.view.next();
        panel.menu = None;
        cx.notify();
    }

    fn diff_toggle_folder(&mut self, key: FolderKey, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        if !panel.folded.remove(&key) {
            panel.folded.insert(key.clone());
        }
        panel.folder_toggled = Some(key);
        cx.notify();
    }

    /// Open a file the user chose, as a step the column's Back returns from.
    fn diff_pick(&mut self, key: FileKey, cx: &mut Context<Self>) {
        self.diff_panel.searching = false;
        self.right_navigate(cx, |this, cx| this.diff_select(key, cx));
    }

    /// Fold the open file down to its header, or open it again.
    fn diff_toggle_collapsed(&mut self, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        let Some(key) = panel.selected.clone() else {
            return;
        };
        if !panel.collapsed.remove(&key) {
            panel.collapsed.insert(key);
            // A form inside the folded patch would be left open out of sight.
            panel.composer = None;
            panel.anchor = None;
        }
        panel.toggled = true;
        cx.notify();
    }

    /// Flip between the rendered document and the patch for a markdown file.
    fn diff_toggle_rendered(&mut self, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        panel.rendered = !panel.rendered;
        // The form's place means a patch row in one view and a block in the
        // other, so it does not survive the switch.
        panel.composer = None;
        panel.anchor = None;
        self.diff_fetch_text_if_stale();
        cx.notify();
    }

    /// Bring the laid-out rows up to date with the width and the notes.
    /// Returns whether another frame is wanted to finish a scroll.
    fn diff_sync(&mut self, split: bool) -> bool {
        let panel = &mut self.diff_panel;
        let Some(loaded) = panel.loaded.as_mut() else {
            return false;
        };
        let key = BuildKey {
            split,
            notes_gen: panel.notes_gen,
            composer_after: panel.composer.as_ref().map(|composer| composer.after),
        };
        if loaded.built != Some(key) {
            let notes: Vec<&DiffNote> = panel.file_notes.iter().collect();
            loaded.display = model::build(&loaded.flat, split, &notes, key.composer_after);
            loaded.built = Some(key);
            let count = loaded.display.rows.len();
            if loaded.fresh {
                panel.list.reset(count);
                loaded.fresh = false;
            } else {
                panel.list.splice(0..panel.list.item_count(), count);
            }
            panel.reveal = true;
        }
        if std::mem::take(&mut panel.reveal)
            && let Some(&row) = loaded.display.src_row.get(panel.cursor.src)
        {
            panel.list.scroll_to_reveal_item(row);
        }
        // After the line's own reveal, so the form wins when both are asked
        // for: the form sits below its line and can be taller than the room
        // left under it.
        let mut again = false;
        if panel.reveal_composer > 0 {
            panel.reveal_composer -= 1;
            again = panel.reveal_composer > 0;
            if let Some(row) = loaded
                .display
                .rows
                .iter()
                .position(|row| *row == Row::Composer)
            {
                panel.list.scroll_to_reveal_item(row);
            }
        }
        again
    }

    /// Bring the rendered document's rows up to date with the notes and the
    /// form. Returns whether another frame is wanted to finish a scroll.
    fn diff_sync_preview(&mut self) -> bool {
        use std::hash::{Hash, Hasher};

        let panel = &mut self.diff_panel;
        if !panel.previewing() {
            return false;
        }
        let composing = panel.composer.as_ref().map(|composer| Composing {
            end: composer.range.end,
            editing: composer.editing.as_deref(),
        });
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        panel.notes_gen.hash(&mut hasher);
        composing.hash(&mut hasher);
        let stamp = hasher.finish();
        let Some(md) = panel.md.as_mut().and_then(|md| md.state.as_mut().ok()) else {
            return false;
        };
        md.sync(stamp, &panel.file_notes, composing)
    }

    fn diff_move(&mut self, cursor: Cursor, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        panel.cursor = cursor;
        panel.reveal = true;
        panel.confirm_delete = None;
        cx.notify();
    }

    fn diff_spawn(
        &mut self,
        pending: String,
        call: impl FnOnce(&mut ControlClient) -> pilus_client::Result<String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let tx = self.diff_panel.tx.clone();
        let client = self.control_client_for_endpoint(&self.active);
        self.diff_panel.status = pending;
        std::thread::spawn(move || {
            let result = match client {
                Some(mut client) => call(&mut client).map_err(|error| error.to_string()),
                None => Err("Could not reach the daemon".to_string()),
            };
            tx.send(DiffMsg::Done(result)).ok();
        });
        cx.notify();
    }

    /// Start choosing a range at the cursor.
    fn diff_begin_selection(&mut self, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        let on_line = panel
            .loaded
            .as_ref()
            .and_then(|loaded| loaded.flat.src.get(panel.cursor.src))
            .is_some_and(|src| src.kind != SrcKind::Hunk);
        if !on_line {
            self.toast(ToastKind::Info, "Move onto a line to leave a note");
        } else {
            panel.anchor = Some(panel.cursor.src);
            panel.status = "Extend with j/k, Enter to write, Esc to cancel".to_string();
        }
        cx.notify();
    }

    /// Open the composer over the selected range, or the cursor's line.
    fn diff_open_composer(&mut self, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        let Some(loaded) = panel.loaded.as_ref() else {
            return;
        };
        let anchor = panel.anchor.take().unwrap_or(panel.cursor.src);
        let Some(range) = model::note_range(&loaded.flat, anchor, panel.cursor) else {
            self.toast(ToastKind::Info, "Move onto a line to leave a note");
            cx.notify();
            return;
        };
        panel.composer = Some(Composer {
            range,
            after: anchor.max(panel.cursor.src),
            kind: NoteKind::default(),
            editing: None,
        });
        panel.reveal_composer = 2;
        panel.status.clear();
        self.diff_inputs
            .note
            .update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    /// Open the form on `src` from its hover button. A line inside a range
    /// already selected comments on that whole range; otherwise shift extends
    /// from the line the cursor was on, as shift-click does.
    fn diff_comment_at(
        &mut self,
        src: usize,
        side: NoteSide,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        let panel = &mut self.diff_panel;
        let in_selection = panel.anchor.is_some_and(|anchor| {
            (anchor.min(panel.cursor.src)..=anchor.max(panel.cursor.src)).contains(&src)
        });
        if !in_selection {
            let from = panel.cursor.src;
            panel.cursor = Cursor { src, side };
            panel.anchor = extend.then_some(from);
        }
        self.diff_open_composer(cx);
    }

    /// Open the form over `first..=last` of the rendered file, from a block's
    /// comment button. The notes are on the new side: the rendered text is the
    /// file as it now stands.
    pub(super) fn diff_comment_on_lines(&mut self, first: u32, last: u32, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        panel.sending = None;
        panel.composer = Some(Composer {
            range: NoteRange {
                side: NoteSide::New,
                start: first,
                end: last,
            },
            // Only the patch view reads this, and the form is dropped on the
            // way back to it.
            after: 0,
            kind: NoteKind::default(),
            editing: None,
        });
        if let Some(md) = panel.md.as_mut().and_then(|md| md.state.as_mut().ok()) {
            md.reveal_composer();
        }
        self.diff_inputs
            .note
            .update(cx, |input, cx| input.clear(cx));
        cx.notify();
    }

    fn diff_edit_note(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.diff_panel.note_at_cursor().map(|note| note.id.clone()) else {
            self.toast(ToastKind::Info, "No note on this line");
            cx.notify();
            return;
        };
        self.diff_edit_note_id(&id, cx);
    }

    /// Edit the note `id`, from its card or from the cursor.
    fn diff_edit_note_id(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(note) = self
            .diff_panel
            .file_notes
            .iter()
            .find(|note| note.id == id)
            .cloned()
        else {
            return;
        };
        let after = self.diff_panel.loaded.as_ref().map_or(0, |loaded| {
            loaded
                .flat
                .src
                .iter()
                .position(|src| {
                    src.kind != SrcKind::Hunk
                        && match note.side {
                            NoteSide::Old => src.old_line,
                            NoteSide::New => src.new_line,
                        } == Some(note.end_line)
                })
                .unwrap_or(0)
        });
        self.diff_panel.composer = Some(Composer {
            range: NoteRange {
                side: note.side,
                start: note.start_line,
                end: note.end_line,
            },
            after,
            kind: note.kind,
            editing: Some(note.id.clone()),
        });
        self.diff_panel.reveal_composer = 2;
        if let Some(md) = self
            .diff_panel
            .md
            .as_mut()
            .and_then(|md| md.state.as_mut().ok())
        {
            md.reveal_composer();
        }
        self.diff_inputs
            .note
            .update(cx, |input, cx| input.set_value(note.body, cx));
        cx.notify();
    }

    fn diff_save_note(&mut self, cx: &mut Context<Self>) {
        let body = self.diff_inputs.note.read(cx).value().trim().to_string();
        if body.is_empty() {
            self.toast(ToastKind::Info, "A note needs some text");
            cx.notify();
            return;
        }
        let Some(composer) = self.diff_panel.composer.take() else {
            return;
        };
        let Some((layer, path)) = self.diff_panel.selected.clone() else {
            return;
        };
        match composer.editing {
            Some(id) => self.diff_spawn(
                "Saving note…".to_string(),
                move |client| {
                    client.diff_note_edit(&id, &body)?;
                    Ok("Note saved".to_string())
                },
                cx,
            ),
            None => self.diff_spawn(
                "Adding note…".to_string(),
                move |client| {
                    client.diff_note_add(&path, layer, composer.range, composer.kind, &body)?;
                    Ok("Note added".to_string())
                },
                cx,
            ),
        }
    }

    fn diff_toggle_resolved(&mut self, id: String, resolved: bool, cx: &mut Context<Self>) {
        self.diff_spawn(
            if resolved {
                "Resolving…"
            } else {
                "Reopening…"
            }
            .to_string(),
            move |client| {
                client.diff_note_resolve(&id, resolved)?;
                Ok(if resolved {
                    "Note resolved"
                } else {
                    "Note reopened"
                }
                .to_string())
            },
            cx,
        );
    }

    /// Delete a note, asking for the same key a second time first.
    fn diff_delete_note(&mut self, id: String, cx: &mut Context<Self>) {
        if self.diff_panel.confirm_delete.as_ref() != Some(&id) {
            self.diff_panel.confirm_delete = Some(id);
            self.toast(ToastKind::Info, "Press D again to delete this note");
            cx.notify();
            return;
        }
        self.diff_panel.confirm_delete = None;
        self.diff_spawn(
            "Deleting note…".to_string(),
            move |client| {
                client.diff_note_remove(&id)?;
                Ok("Note deleted".to_string())
            },
            cx,
        );
    }

    fn diff_toggle_viewed(&mut self, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        let Some(fingerprint) = panel.selected_file().map(|file| file.fingerprint.clone()) else {
            return;
        };
        if !panel.viewed.remove(&fingerprint) {
            panel.viewed.insert(fingerprint);
        }
        cx.notify();
    }

    /// Move a file into the index, or back out of it, with Git. A conflicted
    /// file is the user's to resolve first, so it takes neither.
    fn diff_set_staged(&mut self, key: FileKey, action: StageAction, cx: &mut Context<Self>) {
        let panel = &self.diff_panel;
        let Some(snapshot) = panel.snapshot.as_ref() else {
            return;
        };
        let Some(file) = snapshot
            .files
            .iter()
            .find(|file| file.layer == key.0 && file.path == key.1)
        else {
            return;
        };
        let applies = match action {
            StageAction::Stage => matches!(file.layer, DiffLayer::Worktree | DiffLayer::Untracked),
            StageAction::Unstage => file.layer == DiffLayer::Staged,
        };
        if !applies {
            return;
        }
        // A rename is two index entries; moving only the new name would
        // leave the old one deleted on its own.
        let mut paths = vec![file.path.clone()];
        if file.renamed() {
            paths.push(file.old_path.clone());
        }
        let repo = snapshot.repo.clone();
        let target = match self.read_target(&self.active) {
            Ok(target) => target,
            Err(error) => {
                self.toast(ToastKind::Error, error);
                cx.notify();
                return;
            }
        };
        let panel = &mut self.diff_panel;
        if panel.selected.as_ref() == Some(&key) {
            panel.follow = Some((key.clone(), action));
        }
        panel.status = match action {
            StageAction::Stage => format!("Staging {}…", key.1),
            StageAction::Unstage => format!("Unstaging {}…", key.1),
        };
        let tx = panel.tx.clone();
        std::thread::spawn(move || {
            let result = set_staged(target.as_ref(), &repo, &paths, action);
            tx.send(DiffMsg::Staged(result)).ok();
        });
        cx.notify();
    }

    /// Ask before throwing a file's working-tree changes away. Only a file
    /// that is not in the index qualifies: a staged one has Unstage, and a
    /// conflicted one is the user's to resolve.
    fn diff_ask_discard(&mut self, key: FileKey, cx: &mut Context<Self>) {
        let Some(snapshot) = self.diff_panel.snapshot.as_ref() else {
            return;
        };
        let Some(file) = snapshot
            .files
            .iter()
            .find(|file| file.layer == key.0 && file.path == key.1)
        else {
            return;
        };
        let kind = match file.layer {
            DiffLayer::Worktree => Discard::Edits,
            DiffLayer::Untracked => Discard::Untracked,
            DiffLayer::Staged | DiffLayer::Conflict => return,
        };
        let action = super::PromptAction::DiscardChanges {
            endpoint: self.active.clone(),
            repo: snapshot.repo.clone(),
            paths: vec![file.path.clone()],
            kind,
            label: file.path.clone(),
        };
        self.open_menu_prompt(action, "Discard changes", "", "", cx);
    }

    /// Run the confirmed discard off the GUI thread.
    pub(super) fn diff_discard(
        &mut self,
        endpoint: &EndpointId,
        repo: String,
        paths: Vec<String>,
        kind: Discard,
        cx: &mut Context<Self>,
    ) {
        let target = match self.read_target(endpoint) {
            Ok(target) => target,
            Err(error) => {
                self.toast(ToastKind::Error, error);
                cx.notify();
                return;
            }
        };
        let panel = &mut self.diff_panel;
        panel.status = "Discarding changes…".to_string();
        let tx = panel.tx.clone();
        std::thread::spawn(move || {
            let result = discard(target.as_ref(), &repo, &paths, kind);
            tx.send(DiffMsg::Discarded(result)).ok();
        });
        cx.notify();
    }

    /// Stage the open file, or unstage it when it is already in the index.
    fn diff_toggle_staged(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.diff_panel.selected_key() else {
            return;
        };
        let action = match key.0 {
            DiffLayer::Staged => StageAction::Unstage,
            _ => StageAction::Stage,
        };
        self.diff_set_staged(key, action, cx);
    }

    /// Give the search field the keyboard.
    fn diff_begin_search(&mut self, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        panel.searching = true;
        panel.menu = None;
        self.right.focused = true;
        cx.notify();
    }

    /// Hand the keyboard back to the page, keeping what was typed.
    fn diff_end_search(&mut self, cx: &mut Context<Self>) {
        self.diff_panel.searching = false;
        cx.notify();
    }

    /// Empty the search and hand the keyboard back.
    fn diff_clear_search(&mut self, cx: &mut Context<Self>) {
        self.diff_inputs
            .search
            .update(cx, |input, cx| input.clear(cx));
        self.diff_panel.query.clear();
        self.diff_end_search(cx);
    }

    fn diff_refresh_now(&mut self, cx: &mut Context<Self>) {
        self.diff_panel.needs_refresh = true;
        self.diff_panel.last_poll = None;
        self.diff_panel.status = "Refreshing…".to_string();
        cx.notify();
    }

    /// Every live agent, those in the workspace the daemon is showing first.
    ///
    /// Feedback usually belongs to an agent working in the same checkout, but
    /// hiding the rest made the picker look empty whenever the agent lived in
    /// a sibling worktree's workspace.
    pub(super) fn diff_agents(&self) -> Vec<Agent> {
        let workspace = self.active_workspace_id();
        let mut agents = self.agents.clone();
        // A stable sort keeps the daemon's order within each group.
        agents.sort_by_key(|agent| {
            workspace
                .as_deref()
                .is_some_and(|id| agent.workspace_id != id)
        });
        agents
    }

    fn diff_send(&mut self, cx: &mut Context<Self>) {
        let Some(sending) = self.diff_panel.sending.take() else {
            return;
        };
        let agents = self.diff_agents();
        let Some(agent) = agents.get(sending.pick) else {
            self.toast(ToastKind::Error, "No agent to send to");
            cx.notify();
            return;
        };
        let selection = match sending.scope {
            SendScope::Note => match self.diff_panel.note_for_send() {
                Some(note) => NoteSelection::Ids(vec![note.id.clone()]),
                None => {
                    self.toast(ToastKind::Info, "Put the cursor on a note first");
                    cx.notify();
                    return;
                }
            },
            SendScope::File => NoteSelection::Ids(
                self.diff_panel
                    .file_notes
                    .iter()
                    .filter(|note| model::is_open(note))
                    .map(|note| note.id.clone())
                    .collect(),
            ),
            SendScope::Review => NoteSelection::AllOpen,
        };
        if matches!(&selection, NoteSelection::Ids(ids) if ids.is_empty()) {
            self.toast(ToastKind::Info, "No open notes to send");
            cx.notify();
            return;
        }
        let (target, label) = (agent.pane.clone(), agent.label().to_string());
        self.diff_spawn(
            format!("Sending to {label}…"),
            move |client| {
                client.diff_note_send(&target, &selection)?;
                Ok(format!("Sent to {label}"))
            },
            cx,
        );
    }

    /// Keys for the page, after the focused text field has had its turn.
    pub(super) fn diff_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;

        // The search field has typed the character already; only the keys
        // that leave it are the page's.
        if self.diff_panel.searching {
            match key {
                "escape" => self.diff_clear_search(cx),
                "enter" => {
                    // Open the first match when the open file is not one.
                    let panel = &self.diff_panel;
                    let visible = panel.listed();
                    let shown = panel.selected_file().is_some_and(|file| {
                        visible
                            .iter()
                            .any(|&at| panel.files()[at].key() == file.key())
                    });
                    let first = visible
                        .first()
                        .filter(|_| !shown)
                        .map(|&at| (panel.files()[at].layer, panel.files()[at].path.clone()));
                    self.diff_end_search(cx);
                    if let Some(key) = first {
                        self.diff_pick(key, cx);
                    }
                }
                _ => {}
            }
            return;
        }
        if self.diff_panel.menu.is_some() {
            if key == "escape" {
                self.diff_panel.menu = None;
                cx.notify();
            }
            return;
        }
        if self.diff_panel.sending.is_some() {
            self.diff_send_key(key, cx);
            return;
        }
        if let Some(composer) = self.diff_panel.composer.as_mut() {
            match key {
                "escape" => self.diff_panel.composer = None,
                "tab" => composer.kind = model::next_kind(composer.kind),
                "enter" if !mods.shift => self.diff_save_note(cx),
                _ => {}
            }
            cx.notify();
            return;
        }
        if mods.platform || mods.control {
            return;
        }
        // Shifted characters (`J`, `{`) arrive in `key_char`; named keys do
        // not carry one.
        let ch = match (&event.keystroke.key_char, key.chars().count()) {
            (Some(text), 1) => text.as_str(),
            _ => key,
        };
        if self.diff_panel.previewing() {
            self.diff_preview_key(ch, cx);
            return;
        }
        let cursor = self.diff_panel.cursor;
        let (Some(loaded), selecting) = (self.diff_panel.loaded.as_ref(), self.diff_panel.anchor)
        else {
            self.diff_key_without_diff(ch, cx);
            return;
        };
        let moved = match ch {
            "j" | "down" => Some(model::step(&loaded.display, cursor, 1)),
            "k" | "up" => Some(model::step(&loaded.display, cursor, -1)),
            "d" => Some(model::step(&loaded.display, cursor, HALF_PAGE)),
            "u" => Some(model::step(&loaded.display, cursor, -HALF_PAGE)),
            "g" | "home" => Some(model::edge(&loaded.display, cursor, false)),
            "G" | "end" => Some(model::edge(&loaded.display, cursor, true)),
            "left" => Some(model::with_side(&loaded.display, cursor, NoteSide::Old)),
            "right" => Some(model::with_side(&loaded.display, cursor, NoteSide::New)),
            "{" => model::hunk_jump(&loaded.flat, cursor.src, false)
                .map(|src| Cursor { src, ..cursor }),
            "}" => {
                model::hunk_jump(&loaded.flat, cursor.src, true).map(|src| Cursor { src, ..cursor })
            }
            _ => None,
        };
        if let Some(next) = moved {
            self.diff_move(next, cx);
            return;
        }
        if selecting.is_some() {
            match ch {
                "enter" => self.diff_open_composer(cx),
                "escape" => {
                    self.diff_panel.anchor = None;
                    self.diff_panel.status.clear();
                    cx.notify();
                }
                _ => {}
            }
            return;
        }
        match ch {
            "J" => self.diff_step_file(1, cx),
            "K" => self.diff_step_file(-1, cx),
            "t" => self.diff_toggle_view(cx),
            "s" => {
                self.diff_panel.mode = self.diff_panel.mode.next();
                self.diff_panel.reveal = true;
                cx.notify();
            }
            "n" | "c" => self.diff_begin_selection(cx),
            "e" => self.diff_edit_note(cx),
            "x" => {
                if let Some(note) = self.diff_panel.note_at_cursor() {
                    let (id, resolve) = (note.id.clone(), note.state == NoteState::Open);
                    self.diff_toggle_resolved(id, resolve, cx);
                }
            }
            "D" => {
                if let Some(id) = self.diff_panel.note_at_cursor().map(|note| note.id.clone()) {
                    self.diff_delete_note(id, cx);
                }
            }
            "]" | "[" => self.diff_jump_note(ch == "]", cx),
            "m" => self.diff_toggle_viewed(cx),
            "S" => self.diff_toggle_staged(cx),
            "/" => self.diff_begin_search(cx),
            "p" if self.diff_panel.selected_file().is_some_and(previewable) => {
                self.diff_toggle_rendered(cx);
            }
            "f" => {
                self.diff_panel.filter = self.diff_panel.filter.next();
                cx.notify();
            }
            "r" => self.diff_refresh_now(cx),
            "a" => {
                // On a note, send that note; elsewhere, everything open.
                let scope = if self.diff_panel.note_at_cursor().is_some() {
                    SendScope::Note
                } else {
                    SendScope::Review
                };
                self.diff_open_send(scope, cx);
            }
            "escape" | "q" => {
                self.right.focused = false;
                cx.notify();
            }
            _ => {}
        }
    }

    /// Keys while a markdown file is shown as a document. There is no cursor
    /// line to move, so the motion keys scroll; the file-level keys are the
    /// patch's own.
    fn diff_preview_key(&mut self, ch: &str, cx: &mut Context<Self>) {
        const LINE: f32 = 48.0;
        const PAGE: f32 = 320.0;
        if let Some(md) = self
            .diff_panel
            .md
            .as_ref()
            .and_then(|md| md.state.as_ref().ok())
        {
            match ch {
                "j" | "down" => md.scroll_by(LINE),
                "k" | "up" => md.scroll_by(-LINE),
                "d" => md.scroll_by(PAGE),
                "u" => md.scroll_by(-PAGE),
                "g" | "home" => md.scroll_to_edge(false),
                "G" | "end" => md.scroll_to_edge(true),
                _ => {}
            }
        }
        match ch {
            "J" => self.diff_step_file(1, cx),
            "K" => self.diff_step_file(-1, cx),
            "p" => self.diff_toggle_rendered(cx),
            "t" => self.diff_toggle_view(cx),
            "m" => self.diff_toggle_viewed(cx),
            "S" => self.diff_toggle_staged(cx),
            "/" => self.diff_begin_search(cx),
            "f" => {
                self.diff_panel.filter = self.diff_panel.filter.next();
                cx.notify();
            }
            "r" => self.diff_refresh_now(cx),
            "a" => self.diff_open_send(SendScope::Review, cx),
            "escape" | "q" => {
                self.right.focused = false;
                cx.notify();
            }
            _ => {}
        }
        cx.notify();
    }

    /// Keys while no file is open: moving through the list and leaving.
    fn diff_key_without_diff(&mut self, ch: &str, cx: &mut Context<Self>) {
        match ch {
            "j" | "down" | "J" => self.diff_step_file(1, cx),
            "k" | "up" | "K" => self.diff_step_file(-1, cx),
            "t" => self.diff_toggle_view(cx),
            "S" => self.diff_toggle_staged(cx),
            "/" => self.diff_begin_search(cx),
            "f" => {
                self.diff_panel.filter = self.diff_panel.filter.next();
                cx.notify();
            }
            "r" => self.diff_refresh_now(cx),
            "escape" | "q" => {
                self.right.focused = false;
                cx.notify();
            }
            _ => {}
        }
    }

    fn diff_jump_note(&mut self, forward: bool, cx: &mut Context<Self>) {
        let panel = &self.diff_panel;
        let Some(loaded) = panel.loaded.as_ref() else {
            return;
        };
        let mut anchors: Vec<usize> = panel
            .file_notes
            .iter()
            .filter_map(|note| {
                loaded.flat.src.iter().position(|src| {
                    src.kind != SrcKind::Hunk
                        && match note.side {
                            NoteSide::Old => src.old_line,
                            NoteSide::New => src.new_line,
                        } == Some(note.start_line)
                })
            })
            .collect();
        anchors.sort_unstable();
        if let Some(src) = model::next_match(&anchors, panel.cursor.src, forward) {
            let cursor = Cursor {
                src,
                ..panel.cursor
            };
            self.diff_move(cursor, cx);
        }
    }

    /// Open the agent picker. It opens even with no agent running, so the
    /// reason there is nothing to send to is on screen rather than in a footer.
    fn diff_open_send(&mut self, scope: SendScope, cx: &mut Context<Self>) {
        self.diff_panel.composer = None;
        self.diff_panel.sending = Some(Sending {
            scope,
            pick: 0,
            note: None,
        });
        cx.notify();
    }

    /// Open the agent picker for the note `id` alone, from its card.
    fn diff_send_note(&mut self, id: String, cx: &mut Context<Self>) {
        self.diff_panel.composer = None;
        self.diff_panel.sending = Some(Sending {
            scope: SendScope::Note,
            pick: 0,
            note: Some(id),
        });
        cx.notify();
    }

    /// The scopes worth offering now, with the note count each covers: the
    /// note under the cursor, the open notes in this file, and every open
    /// note in the review. A scope with nothing to send is left out, except
    /// the whole review, which stays so the picker is never empty.
    fn send_scopes(&self) -> Vec<(SendScope, usize)> {
        let panel = &self.diff_panel;
        let mut scopes = Vec::new();
        if panel.note_for_send().is_some() {
            scopes.push((SendScope::Note, 1));
        }
        let in_file = panel
            .file_notes
            .iter()
            .filter(|note| model::is_open(note))
            .count();
        if in_file > 0 {
            scopes.push((SendScope::File, in_file));
        }
        scopes.push((SendScope::Review, panel.open_notes_in_review()));
        scopes
    }

    /// One plain sentence on what `scope` would send right now, so the
    /// buttons can stay short.
    fn send_summary(&self, scope: SendScope) -> String {
        let panel = &self.diff_panel;
        let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
        match scope {
            SendScope::Note => match panel.note_for_send() {
                Some(note) => format!("The note on {}", range_label(note)),
                None => "No note selected".to_string(),
            },
            SendScope::File => {
                let open = panel
                    .file_notes
                    .iter()
                    .filter(|note| model::is_open(note))
                    .count();
                let name = panel
                    .selected_file()
                    .map_or("this file", |file| split_path(&file.path).1);
                format!("{} in {name}", plural(open, "open note"))
            }
            SendScope::Review => {
                let open: Vec<&DiffNote> = panel
                    .notes
                    .iter()
                    .filter(|note| model::is_open(note))
                    .collect();
                let files: HashSet<(&str, DiffLayer)> = open
                    .iter()
                    .map(|note| (note.path.as_str(), note.layer))
                    .collect();
                format!(
                    "{} across {}",
                    plural(open.len(), "open note"),
                    plural(files.len(), "file")
                )
            }
        }
    }

    fn diff_send_key(&mut self, key: &str, cx: &mut Context<Self>) {
        let scopes = self.send_scopes();
        let count = self.diff_agents().len();
        if let Some(sending) = self.diff_panel.sending.as_mut() {
            match key {
                "escape" | "q" => self.diff_panel.sending = None,
                "j" | "down" if count > 0 => sending.pick = (sending.pick + 1).min(count - 1),
                "k" | "up" => sending.pick = sending.pick.saturating_sub(1),
                "tab" => {
                    let at = scopes
                        .iter()
                        .position(|(scope, _)| *scope == sending.scope)
                        .unwrap_or(0);
                    if let Some((next, _)) = scopes.get((at + 1) % scopes.len().max(1)) {
                        sending.scope = *next;
                    }
                }
                "enter" => self.diff_send(cx),
                _ => {}
            }
        }
        cx.notify();
    }
}

// Rendering.
impl PilusView {
    /// The page, drawn in place of the terminal card.
    pub(super) fn diff_page(&mut self, page_w: f32, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let show_list = page_w >= LIST_MIN_PAGE;
        self.diff_panel.page_w = page_w;
        let list_w = if show_list {
            self.diff_panel.list_width(&self.shared_settings)
        } else {
            0.0
        };
        let code_w = page_w - list_w;
        // Code is set at the secondary size; a monospace cell is about 0.6em.
        let split = self
            .diff_panel
            .mode
            .splits(code_w - 2.0 * GUTTER, fonts.secondary * 0.6);
        self.diff_inputs.note.update(cx, |input, _| {
            input.set_style(
                theme,
                style::chrome_font(&self.shared_settings),
                px(fonts.body),
            );
        });
        self.diff_inputs.search.update(cx, |input, _| {
            input.set_style(
                theme,
                style::chrome_font(&self.shared_settings),
                px(fonts.body),
            );
        });
        if self.diff_sync(split) | self.diff_sync_preview() {
            cx.notify();
        }

        // Wide: files beside the diff. Narrow (the sidebar): files above it,
        // with a handle between them to drag.
        let mut body = div().flex().min_h(px(0.0)).flex_1();
        body = if show_list {
            body.flex_row()
                .child(self.diff_file_list(Some(list_w), cx))
                .child(self.diff_side_divider(cx))
        } else {
            body.flex_col()
                .child(self.diff_file_list(None, cx))
                .child(self.diff_divider(cx))
        };
        body = body.child(self.diff_viewer(split, cx));

        let footer = (!self.diff_panel.status.is_empty()).then(|| {
            div()
                .px_4()
                .h(px(style::scaled(&self.shared_settings, 28.0)))
                .flex()
                .items_center()
                .flex_shrink_0()
                .border_t_1()
                .border_color(theme.border)
                .text_size(px(fonts.secondary))
                .text_color(theme.muted)
                .child(self.diff_panel.status.clone())
        });

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.0))
            .child(self.diff_header(cx))
            .children(
                self.diff_panel
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| !snapshot.files.is_empty())
                    .then(|| self.diff_search_row(cx)),
            )
            .child(body)
            .children(footer)
            .children(self.diff_send_panel(cx))
    }

    /// One row: what the review holds, then icon controls for refreshing,
    /// the layout, and which files to list.
    fn diff_header(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let panel = &self.diff_panel;
        let summary = match &panel.snapshot {
            Some(snapshot) => {
                let open = panel.open_notes_in_review();
                let mut parts = Vec::new();
                if !snapshot.branch.is_empty() {
                    parts.push(snapshot.branch.clone());
                }
                parts.push(format!("{} changed", snapshot.files.len()));
                if open > 0 {
                    parts.push(format!("{open} open notes"));
                }
                parts.join(" · ")
            }
            None => String::new(),
        };
        let refresh = self
            .diff_icon_button(
                "pilus-diff-refresh",
                include_bytes!("../../assets/icons/refresh-cw.svg"),
                "Refresh".to_string(),
                false,
                false,
            )
            .on_click(cx.listener(|this, _, _, cx| this.diff_refresh_now(cx)));
        // The icon is the view a press switches to.
        let (view_glyph, view_tip) = match panel.view {
            ListView::Flat => (
                include_bytes!("../../assets/icons/list-tree.svg").as_slice(),
                "View as tree",
            ),
            ListView::Tree => (
                include_bytes!("../../assets/icons/list.svg").as_slice(),
                "View as list",
            ),
        };
        let view = self
            .diff_icon_button(
                "pilus-diff-view",
                view_glyph,
                view_tip.to_string(),
                false,
                false,
            )
            .on_click(cx.listener(|this, _, _, cx| this.diff_toggle_view(cx)));
        let layout = self.diff_menu_button(
            DiffMenu::Layout,
            self.diff_icon_button(
                "pilus-diff-layout",
                include_bytes!("../../assets/icons/columns-2.svg"),
                format!("Layout: {}", panel.mode.label()),
                false,
                true,
            ),
            cx,
        );
        let filter = self.diff_menu_button(
            DiffMenu::Filter,
            self.diff_icon_button(
                "pilus-diff-filter",
                include_bytes!("../../assets/icons/list-filter.svg"),
                format!("Show: {}", panel.filter.label()),
                // Lit while a filter hides some files.
                panel.filter != FileFilter::All,
                true,
            ),
            cx,
        );
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
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(summary),
            )
            .child(refresh)
            .child(view)
            .child(layout)
            .child(filter)
    }

    /// The row under the header that narrows the file list by name.
    fn diff_search_row(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let panel = &self.diff_panel;
        let field = style::search_field(
            "pilus-diff-search",
            panel.searching,
            theme,
            style::scaled(&self.shared_settings, style::CONTROL_HEIGHT_SMALL),
            fonts.secondary,
            self.diff_inputs.search.clone(),
        )
        .flex_1()
        .min_w(px(0.0))
        .text_size(px(fonts.body))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| this.diff_begin_search(cx)),
        );
        let clear = (!panel.query.is_empty()).then(|| {
            style::icon_button(
                "pilus-diff-search-clear",
                include_bytes!("../../assets/icons/x.svg"),
                ButtonKind::Ghost,
                theme,
                style::scaled(&self.shared_settings, style::CONTROL_HEIGHT_SMALL),
            )
            .tooltip(super::row_tooltip(
                "Clear search".to_string(),
                theme.panel_fg,
                Vec::new(),
                theme,
            ))
            .on_click(cx.listener(|this, _, _, cx| this.diff_clear_search(cx)))
        });
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
            .children(clear)
    }

    /// A 24px icon control, with a small chevron when it opens a dropdown.
    pub(super) fn diff_icon_button(
        &self,
        id: &'static str,
        glyph: &'static [u8],
        tip: String,
        lit: bool,
        dropdown: bool,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let height = style::scaled(&self.shared_settings, 24.0);
        let mut button = div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap_1()
            .h(px(height))
            .min_w(px(height))
            .flex_shrink_0()
            .radius_md()
            .cursor_pointer()
            .hover_fill(theme.panel_dim, style::RADIUS_MD)
            .child(icon(
                glyph,
                fonts.body,
                if lit { theme.panel_fg } else { theme.muted },
            ));
        if dropdown {
            button = button.px(px(5.0)).child(icon(
                include_bytes!("../../assets/icons/chevron-down.svg"),
                fonts.label,
                theme.muted,
            ));
        }
        button.tooltip(super::row_tooltip(tip, theme.panel_fg, Vec::new(), theme))
    }

    /// `face` made a dropdown button: a press toggles `menu`, which floats
    /// under the button's right edge so it stays inside the narrow column.
    fn diff_menu_button(&self, menu: DiffMenu, face: Stateful<Div>, cx: &mut Context<Self>) -> Div {
        let mut wrap = div().relative().flex_shrink_0().child(
            face.on_click(cx.listener(move |this, _, _, cx| this.diff_toggle_menu(menu, cx))),
        );
        if self.diff_panel.menu == Some(menu) {
            wrap = wrap.child(
                div()
                    .absolute()
                    .top(px(0.0))
                    .right(px(0.0))
                    .child(self.diff_menu_list(menu, cx)),
            );
        }
        wrap
    }

    fn diff_toggle_menu(&mut self, menu: DiffMenu, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        let just_closed = panel
            .menu_dismissed
            .take()
            .is_some_and(|(was, at)| was == menu && at.elapsed() < MENU_DISMISS_WINDOW);
        if just_closed {
            // The outside press already closed it; this click is the same
            // gesture on the button that opened it.
        } else if panel.menu == Some(menu) {
            panel.menu = None;
        } else {
            panel.menu = Some(menu);
        }
        cx.notify();
    }

    /// The open dropdown's options, the current one marked.
    fn diff_menu_list(&self, menu: DiffMenu, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let panel = &self.diff_panel;
        let mut list = super::picker_menu("pilus-diff-menu", theme)
            .w(px(style::scaled(&self.shared_settings, 168.0)))
            .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                let panel = &mut this.diff_panel;
                if let Some(open) = panel.menu.take() {
                    panel.menu_dismissed = Some((open, Instant::now()));
                }
                cx.notify();
            }));
        match menu {
            DiffMenu::Layout => {
                for mode in LayoutMode::ALL {
                    list = list.child(super::picker_row(
                        format!("pilus-diff-layout-{}", mode.label()),
                        mode.label().to_string(),
                        panel.mode == mode,
                        theme,
                        self.chrome_font(),
                        cx.listener(move |this, _, _, cx| {
                            this.diff_panel.mode = mode;
                            this.diff_panel.menu = None;
                            this.diff_panel.reveal = true;
                            cx.notify();
                        }),
                    ));
                }
            }
            DiffMenu::Filter => {
                for filter in FileFilter::ALL {
                    list = list.child(super::picker_row(
                        format!("pilus-diff-filter-{}", filter.label()),
                        format!("Show {}", filter.label().to_lowercase()),
                        panel.filter == filter,
                        theme,
                        self.chrome_font(),
                        cx.listener(move |this, _, _, cx| {
                            this.diff_panel.filter = filter;
                            this.diff_panel.menu = None;
                            cx.notify();
                        }),
                    ));
                }
            }
        }
        deferred(
            anchored()
                .anchor(Anchor::TopRight)
                .offset(point(
                    px(0.0),
                    px(style::scaled(&self.shared_settings, 24.0) + 4.0),
                ))
                .snap_to_window_with_margin(px(8.0))
                .child(style::enter("pilus-diff-menu-enter", list)),
        )
        .with_priority(2)
        .into_any_element()
    }

    /// The sidebar's footer: the way to send the review's open notes to an
    /// agent. It is there only while there are open notes to send.
    pub(super) fn diff_footer(&self, cx: &mut Context<Self>) -> Option<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let open = self.diff_panel.open_notes_in_review();
        if open == 0 {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .h(px(style::scaled(&self.shared_settings, 44.0)))
                .flex_shrink_0()
                .px_3()
                .border_t_1()
                .border_color(theme.border)
                .child(
                    style::button(
                        "pilus-diff-send",
                        format!("Send to agent ({open})"),
                        ButtonKind::Primary,
                        theme,
                        fonts.secondary,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.diff_open_send(SendScope::Review, cx);
                    })),
                ),
        )
    }

    /// The handle between the file list and the diff.
    fn diff_divider(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        div()
            .id("pilus-diff-divider")
            .flex()
            .items_center()
            .h(px(DIVIDER_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .cursor_row_resize()
            .hover_fill(theme.panel_dim, style::RADIUS_SM)
            .child(div().h(px(1.0)).w_full().bg(theme.border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    let panel = &mut this.diff_panel;
                    panel.divider = Some((f32::from(event.position.y), panel.list_h));
                    cx.stop_propagation();
                }),
            )
    }

    /// The handle between the file list and the diff when they sit side by
    /// side, with the hairline the list's edge used to draw.
    fn diff_side_divider(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        div()
            .id("pilus-diff-side-divider")
            .flex()
            .justify_center()
            .w(px(DIVIDER_HEIGHT))
            .h_full()
            .flex_shrink_0()
            .cursor_col_resize()
            .hover_fill(theme.panel_dim, style::RADIUS_SM)
            .child(div().w(px(1.0)).h_full().bg(theme.border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    let shown = this.diff_panel.list_width(&this.shared_settings);
                    this.diff_panel.side_divider = Some((f32::from(event.position.x), shown));
                    cx.stop_propagation();
                }),
            )
    }

    /// Follow a side divider drag; called for every pointer move in the
    /// window.
    pub(super) fn diff_side_divider_move(&mut self, x: f32, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        let Some((start_x, start_width)) = panel.side_divider else {
            return;
        };
        let width = model::clamp_list_width(start_width + (x - start_x), panel.page_w);
        if panel.list_w.is_none_or(|now| (width - now).abs() >= 0.5) {
            panel.list_w = Some(width);
            cx.notify();
        }
    }

    /// Follow a divider drag; called for every pointer move in the window.
    pub(super) fn diff_divider_move(&mut self, y: f32, window_h: f32, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        let Some((start_y, start_height)) = panel.divider else {
            return;
        };
        let height = model::clamp_list_height(start_height + (y - start_y), window_h);
        if (height - panel.list_h).abs() >= 0.5 {
            panel.list_h = height;
            cx.notify();
        }
    }

    /// End a divider drag and keep where it landed.
    pub(super) fn diff_divider_end(&mut self, cx: &mut Context<Self>) {
        let panel = &mut self.diff_panel;
        let dragged_height = panel.divider.take().is_some();
        let dragged_width = panel.side_divider.take().is_some();
        if dragged_height || dragged_width {
            if dragged_height {
                self.settings.diff_list_height = self.diff_panel.list_h;
            }
            if dragged_width {
                self.settings.diff_list_width = self.diff_panel.list_w.unwrap_or(0.0);
            }
            self.settings.save(&self.pilus_root);
            cx.notify();
        }
    }

    pub(super) fn diff_message(&self, text: impl Into<SharedString>) -> Div {
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .px_6()
            .text_center()
            .text_size(px(style::Fonts::resolve(&self.shared_settings).body))
            .text_color(self.theme.muted)
            .child(text.into())
    }

    fn diff_file_list(&self, width: Option<f32>, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let panel = &self.diff_panel;
        let visible = panel.visible();
        let mut column = div().flex().flex_col().gap_1().p_2();
        if panel.snapshot.is_none() {
            for (index, fraction) in [0.7, 0.55, 0.8, 0.5].into_iter().enumerate() {
                column = column.child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(style::scaled(&self.shared_settings, 28.0)))
                        .px_2()
                        .child(style::skeleton(
                            format!("pilus-diff-skeleton-{index}"),
                            theme,
                            gpui::relative(fraction),
                            fonts.body,
                        )),
                );
            }
        }
        if panel.snapshot.is_some() && visible.is_empty() && !panel.files().is_empty() {
            column = column.child(
                div()
                    .px_2()
                    .py_2()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child(if panel.query.trim().is_empty() {
                        "No files match this filter".to_string()
                    } else {
                        format!("No files match “{}”", panel.query.trim())
                    }),
            );
        }
        for layer in DiffLayer::ALL {
            let rows: Vec<&DiffFile> = visible
                .iter()
                .map(|&at| &panel.files()[at])
                .filter(|file| file.layer == layer)
                .collect();
            if rows.is_empty() {
                continue;
            }
            let count = rows.len();
            column = column.child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .px_2()
                    .pt_2()
                    .child(style::section_label(
                        theme,
                        layer.label().to_uppercase(),
                        fonts.label,
                    ))
                    .child(
                        div()
                            .text_size(px(fonts.label))
                            .text_color(theme.muted)
                            .child(count.to_string()),
                    ),
            );
            match panel.view {
                ListView::Flat => {
                    for file in rows {
                        column = column.child(self.diff_file_row(file, None, cx));
                    }
                }
                ListView::Tree => {
                    for row in panel.tree(layer, &visible) {
                        column = column.child(match row {
                            TreeRow::Folder {
                                path,
                                name,
                                depth,
                                open,
                            } => self.diff_folder_row(layer, path, name, depth, open, cx),
                            TreeRow::File { index, depth } => self
                                .diff_file_row(&panel.files()[index], Some(depth), cx)
                                .into_any_element(),
                        });
                    }
                }
            }
        }
        let list = div()
            .id("pilus-diff-files")
            .flex_shrink_0()
            .overflow_y_scroll();
        let list = match width {
            Some(width) => list.w(px(width)).h_full(),
            // Above the diff: the height the divider was dragged to. The
            // divider draws the hairline below it.
            None => list.w_full().h(px(panel.list_h)),
        };
        list.border_color(theme.border).child(column)
    }

    /// A folder in the tree: a press folds it shut or opens it again.
    fn diff_folder_row(
        &self,
        layer: DiffLayer,
        path: String,
        name: String,
        depth: u16,
        open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key: FolderKey = (layer, path);
        style::tree_row(
            format!("pilus-diff-folder-{}-{}", layer.wire(), key.1),
            style::TreeEntry {
                depth,
                name: name.into(),
                folder: Some(open),
                animate: self.diff_panel.folder_toggled.as_ref() == Some(&key),
                mark: None,
                selected: false,
            },
            self.theme,
            &self.shared_settings,
        )
        .on_click(cx.listener(move |this, _, _, cx| this.diff_toggle_folder(key.clone(), cx)))
        .into_any_element()
    }

    /// One file. In the tree it sits at `depth` under its folder and drops the
    /// directory the folders above already say.
    fn diff_file_row(
        &self,
        file: &DiffFile,
        depth: Option<u16>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let panel = &self.diff_panel;
        let key: FileKey = (file.layer, file.path.clone());
        let selected = panel.selected.as_ref() == Some(&key);
        let rest: gpui::Hsla = if selected {
            theme.panel.into()
        } else {
            gpui::transparent_black()
        };
        let hover: gpui::Hsla = if selected {
            theme.panel.into()
        } else {
            theme.panel_dim.into()
        };
        let (dir, name) = split_path(&file.path);
        let dir = if depth.is_some() { "" } else { dir };
        let (glyph, status_color) = status_glyph(theme, file.layer, &file.status);
        let viewed = panel.is_viewed(file);
        let mut meta = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0();
        if file.notes > 0 {
            meta = meta.child(style::chip(
                theme,
                format!("{} notes", file.notes),
                fonts.label,
            ));
        }
        if let Some(additions) = file.additions.filter(|n| *n > 0) {
            meta = meta.child(
                div()
                    .text_size(px(fonts.label))
                    .text_color(theme.ok)
                    .child(format!("+{additions}")),
            );
        }
        if let Some(deletions) = file.deletions.filter(|n| *n > 0) {
            meta = meta.child(
                div()
                    .text_size(px(fonts.label))
                    .text_color(theme.alert)
                    .child(format!("−{deletions}")),
            );
        }
        if viewed {
            meta = meta.child(icon(
                include_bytes!("../../assets/icons/check.svg"),
                fonts.label,
                theme.muted,
            ));
        }
        let stage = match file.layer {
            DiffLayer::Staged => Some((
                StageAction::Unstage,
                include_bytes!("../../assets/icons/minus.svg").as_slice(),
                "Unstage",
            )),
            DiffLayer::Worktree | DiffLayer::Untracked => Some((
                StageAction::Stage,
                include_bytes!("../../assets/icons/plus.svg").as_slice(),
                "Stage",
            )),
            DiffLayer::Conflict => None,
        };
        let stage = stage.map(|(action, glyph, tip)| {
            let key = key.clone();
            style::icon_button(
                format!("pilus-diff-stage-{}-{}", file.layer.wire(), file.path),
                glyph,
                ButtonKind::Ghost,
                theme,
                style::scaled(&self.shared_settings, style::CONTROL_HEIGHT_SMALL),
            )
            .tooltip(super::row_tooltip(
                tip.to_string(),
                theme.panel_fg,
                Vec::new(),
                theme,
            ))
            // The row beneath opens the file; this only moves it.
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.diff_set_staged(key.clone(), action, cx);
            }))
        });
        let discard = matches!(file.layer, DiffLayer::Worktree | DiffLayer::Untracked).then(|| {
            let key = key.clone();
            style::icon_button(
                format!("pilus-diff-discard-{}-{}", file.layer.wire(), file.path),
                include_bytes!("../../assets/icons/undo-2.svg").as_slice(),
                ButtonKind::Ghost,
                theme,
                style::scaled(&self.shared_settings, style::CONTROL_HEIGHT_SMALL),
            )
            .tooltip(super::row_tooltip(
                "Discard changes".to_string(),
                theme.panel_fg,
                Vec::new(),
                theme,
            ))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.diff_ask_discard(key.clone(), cx);
            }))
        });
        div()
            .id(format!(
                "pilus-diff-file-{}-{}",
                file.layer.wire(),
                file.path
            ))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(style::scaled(&self.shared_settings, 32.0)))
            // Margin, not padding, so the hover fill starts at the file and
            // not at the folder's edge. The 8px is the row's own `px_2`.
            .ml(px(
                depth.map_or(0.0, |depth| style::tree_glyph_x(depth) - 8.0)
            ))
            .px_2()
            .radius_md()
            .cursor_pointer()
            .state_fill(rest, hover, style::RADIUS_MD)
            .child(
                div()
                    .w(px(style::scaled(&self.shared_settings, 12.0)))
                    .flex_shrink_0()
                    .font_family(SharedString::from(style::mono_family(
                        &self.shared_settings,
                    )))
                    .text_size(px(fonts.secondary))
                    .text_color(status_color)
                    .child(glyph.to_string()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_w(px(0.0))
                    .gap_2()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(fonts.body))
                            .font_weight(if selected {
                                FontWeight::BOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(if viewed { theme.muted } else { theme.panel_fg })
                            .child(name.to_string()),
                    )
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.muted)
                            .child(dir.to_string()),
                    ),
            )
            .child(meta)
            .children(discard)
            .children(stage)
            .on_click(cx.listener(move |this, _, _, cx| this.diff_pick(key.clone(), cx)))
    }

    fn diff_viewer(&self, split: bool, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let panel = &self.diff_panel;
        let column = div().flex().flex_col().flex_1().min_w(px(0.0)).h_full();
        if let Some(error) = &panel.error
            && panel.snapshot.is_none()
        {
            return column.child(self.diff_message(format!("Could not read changes: {error}")));
        }
        let Some(snapshot) = &panel.snapshot else {
            return column.child(self.diff_message("Reading changes…"));
        };
        if snapshot.files.is_empty() {
            return column.child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(icon(
                        include_bytes!("../../assets/icons/git-compare.svg"),
                        fonts.headline,
                        theme.muted,
                    ))
                    .child(style::title(theme, "No changes", fonts.title))
                    .child(
                        div()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.muted)
                            .child("Staged, working-tree, and untracked changes appear here."),
                    ),
            );
        }
        let Some(file) = panel.selected_file() else {
            return column.child(self.diff_message("Nothing matches this filter"));
        };
        let head = self.diff_file_head(file, cx);
        if panel.collapsed.contains(&(file.layer, file.path.clone())) {
            return column.child(head);
        }
        if panel.previewing() {
            return column.child(head).child(self.diff_preview(cx));
        }
        let Some(loaded) = &panel.loaded else {
            return column.child(head).child(self.diff_message(if file.binary {
                "Binary file".to_string()
            } else {
                "Loading diff…".to_string()
            }));
        };
        if loaded.diff.hunks.is_empty() {
            let text = if file.binary {
                "Binary file: no text diff"
            } else if file.layer == DiffLayer::Conflict {
                "Unresolved conflict: resolve it in the file"
            } else {
                "No textual changes"
            };
            return column.child(head).child(self.diff_message(text));
        }
        let mut body = div().flex().flex_col().flex_1().min_h(px(0.0));
        if loaded.diff.truncated {
            body = body.child(
                div()
                    .px_4()
                    .py_1()
                    .flex_shrink_0()
                    .text_size(px(fonts.label))
                    .text_color(theme.alert)
                    .child(format!(
                        "TRUNCATED: the daemon bounded this patch{}",
                        if loaded.diff.omitted_lines > 0 {
                            format!(" and dropped {} lines", loaded.diff.omitted_lines)
                        } else {
                            String::new()
                        }
                    )),
            );
        }
        let rows = list(
            panel.list.clone(),
            cx.processor(move |this: &mut PilusView, ix: usize, _window, cx| {
                this.diff_row(ix, split, cx)
            }),
        )
        .flex_1()
        .min_h(px(0.0))
        .w_full();
        column.child(head).child(body.child(rows))
    }

    /// The open markdown file as a document, with its review notes in place.
    fn diff_preview(&self, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let body = div().flex().flex_col().flex_1().min_h(px(0.0));
        let Some(md) = &self.diff_panel.md else {
            return body.child(self.diff_message("Loading preview…"));
        };
        let wrong_file = self.diff_panel.selected.as_ref() != Some(&md.key);
        let doc = match &md.state {
            _ if wrong_file => return body.child(self.diff_message("Loading preview…")),
            Err(error) => {
                return body.child(self.diff_message(format!("Could not read this file: {error}")));
            }
            Ok(doc) => doc,
        };
        body.children(doc.truncated.then(|| {
            div()
                .px_4()
                .py_1()
                .flex_shrink_0()
                .text_size(px(fonts.label))
                .text_color(theme.alert)
                .child("TRUNCATED: only the start of this file is shown")
        }))
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .child(self.md_list(Page::Diff, cx)),
        )
    }

    /// The bar over the patch: the open file, how much changed, whether it was
    /// viewed, and the way to comment. A long path gives way before the name
    /// does, so the file stays readable.
    fn diff_file_head(&self, file: &DiffFile, cx: &mut Context<Self>) -> Div {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let panel = &self.diff_panel;
        let viewed = panel.is_viewed(file);
        let (dir, name) = split_path(&file.path);
        let mut title = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .font_family(SharedString::from(style::mono_family(
                &self.shared_settings,
            )))
            .text_size(px(fonts.secondary));
        if !dir.is_empty() {
            title = title.child(
                div()
                    .min_w(px(0.0))
                    .truncate()
                    .text_color(theme.muted)
                    .child(format!("{dir}/")),
            );
        }
        title = title.child(
            div()
                .flex_shrink_0()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.panel_fg)
                .child(name.to_string()),
        );
        let mut counts = div().flex().flex_row().gap_2().flex_shrink_0();
        if let Some(additions) = file.additions {
            counts = counts.child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.ok)
                    .child(format!("+{additions}")),
            );
        }
        if let Some(deletions) = file.deletions {
            counts = counts.child(
                div()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.alert)
                    .child(format!("−{deletions}")),
            );
        }
        let comment = self
            .diff_icon_button(
                "pilus-diff-comment",
                include_bytes!("../../assets/icons/message-square.svg"),
                "Comment".to_string(),
                false,
                false,
            )
            // The composer takes the selected range, or the cursor's line
            // when none is selected; starting a selection here would drop a
            // range made with shift-click.
            .on_click(cx.listener(|this, _, _, cx| this.diff_open_composer(cx)));
        // A markdown file reads either way; a document has its own comment
        // button on every block, so the patch's is left out there.
        let rendering = previewable(file).then(|| {
            div()
                .flex()
                .flex_row()
                .gap_1()
                .flex_shrink_0()
                .child(
                    style::small_button(
                        "pilus-diff-view-preview",
                        "Preview",
                        if panel.rendered {
                            ButtonKind::Secondary
                        } else {
                            ButtonKind::Ghost
                        },
                        theme,
                        fonts.secondary,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        if !this.diff_panel.rendered {
                            this.diff_toggle_rendered(cx);
                        }
                    })),
                )
                .child(
                    style::small_button(
                        "pilus-diff-view-patch",
                        "Diff",
                        if panel.rendered {
                            ButtonKind::Ghost
                        } else {
                            ButtonKind::Secondary
                        },
                        theme,
                        fonts.secondary,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.diff_panel.rendered {
                            this.diff_toggle_rendered(cx);
                        }
                    })),
                )
        });
        let showing_document = panel.previewing();
        let open = !panel.collapsed.contains(&(file.layer, file.path.clone()));
        // The chevron and name fold the file; the controls beside them keep
        // their own clicks.
        let fold = div()
            .id("pilus-diff-file-fold")
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .cursor_pointer()
            .child(style::chevron(
                "pilus-diff-file-chevron",
                open,
                panel.toggled,
                fonts.secondary,
                theme.muted,
            ))
            .child(title)
            .on_click(cx.listener(|this, _, _, cx| this.diff_toggle_collapsed(cx)));
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h(px(style::scaled(&self.shared_settings, 36.0)))
            .flex_shrink_0()
            .px_3()
            .border_b_1()
            .border_color(theme.border)
            .child(fold)
            .child(counts)
            .children(rendering)
            .child(
                style::checkbox(
                    "pilus-diff-viewed",
                    viewed,
                    "Viewed",
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(|this, _, _, cx| this.diff_toggle_viewed(cx))),
            )
            .children((!showing_document).then_some(comment))
    }

    /// One virtualized row of the open diff.
    fn diff_row(&mut self, ix: usize, split: bool, cx: &mut Context<Self>) -> AnyElement {
        let panel = &self.diff_panel;
        let Some(loaded) = &panel.loaded else {
            return div().into_any_element();
        };
        let Some(&row) = loaded.display.rows.get(ix) else {
            return div().into_any_element();
        };
        let _ = split;
        match row {
            Row::Hunk(src) => self.diff_hunk_row(ix, &loaded.flat.src[src].text),
            Row::Stack(src) => self.diff_stack_row(ix, src, cx),
            Row::Split { left, right } => self.diff_split_row(ix, left, right, cx),
            Row::Note(index) => match panel.file_notes.get(index) {
                Some(note) => self.diff_note_card(
                    note,
                    style::scaled(&self.shared_settings, GUTTER) * 2.0,
                    cx,
                ),
                None => div().into_any_element(),
            },
            Row::Composer => {
                self.diff_composer(style::scaled(&self.shared_settings, GUTTER) * 2.0, cx)
            }
        }
    }

    fn code_style(&self) -> (f32, SharedString) {
        let fonts = style::Fonts::resolve(&self.shared_settings);
        (
            fonts.secondary,
            SharedString::from(style::mono_family(&self.shared_settings)),
        )
    }

    pub(super) fn diff_hunk_row(&self, ix: usize, header: &str) -> AnyElement {
        let theme = self.theme;
        let (size, family) = self.code_style();
        div()
            .id(("pilus-diff-hunk", ix))
            .w_full()
            .px_4()
            .py_1()
            .mt_1()
            .bg(theme.panel_dim)
            .font_family(family)
            .text_size(px(size))
            .text_color(theme.muted)
            .child(header.to_string())
            .into_any_element()
    }

    /// The wash behind a line and the color of its marker.
    fn change_colors(&self, kind: SrcKind) -> (Option<gpui::Rgba>, gpui::Rgba, &'static str) {
        let theme = self.theme;
        match kind {
            SrcKind::Addition => (Some(style::tint(theme.ok, CHANGE_WASH)), theme.ok, "+"),
            SrcKind::Deletion => (
                Some(style::tint(theme.alert, CHANGE_WASH)),
                theme.alert,
                "−",
            ),
            _ => (None, theme.muted, " "),
        }
    }

    pub(super) fn line_number(&self, number: Option<u32>) -> Div {
        let (size, family) = self.code_style();
        div()
            .w(px(style::scaled(&self.shared_settings, GUTTER)))
            .flex_shrink_0()
            .pr_2()
            .flex()
            .justify_end()
            .font_family(family)
            .text_size(px(size))
            .text_color(self.theme.muted)
            .child(number.map(|n| n.to_string()).unwrap_or_default())
    }

    /// One side of a line: marker and text, with an optional wash.
    pub(super) fn code_cell(&self, src: &Src) -> Div {
        let theme = self.theme;
        let (size, family) = self.code_style();
        let (wash, marker_color, marker) = self.change_colors(src.kind);
        let mut cell = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_w(px(0.0))
            .font_family(family)
            .text_size(px(size))
            .text_color(theme.panel_fg);
        if let Some(wash) = wash {
            cell = cell.bg(wash);
        }
        cell.child(
            div()
                .w(px(style::scaled(&self.shared_settings, 14.0)))
                .flex_shrink_0()
                .text_color(marker_color)
                .child(marker),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                // An empty line still takes a full row.
                .child(if src.text.is_empty() {
                    StyledText::new("\u{a0}")
                } else {
                    StyledText::new(src.text.clone()).with_highlights(
                        src.spans
                            .iter()
                            .map(|span| (span.range.clone(), self.token_style(span.token))),
                    )
                }),
        )
    }

    /// The text style a syntax role takes in the active theme.
    fn token_style(&self, token: Token) -> HighlightStyle {
        let look = highlight::look(token, self.theme);
        HighlightStyle {
            color: Some(look.color.into()),
            font_style: look.italic.then_some(FontStyle::Italic),
            font_weight: look.bold.then_some(FontWeight::BOLD),
            ..HighlightStyle::default()
        }
    }

    /// The wash for rows inside the range being selected for a note.
    fn range_fill(&self, srcs: &[usize]) -> gpui::Hsla {
        let panel = &self.diff_panel;
        let in_range = panel.anchor.is_some_and(|anchor| {
            let (low, high) = (anchor.min(panel.cursor.src), anchor.max(panel.cursor.src));
            srcs.iter().any(|src| (low..=high).contains(src))
        });
        if in_range {
            style::tint(self.theme.selection_bg, 0.6).into()
        } else {
            gpui::transparent_black()
        }
    }

    /// The wash for a row: a selected range, else the cursor line.
    fn cursor_fill(&self, srcs: &[usize]) -> gpui::Hsla {
        if self.diff_panel.anchor.is_none() && srcs.contains(&self.diff_panel.cursor.src) {
            return style::tint(self.theme.panel_fg, 0.08).into();
        }
        self.range_fill(srcs)
    }

    /// The `+` that appears over a line's number while the row is hovered.
    ///
    /// It sits in the gutter, so pressing it never reaches the row's own
    /// click, which only moves the cursor.
    fn diff_add_button(
        &self,
        id: usize,
        group: SharedString,
        src: usize,
        side: NoteSide,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let size = style::scaled(&self.shared_settings, 14.0);
        div()
            .id(("pilus-diff-add", id))
            .absolute()
            .left(px(2.0))
            .top(px(1.0))
            .w(px(size))
            .h(px(size))
            .flex()
            .items_center()
            .justify_center()
            .radius_sm()
            .bg(theme.accent)
            .cursor_pointer()
            .opacity(0.0)
            .group_hover(group, |style| style.opacity(1.0))
            .child(icon(
                include_bytes!("../../assets/icons/plus.svg"),
                size * 0.7,
                theme.window_bg,
            ))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.diff_comment_at(src, side, event.modifiers.shift, cx);
                    cx.stop_propagation();
                }),
            )
    }

    fn diff_stack_row(&self, ix: usize, src: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(loaded) = &self.diff_panel.loaded else {
            return div().into_any_element();
        };
        let line = &loaded.flat.src[src];
        let side = model::natural_side(line);
        let group = SharedString::from(format!("pilus-diff-line-{ix}"));
        div()
            .id(("pilus-diff-row", ix))
            .group(group.clone())
            .flex()
            .flex_row()
            .w_full()
            .state_fill(
                self.cursor_fill(&[src]),
                style::tint(self.theme.panel_fg, 0.05),
                style::Radius::None,
            )
            .child(self.line_number(line.old_line))
            .child(self.line_number(line.new_line))
            .child(self.code_cell(line))
            .child(self.diff_add_button(ix * 2, group, src, side, cx))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                let cursor = Cursor { src, side };
                if !event.modifiers().shift {
                    // A plain click starts over; shift-click extends from
                    // where the cursor was.
                    this.diff_panel.anchor = None;
                } else if this.diff_panel.anchor.is_none() {
                    this.diff_panel.anchor = Some(this.diff_panel.cursor.src);
                }
                this.diff_move(cursor, cx);
            }))
            .into_any_element()
    }

    fn diff_split_row(
        &self,
        ix: usize,
        left: Option<usize>,
        right: Option<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(loaded) = &self.diff_panel.loaded else {
            return div().into_any_element();
        };
        let theme = self.theme;
        let cursor = self.diff_panel.cursor;
        let covers: Vec<usize> = left.into_iter().chain(right).collect();
        let half = |src: Option<usize>,
                    side: NoteSide,
                    number: fn(&Src) -> Option<u32>,
                    cx: &mut Context<Self>| {
            let group = SharedString::from(format!(
                "pilus-diff-line-{ix}-{}",
                if side == NoteSide::Old { "old" } else { "new" }
            ));
            let mut cell = div()
                .id((
                    "pilus-diff-half",
                    ix * 2 + usize::from(side == NoteSide::New),
                ))
                .group(group.clone())
                .relative()
                .flex()
                .flex_row()
                .flex_1()
                .min_w(px(0.0));
            match src.map(|at| (at, &loaded.flat.src[at])) {
                Some((at, line)) => {
                    let active = cursor.src == at && cursor.side == side;
                    cell = cell
                        .child(self.line_number(number(line)))
                        .child(self.code_cell(line))
                        .child(self.diff_add_button(
                            ix * 2 + usize::from(side == NoteSide::New) + 1,
                            group,
                            at,
                            side,
                            cx,
                        ));
                    if active {
                        cell = cell.bg(style::tint(theme.panel_fg, 0.08));
                    }
                }
                None => {
                    cell = cell
                        .child(self.line_number(None))
                        .child(div().flex_1().bg(style::tint(theme.panel_dim, 0.6)));
                }
            }
            cell
        };
        div()
            .id(("pilus-diff-row", ix))
            .flex()
            .flex_row()
            .w_full()
            .child(half(left, NoteSide::Old, |src| src.old_line, cx))
            .child(
                div()
                    .w(px(1.0))
                    .flex_shrink_0()
                    .self_stretch()
                    .bg(theme.border),
            )
            .child(half(right, NoteSide::New, |src| src.new_line, cx))
            .state_fill(
                self.range_fill(&covers),
                style::tint(theme.panel_fg, 0.03),
                style::Radius::None,
            )
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                // A press lands on the half it is over; without hit-testing
                // per half, take the new side unless only the old exists.
                let (src, side) = match (left, right) {
                    (_, Some(src)) => (src, NoteSide::New),
                    (Some(src), None) => (src, NoteSide::Old),
                    (None, None) => return,
                };
                if !event.modifiers().shift {
                    // A plain click starts over; shift-click extends from
                    // where the cursor was.
                    this.diff_panel.anchor = None;
                } else if this.diff_panel.anchor.is_none() {
                    this.diff_panel.anchor = Some(this.diff_panel.cursor.src);
                }
                this.diff_move(Cursor { src, side }, cx);
            }))
            .into_any_element()
    }

    /// A note's card, set in by `indent` so it lines up under the code of a
    /// patch; a document has no gutter and passes 0.
    pub(super) fn diff_note_card(
        &self,
        note: &DiffNote,
        indent: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let id = note.id.clone();
        let confirming = self.diff_panel.confirm_delete.as_deref() == Some(note.id.as_str());
        let dim = note.state != NoteState::Open;
        let mut head = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(style::chip(theme, note.kind.label(), fonts.label))
            .child(
                div()
                    .font_family(SharedString::from(style::mono_family(
                        &self.shared_settings,
                    )))
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child(range_label(note)),
            );
        if note.state != NoteState::Open {
            head = head.child(style::chip(theme, note.state.label(), fonts.label));
        }
        if note.deliveries > 0 {
            head = head.child(
                div()
                    .text_size(px(fonts.label))
                    .text_color(theme.muted)
                    .child(format!("sent {}×", note.deliveries)),
            );
        }
        let resolved = note.state == NoteState::Resolved;
        let (edit_id, resolve_id, delete_id, send_id) =
            (id.clone(), id.clone(), id.clone(), id.clone());
        let actions = div()
            .flex()
            .flex_row()
            .gap_1()
            .child(
                style::small_button(
                    format!("pilus-diff-note-send-{id}"),
                    "Send",
                    ButtonKind::Secondary,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.diff_send_note(send_id.clone(), cx);
                })),
            )
            .child(
                style::small_button(
                    format!("pilus-diff-note-edit-{id}"),
                    "Edit",
                    ButtonKind::Ghost,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.diff_edit_note_id(&edit_id, cx);
                })),
            )
            .child(
                style::small_button(
                    format!("pilus-diff-note-resolve-{id}"),
                    if resolved { "Reopen" } else { "Resolve" },
                    ButtonKind::Ghost,
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.diff_toggle_resolved(resolve_id.clone(), !resolved, cx);
                })),
            )
            .child(
                style::small_button(
                    format!("pilus-diff-note-delete-{id}"),
                    if confirming {
                        "Confirm delete"
                    } else {
                        "Delete"
                    },
                    if confirming {
                        ButtonKind::Danger
                    } else {
                        ButtonKind::Ghost
                    },
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.diff_delete_note(delete_id.clone(), cx);
                })),
            );
        div()
            .w_full()
            .px_4()
            .py_1()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .ml(px(indent))
                    .p_2()
                    .radius_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.panel_dim)
                    .child(head)
                    .child(
                        div()
                            .text_size(px(fonts.body))
                            .text_color(if dim { theme.muted } else { theme.panel_fg })
                            .child(note.body.clone()),
                    )
                    // Below the text, not beside the chips: the sidebar is too
                    // narrow for both on one line.
                    .child(div().flex().flex_row().justify_end().child(actions)),
            )
            .into_any_element()
    }

    pub(super) fn diff_composer(&self, indent: f32, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let Some(composer) = &self.diff_panel.composer else {
            return div().into_any_element();
        };
        let mut kinds = div().flex().flex_row().gap_1();
        for kind in NoteKind::ALL {
            kinds = kinds.child(
                style::small_button(
                    format!("pilus-diff-kind-{}", kind.wire()),
                    kind.label(),
                    if composer.kind == kind {
                        ButtonKind::Secondary
                    } else {
                        ButtonKind::Ghost
                    },
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(composer) = this.diff_panel.composer.as_mut() {
                        composer.kind = kind;
                    }
                    cx.notify();
                })),
            );
        }
        let side = match composer.range.side {
            NoteSide::Old => 'L',
            NoteSide::New => 'R',
        };
        let range = if composer.range.start == composer.range.end {
            format!("{side}{}", composer.range.start)
        } else {
            format!("{side}{}–{}", composer.range.start, composer.range.end)
        };
        div()
            .w_full()
            .px_4()
            .py_1()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .ml(px(indent))
                    .p_2()
                    .radius_md()
                    .border_1()
                    .border_color(theme.border_focus)
                    .bg(theme.panel_dim)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(fonts.label))
                                    .text_color(theme.muted)
                                    .child(range),
                            )
                            .child(kinds),
                    )
                    .child(
                        div()
                            .min_h(px(style::scaled(&self.shared_settings, 56.0)))
                            .child(self.diff_inputs.note.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                style::small_button(
                                    "pilus-diff-note-save",
                                    "Save",
                                    ButtonKind::Primary,
                                    theme,
                                    fonts.secondary,
                                )
                                .on_click(cx.listener(|this, _, _, cx| this.diff_save_note(cx))),
                            )
                            .child(
                                style::small_button(
                                    "pilus-diff-note-cancel",
                                    "Cancel",
                                    ButtonKind::Ghost,
                                    theme,
                                    fonts.secondary,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.diff_panel.composer = None;
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                div()
                                    .text_size(px(fonts.label))
                                    .text_color(theme.muted)
                                    .child("Enter saves · Tab: kind"),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// The agent picker for sending notes, floating over the page.
    fn diff_send_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sending = self.diff_panel.sending.as_ref()?;
        let theme = self.theme;
        let fonts = style::Fonts::resolve(&self.shared_settings);
        let mut scopes = div().flex().flex_row().flex_wrap().gap_1();
        for (scope, _) in self.send_scopes() {
            scopes = scopes.child(
                style::small_button(
                    format!("pilus-diff-scope-{}", scope.label()),
                    scope.label(),
                    if sending.scope == scope {
                        ButtonKind::Secondary
                    } else {
                        ButtonKind::Ghost
                    },
                    theme,
                    fonts.secondary,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(sending) = this.diff_panel.sending.as_mut() {
                        sending.scope = scope;
                    }
                    cx.notify();
                })),
            );
        }
        let workspace = self.active_workspace_id();
        let mut agents = div().flex().flex_col().gap_1();
        for (index, agent) in self.diff_agents().iter().enumerate() {
            let selected = index == sending.pick;
            let rest: gpui::Hsla = if selected {
                theme.panel.into()
            } else {
                gpui::transparent_black()
            };
            agents = agents.child(
                div()
                    .id(("pilus-diff-agent", index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(style::scaled(&self.shared_settings, 28.0)))
                    .px_2()
                    .radius_md()
                    .cursor_pointer()
                    .state_fill(rest, theme.panel_dim, style::RADIUS_MD)
                    .child(icon(agent_icon(&agent.agent), fonts.body, theme.muted))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(fonts.body))
                            .font_weight(if selected {
                                FontWeight::BOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .text_color(theme.panel_fg)
                            .child(agent.label().to_string()),
                    )
                    .children(
                        (!agent.workspace_name.is_empty()
                            && Some(agent.workspace_id.as_str()) != workspace.as_deref())
                        .then(|| {
                            div()
                                .text_size(px(fonts.label))
                                .text_color(theme.muted)
                                .child(agent.workspace_name.clone())
                        }),
                    )
                    .child(
                        div()
                            .text_size(px(fonts.label))
                            .text_color(theme.muted)
                            .child(agent.status.clone()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(sending) = this.diff_panel.sending.as_mut() {
                            sending.pick = index;
                        }
                        this.diff_send(cx);
                    })),
            );
        }
        if self.diff_agents().is_empty() {
            agents = agents.child(
                div()
                    .px_2()
                    .text_size(px(fonts.secondary))
                    .text_color(theme.muted)
                    .child("No live agent. Start one in a pane, then send."),
            );
        }
        Some(
            style::enter(
                "pilus-diff-send",
                div()
                    .absolute()
                    .right(px(12.0))
                    .left(px(12.0))
                    .bottom(px(8.0))
                    .max_w(px(style::scaled(&self.shared_settings, 420.0)))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .radius_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.panel)
                    .child(style::section_label(theme, "WHAT TO SEND", fonts.label))
                    .child(scopes)
                    .child(
                        div()
                            .text_size(px(fonts.secondary))
                            .text_color(theme.muted)
                            .child(self.send_summary(sending.scope)),
                    )
                    .child(
                        div()
                            .pt_1()
                            .child(style::section_label(theme, "SEND TO", fonts.label)),
                    )
                    .child(agents)
                    .child(
                        div()
                            .text_size(px(fonts.label))
                            .text_color(theme.muted)
                            .child("Enter sends · Tab changes scope · Esc cancels"),
                    ),
            )
            .into_any_element(),
        )
    }
}

/// Shot-harness entry points: a canned review, since the harness runs
/// without a daemon.
#[cfg(feature = "visual-test")]
impl PilusView {
    pub(super) fn diff_show_discard(&mut self, cx: &mut Context<Self>) {
        let key = self
            .diff_panel
            .files()
            .iter()
            .find(|file| file.layer == DiffLayer::Worktree)
            .map(|file| (file.layer, file.path.clone()));
        if let Some(key) = key {
            self.diff_ask_discard(key, cx);
        }
    }

    pub(super) fn diff_show_sample(&mut self, cx: &mut Context<Self>) {
        use pilus_client::{DiffHunk, DiffLine, DiffLineKind as Kind};

        let file = |path: &str, layer, status: &str, add, del, notes, viewed| DiffFile {
            path: path.to_string(),
            old_path: path.to_string(),
            layer,
            status: status.to_string(),
            additions: Some(add),
            deletions: Some(del),
            binary: false,
            notes,
            viewed,
            modified_since_review: false,
            fingerprint: format!("fp-{path}-{}", layer.wire()),
        };
        let line = |kind, old, new, text: &str| DiffLine {
            kind,
            old_line: old,
            new_line: new,
            text: text.to_string(),
        };
        let files = vec![
            file(
                "crates/pilus/src/gui/session.rs",
                DiffLayer::Worktree,
                "M",
                6,
                3,
                2,
                false,
            ),
            file(
                "crates/pilus/src/gui/style.rs",
                DiffLayer::Worktree,
                "M",
                14,
                0,
                0,
                true,
            ),
            file(
                "crates/pilus-client/src/diff.rs",
                DiffLayer::Staged,
                "A",
                420,
                0,
                0,
                false,
            ),
            file("notes/review.md", DiffLayer::Untracked, "?", 9, 0, 0, false),
        ];
        let selected = files[0].clone();
        let diff = FileDiff {
            file: selected.clone(),
            truncated: false,
            omitted_lines: 0,
            hunks: vec![
                DiffHunk {
                    header: "@@ -48,9 +48,12 @@ fn poll(&mut self)".to_string(),
                    lines: vec![
                        line(
                            Kind::Context,
                            Some(48),
                            Some(48),
                            "    fn poll(&mut self) -> bool {",
                        ),
                        line(
                            Kind::Context,
                            Some(49),
                            Some(49),
                            "        let mut changed = false;",
                        ),
                        line(
                            Kind::Deletion,
                            Some(50),
                            None,
                            "        while let Ok(message) = self.rx.try_recv() {",
                        ),
                        line(
                            Kind::Deletion,
                            Some(51),
                            None,
                            "            self.receive(message);",
                        ),
                        line(Kind::Deletion, Some(52), None, "        }"),
                        line(
                            Kind::Addition,
                            None,
                            Some(50),
                            "        // Drain everything the reader queued so one frame paints the latest.",
                        ),
                        line(
                            Kind::Addition,
                            None,
                            Some(51),
                            "        for message in self.rx.try_iter() {",
                        ),
                        line(
                            Kind::Addition,
                            None,
                            Some(52),
                            "            self.receive(message);",
                        ),
                        line(
                            Kind::Addition,
                            None,
                            Some(53),
                            "            changed = true;",
                        ),
                        line(Kind::Addition, None, Some(54), "        }"),
                        line(Kind::Context, Some(53), Some(55), "        changed"),
                        line(Kind::Context, Some(54), Some(56), "    }"),
                    ],
                },
                DiffHunk {
                    header: "@@ -120,4 +123,5 @@ fn row_element".to_string(),
                    lines: vec![
                        line(
                            Kind::Context,
                            Some(120),
                            Some(123),
                            "        let theme = self.theme;",
                        ),
                        line(
                            Kind::Addition,
                            None,
                            Some(124),
                            "        // A very long line wraps inside its own pane in every layout instead of pushing the other side off screen, so a reviewer never scrolls sideways to read it.",
                        ),
                        line(Kind::Context, Some(121), Some(125), "        div()"),
                    ],
                },
            ],
        };
        let note = |id: &str, side, start, end, kind, state, body: &str| DiffNote {
            id: id.to_string(),
            path: selected.path.clone(),
            layer: DiffLayer::Worktree,
            side,
            start_line: start,
            end_line: end,
            kind,
            state,
            body: body.to_string(),
            deliveries: 0,
        };
        let notes = vec![
            note(
                "n1",
                NoteSide::New,
                51,
                53,
                NoteKind::Issue,
                NoteState::Open,
                "This drops the early return when the channel is closed.\n\nKeep the `Err(Disconnected)` arm, and\nadd a test for it.",
            ),
            note(
                "n2",
                NoteSide::Old,
                50,
                50,
                NoteKind::Question,
                NoteState::Resolved,
                "Was the blocking recv intentional?\nIt used to wait on the channel.\n\nNow it spins on try_recv.",
            ),
        ];

        self.right.visible = true;
        self.right.focused = true;
        let panel = &mut self.diff_panel;
        panel.endpoint = Some(self.active.clone());
        // Hold the poll off: there is no daemon to answer it.
        panel.polling = true;
        panel.needs_refresh = false;
        panel.snapshot = Some(DiffSnapshot {
            repo: "/work/pilus".to_string(),
            branch: "git-integration".to_string(),
            files,
            omitted: 0,
            refreshing: false,
            fingerprint: "sample".to_string(),
        });
        panel.notes = notes;
        panel.selected = Some((selected.layer, selected.path.clone()));
        panel.refresh_file_notes();
        self.diff_install((selected.layer, selected.path), selected.fingerprint, diff);
        cx.notify();
    }

    /// The canned review with a markdown file open as a document, a note on
    /// two of its blocks, and one left on a line the file no longer has.
    pub(super) fn diff_show_markdown_sample(&mut self, cx: &mut Context<Self>) {
        self.diff_show_sample(cx);
        let panel = &mut self.diff_panel;
        let path = "docs/review-guide.md";
        let file = DiffFile {
            path: path.to_string(),
            old_path: path.to_string(),
            layer: DiffLayer::Worktree,
            status: "M".to_string(),
            additions: Some(18),
            deletions: Some(2),
            binary: false,
            notes: 2,
            viewed: false,
            modified_since_review: false,
            fingerprint: "fp-guide".to_string(),
        };
        if let Some(snapshot) = panel.snapshot.as_mut() {
            snapshot.files.insert(0, file);
        }
        let note = |id: &str, side, start, end, kind, body: &str| DiffNote {
            id: id.to_string(),
            path: path.to_string(),
            layer: DiffLayer::Worktree,
            side,
            start_line: start,
            end_line: end,
            kind,
            state: NoteState::Open,
            body: body.to_string(),
            deliveries: 0,
        };
        panel.notes.extend([
            note(
                "md1",
                NoteSide::New,
                8,
                9,
                NoteKind::Suggestion,
                "Name the tab and the key together, so the reader can find both.",
            ),
            note(
                "md2",
                NoteSide::New,
                14,
                14,
                NoteKind::Question,
                "Does `n` also work while a range is selected?",
            ),
            note(
                "md3",
                NoteSide::Old,
                4,
                4,
                NoteKind::Issue,
                "The old intro promised a screenshot here.",
            ),
        ]);
        let key = (DiffLayer::Worktree, path.to_string());
        panel.selected = Some(key.clone());
        panel.loaded = None;
        panel.sending = None;
        panel.composer = None;
        panel.rendered = true;
        panel.refresh_file_notes();
        let state = Ok(MdDoc::new(
            &TextFile {
                text: md_view::SAMPLE.to_string(),
                truncated: false,
            },
            &key.1,
        ));
        panel.md = Some(MdText {
            key,
            fingerprint: "fp-guide".to_string(),
            state,
        });
        cx.notify();
    }

    /// Switch a markdown file between its document and its patch.
    pub(super) fn diff_show_mode_rendered(&mut self, rendered: bool, cx: &mut Context<Self>) {
        if self.diff_panel.rendered != rendered {
            self.diff_toggle_rendered(cx);
        }
    }

    pub(super) fn diff_show_menu(&mut self, filter: bool, cx: &mut Context<Self>) {
        self.diff_panel.sending = None;
        self.diff_panel.composer = None;
        self.diff_panel.menu = Some(if filter {
            DiffMenu::Filter
        } else {
            DiffMenu::Layout
        });
        cx.notify();
    }

    pub(super) fn diff_close_menu(&mut self, cx: &mut Context<Self>) {
        self.diff_panel.menu = None;
        cx.notify();
    }

    pub(super) fn diff_show_view(&mut self, view: ListView, cx: &mut Context<Self>) {
        self.diff_panel.view = view;
        cx.notify();
    }

    pub(super) fn diff_show_list_width(&mut self, width: f32, cx: &mut Context<Self>) {
        self.diff_panel.set_list_width(width);
        cx.notify();
    }

    pub(super) fn diff_show_mode(&mut self, mode: LayoutMode, cx: &mut Context<Self>) {
        self.diff_panel.mode = mode;
        cx.notify();
    }

    pub(super) fn diff_show_composer(&mut self, cx: &mut Context<Self>) {
        self.diff_panel.cursor = Cursor {
            src: 6,
            side: NoteSide::New,
        };
        self.diff_begin_selection(cx);
        self.diff_panel.cursor.src = 8;
        self.diff_open_composer(cx);
    }

    pub(super) fn diff_show_send(&mut self, cx: &mut Context<Self>) {
        self.agents = ["claude", "pi"]
            .into_iter()
            .map(|kind| Agent {
                pane: "7".to_string(),
                agent: kind.to_string(),
                status: "idle".to_string(),
                cwd: "~/Developer/pilus".to_string(),
                ..Agent::default()
            })
            .collect();
        self.diff_open_send(SendScope::Review, cx);
    }
}
