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

use super::style::mix;

/// The modules of a QR code. A scanner needs dark on light whatever the
/// theme, so these are the one pair of colors that deliberately ignore it.
pub fn qr_dark() -> Rgba {
    rgb(0x000000)
}

/// The plate (and quiet zone) a QR code sits on.
pub fn qr_light() -> Rgba {
    rgb(0xffffff)
}

/// An agent's states, in colors no theme changes: blocked is red, working
/// orange, done green.
pub fn status_blocked() -> Rgba {
    rgb(0xff5f56)
}

pub fn status_working() -> Rgba {
    rgb(0xff9f0a)
}

pub fn status_done() -> Rgba {
    rgb(0x32d74b)
}

/// The look of a tab's glyph tile for one kind of tab. No theme changes it:
/// like an agent's state colors, the tile is a mark that stays recognizable
/// whatever the scheme around it.
#[derive(Clone, Copy)]
pub struct TileLook {
    pub face: Rgba,
    pub glyph: Rgba,
}

/// A plain terminal: a near-black card with a phosphor-green mark. Every
/// agent without a look of its own falls back to this one.
pub fn tile_terminal() -> TileLook {
    TileLook {
        face: rgb(0x0f1210),
        glyph: rgb(0x4ade80),
    }
}

/// Claude Code: its orange, with the mark in cream.
pub fn tile_claude() -> TileLook {
    TileLook {
        face: rgb(0xd97757),
        glyph: rgb(0xfaf9f5),
    }
}

/// How far translucent chrome darkens `window_bg` toward black.
const CHROME_SHADE: f32 = 0.35;
/// How much of the remaining transparency translucent chrome fills in, so it
/// reads denser than the grid at every opacity.
const CHROME_DENSITY: f32 = 0.45;
/// How much of the accent a light scheme's surface takes on, so the container
/// around the terminal cards reads in the theme's own hue (rose pine dawn is
/// blush) while the cards keep the chrome's ground.
const SURFACE_TINT_LIGHT: f32 = 0.12;
/// The same for a dark scheme, where far less accent already shows: a vivid
/// accent such as noir's lime turns a near-black ground olive at the light
/// scheme's amount.
const SURFACE_TINT_DARK: f32 = 0.05;
/// The opacity every border and hairline is drawn at. The schemes' border
/// roles are tuned as solid fills, which read heavier than a 1px line wants.
const BORDER_ALPHA: f32 = 0.55;
/// How much of the foreground a [`Theme::tint_fill`] takes at weight 1.
const TINT_FILL: f32 = 0.08;

/// `border` at the shared line opacity. Sets the alpha rather than scaling it,
/// so a color that passes through twice (a projected theme falling back to the
/// default's) is not softened twice.
fn soften(border: Rgba) -> Rgba {
    Rgba {
        a: BORDER_ALPHA,
        ..border
    }
}

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
    /// The edge of whatever holds focus: a focused field, panel, or card.
    pub border_focus: Rgba,
    /// A color the daemon paints blank cells with, to draw as `bg` instead.
    /// Set only on the terminal grid's copy of the theme, when pane frames
    /// replace the daemon's own surface; `None` draws cells as sent.
    pub remap_bg: Option<Rgba>,
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
            border: soften(rgb(0x26262e)),
            border_focus: rgb(0x8c8c96),
            remap_bg: None,
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
            border: soften(p.border),
            border_focus: p.border_focus,
            remap_bg: None,
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

    /// Whether this is a light scheme: a lighter ground than its text.
    /// Translucency is dark-glass only, so light schemes stay opaque.
    pub fn is_light(self) -> bool {
        luminance(self.window_bg) > luminance(self.fg)
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

    /// The color a terminal cell paints to read as the sidebar and top bar:
    /// [`Self::chrome_glass`] spread over the cell's own `glass` coat, so the
    /// two layers land on the chrome's opacity.
    pub fn chrome_cell(self, glass: f32) -> Rgba {
        let chrome = self.chrome_glass(glass);
        Rgba {
            a: if glass > 0.0 {
                (chrome.a / glass).min(1.0)
            } else {
                chrome.a
            },
            ..chrome
        }
    }

    /// The color of the surface pilus lays under the terminal cards, as a cell
    /// paints it: `window_bg` tinted toward the accent, shaded and thickened
    /// like [`Self::chrome_cell`].
    pub fn pane_surface(self, glass: f32) -> Rgba {
        let tint = if self.is_light() {
            SURFACE_TINT_LIGHT
        } else {
            SURFACE_TINT_DARK
        };
        Theme {
            window_bg: mix(self.window_bg, self.accent, tint),
            ..self
        }
        .chrome_cell(glass)
    }

    /// The terminal card's own color at `weight`: a raised fill that must
    /// stay opaque, such as a menu row's, rather than wash over the chrome.
    ///
    /// It stays opaque however translucent the window is, so the selection
    /// never takes on the desktop behind it.
    pub fn card_raise(self, weight: f32) -> Rgba {
        Rgba {
            a: weight.clamp(0.0, 1.0),
            ..self.bg
        }
    }

    /// The fill of a selected tab, a sidebar row under selection or hover,
    /// and the chrome's small buttons on hover: the foreground thinned to
    /// [`TINT_FILL`] times `weight`, laid over the chrome's opaque ground.
    /// A light scheme darkens and a dark one lightens, so it shows on any
    /// ground, even one as pale as the terminal card itself. A selection is
    /// set apart by its border, not by a heavier fill.
    pub fn tint_fill(self, weight: f32) -> Rgba {
        Rgba {
            a: (TINT_FILL * weight).clamp(0.0, 1.0),
            ..self.panel_fg
        }
    }

    /// The ground under the sidebar and the tab strip: `window_bg` opaque,
    /// tinted toward the accent while cards float over it.
    pub fn chrome_ground(self, tinted: bool) -> Rgba {
        if tinted {
            self.pane_surface(1.0)
        } else {
            self.window_bg
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
            border: soften(c(chrome.border, base.border)), // border_focus
            // The projection carries one border; its brighter role stands in
            // for the focus edge, as it does for `border` itself.
            border_focus: c(chrome.border, base.border_focus),
            remap_bg: None,
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
    fn pane_surface_leans_toward_the_accent_and_the_card_does_not() {
        let dawn = Theme::from_palette(
            super::super::scheme::builtin("rose-pine-dawn").unwrap_or_else(default_palette),
        );
        let surface = dawn.pane_surface(1.0);
        // Dawn's ground is near-white; its rose accent pulls green and blue
        // down while red holds, so the surface reads blush.
        assert!(surface.g < dawn.window_bg.g && surface.b < dawn.window_bg.b);
        assert!(surface.r - surface.b > dawn.window_bg.r - dawn.window_bg.b);
        assert_eq!(dawn.chrome_cell(1.0), dawn.window_bg);
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
