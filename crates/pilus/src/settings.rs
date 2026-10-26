//! Client-owned settings, persisted beside the daemon's state.
//!
//! Fonts are entirely a client concern: the daemon only hears the resulting
//! cell size in `Resize`, so pilus owns the family and size for both its chrome
//! and the terminal grid. Update preferences live here too so both the window
//! and `pilus --check-updates` read the same channel and feed URL.
//!
//! `settings.json` holds the preferences, which `pilus-agent` and
//! `--check-updates` also read and a person may edit by hand. The state the
//! window itself accumulates per item (where it sat, which label a session was
//! given, which repositories are pinned, how the Sipil rows were dragged, which
//! session it was last attached to, how wide its panes were left, which Files
//! folders were folded) is the
//! one `ui_state` document of `pilus.db` instead (`UiState`). The fields stay
//! on [`Settings`] so a reader does not care where they live; [`Settings::load`]
//! fills them from the database and [`Settings::save`] writes them back there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pilus_db::Db;
use serde::{Deserialize, Serialize};

/// The `pilus.db` document the window state is kept in.
const STATE_DOC: &str = "ui_state";

/// The `Settings` fields that live in `pilus.db`, not in `settings.json`.
const STATE_KEYS: [&str; 16] = [
    "window",
    "session_labels",
    "session_icons",
    "github_pinned",
    "sipil_order",
    "last_session",
    "last_endpoint",
    "right_sidebar_visible",
    "right_sidebar_width",
    "right_sidebar_tab",
    "diff_list_height",
    "diff_list_width",
    "git_list_height",
    "pinned_workspaces",
    "recent_workspaces",
    "files_folded",
];

/// How many opened folders the Home page and the sidebar remember.
pub const RECENT_WORKSPACES: usize = 8;

/// The icons a session may take in the header, as `session_icons` values.
pub const SESSION_ICONS: [&str; 7] = [
    "layers",
    "blocks",
    "briefcase",
    "monitor-cloud",
    "cloud",
    "user",
    "flask-conical",
];

/// What a session shows until it is mapped to another icon.
pub const DEFAULT_SESSION_ICON: &str = "layers";

/// Default monospace family for the terminal grid: bundled with the app, in
/// its terminal cut (no ligatures, so cells stay one column wide).
pub const FONT_FAMILY: &str = "Ioskeley Mono Term";
/// Default chrome family, bundled with the app.
pub const CHROME_FONT_FAMILY: &str = "Arimo";
/// Default terminal cell text size in logical pixels.
pub const FONT_SIZE: f32 = 13.0;
/// Default chrome text size in logical pixels.
pub const CHROME_FONT: f32 = 13.0;
/// Default window opacity for glass surfaces (terminal, sidebar, tab strip):
/// fully opaque, the Appearances "Solid" material.
pub const OPACITY: f32 = 1.0;
/// Default blur strength over the desktop: none, which is what an opaque
/// window wants.
pub const BLUR: f32 = 0.0;
/// The corner radius `rounded_md` drew before the radius became a setting.
/// It stays the floor the tab strip never squares below.
pub const CORNER_RADIUS: f32 = 6.0;
/// Default corner radius in logical pixels: the Appearances "Rounded"
/// material. Controls, cards, and overlays use it directly; the dense and
/// large steps scale from it.
pub const DEFAULT_CORNER_RADIUS: f32 = 8.0;
/// The largest corner radius the Appearances slider offers.
pub const CORNER_RADIUS_MAX: f32 = 16.0;

/// The color scheme a light device takes until the user picks another.
pub const DEFAULT_LIGHT_THEME: &str = "rose-pine-dawn";
/// The color scheme a dark device takes until the user picks another.
pub const DEFAULT_DARK_THEME: &str = "noir";

/// The main window's frame when it was last seen, so the next launch reopens
/// it where the user left it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowGeometry {
    /// Left edge in the display's own coordinates, like the window reports it.
    pub x: f32,
    /// Top edge in the display's own coordinates.
    pub y: f32,
    /// Frame width in logical pixels.
    pub width: f32,
    /// Frame height in logical pixels.
    pub height: f32,
    /// Whether the window was zoomed (maximized) when it was recorded.
    #[serde(default)]
    pub maximized: bool,
    /// Stable UUID of the display it sat on, so the frame is restored there
    /// even after the runtime display id changes.
    #[serde(default)]
    pub display: Option<String>,
}

/// A folder on one machine that can be opened again: where the Pinned and
/// Recent workspace lists get their rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    /// The Luvus session the folder belongs to; `None` is the default session.
    /// `workspace.open` goes to that session's daemon, and a machine id is only
    /// meaningful inside the session that saved it, so a folder is listed and
    /// opened only from its own session. A row saved before sessions were
    /// recorded reads as the default session.
    #[serde(default)]
    pub session: Option<String>,
    /// The endpoint that owns the folder: `local`, or a saved machine's id.
    pub endpoint: String,
    /// The folder on that machine, absolute when its home was known.
    pub path: String,
    /// What the row is called; the folder's own name when blank.
    #[serde(default)]
    pub name: String,
}

impl SavedWorkspace {
    /// Whether this is the folder `path` on `endpoint` in `session`, whatever
    /// trailing slash either spelling carries.
    pub fn is(&self, session: Option<&str>, endpoint: &str, path: &str) -> bool {
        self.session.as_deref() == session
            && self.endpoint == endpoint
            && trim_slashes(&self.path) == trim_slashes(path)
    }

    /// The row's label: its name, else the last folder of the path, else the
    /// path itself (`~` and `/` have no last folder).
    pub fn label(&self) -> &str {
        let name = self.name.trim();
        if !name.is_empty() {
            return name;
        }
        let path = trim_slashes(&self.path);
        match path.rsplit('/').next() {
            Some(last) if !last.is_empty() => last,
            _ => path,
        }
    }
}

fn trim_slashes(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() && !path.is_empty() {
        "/"
    } else {
        trimmed
    }
}

/// The Files tab's folded folders: endpoint key (`local` or a machine id), then
/// the tree's root, then the folder paths folded under it.
pub type FilesFolded = BTreeMap<String, BTreeMap<String, Vec<String>>>;

/// The part of [`Settings`] that lives in `pilus.db` rather than `settings.json`.
///
/// Kept apart from the preferences because it is the window's own bookkeeping:
/// it changes with use (a drag, a pin, a resize) and nothing else reads it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct UiState {
    window: Option<WindowGeometry>,
    session_labels: BTreeMap<String, String>,
    session_icons: BTreeMap<String, String>,
    github_pinned: Vec<String>,
    sipil_order: Vec<String>,
    pinned_workspaces: Vec<SavedWorkspace>,
    recent_workspaces: Vec<SavedWorkspace>,
    files_folded: FilesFolded,
    /// Absent in a document written before these moved, which means the file
    /// still holds them and they have not been taken yet.
    layout: Option<Layout>,
}

/// Where the window was left: the session it was attached to and the size of its
/// panes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Layout {
    last_session: Option<String>,
    last_endpoint: Option<String>,
    right_sidebar_visible: bool,
    right_sidebar_width: f32,
    right_sidebar_tab: String,
    diff_list_height: f32,
    diff_list_width: f32,
    git_list_height: f32,
}

