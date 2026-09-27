//! Built-in Luvus appearance palettes.
//!
//! Vendored from `RizRiyz/luvus` (`src/ui/theme.rs` and `src/theme/*`) so the
//! client can preview and apply a theme without a round trip. Regenerate when
//! Luvus adds or retunes a built-in theme; unknown ids fall back to the colors
//! the daemon projects in `MachineFormTheme`.

use gpui::{Rgba, rgb};

/// The eighteen semantic color roles of a Luvus theme, same order as the
/// theme-file contract.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// `crust` role.
    pub crust: Rgba,
    /// `mantle` role.
    pub mantle: Rgba,
    /// `base` role.
    pub base: Rgba,
    /// `surface0` role.
    pub surface0: Rgba,
    /// `surface1` role.
    pub surface1: Rgba,
    /// `overlay0` role.
    pub overlay0: Rgba,
    /// `overlay1` role.
    pub overlay1: Rgba,
    /// `subtext0` role.
    pub subtext0: Rgba,
    /// `subtext1` role.
    pub subtext1: Rgba,
    /// `text` role.
    pub text: Rgba,
    /// `accent` role.
    pub accent: Rgba,
    /// `sel_bg` role.
    pub sel_bg: Rgba,
    /// `border` role.
    pub border: Rgba,
    /// `border_focus` role.
    pub border_focus: Rgba,
    /// `green` role.
    pub green: Rgba,
    /// `mint` role.
    pub mint: Rgba,
    /// `amber` role.
    pub amber: Rgba,
    /// `coral` role.
    pub coral: Rgba,
}

#[allow(clippy::too_many_arguments)]
fn p(
    crust: u32,
    mantle: u32,
    base: u32,
    surface0: u32,
    surface1: u32,
    overlay0: u32,
    overlay1: u32,
    subtext0: u32,
    subtext1: u32,
    text: u32,
    accent: u32,
    sel_bg: u32,
    border: u32,
    border_focus: u32,
    green: u32,
    mint: u32,
    amber: u32,
    coral: u32,
) -> Palette {
    Palette {
        crust: rgb(crust),
        mantle: rgb(mantle),
        base: rgb(base),
        surface0: rgb(surface0),
        surface1: rgb(surface1),
        overlay0: rgb(overlay0),
        overlay1: rgb(overlay1),
        subtext0: rgb(subtext0),
        subtext1: rgb(subtext1),
        text: rgb(text),
        accent: rgb(accent),
        sel_bg: rgb(sel_bg),
        border: rgb(border),
        border_focus: rgb(border_focus),
        green: rgb(green),
        mint: rgb(mint),
        amber: rgb(amber),
        coral: rgb(coral),
    }
}

