//! A native page (Mission Control, Tasks, Automations) sharing the card with
//! the terminal.
//!
//! Pi's split is the daemon's: the panel overlays one pane's frame. These
//! pages have no pane behind them, so the split is the window's own. The page
//! takes the leading half of the card and the terminal the rest; the daemon
//! sees only a smaller viewport, so it reflows once when the split opens or
//! closes and never while it stays.

/// Which way the card divides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SplitAxis {
    /// Page on the left, terminal on the right.
    Right,
    /// Page on top, terminal below.
    Down,
}

impl SplitAxis {
    /// The axis a `split("right" | "down")` request names.
    pub(super) fn from_direction(direction: &str) -> Option<Self> {
        match direction {
            "right" => Some(Self::Right),
            "down" => Some(Self::Down),
            _ => None,
        }
    }
}

/// A page split in effect, and which half holds the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PageSplit {
    pub(super) axis: SplitAxis,
    pub(super) terminal_focused: bool,
}

impl PageSplit {
    /// The split after a `direction` request: the same axis again closes it,
    /// the other axis turns it, and none opens one with the terminal focused
    /// (the page is already on screen; the new half is what was asked for).
    pub(super) fn after(current: Option<Self>, axis: SplitAxis) -> Option<Self> {
        match current {
            Some(split) if split.axis == axis => None,
            Some(split) => Some(Self { axis, ..split }),
            None => Some(Self {
                axis,
                terminal_focused: true,
            }),
        }
    }
}

/// The page's extent along the split axis inside a card `inner` pixels
/// long. Floored so the terminal half keeps the odd pixel.
pub(super) fn page_extent(inner: f32) -> f32 {
    (inner / 2.0).floor().max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_axis_twice_closes_the_split() {
        let open = PageSplit::after(None, SplitAxis::Right);
        assert_eq!(
            open,
            Some(PageSplit {
                axis: SplitAxis::Right,
                terminal_focused: true
            })
        );
        assert_eq!(PageSplit::after(open, SplitAxis::Right), None);
    }

    #[test]
    fn the_other_axis_turns_the_split_and_keeps_focus() {
        let mut open = PageSplit::after(None, SplitAxis::Right);
        if let Some(split) = open.as_mut() {
            split.terminal_focused = false;
        }
        let turned = PageSplit::after(open, SplitAxis::Down);
        assert_eq!(
            turned,
            Some(PageSplit {
                axis: SplitAxis::Down,
                terminal_focused: false
            })
        );
    }

    #[test]
    fn the_terminal_keeps_the_odd_pixel() {
        assert_eq!(page_extent(801.0), 400.0);
        assert_eq!(page_extent(-5.0), 0.0);
    }

    #[test]
    fn directions_map_to_axes() {
        assert_eq!(SplitAxis::from_direction("right"), Some(SplitAxis::Right));
        assert_eq!(SplitAxis::from_direction("down"), Some(SplitAxis::Down));
        assert_eq!(SplitAxis::from_direction("auto"), None);
    }
}
