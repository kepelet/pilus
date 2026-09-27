//! Client-owned appearance settings, persisted beside the daemon's state.
//!
//! Fonts are entirely a client concern: the daemon only hears the resulting
//! cell size in `Resize`, so l3s owns the family and size for both its chrome
//! and the terminal grid.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{CHROME_FONT, FONT_FAMILY, FONT_SIZE};

/// The font choices a user can make in the Appearances window.
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
    /// Update channel: `stable` or `beta`.
    pub update_channel: String,
    /// Check for updates automatically on launch.
    pub auto_check_updates: bool,
    /// Override the release feed with a static manifest URL.
    pub update_feed_url: Option<String>,
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
        }
    }
}

impl Settings {
    /// Load `<root>/l3s-client.json`, falling back to the defaults.
    pub fn load(root: &Path) -> Self {
        std::fs::read_to_string(path(root))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Persist to `<root>/l3s-client.json`, best-effort like the daemon's own
    /// state writes.
    pub fn save(&self, root: &Path) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path(root), text);
        }
    }
}

fn path(root: &Path) -> PathBuf {
    root.join("l3s-client.json")
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
    }

    #[test]
    fn round_trips_through_json() {
        let settings = Settings {
            ui_font: Some("Helvetica".to_string()),
            ui_font_size: 15.0,
            terminal_font: "JetBrains Mono".to_string(),
            terminal_font_size: 14.0,
            update_channel: "beta".to_string(),
            auto_check_updates: false,
            update_feed_url: Some("https://dl.example.com/beta/latest.json".to_string()),
        };
        let text = serde_json::to_string(&settings).expect("serialize");
        let back: Settings = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, settings);
    }
}
