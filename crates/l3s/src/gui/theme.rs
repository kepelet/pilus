//! Colors for the native chrome and the cell grid.
//!
//! The server packs a color into a `u32` with a tag in the high byte:
//! `0` is "use the client default", `1` is an ANSI palette index, and `2` is a
//! 24-bit sRGB value. [`resolve`] turns any of those into a paintable color.

use gpui::{Rgba, rgb};

/// The client's own chrome palette.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// Window background.
    pub bg: Rgba,
    /// Default cell foreground.
    pub fg: Rgba,
    /// Header and footer background.
    pub panel: Rgba,
    /// Borders and dividers.
    pub border: Rgba,
    /// Accent for the connection indicator.
    pub accent: Rgba,
    /// Muted text.
    pub muted: Rgba,
    /// Text drawn on the header.
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
            bg: rgb(0x101316),
            fg: rgb(0xd8dde9),
            panel: rgb(0x18181f),
            border: rgb(0x2c2c38),
            accent: rgb(0x7aa2f7),
            muted: rgb(0x6b6b82),
            panel_fg: rgb(0xc0c0d0),
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