impl UiState {
    fn of(settings: &Settings) -> Self {
        Self {
            window: settings.window.clone(),
            session_labels: settings.session_labels.clone(),
            session_icons: settings.session_icons.clone(),
            github_pinned: settings.github_pinned.clone(),
            sipil_order: settings.sipil_order.clone(),
            pinned_workspaces: settings.pinned_workspaces.clone(),
            recent_workspaces: settings.recent_workspaces.clone(),
            files_folded: settings.files_folded.clone(),
            layout: Some(Layout {
                last_session: settings.last_session.clone(),
                last_endpoint: settings.last_endpoint.clone(),
                right_sidebar_visible: settings.right_sidebar_visible,
                right_sidebar_width: settings.right_sidebar_width,
                right_sidebar_tab: settings.right_sidebar_tab.clone(),
                diff_list_height: settings.diff_list_height,
                diff_list_width: settings.diff_list_width,
                git_list_height: settings.git_list_height,
            }),
        }
    }

    fn put_in(self, settings: &mut Settings) {
        settings.window = self.window;
        settings.session_labels = self.session_labels;
        settings.session_icons = self.session_icons;
        settings.github_pinned = self.github_pinned;
        settings.sipil_order = self.sipil_order;
        settings.pinned_workspaces = self.pinned_workspaces;
        settings.recent_workspaces = self.recent_workspaces;
        settings.files_folded = self.files_folded;
        if let Some(layout) = self.layout {
            settings.last_session = layout.last_session;
            settings.last_endpoint = layout.last_endpoint;
            settings.right_sidebar_visible = layout.right_sidebar_visible;
            settings.right_sidebar_width = layout.right_sidebar_width;
            settings.right_sidebar_tab = layout.right_sidebar_tab;
            settings.diff_list_height = layout.diff_list_height;
            settings.diff_list_width = layout.diff_list_width;
            settings.git_list_height = layout.git_list_height;
        }
    }
}

/// The font choices and update preferences a user can change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Chrome font family; `None` keeps the platform default.
    pub ui_font: Option<String>,
    /// Chrome base text size in logical pixels.
    pub ui_font_size: f32,
    /// Monospace family for the terminal grid.
    pub terminal_font: String,
    /// Terminal cell text size in logical pixels.
    pub terminal_font_size: f32,
    /// Update channel: `stable` or `preview` (aliases: beta, nightly).
    pub update_channel: String,
    /// Check for updates automatically on launch.
    pub auto_check_updates: bool,
    /// Override the release feed with a static manifest URL; empty uses the
    /// channel default (`releases.kepelet.com/pilus/<channel>.json`).
    pub update_feed_url: Option<String>,
    /// Show the agent list in the macOS menu bar. On by default; a no-op on
    /// other platforms.
    pub menu_bar: bool,
    /// Frame of the main window, restored on the next launch. Kept in
    /// `pilus.db` (`UiState`).
    pub window: Option<WindowGeometry>,
    /// Run the host connector ("this node" remote access) while pilus is open.
    /// Off by default; the Remote Machines window toggles it.
    pub connector: bool,
    /// Window opacity for glass surfaces (terminal, sidebar, tab strip);
    /// 1.0 is opaque. Files written before the control existed take the
    /// default.
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    /// Blur strength over the desktop; 1.0 is the full system blur and 0.0
    /// drops the blur view so the desktop shows sharp. Files written before
    /// the control existed take the default.
    #[serde(default = "default_blur")]
    pub blur: f32,
    /// Corner radius of controls, cards, and overlays, in logical pixels.
    /// Every rounded element scales from it, and zero squares them all.
    pub corner_radius: f32,
    /// Energy mode: square corners, a solid window and no animation, drawn
    /// over the settings above without changing them, so turning it off
    /// brings back the look the person chose.
    pub energy_mode: bool,
    /// Whether WORKSPACES rows draw their path line. Off by default; the
    /// sidebar's Show Path row turns it on and persists the choice.
    pub sidebar_workspace_paths: bool,
    /// Whether AGENTS rows draw their path line. Off by default, persisted
    /// like the workspace toggle.
    pub sidebar_agent_paths: bool,
    /// On remote machines, show the daemon row just below its tab strip, so a
    /// `clear`ed prompt sits under the native tab bar instead of behind it.
    /// On by default for newer Luvus, which no longer draws a title row
    /// there; turn it off for an older Luvus or one with `show_title`
    /// enabled.
    pub terminal_top_spacing: bool,
    /// The same choice for the local daemon. Off by default, unlike remote
    /// machines; turn it on for a Luvus that no longer draws a title row.
    pub local_terminal_top_spacing: bool,
    /// Draw split panes with pilus's own rounded frames in place of
    /// the daemon's square box-drawing borders. Off keeps the daemon's.
    pub pane_ui: bool,
    /// Float each pane as a card with a gap between them. Off (the default),
    /// panes sit flush and pilus draws a border around each one. Only applies with
    /// `pane_ui`, which owns the frames.
    pub pane_gaps: bool,
    /// Draw the focused pane's corner toolbar (split, zoom, close).
    /// Off by default; only shows with `pane_ui`, which owns the frames it
    /// sits on.
    pub pane_toolbar: bool,
    /// Whether the terminal cursor fades in and out. Off by default: a steady
    /// cursor needs no repaints while the window is idle.
    pub cursor_blink: bool,
    /// Show the GitHub sections in the sidebar. They need the `gh` CLI on
    /// this Mac and stay out of the way when it is missing, so on is the
    /// right default; off also stops every request to GitHub.
    pub github: bool,
    /// Show the Repositories section while `github` is on.
    pub github_repositories: bool,
    /// Repositories (`owner/name`) kept in the sidebar's Repositories section
    /// ahead of the most recent ones, in the order they were pinned. Kept in
    /// `pilus.db` (`UiState`).
    pub github_pinned: Vec<String>,
    /// Let other pilus windows reach this one by its device ID, and start the
    /// network side at launch. Off until the Sipil tab turns it on. Contacts
    /// can be added and a daemon used without it, but nobody can message back.
    pub sipil_online: bool,
    /// Whether pilus may read the coding agents' session transcripts (Claude,
    /// Pi) to index their titles and metadata for Search. Off until the person
    /// agrees on the Search page; turning it off deletes the index. Nothing is
    /// read from disk or over SSH while it is off.
    pub index_agents: bool,
    /// The machines the index reads while `index_agents` is on: `local`, and the
    /// ids of saved SSH profiles. A remote one is never read unless named here.
    pub index_machines: Vec<String>,
    /// The SIPIL sidebar rows (`daemon:<id>`, `channel:<id>`, `person:<id>`) in
    /// the order the person dragged them into. A row not named here comes after
    /// the ones that are, in the order the section would list it. Kept in
    /// `pilus.db` (`UiState`).
    pub sipil_order: Vec<String>,
    /// Folders kept to open again in one click, local or on a saved machine, in
    /// the order they were pinned. Each belongs to the session that pinned it
    /// and shows only there, under PINNED WORKSPACES in the sidebar and on
    /// Home. Kept in `pilus.db` (`UiState`).
    pub pinned_workspaces: Vec<SavedWorkspace>,
    /// The folders opened last, newest first, at most `RECENT_WORKSPACES`
    /// across all sessions; each is listed only in its own. Kept in `pilus.db`
    /// (`UiState`).
    pub recent_workspaces: Vec<SavedWorkspace>,
    /// The folders folded in the Files tab, by machine and root. The daemon
    /// keeps every folder open and has no way to close one, so without this a
    /// relaunch shows the whole tree open again. Kept in `pilus.db` (`UiState`).
    pub files_folded: FilesFolded,
    /// Whether the right sidebar (Changes and History tabs) is open. This and the
    /// other pane layout fields below are kept in `pilus.db` (`UiState`).
    pub right_sidebar_visible: bool,
    /// The right sidebar's width in logical pixels; 0 takes the default.
    pub right_sidebar_width: f32,
    /// The right sidebar's open tab, `diff` or `git`; anything else, such as
    /// a fresh install's empty value, opens the Diff.
    pub right_sidebar_tab: String,
    /// Display names for Luvus sessions, keyed by the real session name. The
    /// CLI and daemon cannot rename a session, so a label lives only here:
    /// the session keeps its name on disk, in sockets, and in `--session`. Kept
    /// in `pilus.db` (`UiState`).
    pub session_labels: BTreeMap<String, String>,
    /// Which of `SESSION_ICONS` stands for a session in the window's header,
    /// keyed by the real session name, kept with `session_labels`; a missing or
    /// unknown entry shows `DEFAULT_SESSION_ICON`.
    pub session_icons: BTreeMap<String, String>,
    /// The named session the window was attached to when it last closed;
    /// `None` is the default session. The next plain launch reopens it. Kept in
    /// `pilus.db` (`UiState`).
    pub last_session: Option<String>,
    /// The machine id that owned the surface in `last_session`; `None` is the
    /// local daemon. Machine profiles belong to a session, so a session switch
    /// clears it. Kept in `pilus.db` (`UiState`).
    pub last_endpoint: Option<String>,
    /// Height of the Diff's file list above the patch, in logical pixels; 0
    /// takes the default.
    pub diff_list_height: f32,
    /// Width of the Diff's file list beside the patch when the page is wide,
    /// in logical pixels; 0 takes the default.
    pub diff_list_width: f32,
    /// Height of the Git tab's commit list above the open commit, in logical
    /// pixels; 0 takes the default.
    pub git_list_height: f32,
    /// Relay the host connector dials out through. Owned by the agent too, so
    /// it is kept here to survive the window rewriting this file.
    pub relay: pilus_client::RelaySettings,
    /// Follow the device appearance: the color scheme switches between
    /// `theme_light` and `theme_dark` with the system, and replaces the one
    /// the attached Luvus daemon had selected, remote machines included.
    pub auto_theme: bool,
    /// Color scheme id for a light device; `None` takes `DEFAULT_LIGHT_THEME`.
    pub theme_light: Option<String>,
    /// Color scheme id for a dark device; `None` takes `DEFAULT_DARK_THEME`.
    pub theme_dark: Option<String>,
    /// Whether pilus has applied its default daemon layout and theme to the
    /// daemon it first attached to. `None` and `false` both mean "not yet",
    /// so a launch that missed a stopped daemon retries next time.
    pub init: Option<bool>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ui_font: Some(CHROME_FONT_FAMILY.to_string()),
            ui_font_size: CHROME_FONT,
            terminal_font: FONT_FAMILY.to_string(),
            terminal_font_size: FONT_SIZE,
            update_channel: "stable".to_string(),
            auto_check_updates: false,
            update_feed_url: None,
            menu_bar: true,
            window: None,
            connector: false,
            opacity: OPACITY,
            blur: BLUR,
            corner_radius: DEFAULT_CORNER_RADIUS,
            energy_mode: false,
            sidebar_workspace_paths: false,
            sidebar_agent_paths: false,
            terminal_top_spacing: true,
            local_terminal_top_spacing: false,
            pane_ui: true,
            pane_gaps: false,
            pane_toolbar: false,
            cursor_blink: false,
            github: true,
            github_repositories: true,
            github_pinned: Vec::new(),
            sipil_online: false,
            index_agents: false,
            index_machines: Vec::new(),
            sipil_order: Vec::new(),
            pinned_workspaces: Vec::new(),
            recent_workspaces: Vec::new(),
            files_folded: BTreeMap::new(),
            right_sidebar_visible: false,
            right_sidebar_width: 0.0,
            right_sidebar_tab: String::new(),
            session_labels: BTreeMap::new(),
            session_icons: BTreeMap::new(),
            last_session: None,
            last_endpoint: None,
            diff_list_height: 0.0,
            diff_list_width: 0.0,
            git_list_height: 0.0,
            relay: pilus_client::RelaySettings::default(),
            auto_theme: false,
            theme_light: None,
            theme_dark: None,
            init: Some(false),
        }
    }
}

