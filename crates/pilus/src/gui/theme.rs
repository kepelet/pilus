//! Colors for the native chrome and the cell grid.
//!
//! The server packs a color into a `u32` with a tag in the high byte:
//! `0` is "use the client default", `1` is an ANSI palette index, and `2` is a
//! 24-bit sRGB value. [`resolve`] turns any of those into a paintable color.
//! The sixteen named ANSI slots come from the active theme, so terminal app
//! output follows the appearance instead of the fixed xterm palette.
//!
//! The chrome palette is deliberately near-black with a lighter sidebar,
//! matching the product mock: the terminal grid stays pure black while panels,
//! pills, and the breadcrumb float above it in dark greys.

use gpui::{Rgba, rgb};

/// How far translucent chrome darkens `window_bg` toward black.
const CHROME_SHADE: f32 = 0.35;
/// How much of the remaining transparency translucent chrome fills in, so it
/// reads denser than the grid at every opacity.
const CHROME_DENSITY: f32 = 0.45;

/// The client's own chrome palette.
#[derive(Clone, Copy, Debug, PartialEq)]
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
    /// Background of the composer's footer bar.
    pub composer_footer: Rgba,
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
    /// The sixteen named ANSI terminal colors, indexed 0-15.
    pub ansi: [Rgba; 16],
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
            composer_footer: rgb(0x181818),
            border: rgb(0x26262e),
            accent: rgb(0x7aa2f7),
            branch: rgb(0xbb9af7),
            muted: rgb(0x8e8e9e),
            panel_fg: rgb(0xe8e8ec),
            selection_bg: rgb(0x2f4f7f),
            selection_fg: rgb(0xffffff),
            ok: rgb(0x7fd88f),
            alert: rgb(0xf07178),
            ansi: ansi_from(&default_palette()),
        }
    }
}

impl Theme {
    /// The chrome palette for a built-in Luvus theme: the same semantic roles
    /// the daemon renders with, so client chrome and the grid agree.
    pub fn from_palette(p: super::scheme::Palette) -> Self {
        Self {
            // `crust` is the deepest chrome; the terminal pane sits on
            // `mantle`, which is also what the card's padding paints.
            window_bg: p.crust,
            bg: p.mantle,
            sidebar_bg: p.base,
            fg: p.text,
            panel: p.surface0,
            panel_dim: p.base,
            composer_footer: p.surface0,
            border: p.border,
            accent: p.accent,
            branch: p.green,
            muted: p.subtext0,
            panel_fg: p.text,
            selection_bg: p.sel_bg,
            selection_fg: p.text,
            ok: p.green,
            alert: p.coral,
            ansi: ansi_from(&p),
        }
    }

    /// The glass fill behind the sidebar, the tab strip, and any native page
    /// that owns the card, at window opacity `glass`.
    ///
    /// Under translucency every surface would otherwise paint the same
    /// `window_bg` at the same alpha, so chrome and terminal blur into one
    /// sheet and a page's text sits straight on the desktop. The chrome
    /// takes a deeper shade and a denser coat than the grid beside it: it
    /// frames the terminal, and stays readable however low the slider goes.
    /// An opaque window keeps the seamless base.
    pub fn chrome_glass(self, glass: f32) -> Rgba {
        if glass >= 1.0 {
            return self.window_bg;
        }
        let shade = 1.0 - CHROME_SHADE;
        Rgba {
            r: self.window_bg.r * shade,
            g: self.window_bg.g * shade,
            b: self.window_bg.b * shade,
            a: glass + (1.0 - glass) * CHROME_DENSITY,
        }
    }

    /// The raised fill under a selected or hovered sidebar row, at window
    /// opacity `glass`.
    ///
    /// `panel` is tuned against the opaque `window_bg`, but under translucency
    /// the sidebar is shaded darker than that (`chrome_glass`), so the same
    /// pill stands out harder than the opaque one does. Blending the pill
    /// toward the chrome shade as the window turns translucent keeps its
    /// contrast with the sidebar constant; `weight` scales it down for hover.
    pub fn sidebar_raise(self, glass: f32, weight: f32) -> Rgba {
        let chrome = self.chrome_glass(glass);
        let keep = glass.clamp(0.0, 1.0);
        let mix = |panel: f32, base: f32| panel * keep + base * (1.0 - keep);
        Rgba {
            r: mix(self.panel.r, chrome.r),
            g: mix(self.panel.g, chrome.g),
            b: mix(self.panel.b, chrome.b),
            a: chrome.a * weight,
        }
    }

