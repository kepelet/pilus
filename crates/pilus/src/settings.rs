//! Client-owned settings, persisted beside the daemon's state.
//!
//! Fonts are entirely a client concern: the daemon only hears the resulting
//! cell size in `Resize`, so pilus owns the family and size for both its chrome
//! and the terminal grid. Update preferences live here too so both the window
//! and `pilus --check-updates` read the same channel and feed URL.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default monospace family for the terminal grid: bundled with the app, in
/// its terminal cut (no ligatures, so cells stay one column wide).
pub const FONT_FAMILY: &str = "Ioskeley Mono Term";
/// Default chrome family, bundled with the app.
pub const CHROME_FONT_FAMILY: &str = "Geist";
/// Default terminal cell text size in logical pixels.
pub const FONT_SIZE: f32 = 13.0;
/// Default chrome text size in logical pixels.
pub const CHROME_FONT: f32 = 13.0;
/// Default window opacity for glass surfaces (terminal, sidebar, tab strip).
pub const OPACITY: f32 = 0.85;
/// Default blur strength over the desktop: full system blur.
pub const BLUR: f32 = 1.0;
/// Default corner radius in logical pixels: what `rounded_md` drew before the
/// radius became a setting. Controls, cards, and overlays use it directly;
/// the dense and large steps scale from it.
pub const CORNER_RADIUS: f32 = 6.0;
/// The largest corner radius the Appearances slider offers.
pub const CORNER_RADIUS_MAX: f32 = 16.0;

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
    /// Frame of the main window, restored on the next launch.
    pub window: Option<WindowGeometry>,
    /// Run the host connector ("this node" remote access) while pilus is open.
    /// Off by default; the Remote Machines window toggles it.
    pub connector: bool,
    /// Window opacity for glass surfaces (terminal, sidebar, tab strip);
    /// 1.0 is opaque. Files written before the control existed stay
    /// translucent via the default.
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    /// Blur strength over the desktop; 1.0 is the full system blur and 0.0
    /// drops the blur view so the desktop shows sharp. Files written before
    /// the control existed keep full blur via the default.
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
    /// The same choice for the local daemon. Off by default: the Luvus
    /// installed alongside pilus still draws its title row.
    pub local_terminal_top_spacing: bool,
    /// Draw split panes with pilus's own rounded frames in place of
    /// the daemon's square box-drawing borders. Off keeps the daemon's.
    pub pane_ui: bool,
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
    /// Height of the Git tab's commit list above the open commit, in logical
    /// pixels; 0 takes the default.
    pub git_list_height: f32,
    /// Relay the host connector dials out through. Owned by the agent too, so
    /// it is kept here to survive the window rewriting this file.
    pub relay: pilus_client::RelaySettings,
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
            window: None,
            connector: false,
            opacity: OPACITY,
            blur: BLUR,
            corner_radius: CORNER_RADIUS,
            sidebar_workspace_paths: false,
            sidebar_agent_paths: false,
            terminal_top_spacing: true,
            local_terminal_top_spacing: false,
            pane_ui: true,
            right_sidebar_visible: false,
            right_sidebar_width: 0.0,
            right_sidebar_tab: String::new(),
            session_labels: BTreeMap::new(),
            last_session: None,
            last_endpoint: None,
            diff_list_height: 0.0,
            git_list_height: 0.0,
            relay: pilus_client::RelaySettings::default(),
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

    /// What to call `session` in the UI: its label, else its real name.
    pub fn session_label<'a>(&'a self, session: &'a str) -> &'a str {
        self.session_labels
            .get(session)
            .map_or(session, String::as_str)
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

    /// The corner radius, clamped so a stray value cannot draw blobs.
    pub fn radius(&self) -> f32 {
        if self.corner_radius.is_finite() {
            self.corner_radius.clamp(0.0, CORNER_RADIUS_MAX)
        } else {
            CORNER_RADIUS
        }
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
    fn a_retired_notifications_key_is_ignored() {
        // The toggle is gone; files that still carry it must keep loading.
        serde_json::from_str::<Settings>(r#"{"terminal_font":"Menlo","notifications":false}"#)
            .expect("deserialize");
    }

    #[test]
    fn missing_top_spacing_defaults_on() {
        // Files written before the toggle existed follow newer Luvus.
        let back: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert!(back.terminal_top_spacing);
        assert!(!back.local_terminal_top_spacing);
        assert!(back.pane_ui);
    }

    #[test]
    fn missing_opacity_stays_translucent() {
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
    fn missing_blur_keeps_full_blur() {
        // Files written before the control existed have no blur key.
        let back: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert_eq!(back.blur, BLUR);
    }

    #[test]
    fn missing_corner_radius_keeps_the_old_look() {
        let back: Settings =
            serde_json::from_str(r#"{"terminal_font":"Menlo"}"#).expect("deserialize");
        assert_eq!(back.radius(), CORNER_RADIUS);
    }

    #[test]
    fn radius_clamps_to_the_slider_range() {
        let mut settings = Settings::default();
        settings.corner_radius = -3.0;
        assert_eq!(settings.radius(), 0.0);
        settings.corner_radius = 99.0;
        assert_eq!(settings.radius(), CORNER_RADIUS_MAX);
        settings.corner_radius = f32::NAN;
        assert_eq!(settings.radius(), CORNER_RADIUS);
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
            connector: true,
            opacity: 0.7,
            blur: 0.5,
            corner_radius: 0.0,
            sidebar_workspace_paths: true,
            sidebar_agent_paths: true,
            terminal_top_spacing: false,
            local_terminal_top_spacing: true,
            pane_ui: false,
            right_sidebar_visible: true,
            right_sidebar_width: 480.0,
            right_sidebar_tab: "git".to_string(),
            session_labels: BTreeMap::from([("kind-voyager".to_string(), "Work".to_string())]),
            last_session: Some("kind-voyager".to_string()),
            last_endpoint: Some("build-box".to_string()),
            diff_list_height: 200.0,
            git_list_height: 260.0,
            relay: pilus_client::RelaySettings {
                secret: Some("project-secret".to_string()),
                url: vec!["https://relay.example".to_string()],
            },
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