/// The opacity files written before the control existed get.
fn default_opacity() -> f32 {
    OPACITY
}

/// The blur strength files written before the control existed get.
fn default_blur() -> f32 {
    BLUR
}

impl Settings {
    /// The preferences in `<root>/settings.json`, falling back to the defaults,
    /// without the window state in `pilus.db`. For a reader that has no use for
    /// it, such as `--check-updates`, which should not open the database.
    pub fn preferences(root: &Path) -> Self {
        std::fs::read_to_string(path(root))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// [`Self::preferences`] with the window state from `pilus.db`. The first
    /// time, there is none there and an older `settings.json` may still carry
    /// it, so what the file held becomes the state.
    pub fn load(root: &Path) -> Self {
        let mut settings = Self::preferences(root);
        let db = Db::at(root);
        match db.doc::<UiState>(STATE_DOC) {
            Ok(Some(state)) => {
                let complete = state.layout.is_some();
                state.put_in(&mut settings);
                if !complete && let Err(error) = db.put_doc(STATE_DOC, &UiState::of(&settings)) {
                    tracing::warn!("could not move the pane layout into pilus.db: {error}");
                }
            }
            Ok(None) => {
                if let Err(error) = db.put_doc(STATE_DOC, &UiState::of(&settings)) {
                    tracing::warn!("could not move the window state into pilus.db: {error}");
                }
            }
            Err(error) => tracing::warn!("could not read the window state: {error}"),
        }
        settings
    }

    /// Persist the preferences to `<root>/settings.json` and the window state to
    /// `pilus.db`, best-effort like the daemon's own state writes. The state
    /// leaves the file only once the database has it, so a database that cannot
    /// be written never costs the person their labels and pins.
    /// Only the window writes settings; `--check-updates` reads them without
    /// the GUI feature, where this method is unused.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn save(&self, root: &Path) {
        let db = Db::at(root);
        let kept = db.durable()
            && db
                .put_doc(STATE_DOC, &UiState::of(self))
                .map_err(|error| tracing::warn!("could not save the window state: {error}"))
                .is_ok();
        let Ok(mut value) = serde_json::to_value(self) else {
            return;
        };
        if kept && let Some(object) = value.as_object_mut() {
            for key in STATE_KEYS {
                object.remove(key);
            }
        }
        if let Ok(text) = serde_json::to_string_pretty(&value) {
            let _ = pilus_db::private_dir(root);
            // Staged and renamed: a crash mid-save must not leave half a file,
            // which would read as no settings at all. Owner-only from the first
            // byte, since the relay block can hold an API secret.
            if let Err(error) = pilus_db::write_private(&path(root), &text) {
                tracing::warn!("could not save the settings: {error}");
            }
        }
    }

