//! The shape a Browser tab gives its page: the column's full width, or a
//! smaller frame with the proportions of a screen the person wants to check.
//!
//! The webview is a native view that lays out from GPUI's bounds, so a
//! viewport is nothing but a smaller box the page is placed in; the page's own
//! CSS sees a narrower window and answers with its responsive layout. This
//! module is the arithmetic only: `browser_tab` measures the room the column
//! leaves, asks [`Viewport::fit`] for the box, and centres it.
//!
//! Every shape fits inside the room it is given and keeps its proportions, so
//! a narrow column shrinks a phone rather than clipping it. A phone or tablet
//! never grows past its real size, as a larger one would stop being a test of
//! that device.

/// What a page tab shows its page in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Viewport {
    /// The whole room the column leaves. The page decides how it lays out.
    #[default]
    Responsive,
    /// A widescreen laptop panel.
    Wide,
    /// An older monitor or a tablet held sideways.
    Standard,
    /// A phone held upright.
    Mobile,
    /// A tablet held upright.
    Tablet,
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

    /// The menu's wording.
    pub fn label(self) -> &'static str {
        match self {
            Self::Responsive => "Responsive",
            Self::Wide => "16:10",
            Self::Standard => "4:3",
            Self::Mobile => "Mobile",
            Self::Tablet => "Tablet",
        }
    }

    /// Width and height in CSS pixels. A device viewport is its real size; a
    /// ratio is only a proportion, so it has no size of its own.
    fn shape(self) -> Option<(f32, f32)> {
        match self {
            Self::Responsive => None,
            Self::Wide => Some((16.0, 10.0)),
            Self::Standard => Some((4.0, 3.0)),
            Self::Mobile => Some((390.0, 844.0)),
            Self::Tablet => Some((768.0, 1024.0)),
        }
    }

    /// Whether the shape is a real device, which is never drawn larger than
    /// it is.
    fn capped(self) -> bool {
        matches!(self, Self::Mobile | Self::Tablet)
    }

    /// Whether the page is held to a box smaller than the room.
    pub fn constrained(self) -> bool {
        self != Self::Responsive
    }

    /// The box for the page inside `room` (width, height), or `None` when it
    /// takes all of it. Proportions are kept and neither side overruns `room`.
    pub fn fit(self, room: (f32, f32)) -> Option<(f32, f32)> {
        let (shape_w, shape_h) = self.shape()?;
        let (room_w, room_h) = room;
        if room_w <= 0.0 || room_h <= 0.0 {
            return None;
        }
        let mut scale = (room_w / shape_w).min(room_h / shape_h);
        if self.capped() {
            scale = scale.min(1.0);
        }
        Some(((shape_w * scale).floor(), (shape_h * scale).floor()))
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
    fn every_viewport_has_a_distinct_label() {
        let mut labels: Vec<_> = Viewport::ALL.iter().map(|v| v.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), Viewport::ALL.len());
    }
}