/// The palette for a built-in theme id, canonicalising the aliases Luvus
/// accepts (`mocha`, `latte`, `gruvboxlight`, ...).
pub fn builtin(id: &str) -> Option<Palette> {
    let id = match id {
        "frappe" => "catppuccin-frappe",
        "gruvboxlight" => "gruvbox-light",
        "latte" => "catppuccin-latte",
        "macchiato" => "catppuccin-macchiato",
        "mocha" => "catppuccin-mocha",
        other => other,
    };
    Some(match id {
        "quattro-rally" => p(
            0x181926, 0x1e2030, 0x24273a, 0x363a4f, 0x494d64, 0x6e738d, 0x8087a2, 0xa5adcb,
            0xb8c0e0, 0xcad3f5, 0xdbc66f, 0x3a3416, 0x5a5130, 0x9a8a50, 0x94a143, 0xb8cf6a,
            0xe0a154, 0xcf5a44,
        ),
        "noir" => p(
            0x070709, 0x111116, 0x202028, 0x1a1a20, 0x25252d, 0x4a4a54, 0x686873, 0x93939f,
            0xb6b6c0, 0xe7e7ed, 0xc6ff1a, 0x33450e, 0x383840, 0x8c8c96, 0x8fbc7a, 0x6fc6a3,
            0xe09a4d, 0xe06c66,
        ),
        "ocean" => p(
            0x02102a, 0x061d42, 0x0c2e5e, 0x05224a, 0x123a6e, 0x355886, 0x5276a4, 0x8fa8c8,
            0xb8cce6, 0xe8f2ff, 0x46c6ff, 0x123e76, 0x2a4e80, 0x5a86c0, 0x6fcf97, 0x4fd6c8,
            0xf2c14e, 0xf06a6a,
        ),
        "dracula" => p(
            0x1a1b23, 0x282a36, 0x333647, 0x21222c, 0x3c3f52, 0x565869, 0x6272a4, 0xa0a4c0,
            0xcccee0, 0xf8f8f2, 0xbd93f9, 0x44475a, 0x44475a, 0x6272a4, 0x50fa7b, 0x8be9fd,
            0xffb86c, 0xff5555,
        ),
        "nord" => p(
            0x242933, 0x2e3440, 0x3b4252, 0x292f3a, 0x434c5e, 0x4c566a, 0x616e88, 0xb0b8c8,
            0xd8dee9, 0xeceff4, 0x88c0d0, 0x3b4a5e, 0x434c5e, 0x6a7690, 0xa3be8c, 0x8fbcbb,
            0xebcb8b, 0xbf616a,
        ),
        "tokyo-night" => p(
            0x16161e, 0x1a1b26, 0x1f2335, 0x24283b, 0x292e42, 0x32344a, 0x444b6a, 0x787c99,
            0xacb0d0, 0xa9b1d6, 0x7aa2f7, 0x292e42, 0x32344a, 0x7da6ff, 0x9ece6a, 0x0db9d7,
            0xe0af68, 0xf7768e,
        ),
        "sky" => p(
            0xf5f8fc, 0xebf1f9, 0xe1eaf5, 0xd6e1f0, 0xc4d3e8, 0x93a4bd, 0x74849e, 0x566579,
            0x42505f, 0x2f3a48, 0x2477c7, 0xcbe2f7, 0xc0cde0, 0x6a7a92, 0x3f8f56, 0x1c8f88,
            0xc07d12, 0xcc3b52,
        ),
        "catppuccin-mocha" => p(
            0x11111b, 0x181825, 0x1e1e2e, 0x313244, 0x45475a, 0x6c7086, 0x7f849c, 0xa6adc8,
            0xbac2de, 0xcdd6f4, 0xcba6f7, 0x3f3359, 0x45475a, 0x7f849c, 0xa6e3a1, 0x94e2d5,
            0xfab387, 0xf38ba8,
        ),
        "catppuccin-macchiato" => p(
            0x181926, 0x1e2030, 0x24273a, 0x363a4f, 0x494d64, 0x6e738d, 0x8087a2, 0xa5adcb,
            0xb8c0e0, 0xcad3f5, 0xc6a0f6, 0x3b3254, 0x494d64, 0x8087a2, 0xa6da95, 0x8bd5ca,
            0xf5a97f, 0xed8796,
        ),
        "catppuccin-frappe" => p(
            0x232634, 0x292c3c, 0x303446, 0x414559, 0x51576d, 0x737994, 0x838ba7, 0xa5adce,
            0xb5bfe2, 0xc6d0f5, 0xca9ee6, 0x463e5e, 0x51576d, 0x838ba7, 0xa6d189, 0x81c8be,
            0xef9f76, 0xe78284,
        ),
        "rose-pine" => p(
            0x16141f, 0x191724, 0x1f1d2e, 0x21202e, 0x26233a, 0x403d52, 0x524f67, 0x6e6a86,
            0x908caa, 0xe0def4, 0xebbcba, 0x403d52, 0x403d52, 0x908caa, 0x95b1ac, 0x9ccfd8,
            0xf6c177, 0xeb6f92,
        ),
        "rose-pine-moon" => p(
            0x1f1d30, 0x232136, 0x2a273f, 0x2a283e, 0x393552, 0x44415a, 0x56526e, 0x6e6a86,
            0x908caa, 0xe0def4, 0xea9a97, 0x44415a, 0x44415a, 0x908caa, 0x95b1ac, 0x9ccfd8,
            0xf6c177, 0xeb6f92,
        ),
        "gruvbox" => p(
            0x1d2021, 0x282828, 0x3c3836, 0x32302f, 0x504945, 0x665c54, 0x7c6f64, 0xa89984,
            0xbdae93, 0xebdbb2, 0xfabd2f, 0x453d21, 0x504945, 0x928374, 0xb8bb26, 0x8ec07c,
            0xfe8019, 0xfb4934,
        ),
        "sunset" => p(
            0x160a1e, 0x221030, 0x2e1640, 0x1c0d2a, 0x3a1d50, 0x5a3a70, 0x7a5a90, 0xb89ad0,
            0xd6bce8, 0xf6e8ff, 0xff5fd0, 0x4a2466, 0x4a2a64, 0x8a5aa8, 0x5fe0a8, 0x5fd6e0,
            0xffb54f, 0xff5f8f,
        ),
        "homebrew" => p(
            0x000000, 0x040a04, 0x081608, 0x061006, 0x0e240e, 0x1e4c1e, 0x2e6c2e, 0x22b422,
            0x2ee02e, 0x3cff3c, 0x00ff41, 0x0a3a0a, 0x1a4e1a, 0x2ea82e, 0x35e035, 0x5effb0,
            0xc8ff3c, 0xff6050,
        ),
        "grass" => p(
            0x052012, 0x0a361d, 0x104c28, 0x082c18, 0x155830, 0x367850, 0x54966c, 0xa8c898,
            0xc8e0ae, 0xfff0a5, 0xbee632, 0x1a6c3a, 0x287648, 0x56a66c, 0x8fd07a, 0x5ed6b0,
            0xf2c84e, 0xf06a5a,
        ),
        "baitong" => p(
            0x081b1b, 0x112a2a, 0x173838, 0x1d4646, 0x255454, 0x326b6b, 0x4b8585, 0x6fa5a5,
            0x87cefa, 0x33ff33, 0xff00ff, 0x254d32, 0x326b6b, 0xff66ff, 0x33ff33, 0x68fdfe,
            0x1ae642, 0xf77272,
        ),
        "redsands" => p(
            0x1f0a06, 0x38120c, 0x4e1c12, 0x2c0e08, 0x5c261a, 0x8a4a38, 0xa86854, 0xc89a80,
            0xdcba9c, 0xf2daba, 0xff8a3c, 0x702e1e, 0x7c3c2a, 0xba6e4c, 0xa6bf5e, 0x5ec8a8,
            0xffb454, 0xff6a5a,
        ),
        "catppuccin-latte" => p(
            0xeff1f5, 0xe6e9ef, 0xdce0e8, 0xccd0da, 0xbcc0cc, 0x9ca0b0, 0x7c8090, 0x6c6f85,
            0x50526c, 0x4c4f69, 0x40a02b, 0xc6e8a8, 0xacb0be, 0x7c8090, 0x40a02b, 0x179299,
            0xdf8e1d, 0xd20f39,
        ),
        "rose-pine-dawn" => p(
            0xfffaf3, 0xfaf4ed, 0xf8f0e7, 0xf4ede8, 0xf2e9e1, 0xdfdad9, 0xcecacd, 0x9893a5,
            0x797593, 0x464261, 0xd7827e, 0xdfdad9, 0xcecacd, 0x797593, 0x6d8f89, 0x56949f,
            0xea9d34, 0xb4637a,
        ),
        "papercolor" => p(
            0xffffff, 0xeeeeee, 0xe4e4e4, 0xdadada, 0xd0d0d0, 0xc6c6c6, 0xbcbcbc, 0x6c6c6c,
            0x555555, 0x444444, 0x005f87, 0xafd7ff, 0xbcbcbc, 0x005f87, 0x008700, 0x0087af,
            0xaf5f00, 0xaf0000,
        ),
        "paper" => p(
            0xfffdf5, 0xf2eede, 0xe9e5d7, 0xe0dccf, 0xd8d5c7, 0xc8c4b7, 0xaaaaaa, 0x77746c,
            0x555555, 0x000000, 0x1e6fcc, 0xd8d5c7, 0xc8c4b7, 0x1e6fcc, 0x216609, 0x158c86,
            0xb58900, 0xcc3e28,
        ),
        "gruvbox-light" => p(
            0xfbf1c7, 0xf2e5bc, 0xebdbb2, 0xd5c4a1, 0xbdae93, 0xa89984, 0x928374, 0x665c54,
            0x504945, 0x3c3836, 0xaf3a03, 0xe0c68a, 0xbdae93, 0x7c6f64, 0x79740e, 0x427b58,
            0xb57614, 0x9d0006,
        ),
        "mono" => p(
            0x070707, 0x121212, 0x1e1e1e, 0x181818, 0x282828, 0x4a4a4a, 0x686868, 0x939393,
            0xb6b6b6, 0xececec, 0xeaeaea, 0x333333, 0x525252, 0x909090, 0x828282, 0xa6a6a6,
            0xc8c8c8, 0xe6e6e6,
        ),
        _ => return None,
    })
}
