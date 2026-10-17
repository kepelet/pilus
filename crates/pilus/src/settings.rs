//! Client-owned settings, persisted beside the daemon's state.
//!
//! Fonts are entirely a client concern: the daemon only hears the resulting
//! cell size in `Resize`, so pilus owns the family and size for both its chrome
//! and the terminal grid. Update preferences live here too so both the window
//! and `pilus --check-updates` read the same channel and feed URL.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
pub const CHROME_FONT_FAMILY: &str = "Geist";
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
    /// Frame of the main window, restored on the next launch.
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
    /// Show the GitHub sections in the sidebar. They need the `gh` CLI on
    /// this Mac and stay out of the way when it is missing, so on is the
    /// right default; off also stops every request to GitHub.
    pub github: bool,
    /// Show the Repositories section while `github` is on.
    pub github_repositories: bool,
    /// Repositories (`owner/name`) kept in the sidebar's Repositories section
    /// ahead of the most recent ones, in the order they were pinned.
    pub github_pinned: Vec<String>,
    /// Whether the right sidebar (Changes and History tabs) is open.
    pub right_sidebar_visible: bool,
    /// The right sidebar's width in logical pixels; 0 takes the default.
    pub right_sidebar_width: f32,
    /// The right sidebar's open tab, `diff` or `git`; anything else, such as
    /// a fresh install's empty value, opens the Diff.
    pub right_sidebar_tab: String,
    /// Display names for Luvus sessions, keyed by the real session name. The
    /// CLI and daemon cannot rename a session, so a label lives only here:
    /// the session keeps its name on disk, in sockets, and in `--session`.
    pub session_labels: BTreeMap<String, String>,
    /// Which of `SESSION_ICONS` stands for a session in the window's header,
    /// keyed by the real session name. Hand-edited like `session_labels`; a
    /// missing or unknown entry shows `DEFAULT_SESSION_ICON`.
    pub session_icons: BTreeMap<String, String>,
    /// The named session the window was attached to when it last closed;
    /// `None` is the default session. The next plain launch reopens it.
    pub last_session: Option<String>,
    /// The machine id that owned the surface in `last_session`; `None` is the
    /// local daemon. Machine profiles belong to a session, so a session switch
    /// clears it.
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
            sidebar_workspace_paths: false,
            sidebar_agent_paths: false,
            terminal_top_spacing: true,
            local_terminal_top_spacing: false,
            pane_ui: true,
            pane_gaps: false,
            pane_toolbar: false,
            github: true,
            github_repositories: true,
            github_pinned: Vec::new(),
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
    /// Load `<root>/settings.json`, falling back to the defaults.
    pub fn load(root: &Path) -> Self {
        std::fs::read_to_string(path(root))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Persist to `<root>/settings.json`, best-effort like the daemon's own
    /// state writes.
    /// Only the window writes settings; `--check-updates` reads them without
    /// the GUI feature, where this method is unused.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn save(&self, root: &Path) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::create_dir_all(root);
            let path = path(root);
            let _ = std::fs::write(&path, text);
            // The relay block can hold an API secret, and `write` keeps the
            // mode of a file that already exists.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
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
        let mut settings = Settings::default();
        settings.github = false;
        assert!(!settings.github_shows_repositories());
        // The section keeps its own choice for when GitHub comes back on.
        assert!(settings.github_repositories);
    }

    #[test]
    fn pinning_is_idempotent_and_ignores_case() {
        let mut settings = Settings::default();
        settings.set_pinned("kepelet/pilus", true);
        settings.set_pinned("Kepelet/Pilus", true);
        settings.set_pinned("o/second", true);
        assert_eq!(settings.github_pinned, vec!["kepelet/pilus", "o/second"]);
        assert!(settings.is_pinned("KEPELET/PILUS"));

        settings.set_pinned("KEPELET/pilus", false);
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
        let mut settings = Settings::default();
        settings.corner_radius = -3.0;
        assert_eq!(settings.radius(), 0.0);
        settings.corner_radius = 99.0;
        assert_eq!(settings.radius(), CORNER_RADIUS_MAX);
        settings.corner_radius = f32::NAN;
        assert_eq!(settings.radius(), DEFAULT_CORNER_RADIUS);
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
            sidebar_workspace_paths: true,
            sidebar_agent_paths: true,
            terminal_top_spacing: false,
            local_terminal_top_spacing: true,
            pane_ui: false,
            pane_gaps: true,
            pane_toolbar: false,
            github: false,
            github_repositories: false,
            github_pinned: vec!["o/r".to_string()],
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
    fn missing_sidebar_paths_default_hidden() {
        // Files written before the toggles existed hide both path lines.
        let back: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert!(!back.sidebar_workspace_paths);
        assert!(!back.sidebar_agent_paths);
    }
}