    /// The chrome palette the daemon projects for a thin client, used when the
    /// active theme is not one we have vendored. Roles the projection omits
    /// keep the built-in default, as does the window/terminal background.
    pub fn from_chrome(chrome: pilus_protocol::MachineFormTheme) -> Self {
        let base = Self::default();
        let c = |packed, fallback| resolve(packed, fallback, &base.ansi);
        Self {
            // The base stays ours: it is the terminal's background too, and
            // that should not shift with whichever theme the daemon runs.
            window_bg: base.window_bg,
            bg: base.bg,
            sidebar_bg: c(chrome.rule, base.sidebar_bg), // surface1
            fg: c(chrome.text, base.fg),
            panel: c(chrome.surface, base.panel), // surface0
            panel_dim: c(chrome.rule, base.panel_dim),
            composer_footer: c(chrome.surface, base.composer_footer),
            border: c(chrome.border, base.border), // border_focus
            accent: c(chrome.accent, base.accent),
            branch: base.branch,
            muted: c(chrome.subtext0, base.muted),
            panel_fg: c(chrome.text, base.panel_fg),
            selection_bg: base.selection_bg,
            selection_fg: base.selection_fg,
            ok: base.ok,
            alert: c(chrome.error, base.alert), // coral
            // The projection names too few roles to rebuild a terminal palette,
            // so a projected theme keeps the client default's ANSI colors.
            ansi: base.ansi,
        }
    }
}

/// The semantic palette behind [`Theme::default`], used to derive its ANSI
/// colors the same way a vendored theme derives theirs.
fn default_palette() -> super::scheme::Palette {
    super::scheme::Palette {
        crust: rgb(0x060608),
        mantle: rgb(0x111116),
        base: rgb(0x121215),
        surface0: rgb(0x1e1e24),
        surface1: rgb(0x26262e),
        overlay0: rgb(0x8e8e9e),
        overlay1: rgb(0xa0a0b0),
        subtext0: rgb(0x8e8e9e),
        subtext1: rgb(0xd8dde9),
        text: rgb(0xd8dde9),
        accent: rgb(0x7aa2f7),
        sel_bg: rgb(0x2f4f7f),
        border: rgb(0x26262e),
        border_focus: rgb(0x8c8c96),
        green: rgb(0x7fd88f),
        mint: rgb(0x7dcfff),
        amber: rgb(0xe0af68),
        coral: rgb(0xf07178),
    }
}

/// The sixteen named ANSI colors a Luvus semantic palette implies.
///
/// A Luvus theme names no terminal palette, so each slot reuses the role that
/// already means it (`coral` is red, `mint` is cyan, ...). Blue and magenta are
/// the two hues the roles never name; both are placed on the standard wheel at
/// the theme's own chroma, so a light theme gets a light blue rather than the
/// fixed xterm one. The greys follow the theme's dark or light cast.
fn ansi_from(p: &super::scheme::Palette) -> [Rgba; 16] {
    let dark = luminance(p.mantle) < luminance(p.text);
    let (black, bright_black, white, bright_white) = if dark {
        (p.crust, p.overlay0, p.subtext1, p.text)
    } else {
        (p.text, p.subtext0, p.base, p.crust)
    };
    let (sat, val) = chroma([p.coral, p.green, p.amber, p.mint]);
    let blue = hsv_to_rgb(240.0, sat, val);
    let magenta = hsv_to_rgb(300.0, sat, val);
    [
        black,
        p.coral,
        p.green,
        p.amber,
        blue,
        magenta,
        p.mint,
        white,
        bright_black,
        p.coral,
        p.green,
        p.amber,
        blue,
        magenta,
        p.mint,
        bright_white,
    ]
}

/// Rec. 709 relative luminance, for choosing a theme's dark or light greys.
fn luminance(c: Rgba) -> f32 {
    0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b
}

