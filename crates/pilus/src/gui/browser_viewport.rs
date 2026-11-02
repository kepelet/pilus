//! The shape a Browser tab gives its page: the column's full width, or a
//! smaller frame with the proportions of a screen the person wants to check.
//!
//! The webview is a native view that lays out from GPUI's bounds, and GPUI can
//! neither clip nor scroll one, so a card wider than the column would paint
//! over its neighbour. The card is therefore always inside the room, with the
//! shape's proportions, and the page is zoomed (`WKWebView.pageZoom`) by the
//! amount the card is smaller than the shape. The page's layout viewport is
//! `card / zoom`, which is the shape's own size: a 16:10 page lays out at 1280
//! CSS pixels and its media queries answer for that width, however narrow the
//! column, and is drawn smaller to fit. This module is the arithmetic only:
//! `browser_tab` measures the room the column leaves, asks [`Viewport::fit`]
//! for the card and [`Viewport::scale`] for the zoom, and centres the card.
//!
//! A shape never grows past its real size, as a larger one would stop being a
//! test of that screen. An exact size an agent asks for
//! (`browser viewport 1000x800`) is held to the same rule: the page always
//! lays out at that size, and the answer says how far it was scaled to fit.

/// What a page tab shows its page in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Viewport {
    /// The whole room the column leaves. The page decides how it lays out.
    #[default]
    Responsive,
    /// A widescreen laptop panel, 1280×800.
    Wide,
    /// An older monitor or a tablet held sideways, 1024×768.
    Standard,
    /// A phone held upright.
    Mobile,
    /// A tablet held upright.
    Tablet,
    /// An exact size in CSS pixels, which only an agent can ask for: the menu
    /// lists the presets.
    Custom(u16, u16),
}

impl Viewport {
    /// Every viewport, in the order the menu lists them.
    pub const ALL: [Self; 5] = [
        Self::Responsive,
        Self::Wide,
        Self::Standard,
        Self::Mobile,
        Self::Tablet,
    ];

    /// The smallest and largest side an exact size may have. Below the first
    /// a page is not a page; above the second no column has the room, so the
    /// request could only ever be shrunk.
    pub const MIN_SIDE: u16 = 100;
    pub const MAX_SIDE: u16 = 4096;