    /// Glass-surface opacity, clamped to 0..=1 so a stray value in the file
    /// stays meaningful. Zero is a real setting, not a hazard: the glass coat
    /// and blur keep the grid text readable with every surface clear.
    pub fn glass_opacity(&self) -> f32 {
        if self.opacity.is_finite() {
            self.opacity.clamp(0.0, 1.0)
        } else {
            OPACITY
        }
    }

    /// The color scheme id `auto_theme` selects on a light or dark device.
    pub fn device_theme(&self, dark: bool) -> &str {
        let (chosen, fallback) = if dark {
            (&self.theme_dark, DEFAULT_DARK_THEME)
        } else {
            (&self.theme_light, DEFAULT_LIGHT_THEME)
        };
        chosen.as_deref().unwrap_or(fallback)
    }

    /// Choose the color scheme for a light or dark device.
    pub fn set_device_theme(&mut self, dark: bool, id: &str) {
        let slot = if dark {
            &mut self.theme_dark
        } else {
            &mut self.theme_light
        };
        *slot = Some(id.to_string());
    }

    /// What to call `session` in the UI: its label, else its real name.
    pub fn session_label<'a>(&'a self, session: &'a str) -> &'a str {
        self.session_labels
            .get(session)
            .map_or(session, String::as_str)
    }

    /// The icon key for `session`: its mapped one when that names a known
    /// icon, else the default. A typo in the file degrades to the default
    /// instead of drawing nothing.
    pub fn session_icon<'a>(&'a self, session: &str) -> &'a str {
        self.session_icons
            .get(session)
            .and_then(|key| SESSION_ICONS.iter().find(|known| **known == key))
            .copied()
            .unwrap_or(DEFAULT_SESSION_ICON)
    }

    /// Give `session` the icon `key`. The default or an unknown key drops the
    /// mapping, so the file never holds an entry that changes nothing.
    pub fn set_session_icon(&mut self, session: &str, key: &str) {
        if key != DEFAULT_SESSION_ICON && SESSION_ICONS.contains(&key) {
            self.session_icons
                .insert(session.to_string(), key.to_string());
        } else {
            self.session_icons.remove(session);
        }
    }

    /// Label `session`, or restore its real name when `label` is blank.
    pub fn set_session_label(&mut self, session: &str, label: &str) {
        let label = label.trim();
        if label.is_empty() || label == session {
            self.session_labels.remove(session);
        } else {
            self.session_labels
                .insert(session.to_string(), label.to_string());
        }
    }

    /// The session a plain launch should reopen: the one left last, provided
    /// it is still running. A session that was stopped or deleted since falls
    /// back to the default rather than starting a daemon nobody asked for.
    pub fn restorable_session(&self, sessions: &[pilus_client::SessionInfo]) -> Option<String> {
        let last = self.last_session.as_deref()?;
        sessions
            .iter()
            .any(|session| session.name == last && session.running)
            .then(|| last.to_string())
    }

    /// Whether the Repositories section is drawn: its own switch and the
    /// master one.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn github_shows_repositories(&self) -> bool {
        self.github && self.github_repositories
    }

    /// Whether `repo` (`owner/name`, any case) is pinned.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn is_pinned(&self, repo: &str) -> bool {
        self.github_pinned
            .iter()
            .any(|pinned| pinned.eq_ignore_ascii_case(repo))
    }

    /// Pin or unpin `repo`. A repository pins once, and unpinning one that
    /// was never pinned changes nothing.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn set_pinned(&mut self, repo: &str, on: bool) {
        let pinned = self.is_pinned(repo);
        if on && !pinned {
            self.github_pinned.push(repo.to_string());
        } else if !on && pinned {
            self.github_pinned
                .retain(|known| !known.eq_ignore_ascii_case(repo));
        }
    }

    /// Move pinned `repo` one place toward the front (`earlier`) or the back.
    /// The sidebar lists pins in this order; a repository already at that end,
    /// or never pinned, stays where it is.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn move_pinned(&mut self, repo: &str, earlier: bool) {
        let Some(at) = self
            .github_pinned
            .iter()
            .position(|known| known.eq_ignore_ascii_case(repo))
        else {
            return;
        };
        let to = if earlier {
            at.checked_sub(1)
        } else {
            Some(at + 1).filter(|to| *to < self.github_pinned.len())
        };
        if let Some(to) = to {
            self.github_pinned.swap(at, to);
        }
    }

    /// Whether the folder `path` on `endpoint` is pinned in `session`.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn workspace_pinned(&self, session: Option<&str>, endpoint: &str, path: &str) -> bool {
        self.pinned_workspaces
            .iter()
            .any(|saved| saved.is(session, endpoint, path))
    }

    /// The pins of `session`, in the order they were pinned.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn pinned_in<'a>(
        &'a self,
        session: Option<&'a str>,
    ) -> impl Iterator<Item = &'a SavedWorkspace> {
        self.pinned_workspaces
            .iter()
            .filter(move |saved| saved.session.as_deref() == session)
    }

    /// Drop the pin `from` into the slot of the pin `to`, which must be in the
    /// same session. The list is shared by every session, so the pin moves
    /// within it and the other sessions' pins keep their relative order. A pin
    /// that is not there, or one dropped on itself, changes nothing.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn move_workspace_pin(&mut self, from: &SavedWorkspace, to: &SavedWorkspace) {
        if from.session != to.session {
            return;
        }
        let session = from.session.as_deref();
        let find = |pins: &[SavedWorkspace], target: &SavedWorkspace| {
            pins.iter()
                .position(|saved| saved.is(session, &target.endpoint, &target.path))
        };
        let (Some(at), Some(into)) = (
            find(&self.pinned_workspaces, from),
            find(&self.pinned_workspaces, to),
        ) else {
            return;
        };
        let pin = self.pinned_workspaces.remove(at);
        self.pinned_workspaces.insert(into, pin);
    }

    /// The pin for the folder `path` on `endpoint` in `session`; a folder
    /// unpinned since a menu was drawn comes back unnamed, so a stale click
    /// still opens it.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn saved_workspace(
        &self,
        session: Option<&str>,
        endpoint: &str,
        path: &str,
    ) -> SavedWorkspace {
        self.pinned_workspaces
            .iter()
            .find(|saved| saved.is(session, endpoint, path))
            .cloned()
            .unwrap_or_else(|| SavedWorkspace {
                session: session.map(str::to_string),
                endpoint: endpoint.to_string(),
                path: path.to_string(),
                name: String::new(),
            })
    }

    /// Pin `workspace`, or unpin the folder it names. A folder pins once per
    /// session, and unpinning one that was never pinned changes nothing.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn set_workspace_pinned(&mut self, workspace: SavedWorkspace, on: bool) {
        let session = workspace.session.as_deref();
        let pinned = self.workspace_pinned(session, &workspace.endpoint, &workspace.path);
        if on && !pinned {
            self.pinned_workspaces.push(workspace);
        } else if !on && pinned {
            self.pinned_workspaces
                .retain(|saved| !saved.is(session, &workspace.endpoint, &workspace.path));
        }
    }

    /// Note that `workspace` was just opened: it leads the recent list, and the
    /// oldest fall off past `RECENT_WORKSPACES`. A folder opened again moves
    /// up instead of listing twice. The list is shared by every session, so a
    /// busy session pushes the others' folders out.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn remember_workspace(&mut self, workspace: SavedWorkspace) {
        self.recent_workspaces.retain(|saved| {
            !saved.is(
                workspace.session.as_deref(),
                &workspace.endpoint,
                &workspace.path,
            )
        });
        self.recent_workspaces.insert(0, workspace);
        self.recent_workspaces.truncate(RECENT_WORKSPACES);
    }

    /// The folders `session` opened last that it has not pinned, which are
    /// listed in their own block so a folder never shows twice.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn recent_unpinned<'a>(
        &'a self,
        session: Option<&'a str>,
    ) -> impl Iterator<Item = &'a SavedWorkspace> {
        self.recent_workspaces
            .iter()
            .filter(move |saved| saved.session.as_deref() == session)
            .filter(move |saved| !self.workspace_pinned(session, &saved.endpoint, &saved.path))
    }

    /// Drop the SIPIL row `from` into the slot of row `to`. `shown` is the rows
    /// as the sidebar lists them now, so the order saved is the one on screen,
    /// with whatever the person had not yet placed.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn move_sipil_row(&mut self, shown: &[String], from: &str, to: &str) {
        let mut order: Vec<String> = Vec::new();
        for key in shown {
            if !order.contains(key) {
                order.push(key.clone());
            }
        }
        let (Some(at), Some(into)) = (
            order.iter().position(|key| key == from),
            order.iter().position(|key| key == to),
        ) else {
            return;
        };
        let row = order.remove(at);
        order.insert(into, row);
        self.sipil_order = order;
    }

    /// Whether energy mode is in effect.
    pub fn energy_saving(&self) -> bool {
        self.energy_mode
    }

    /// The corner radius to draw: square under energy mode, since a rounded
    /// edge on a translucent card is what makes the compositor blend it.
    /// [`Self::radius`] stays the person's own choice, for the slider.
    pub fn drawn_radius(&self) -> f32 {
        if self.energy_saving() {
            0.0
        } else {
            self.radius()
        }
    }

    /// The window opacity to draw: solid under energy mode, so nothing behind
    /// the window has to be blurred or blended. [`Self::glass_opacity`] stays
    /// the person's own choice, for the slider.
    pub fn drawn_opacity(&self) -> f32 {
        if self.energy_saving() {
            1.0
        } else {
            self.glass_opacity()
        }
    }

    /// The corner radius, clamped so a stray value cannot draw blobs.
    pub fn radius(&self) -> f32 {
        if self.corner_radius.is_finite() {
            self.corner_radius.clamp(0.0, CORNER_RADIUS_MAX)
        } else {
            DEFAULT_CORNER_RADIUS
        }
    }

    /// Whether the one-time daemon defaults have already been applied. A
    /// missing or null `init` counts as not yet, matching a fresh install.
    pub fn initialized(&self) -> bool {
        self.init == Some(true)
    }
}