/// Average saturation and value of the theme's four named hues, so a derived
/// hue matches the vividness of the colors the theme actually uses.
fn chroma(colors: [Rgba; 4]) -> (f32, f32) {
    let mut sat = 0.0;
    let mut val = 0.0;
    for c in colors {
        let max = c.r.max(c.g).max(c.b);
        let min = c.r.min(c.g).min(c.b);
        sat += if max > 0.0 { (max - min) / max } else { 0.0 };
        val += max;
    }
    (sat / 4.0, val / 4.0)
}

/// HSV to RGB; `hue` is degrees, `sat` and `val` are 0..=1.
fn hsv_to_rgb(hue: f32, sat: f32, val: f32) -> Rgba {
    let chroma = val * sat;
    let second = chroma * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let base = val - chroma;
    let (r, g, b) = match hue {
        h if h < 60.0 => (chroma, second, 0.0),
        h if h < 120.0 => (second, chroma, 0.0),
        h if h < 180.0 => (0.0, chroma, second),
        h if h < 240.0 => (0.0, second, chroma),
        h if h < 300.0 => (second, 0.0, chroma),
        _ => (chroma, 0.0, second),
    };
    Rgba {
        r: r + base,
        g: g + base,
        b: b + base,
        a: 1.0,
    }
}

/// Resolve a wire-packed color.
///
/// `Reset` falls back to `default`. The sixteen named ANSI colors come from the
/// theme's `ansi` palette, so terminal app output follows the appearance; the
/// 6x6x6 cube and greys (16..=255) stay fixed, as themes do not restyle them.
pub fn resolve(packed: u32, default: Rgba, ansi: &[Rgba; 16]) -> Rgba {
    match packed >> 24 {
        0 => default,
        1 => {
            let index = packed as u8;
            if index < 16 {
                ansi[usize::from(index)]
            } else {
                let (r, g, b) = indexed_rgb(index);
                rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b))
            }
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
    fn translucent_chrome_is_darker_and_denser_than_the_grid() {
        let theme = Theme::from_palette(default_palette());
        let glass = 0.6;
        let chrome = theme.chrome_glass(glass);
        assert!(
            chrome.a > glass,
            "chrome coat {} must out-cover the grid",
            chrome.a
        );
        assert!(luminance(chrome) < luminance(theme.window_bg));
        // Opaque, chrome and terminal are one seamless base.
        assert_eq!(theme.chrome_glass(1.0), theme.window_bg);
    }

    #[test]
    fn reset_uses_default() {
        let theme = Theme::default();
        let default = rgb(0x112233);
        assert_eq!(resolve(0, default, &theme.ansi), default);
    }

    #[test]
    fn rgb_tag_round_trips() {
        let theme = Theme::default();
        assert_eq!(
            resolve((2 << 24) | 0x00ff_8800, rgb(0), &theme.ansi),
            rgb(0x00ff_8800)
        );
    }

    #[test]
    fn palette_edges_are_black_and_white() {
        assert_eq!(indexed_rgb(0), (0, 0, 0));
        assert_eq!(indexed_rgb(231), (255, 255, 255));
        assert_eq!(indexed_rgb(255), (238, 238, 238));
    }

    #[test]
    fn named_ansi_colors_follow_the_theme() {
        let mocha =
            Theme::from_palette(crate::gui::scheme::builtin("catppuccin-mocha").expect("vendored"));
        // ANSI 1 (red) is Mocha's coral, not xterm's #cd0000.
        assert_eq!(resolve(0x0100_0001, mocha.bg, &mocha.ansi), mocha.ansi[1]);
        assert_ne!(mocha.ansi[1], rgb(0xcd_0000));
        // The 6x6x6 cube and greys stay on the fixed xterm palette.
        assert_eq!(resolve(0x0100_0010, mocha.bg, &mocha.ansi), rgb(0));
    }

    #[test]
    fn light_themes_keep_dark_ansi_black() {
        let latte =
            Theme::from_palette(crate::gui::scheme::builtin("catppuccin-latte").expect("vendored"));
        assert!(
            luminance(latte.ansi[0]) < luminance(latte.ansi[7]),
            "ANSI black must stay darker than ANSI white on a light theme"
        );
    }

    #[test]
    fn derived_blue_and_magenta_are_distinct() {
        let theme = Theme::default();
        assert_ne!(
            theme.ansi[4], theme.ansi[1],
            "blue must not collapse to red"
        );
        assert_ne!(
            theme.ansi[5], theme.ansi[4],
            "magenta must differ from blue"
        );
    }
}
