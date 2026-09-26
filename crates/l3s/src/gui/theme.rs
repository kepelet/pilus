//! Colors for the native chrome and the cell grid.
//!
//! The server packs a color into a `u32` with a tag in the high byte:
//! `0` is "use the client default", `1` is an ANSI palette index, and `2` is a
//! 24-bit sRGB value. [`resolve`] turns any of those into a paintable color.
//!
//! The chrome palette is deliberately near-black with a lighter sidebar,
//! matching the product mock: the terminal grid stays pure black while panels,
//! pills, and the breadcrumb float above it in dark greys.

use gpui::{Rgba, rgb};

/// The client's own chrome palette.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// Main window background behind the floating card.
    pub window_bg: Rgba,
    /// Terminal card background (also the terminal grid default).
    pub bg: Rgba,
    /// Sidebar column background, a touch lighter than the main bg.
    pub sidebar_bg: Rgba,
    /// Default cell foreground.
    pub fg: Rgba,
    /// Selected pills, tab actives, and the breadcrumb bar.
    pub panel: Rgba,
    /// Dimmed panel for inactive tabs.
    pub panel_dim: Rgba,
    /// Borders and dividers.
    pub border: Rgba,
    /// Accent for the connection indicator and links.
    pub accent: Rgba,
    /// Branch names in the sidebar.
    pub branch: Rgba,
    /// Muted text.
    pub muted: Rgba,
    /// Text drawn on panels.
    pub panel_fg: Rgba,
    /// Background behind selected cells.
    pub selection_bg: Rgba,
    /// Foreground on selected cells.
    pub selection_fg: Rgba,
    /// Healthy/complete state.
    pub ok: Rgba,
    /// Error/blocked state.
    pub alert: Rgba,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            window_bg: rgb(0x060608),
            // Matches the daemon's own base surface: blank cells arrive with
            // this exact color, so unpainted card interior is seamless.
            bg: rgb(0x111116),
            sidebar_bg: rgb(0x121215),
            fg: rgb(0xd8dde9),
            panel: rgb(0x1e1e24),
            panel_dim: rgb(0x141419),
            border: rgb(0x26262e),
            accent: rgb(0x7aa2f7),
            branch: rgb(0xbb9af7),
            muted: rgb(0x8e8e9e),
            panel_fg: rgb(0xe8e8ec),
            selection_bg: rgb(0x2f4f7f),
            selection_fg: rgb(0xffffff),
            ok: rgb(0x7fd88f),
            alert: rgb(0xf07178),
        }
    }
}

/// Resolve a wire-packed color, falling back to `default` for `Reset`.
pub fn resolve(packed: u32, default: Rgba) -> Rgba {
    match packed >> 24 {
        0 => default,
        1 => {
            let (r, g, b) = indexed_rgb(packed as u8);
            rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b))
        }
        _ => rgb(packed & 0x00ff_ffff),
    }
}

/// The standard xterm 256-color palette.
fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xcd, 0x00, 0x00),
        (0x00, 0xcd, 0x00),
        (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee),
        (0xcd, 0x00, 0xcd),
        (0x00, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    match index {
        0..=15 => BASE[usize::from(index)],
        16..=231 => {
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let index = index - 16;
            let r = LEVELS[usize::from(index / 36) % 6];
            let g = LEVELS[usize::from(index / 6) % 6];
            let b = LEVELS[usize::from(index % 6)];
            (r, g, b)
        }
        _ => {
            let level = 8 + (index - 232) * 10;
            (level, level, level)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_uses_default() {
        let default = rgb(0x112233);
        assert_eq!(resolve(0, default), default);
    }

    #[test]
    fn rgb_tag_round_trips() {
        assert_eq!(resolve((2 << 24) | 0x00ff_8800, rgb(0)), rgb(0x00ff_8800));
    }

    #[test]
    fn palette_edges_are_black_and_white() {
        assert_eq!(indexed_rgb(0), (0, 0, 0));
        assert_eq!(indexed_rgb(231), (255, 255, 255));
        assert_eq!(indexed_rgb(255), (238, 238, 238));
    }
}