fn path(root: &Path) -> PathBuf {
    root.join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_builtin_fonts() {
        let settings = Settings::default();
        assert_eq!(settings.terminal_font, FONT_FAMILY);
        assert_eq!(settings.terminal_font_size, FONT_SIZE);
        assert_eq!(settings.ui_font_size, CHROME_FONT);
        assert_eq!(settings.ui_font.as_deref(), Some(CHROME_FONT_FAMILY));
        assert_eq!(settings.update_channel, "stable");
        assert!(settings.update_feed_url.is_none());
        assert!(settings.window.is_none());
        assert_eq!(settings.opacity, OPACITY);
    }

    #[test]
    fn device_themes_fall_back_until_chosen() {
        let mut settings = Settings::default();
        assert!(!settings.auto_theme);
        assert_eq!(settings.device_theme(false), DEFAULT_LIGHT_THEME);
        assert_eq!(settings.device_theme(true), DEFAULT_DARK_THEME);
        settings.set_device_theme(true, "noir");
        assert_eq!(settings.device_theme(true), "noir");
        assert_eq!(settings.device_theme(false), DEFAULT_LIGHT_THEME);
    }

    #[test]
    fn a_retired_notifications_key_is_ignored() {
        // The toggle is gone; files that still carry it must keep loading.
        serde_json::from_str::<Settings>(r#"{"terminal_font":"Menlo","notifications":false}"#)
            .expect("deserialize");
    }

    #[test]
    fn github_is_on_for_files_written_before_it_existed() {
        let back: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert!(back.github_shows_repositories());
        assert!(back.github_pinned.is_empty());
    }

    #[test]
    fn retired_github_section_switches_are_ignored() {
        // Pull Requests and Issues were sidebar sections once.
        let back: Settings = serde_json::from_str(
            r#"{"github_pull_requests":false,"github_issues":false,"github_repositories":false}"#,
        )
        .expect("deserialize");
        assert!(!back.github_shows_repositories());
    }

    #[test]
    fn the_master_switch_outranks_the_section() {
        let settings = Settings {
            github: false,
            ..Settings::default()
        };
        assert!(!settings.github_shows_repositories());
        // The section keeps its own choice for when GitHub comes back on.
        assert!(settings.github_repositories);
    }

    #[test]
    fn a_sipil_row_dropped_on_another_takes_its_slot_and_the_rest_keep_their_order() {
        let shown: Vec<String> = ["daemon:a", "channel:b", "person:c", "person:d"]
            .map(String::from)
            .to_vec();
        let mut settings = Settings::default();
        settings.move_sipil_row(&shown, "person:d", "daemon:a");
        assert_eq!(
            settings.sipil_order,
            ["person:d", "daemon:a", "channel:b", "person:c"]
        );
        settings.move_sipil_row(&shown, "daemon:a", "person:c");
        assert_eq!(
            settings.sipil_order,
            ["channel:b", "person:c", "daemon:a", "person:d"]
        );
        // A row that is not on show changes nothing.
        settings.move_sipil_row(&shown, "person:gone", "daemon:a");
        assert_eq!(
            settings.sipil_order,
            ["channel:b", "person:c", "daemon:a", "person:d"]
        );
    }

    #[test]
    fn pinning_is_idempotent_and_ignores_case() {
        let mut settings = Settings::default();
        settings.set_pinned("acme/pilus", true);
        settings.set_pinned("Acme/Pilus", true);
        settings.set_pinned("o/second", true);
        assert_eq!(settings.github_pinned, vec!["acme/pilus", "o/second"]);
        assert!(settings.is_pinned("ACME/PILUS"));

        settings.set_pinned("ACME/pilus", false);
        settings.set_pinned("never/pinned", false);
        assert_eq!(settings.github_pinned, vec!["o/second"]);
    }

    #[test]
    fn pins_reorder_one_place_and_stop_at_the_ends() {
        let mut settings = Settings::default();
        for repo in ["a/1", "a/2", "a/3"] {
            settings.set_pinned(repo, true);
        }
        settings.move_pinned("A/3", true);
        assert_eq!(settings.github_pinned, vec!["a/1", "a/3", "a/2"]);
        settings.move_pinned("a/1", true);
        settings.move_pinned("a/2", false);
        settings.move_pinned("never/pinned", false);
        assert_eq!(settings.github_pinned, vec!["a/1", "a/3", "a/2"]);
        settings.move_pinned("a/1", false);
        assert_eq!(settings.github_pinned, vec!["a/3", "a/1", "a/2"]);
    }

    #[test]
    fn missing_top_spacing_defaults() {
        // Files written before the toggle existed follow newer Luvus for
        // remote machines; the local terminal stays flush with the tab strip.
        let back: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert!(back.terminal_top_spacing);
        assert!(!back.local_terminal_top_spacing);
        assert!(back.pane_ui);
        assert!(!back.pane_gaps);
        assert!(!back.pane_toolbar);
        assert!(!back.cursor_blink);
    }

    #[test]
    fn missing_opacity_takes_the_solid_default() {
        // Files written before the control existed have no opacity key.
        let back: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert_eq!(back.opacity, OPACITY);
    }

    #[test]
    fn a_session_label_shadows_the_name_until_cleared() {
        let mut settings = Settings::default();
        assert_eq!(settings.session_label("kind-voyager"), "kind-voyager");
        settings.set_session_label("kind-voyager", "  Work  ");
        assert_eq!(settings.session_label("kind-voyager"), "Work");
        assert_eq!(settings.session_label("other"), "other");
        // Blank, or the real name itself, drops the mapping.
        settings.set_session_label("kind-voyager", "kind-voyager");
        assert!(settings.session_labels.is_empty());
        settings.set_session_label("kind-voyager", "Work");
        settings.set_session_label("kind-voyager", " ");
        assert!(settings.session_labels.is_empty());
    }

    #[test]
    fn a_session_icon_falls_back_to_the_default_when_unmapped_or_unknown() {
        let mut settings = Settings::default();
        assert_eq!(settings.session_icon("kind-voyager"), DEFAULT_SESSION_ICON);
        settings
            .session_icons
            .insert("kind-voyager".to_string(), "flask-conical".to_string());
        assert_eq!(settings.session_icon("kind-voyager"), "flask-conical");
        settings
            .session_icons
            .insert("kind-voyager".to_string(), "rocket".to_string());
        assert_eq!(settings.session_icon("kind-voyager"), DEFAULT_SESSION_ICON);
        assert_eq!(settings.session_icon("other"), DEFAULT_SESSION_ICON);
    }

    #[test]
    fn setting_a_session_icon_stores_only_real_changes() {
        let mut settings = Settings::default();
        settings.set_session_icon("kind-voyager", "user");
        assert_eq!(settings.session_icon("kind-voyager"), "user");
        settings.set_session_icon("kind-voyager", "rocket");
        assert!(settings.session_icons.is_empty());
        settings.set_session_icon("kind-voyager", "cloud");
        settings.set_session_icon("kind-voyager", DEFAULT_SESSION_ICON);
        assert!(settings.session_icons.is_empty());
    }

    #[test]
    fn missing_or_null_init_counts_as_fresh() {
        let missing: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert!(!missing.initialized());
        let null: Settings = serde_json::from_str(r#"{"init":null}"#).expect("deserialize");
        assert!(!null.initialized());
        let done: Settings = serde_json::from_str(r#"{"init":true}"#).expect("deserialize");
        assert!(done.initialized());
    }

    #[test]
    fn missing_session_labels_load_empty() {
        let back: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert!(back.session_labels.is_empty());
    }

    #[test]
    fn missing_last_state_opens_the_default_session_on_local() {
        let back: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert!(back.last_session.is_none());
        assert!(back.last_endpoint.is_none());
    }

    #[test]
    fn only_a_running_session_is_restored() {
        let session = |name: &str, running| pilus_client::SessionInfo {
            name: name.to_string(),
            running,
        };
        let sessions = [
            session("default", true),
            session("kind-voyager", true),
            session("stopped", false),
        ];
        let mut settings = Settings::default();
        assert_eq!(settings.restorable_session(&sessions), None);
        settings.last_session = Some("kind-voyager".to_string());
        assert_eq!(
            settings.restorable_session(&sessions).as_deref(),
            Some("kind-voyager")
        );
        settings.last_session = Some("stopped".to_string());
        assert_eq!(settings.restorable_session(&sessions), None);
        settings.last_session = Some("deleted".to_string());
        assert_eq!(settings.restorable_session(&sessions), None);
    }

    #[test]
    fn missing_blur_takes_the_solid_default() {
        // Files written before the control existed have no blur key.
        let back: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert_eq!(back.blur, BLUR);
    }

    #[test]
    fn missing_corner_radius_takes_the_rounded_default() {
        let back: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert_eq!(back.radius(), DEFAULT_CORNER_RADIUS);
    }

    #[test]
    fn radius_clamps_to_the_slider_range() {
        let mut settings = Settings {
            corner_radius: -3.0,
            ..Settings::default()
        };
        assert_eq!(settings.radius(), 0.0);
        settings.corner_radius = 99.0;
        assert_eq!(settings.radius(), CORNER_RADIUS_MAX);
        settings.corner_radius = f32::NAN;
        assert_eq!(settings.radius(), DEFAULT_CORNER_RADIUS);
    }

    #[test]
    fn energy_mode_draws_square_and_solid_without_touching_the_choices() {
        let mut settings = Settings {
            opacity: 0.6,
            corner_radius: 8.0,
            ..Settings::default()
        };
        assert_eq!(settings.drawn_radius(), 8.0);
        assert_eq!(settings.drawn_opacity(), 0.6);
        settings.energy_mode = true;
        assert_eq!(settings.drawn_radius(), 0.0);
        assert_eq!(settings.drawn_opacity(), 1.0);
        // The sliders keep reading the person's own values.
        assert_eq!(settings.radius(), 8.0);
        assert_eq!(settings.glass_opacity(), 0.6);
    }

    #[test]
    fn missing_energy_keys_leave_energy_mode_off() {
        let back: Settings = serde_json::from_str("{}").expect("deserialize");
        assert!(!back.energy_mode);
    }

    #[test]
    fn glass_opacity_clamps_to_a_usable_range() {
        let mut settings = Settings::default();
        assert_eq!(settings.glass_opacity(), OPACITY);
        settings.opacity = 0.0;
        assert_eq!(settings.glass_opacity(), 0.0);
        settings.opacity = -1.0;
        assert_eq!(settings.glass_opacity(), 0.0);
        settings.opacity = 2.0;
        assert_eq!(settings.glass_opacity(), 1.0);
        settings.opacity = f32::NAN;
        assert_eq!(settings.glass_opacity(), OPACITY);
    }

    #[test]
    fn missing_window_is_none() {
        // Settings written before the frame was remembered must still load.
        let back: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert!(back.window.is_none());
    }

    #[test]
    fn menu_bar_is_on_unless_a_file_says_otherwise() {
        assert!(Settings::default().menu_bar);
        let old: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert!(old.menu_bar);
        let off: Settings = serde_json::from_str(r#"{"menu_bar":false}"#).expect("deserialize");
        assert!(!off.menu_bar);
    }

    #[test]
    fn round_trips_through_json() {
        let settings = Settings {
            ui_font: Some("Helvetica".to_string()),
            ui_font_size: 15.0,
            terminal_font: "JetBrains Mono".to_string(),
            terminal_font_size: 14.0,
            update_channel: "preview".to_string(),
            auto_check_updates: false,
            update_feed_url: Some("https://dl.example.com/preview/latest.json".to_string()),
            window: Some(WindowGeometry {
                x: 120.0,
                y: 64.0,
                width: 1280.0,
                height: 800.0,
                maximized: true,
                display: Some("6d0f9a4c-30b2-4f25-9a2e-4a8f9c3b7d10".to_string()),
            }),
            menu_bar: false,
            connector: true,
            opacity: 0.7,
            blur: 0.5,
            corner_radius: 0.0,
            energy_mode: true,
            sidebar_workspace_paths: true,
            sidebar_agent_paths: true,
            terminal_top_spacing: false,
            local_terminal_top_spacing: true,
            pane_ui: false,
            pane_gaps: true,
            pane_toolbar: false,
            cursor_blink: true,
            github: false,
            github_repositories: false,
            github_pinned: vec!["o/r".to_string()],
            sipil_online: true,
            index_agents: true,
            index_machines: vec!["local".to_string(), "build-box".to_string()],
            sipil_order: vec!["person:b".to_string(), "daemon:a".to_string()],
            pinned_workspaces: vec![SavedWorkspace {
                session: Some("kind-voyager".to_string()),
                endpoint: "build-box".to_string(),
                path: "/srv/api".to_string(),
                name: "api".to_string(),
            }],
            recent_workspaces: vec![SavedWorkspace {
                session: None,
                endpoint: "local".to_string(),
                path: "/work/site".to_string(),
                name: String::new(),
            }],
            files_folded: BTreeMap::from([(
                "local".to_string(),
                BTreeMap::from([("/work/site".to_string(), vec!["/work/site/src".to_string()])]),
            )]),
            right_sidebar_visible: true,
            right_sidebar_width: 480.0,
            right_sidebar_tab: "git".to_string(),
            session_labels: BTreeMap::from([("kind-voyager".to_string(), "Work".to_string())]),
            session_icons: BTreeMap::from([("kind-voyager".to_string(), "briefcase".to_string())]),
            last_session: Some("kind-voyager".to_string()),
            last_endpoint: Some("build-box".to_string()),
            diff_list_height: 200.0,
            diff_list_width: 320.0,
            git_list_height: 260.0,
            relay: pilus_client::RelaySettings {
                secret: Some("project-secret".to_string()),
                url: vec!["https://relay.example".to_string()],
            },
            auto_theme: true,
            theme_light: Some("catppuccin-latte".to_string()),
            theme_dark: Some("noir".to_string()),
            init: Some(true),
        };
        let text = serde_json::to_string(&settings).expect("serialize");
        let back: Settings = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, settings);
    }

    #[test]
    fn saving_keeps_a_hand_written_relay_block_private() {
        // The agent reads `relay` from this file, and the window rewrites it.
        let dir = std::env::temp_dir().join(format!("pilus-relay-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("settings.json"),
            r#"{"relay":{"secret":"abc","url":["https://relay.example"]}}"#,
        )
        .expect("write");

        let mut settings = Settings::load(&dir);
        settings.opacity = 0.5;
        settings.save(&dir);

        let relay = pilus_client::RelaySettings::load(&dir).expect("load");
        assert_eq!(relay.secret.as_deref(), Some("abc"));
        assert_eq!(relay.url, ["https://relay.example"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("settings.json"))
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_window_state_moves_into_the_database_and_out_of_the_file() {
        let dir = std::env::temp_dir().join(format!("pilus-ui-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        // What an older pilus wrote: preferences and state together.
        std::fs::write(
            dir.join("settings.json"),
            r#"{"opacity":0.5,"github_pinned":["o/a","o/b"],"session_labels":{"s":"Work"},
                "sipil_order":["daemon:1"],"last_session":"work","right_sidebar_width":320.0,
                "relay":{"secret":"abc"}}"#,
        )
        .expect("write");

        let mut settings = Settings::load(&dir);
        assert_eq!(settings.github_pinned, ["o/a", "o/b"]);
        assert_eq!(settings.session_label("s"), "Work");
        assert_eq!(settings.last_session.as_deref(), Some("work"));

        settings.right_sidebar_visible = true;
        settings.set_pinned("o/c", true);
        settings.window = Some(WindowGeometry {
            x: 1.0,
            y: 2.0,
            width: 800.0,
            height: 600.0,
            maximized: false,
            display: None,
        });
        settings.save(&dir);

        let text = std::fs::read_to_string(dir.join("settings.json")).expect("read");
        let file: serde_json::Value = serde_json::from_str(&text).expect("json");
        for key in STATE_KEYS {
            assert!(file.get(key).is_none(), "{key} left the file");
        }
        assert_eq!(file["opacity"], 0.5);
        assert_eq!(file["relay"]["secret"], "abc", "the agent's block stays");

        // A hand edit that puts the old keys back does not beat the database.
        std::fs::write(
            dir.join("settings.json"),
            r#"{"opacity":0.75,"github_pinned":["stale/repo"]}"#,
        )
        .expect("write");
        let again = Settings::load(&dir);
        assert_eq!(again.opacity, 0.75);
        assert_eq!(again.github_pinned, ["o/a", "o/b", "o/c"]);
        assert_eq!(again.window.map(|w| w.width), Some(800.0));
        assert_eq!(again.last_session.as_deref(), Some("work"));
        assert_eq!(again.right_sidebar_width, 320.0);
        assert!(again.right_sidebar_visible);
        assert_eq!(Settings::preferences(&dir).github_pinned, ["stale/repo"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_state_document_from_before_the_layout_moved_takes_it_from_the_file() {
        let dir = std::env::temp_dir().join(format!("pilus-ui-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        Db::at(&dir)
            .put_doc(STATE_DOC, &serde_json::json!({"github_pinned": ["o/a"]}))
            .expect("old document");
        std::fs::write(
            dir.join("settings.json"),
            r#"{"last_session":"work","diff_list_height":210.0}"#,
        )
        .expect("write");

        let settings = Settings::load(&dir);
        assert_eq!(settings.github_pinned, ["o/a"]);
        assert_eq!(settings.last_session.as_deref(), Some("work"));
        assert_eq!(settings.diff_list_height, 210.0);

        // Now the database has them, so the file no longer decides.
        std::fs::write(dir.join("settings.json"), r#"{"last_session":"other"}"#).expect("write");
        assert_eq!(Settings::load(&dir).last_session.as_deref(), Some("work"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn saved(endpoint: &str, path: &str) -> SavedWorkspace {
        SavedWorkspace {
            session: None,
            endpoint: endpoint.to_string(),
            path: path.to_string(),
            name: String::new(),
        }
    }

    #[test]
    fn a_folder_pins_once_and_the_same_path_on_another_machine_is_another_pin() {
        let mut settings = Settings::default();
        settings.set_workspace_pinned(saved("local", "/work/api"), true);
        settings.set_workspace_pinned(saved("local", "/work/api/"), true);
        settings.set_workspace_pinned(saved("build-box", "/work/api"), true);
        assert_eq!(settings.pinned_workspaces.len(), 2);
        assert!(settings.workspace_pinned(None, "local", "/work/api/"));

        settings.set_workspace_pinned(saved("local", "/work/api"), false);
        assert!(!settings.workspace_pinned(None, "local", "/work/api"));
        assert!(settings.workspace_pinned(None, "build-box", "/work/api"));
        // Unpinning what was never pinned is a no-op.
        settings.set_workspace_pinned(saved("local", "/nowhere"), false);
        assert_eq!(settings.pinned_workspaces.len(), 1);
    }

    #[test]
    fn a_pin_dropped_on_another_takes_its_slot_and_other_sessions_keep_theirs() {
        let mut settings = Settings::default();
        for pin in [
            saved("local", "/w/a"),
            saved_in("work", "local", "/w/x"),
            saved("local", "/w/b"),
            saved("local", "/w/c"),
            saved_in("work", "local", "/w/y"),
        ] {
            settings.set_workspace_pinned(pin, true);
        }
        let order = |settings: &Settings, session: Option<&str>| -> Vec<String> {
            settings
                .pinned_in(session)
                .map(|saved| saved.path.clone())
                .collect()
        };

        settings.move_workspace_pin(&saved("local", "/w/c"), &saved("local", "/w/a"));
        assert_eq!(order(&settings, None), ["/w/c", "/w/a", "/w/b"]);
        assert_eq!(order(&settings, Some("work")), ["/w/x", "/w/y"]);

        // Moving later lands after the row it was dropped on.
        settings.move_workspace_pin(&saved("local", "/w/c"), &saved("local", "/w/b"));
        assert_eq!(order(&settings, None), ["/w/a", "/w/b", "/w/c"]);

        // A pin from another session, or one that is gone, changes nothing.
        settings.move_workspace_pin(&saved_in("work", "local", "/w/x"), &saved("local", "/w/a"));
        settings.move_workspace_pin(&saved("local", "/w/gone"), &saved("local", "/w/a"));
        assert_eq!(order(&settings, None), ["/w/a", "/w/b", "/w/c"]);
        assert_eq!(order(&settings, Some("work")), ["/w/x", "/w/y"]);
    }

    #[test]
    fn recent_folders_lead_with_the_newest_and_do_not_repeat() {
        let mut settings = Settings::default();
        for n in 0..RECENT_WORKSPACES + 3 {
            settings.remember_workspace(saved("local", &format!("/w/{n}")));
        }
        assert_eq!(settings.recent_workspaces.len(), RECENT_WORKSPACES);
        assert_eq!(settings.recent_workspaces[0].path, "/w/10");

        // Opening an older one again moves it to the front instead of adding.
        settings.remember_workspace(saved("local", "/w/5"));
        assert_eq!(settings.recent_workspaces.len(), RECENT_WORKSPACES);
        assert_eq!(settings.recent_workspaces[0].path, "/w/5");
        assert_eq!(
            settings
                .recent_workspaces
                .iter()
                .filter(|saved| saved.path == "/w/5")
                .count(),
            1
        );
    }

    #[test]
    fn a_pinned_folder_is_left_out_of_the_recent_block() {
        let mut settings = Settings::default();
        settings.remember_workspace(saved("local", "/w/a"));
        settings.remember_workspace(saved("local", "/w/b"));
        settings.set_workspace_pinned(saved("local", "/w/a"), true);
        let rest: Vec<_> = settings
            .recent_unpinned(None)
            .map(|s| s.path.as_str())
            .collect();
        assert_eq!(rest, ["/w/b"]);
    }

    fn saved_in(session: &str, endpoint: &str, path: &str) -> SavedWorkspace {
        SavedWorkspace {
            session: Some(session.to_string()),
            ..saved(endpoint, path)
        }
    }

    #[test]
    fn a_pin_belongs_to_the_session_that_made_it() {
        let mut settings = Settings::default();
        settings.set_workspace_pinned(saved("local", "/work/api"), true);
        settings.set_workspace_pinned(saved_in("work", "local", "/work/api"), true);
        settings.set_workspace_pinned(saved_in("work", "local", "/work/web"), true);

        // The same folder pins once per session.
        assert_eq!(settings.pinned_workspaces.len(), 3);
        assert!(settings.workspace_pinned(Some("work"), "local", "/work/web"));
        assert!(!settings.workspace_pinned(None, "local", "/work/web"));
        assert!(!settings.workspace_pinned(Some("play"), "local", "/work/api"));
        assert_eq!(settings.pinned_in(None).count(), 1);
        assert_eq!(settings.pinned_in(Some("work")).count(), 2);
        assert_eq!(settings.pinned_in(Some("play")).count(), 0);

        // Unpinning in one session leaves the others' pin of the folder alone.
        settings.set_workspace_pinned(saved_in("work", "local", "/work/api"), false);
        assert!(settings.workspace_pinned(None, "local", "/work/api"));
        assert!(!settings.workspace_pinned(Some("work"), "local", "/work/api"));
    }

    #[test]
    fn recent_folders_are_listed_only_in_their_own_session() {
        let mut settings = Settings::default();
        settings.remember_workspace(saved("local", "/w/a"));
        settings.remember_workspace(saved_in("work", "local", "/w/a"));
        settings.remember_workspace(saved_in("work", "local", "/w/b"));
        settings.set_workspace_pinned(saved_in("work", "local", "/w/b"), true);

        // The same folder in two sessions is two entries, not one that moves.
        assert_eq!(settings.recent_workspaces.len(), 3);
        let default: Vec<_> = settings.recent_unpinned(None).map(|s| &*s.path).collect();
        assert_eq!(default, ["/w/a"]);
        let work: Vec<_> = settings
            .recent_unpinned(Some("work"))
            .map(|s| &*s.path)
            .collect();
        assert_eq!(work, ["/w/a"]);
    }

    #[test]
    fn a_pin_saved_before_sessions_were_recorded_is_the_default_sessions() {
        let old = r#"{"endpoint":"local","path":"/work/api","name":"api"}"#;
        let saved: SavedWorkspace = serde_json::from_str(old).expect("old row");
        assert_eq!(saved.session, None);
        assert!(saved.is(None, "local", "/work/api"));
        assert!(!saved.is(Some("work"), "local", "/work/api"));
    }

    #[test]
    fn a_row_is_named_for_its_folder_unless_it_has_a_name() {
        assert_eq!(saved("local", "/work/api/").label(), "api");
        assert_eq!(saved("local", "~").label(), "~");
        assert_eq!(saved("local", "/").label(), "/");
        let mut named = saved("local", "/work/api");
        named.name = " Backend ".to_string();
        assert_eq!(named.label(), "Backend");
    }

    #[test]
    fn pinned_and_recent_folders_survive_the_database_round_trip() {
        let dir = std::env::temp_dir().join(format!("pilus-ui-pins-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let mut settings = Settings::load(&dir);
        settings.set_workspace_pinned(saved_in("work", "build-box", "/srv/api"), true);
        settings.remember_workspace(saved("local", "/work/site"));
        settings.save(&dir);

        let back = Settings::load(&dir);
        assert_eq!(back.pinned_workspaces, settings.pinned_workspaces);
        assert_eq!(back.recent_workspaces, settings.recent_workspaces);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folded_files_folders_survive_a_relaunch_and_stay_out_of_the_file() {
        let dir = std::env::temp_dir().join(format!("pilus-ui-folds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let mut settings = Settings::load(&dir);
        settings.files_folded = BTreeMap::from([(
            "build-box".to_string(),
            BTreeMap::from([("/srv/api".to_string(), vec!["/srv/api/docs".to_string()])]),
        )]);
        settings.save(&dir);

        assert_eq!(Settings::load(&dir).files_folded, settings.files_folded);
        let file = std::fs::read_to_string(dir.join("settings.json")).expect("read");
        assert!(!file.contains("files_folded"), "{file}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_sidebar_paths_default_hidden() {
        // Files written before the toggles existed hide both path lines.
        let back: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert!(!back.sidebar_workspace_paths);
        assert!(!back.sidebar_agent_paths);
    }
}
