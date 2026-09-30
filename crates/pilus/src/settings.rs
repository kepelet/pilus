//! Client-owned settings, persisted beside the daemon's state.
//!
//! Fonts are entirely a client concern: the daemon only hears the resulting
//! cell size in `Resize`, so pilus owns the family and size for both its chrome
//! and the terminal grid. Update preferences live here too so both the window
//! and `pilus --check-updates` read the same channel and feed URL.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default monospace family for the terminal grid.
pub const FONT_FAMILY: &str = "Menlo";
/// Default terminal cell text size in logical pixels.
pub const FONT_SIZE: f32 = 13.0;
/// Default chrome text size in logical pixels.
pub const CHROME_FONT: f32 = 13.0;

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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ui_font: None,
            ui_font_size: CHROME_FONT,
            terminal_font: FONT_FAMILY.to_string(),
            terminal_font_size: FONT_SIZE,
            update_channel: "stable".to_string(),
            auto_check_updates: false,
            update_feed_url: None,
            window: None,
            connector: false,
        }
    }
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
    ///
    /// Only the window writes settings; `--check-updates` reads them without
    /// the GUI feature, where this method is unused.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn save(&self, root: &Path) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::create_dir_all(root);
            let _ = std::fs::write(path(root), text);
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
        assert!(settings.ui_font.is_none());
        assert_eq!(settings.update_channel, "stable");
        assert!(settings.update_feed_url.is_none());
        assert!(settings.window.is_none());
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
        };
        let text = serde_json::to_string(&settings).expect("serialize");
        let back: Settings = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, settings);
    }
}
