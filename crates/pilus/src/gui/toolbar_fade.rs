//! The pane toolbar's fade while a sidebar moves.
//!
//! The toolbar floats over the focused pane's corner, and a sidebar sliding
//! beside it drags that corner across the screen. The poll loop calls
//! [`ToolbarFade::tick`] each frame with whether a sidebar tween is in
//! flight; `render` draws the toolbar at [`ToolbarFade::alpha`] and leaves it
//! out once it is clear. Coming back, the toolbar is re-added at full
//! strength and the `style::enter` fade it is wrapped in does the easing, so
//! the two fades never stack. Nothing here touches GPUI, so the timing is
//! unit-testable on its own.

use std::time::{Duration, Instant};

use super::style::ease_out_cubic;

/// How long the toolbar takes to clear once a sidebar starts moving. Short:
/// the sidebar's own slide is the main event.
const FADE_OUT: Duration = Duration::from_millis(60);

/// Opacity moves smaller than this are not worth a repaint.
const MIN_STEP: f32 = 1.0 / 64.0;

#[derive(Debug)]
pub(super) struct ToolbarFade {
    alpha: f32,
    from: f32,
    to: f32,
    started: Instant,
}

impl ToolbarFade {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            alpha: 1.0,
            from: 1.0,
            to: 1.0,
            started: now,
        }
    }

    /// The toolbar's opacity: 1.0 solid, 0.0 clear.
    pub(super) fn alpha(&self) -> f32 {
        self.alpha
    }

    /// Whether the toolbar should be drawn at all. A clear toolbar is left
    /// out so its buttons take no pointer.
    pub(super) fn shown(&self) -> bool {
        self.alpha > 0.0
    }

    /// Advance to `now`. `hide` is true while a sidebar is moving; `animate`
    /// is false under reduced motion, which makes either change instant.
    /// Returns whether the opacity moved enough to repaint.
    pub(super) fn tick(&mut self, now: Instant, hide: bool, animate: bool) -> bool {
        let target = if hide { 0.0 } else { 1.0 };
        if target != self.to {
            self.to = target;
            self.started = now;
            // Fully gone: the toolbar is not in the tree, so let `enter`
            // fade it back instead of ramping an element nobody can see.
            self.from = if target == 1.0 && self.alpha == 0.0 {
                1.0
            } else {
                self.alpha
            };
            if !animate {
                self.from = target;
            }
        }
        let step = (now.saturating_duration_since(self.started).as_secs_f32()
            / FADE_OUT.as_secs_f32())
        .clamp(0.0, 1.0);
        let next = if step >= 1.0 {
            self.to
        } else {
            self.from + (self.to - self.from) * ease_out_cubic(step)
        };
        let moved = (next - self.alpha).abs() >= MIN_STEP || (next != self.alpha && step >= 1.0);
        if moved {
            self.alpha = next;
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn clears_while_a_sidebar_moves_and_returns_after() {
        let t0 = Instant::now();
        let mut fade = ToolbarFade::new(t0);
        assert!(fade.shown());

        fade.tick(t0, true, true);
        fade.tick(t0 + ms(30), true, true);
        assert!(fade.alpha() > 0.0 && fade.alpha() < 1.0);
        fade.tick(t0 + ms(120), true, true);
        assert!(!fade.shown());

        // Back from fully clear: full strength at once, `enter` eases it.
        fade.tick(t0 + ms(500), false, true);
        assert_eq!(fade.alpha(), 1.0);
    }

    #[test]
    fn a_short_move_ramps_back_from_where_it_was() {
        let t0 = Instant::now();
        let mut fade = ToolbarFade::new(t0);
        fade.tick(t0, true, true);
        fade.tick(t0 + ms(30), true, true);
        let mid = fade.alpha();
        fade.tick(t0 + ms(35), false, true);
        assert!(fade.alpha() <= 1.0 && fade.alpha() >= mid - 0.05);
        fade.tick(t0 + ms(400), false, true);
        assert_eq!(fade.alpha(), 1.0);
    }

    #[test]
    fn reduced_motion_is_instant() {
        let t0 = Instant::now();
        let mut fade = ToolbarFade::new(t0);
        assert!(fade.tick(t0, true, false));
        assert!(!fade.shown());
        assert!(fade.tick(t0, false, false));
        assert_eq!(fade.alpha(), 1.0);
    }
}