    /// The preset's wording.
    pub fn label(self) -> &'static str {
        match self {
            Self::Responsive => "Responsive",
            Self::Wide => "16:10",
            Self::Standard => "4:3",
            Self::Mobile => "Mobile",
            Self::Tablet => "Tablet",
            Self::Custom(..) => "Custom",
        }
    }

    /// The name a script uses for it, which [`Self::parse`] takes back.
    pub fn name(self) -> String {
        match self {
            Self::Responsive => "responsive".into(),
            Self::Wide => "16:10".into(),
            Self::Standard => "4:3".into(),
            Self::Mobile => "mobile".into(),
            Self::Tablet => "tablet".into(),
            Self::Custom(width, height) => format!("{width}x{height}"),
        }
    }

    /// A viewport from the word a script or a person types: a preset's name
    /// (`mobile`, `16:10`), or `WIDTHxHEIGHT` in CSS pixels.
    pub fn parse(text: &str) -> Result<Self, String> {
        let word = text.trim().to_ascii_lowercase();
        match word.as_str() {
            "responsive" | "full" | "reset" | "none" => return Ok(Self::Responsive),
            "16:10" | "wide" => return Ok(Self::Wide),
            "4:3" | "standard" => return Ok(Self::Standard),
            "mobile" | "phone" => return Ok(Self::Mobile),
            "tablet" => return Ok(Self::Tablet),
            _ => {}
        }
        let bad = || {
            format!(
                "unknown viewport {text:?}; use responsive, 16:10, 4:3, mobile, tablet or \
                 WIDTHxHEIGHT in pixels, like 1000x800"
            )
        };
        let (width, height) = word.split_once(['x', '\u{d7}']).ok_or_else(bad)?;
        let side = |digits: &str| -> Result<u16, String> {
            let value: u16 = digits.trim().parse().map_err(|_| bad())?;
            if (Self::MIN_SIDE..=Self::MAX_SIDE).contains(&value) {
                Ok(value)
            } else {
                Err(format!(
                    "a viewport side is {} to {} pixels, not {value}",
                    Self::MIN_SIDE,
                    Self::MAX_SIDE
                ))
            }
        };
        Ok(Self::Custom(side(width)?, side(height)?))
    }

    /// Width and height in CSS pixels. Every shape but Responsive is a real
    /// size: the two ratios stand for a laptop panel (1280×800) and an older
    /// monitor (1024×768), so a script that asks for one gets the same layout
    /// whenever the room allows.
    fn shape(self) -> Option<(f32, f32)> {
        match self {
            Self::Responsive => None,
            Self::Wide => Some((1280.0, 800.0)),
            Self::Standard => Some((1024.0, 768.0)),
            Self::Mobile => Some((390.0, 844.0)),
            Self::Tablet => Some((768.0, 1024.0)),
            Self::Custom(width, height) => Some((f32::from(width), f32::from(height))),
        }
    }

    /// Whether the shape is a real size, which is never drawn larger than it
    /// is.
    fn capped(self) -> bool {
        self != Self::Responsive
    }

    /// Whether the page is held to a box smaller than the room.
    pub fn constrained(self) -> bool {
        self != Self::Responsive
    }

    /// How much of the shape's size fits in `room`, at most 1: the zoom the
    /// page is drawn at. `None` for Responsive, which has no shape, and when
    /// there is no room.
    fn fit_scale(self, room: (f32, f32)) -> Option<f32> {
        let (shape_w, shape_h) = self.shape()?;
        let (room_w, room_h) = room;
        if room_w <= 0.0 || room_h <= 0.0 {
            return None;
        }
        let scale = (room_w / shape_w).min(room_h / shape_h);
        Some(if self.capped() { scale.min(1.0) } else { scale })
    }

    /// The card the page is drawn in inside `room` (width, height), or `None`
    /// when it takes all of it. Proportions are kept and neither side overruns
    /// `room`; the page inside lays out at the shape's size (see
    /// [`Self::scale`]).
    pub fn fit(self, room: (f32, f32)) -> Option<(f32, f32)> {
        let (shape_w, shape_h) = self.shape()?;
        let scale = self.fit_scale(room)?;
        Some(((shape_w * scale).floor(), (shape_h * scale).floor()))
    }

    /// The zoom the page is drawn at in `room`: 1 when the card is the shape's
    /// own size or the page is Responsive, less when the column is too small
    /// for it.
    pub fn scale(self, room: (f32, f32)) -> f64 {
        let Some((shape_w, _)) = self.shape() else {
            return 1.0;
        };
        // The card is floored to whole pixels, so the zoom is taken from its
        // width: the layout viewport is then the shape's width to the pixel.
        self.fit(room)
            .map_or(1.0, |(width, _)| f64::from(width / shape_w))
    }

    /// The size in CSS pixels the page lays out at in `room`: the shape's own
    /// size, however small the card, or all of the room for Responsive. `None`
    /// before the Browser has ever been drawn, when there is no room to speak
    /// of.
    pub fn size_in(self, room: (f32, f32)) -> Option<(u32, u32)> {
        if room.0 <= 0.0 || room.1 <= 0.0 {
            return None;
        }
        let (width, height) = self.shape().unwrap_or(room);
        // Both are floored and positive, so the casts lose nothing.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some((width.floor() as u32, height.floor() as u32))
    }

    /// The menu's row: the name and the size the page lays out at, like
    /// `Mobile (390×844)`. Responsive shows the room it takes.
    pub fn describe(self, room: (u32, u32)) -> String {
        #[allow(clippy::cast_precision_loss)]
        let size = self.size_in((room.0 as f32, room.1 as f32));
        match size {
            Some((width, height)) => format!("{} ({width}\u{d7}{height})", self.label()),
            None => self.label().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responsive_takes_the_whole_room() {
        assert_eq!(Viewport::Responsive.fit((500.0, 700.0)), None);
        assert!(!Viewport::Responsive.constrained());
    }

    #[test]
    fn a_ratio_fills_the_limiting_side() {
        // A tall, narrow column is limited by its width.
        assert_eq!(Viewport::Wide.fit((480.0, 900.0)), Some((480.0, 300.0)));
        // A wide, short one by its height.
        assert_eq!(
            Viewport::Standard.fit((1200.0, 600.0)),
            Some((800.0, 600.0))
        );
    }

    #[test]
    fn a_ratio_stops_at_its_real_size() {
        assert_eq!(Viewport::Wide.fit((2000.0, 1500.0)), Some((1280.0, 800.0)));
        assert_eq!(
            Viewport::Standard.fit((2000.0, 1500.0)),
            Some((1024.0, 768.0))
        );
        assert_eq!(
            Viewport::Wide.describe((2000, 1500)),
            "16:10 (1280\u{d7}800)"
        );
    }

    #[test]
    fn a_device_never_grows_past_its_real_size() {
        assert_eq!(Viewport::Mobile.fit((1400.0, 1400.0)), Some((390.0, 844.0)));
        assert_eq!(
            Viewport::Tablet.fit((2000.0, 2000.0)),
            Some((768.0, 1024.0))
        );
    }

    #[test]
    fn a_device_shrinks_to_a_small_room_and_keeps_its_proportions() {
        let (w, h) = Viewport::Mobile.fit((300.0, 900.0)).expect("fits");
        assert_eq!(w, 300.0);
        assert!((w / h - 390.0 / 844.0).abs() < 0.01);
        let (w, h) = Viewport::Tablet.fit((900.0, 512.0)).expect("fits");
        assert_eq!(h, 512.0);
        assert!(w <= 900.0);
    }

    #[test]
    fn no_room_means_no_box() {
        assert_eq!(Viewport::Mobile.fit((0.0, 500.0)), None);
        assert_eq!(Viewport::Wide.fit((500.0, -1.0)), None);
    }

    #[test]
    fn a_name_parses_back_to_its_viewport() {
        for viewport in Viewport::ALL {
            assert_eq!(Viewport::parse(&viewport.name()), Ok(viewport));
        }
        assert_eq!(Viewport::parse("Phone"), Ok(Viewport::Mobile));
        assert_eq!(Viewport::parse("1000x800"), Ok(Viewport::Custom(1000, 800)));
        assert_eq!(
            Viewport::parse(" 640\u{d7}480 "),
            Ok(Viewport::Custom(640, 480))
        );
        assert_eq!(Viewport::Custom(1000, 800).name(), "1000x800");
    }

    #[test]
    fn a_size_outside_the_limits_or_a_stray_word_is_refused() {
        for text in [
            "",
            "huge",
            "1000",
            "1000x",
            "x800",
            "50x800",
            "1000x9000",
            "ax9",
        ] {
            assert!(Viewport::parse(text).is_err(), "{text:?} should be refused");
        }
    }

    #[test]
    fn an_exact_size_is_drawn_whole_when_it_fits_and_scaled_when_it_does_not() {
        let custom = Viewport::Custom(1000, 500);
        assert_eq!(custom.fit((1400.0, 900.0)), Some((1000.0, 500.0)));
        assert_eq!(custom.scale((1400.0, 900.0)), 1.0);
        assert_eq!(custom.fit((500.0, 900.0)), Some((500.0, 250.0)));
        assert_eq!(custom.scale((500.0, 900.0)), 0.5);
    }

    #[test]
    fn the_page_always_lays_out_at_the_shapes_width() {
        // A column of 480 or 700 pixels: the card shrinks, the zoom follows, and
        // card ÷ zoom is the shape's width either way.
        for room in [(480.0, 900.0), (700.0, 900.0), (1000.0, 500.0)] {
            for viewport in [Viewport::Wide, Viewport::Standard, Viewport::Mobile] {
                let (card_w, _) = viewport.fit(room).expect("fits");
                let (shape_w, _) = viewport.shape().expect("a shape");
                let layout = f64::from(card_w) / viewport.scale(room);
                assert!(
                    (layout - f64::from(shape_w)).abs() < 1e-3,
                    "{viewport:?} in {room:?} laid out at {layout}"
                );
            }
        }
        assert_eq!(Viewport::Responsive.scale((480.0, 900.0)), 1.0);
    }

    #[test]
    fn the_size_the_page_gets_is_described() {
        assert_eq!(
            Viewport::Mobile.describe((1400, 1400)),
            "Mobile (390\u{d7}844)"
        );
        // A narrow column scales the page; it does not change what it lays out at.
        assert_eq!(
            Viewport::Wide.describe((480, 900)),
            "16:10 (1280\u{d7}800)"
        );
        assert_eq!(
            Viewport::Responsive.describe((480, 700)),
            "Responsive (480\u{d7}700)"
        );
        // Before the Browser has been drawn there is no size to show.
        assert_eq!(Viewport::Mobile.describe((0, 0)), "Mobile");
        assert_eq!(Viewport::Responsive.size_in((0.0, 0.0)), None);
    }

    #[test]
    fn every_viewport_has_a_distinct_label() {
        let mut labels: Vec<_> = Viewport::ALL.iter().map(|v| v.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), Viewport::ALL.len());
    }
}
