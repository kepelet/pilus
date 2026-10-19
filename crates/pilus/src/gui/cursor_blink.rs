//! The terminal cursor's blink clock.
//!
//! The daemon sends a steady cursor, so the blink is ours. The poll loop
//! calls [`CursorBlink::tick`] each frame and repaints only when the returned
//! opacity moved, which means the long holds at either end cost no redraws.
//! `render_grid` fades the cursor block and its glyph by [`CursorBlink::alpha`].
//!
//! A cursor that has not moved for [`BLINK_FOR`] stops blinking and stays solid,
//! as terminals commonly do: the fade repaints the whole grid some twenty times
//! a second, which on an idle window is most of what it costs. Moving the cursor,
//! or coming back to the window, starts the blink again.
//! Nothing here touches GPUI, so the timing is unit-testable on its own.

use std::time::{Duration, Instant};

/// How long the cursor stays solid after it appears or moves, so a typist
/// never watches it vanish mid-word.
const HOLD_ON: Duration = Duration::from_millis(520);
/// Fade from solid to clear.
const FADE_OUT: Duration = Duration::from_millis(240);
/// Clear dwell: shorter than the solid one so the cursor is easy to find.
const HOLD_OFF: Duration = Duration::from_millis(180);
/// Fade from clear back to solid.
const FADE_IN: Duration = Duration::from_millis(180);

/// How long a cursor keeps blinking after it last moved.
const BLINK_FOR: Duration = Duration::from_secs(10);

/// Opacity moves smaller than this are not worth a repaint.
const MIN_STEP: f32 = 1.0 / 64.0;

/// Where the blink cycle is, and the cell it was last restarted for.
#[derive(Debug)]
pub(super) struct CursorBlink {
    epoch: Instant,
    cursor: Option<(u16, u16)>,
    alpha: f32,
}

impl CursorBlink {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            epoch: now,
            cursor: None,
            alpha: 1.0,
        }
    }

    /// The cursor's opacity: 1.0 solid, 0.0 clear.
    pub(super) fn alpha(&self) -> f32 {
        self.alpha
    }

    /// Advance to `now`. `cursor` is the cell to blink, or `None` when there
    /// is nothing to draw; `blinking` is false while the window is in the
    /// background or motion is reduced, which pins the cursor solid. Returns
    /// whether the opacity changed enough to repaint.
    pub(super) fn tick(
        &mut self,
        now: Instant,
        cursor: Option<(u16, u16)>,
        blinking: bool,
    ) -> bool {
        // Moving the cursor (typing, an app redrawing its caret) restarts the
        // cycle at solid. So does a pause: the epoch trails `now` while not
        // blinking, so returning to the window starts from solid too.
        if cursor != self.cursor {
            self.cursor = cursor;
            self.epoch = now;
        }
        let target = if blinking && cursor.is_some() {
            let since = now.saturating_duration_since(self.epoch);
            // The epoch is left where it is, or the blink would restart.
            if since < BLINK_FOR {
                alpha_at(since)
            } else {
                1.0
            }
        } else {
            self.epoch = now;
            1.0
        };
        let settled = target == 0.0 || target == 1.0;
        let moved = (target - self.alpha).abs() >= MIN_STEP || (settled && target != self.alpha);
        if moved {
            self.alpha = target;
        }
        moved
    }
}

/// The opacity `elapsed` into a cycle that starts solid.
fn alpha_at(elapsed: Duration) -> f32 {
    let cycle = HOLD_ON + FADE_OUT + HOLD_OFF + FADE_IN;
    let mut at = Duration::from_nanos((elapsed.as_nanos() % cycle.as_nanos()) as u64);
    if at < HOLD_ON {
        return 1.0;
    }
    at -= HOLD_ON;
    if at < FADE_OUT {
        return 1.0 - smoothstep(at.as_secs_f32() / FADE_OUT.as_secs_f32());
    }
    at -= FADE_OUT;
    if at < HOLD_OFF {
        return 0.0;
    }
    at -= HOLD_OFF;
    smoothstep(at.as_secs_f32() / FADE_IN.as_secs_f32())
}

/// Ease in and out, so neither end of a fade has a visible corner.
fn smoothstep(step: f32) -> f32 {
    let step = step.clamp(0.0, 1.0);
    step * step * (3.0 - 2.0 * step)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    #[test]
    fn the_cycle_holds_solid_fades_out_rests_clear_and_fades_back() {
        assert_eq!(alpha_at(ms(0)), 1.0);
        assert_eq!(alpha_at(HOLD_ON - ms(1)), 1.0);
        let midway = alpha_at(HOLD_ON + FADE_OUT / 2);
        assert!((midway - 0.5).abs() < 0.01, "{midway}");
        assert_eq!(alpha_at(HOLD_ON + FADE_OUT + HOLD_OFF / 2), 0.0);
        let rising = alpha_at(HOLD_ON + FADE_OUT + HOLD_OFF + FADE_IN / 2);
        assert!((rising - 0.5).abs() < 0.01, "{rising}");
        let cycle = HOLD_ON + FADE_OUT + HOLD_OFF + FADE_IN;
        assert_eq!(alpha_at(cycle), 1.0);
    }

    #[test]
    fn a_moved_cursor_restarts_the_cycle_solid() {
        let start = Instant::now();
        let mut blink = CursorBlink::new(start);
        blink.tick(start, Some((1, 1)), true);
        let cleared = start + HOLD_ON + FADE_OUT + HOLD_OFF / 2;
        assert!(blink.tick(cleared, Some((1, 1)), true));
        assert_eq!(blink.alpha(), 0.0);
        assert!(blink.tick(cleared, Some((2, 1)), true));
        assert_eq!(blink.alpha(), 1.0);
    }

    #[test]
    fn holds_do_not_ask_for_repaints() {
        let start = Instant::now();
        let mut blink = CursorBlink::new(start);
        blink.tick(start, Some((0, 0)), true);
        assert!(!blink.tick(start + ms(100), Some((0, 0)), true));
        assert!(!blink.tick(start + ms(400), Some((0, 0)), true));
    }

    #[test]
    fn a_cursor_that_has_not_moved_for_a_while_stops_blinking_until_it_moves() {
        let start = Instant::now();
        let mut blink = CursorBlink::new(start);
        blink.tick(start, Some((3, 4)), true);
        // Mid-fade, still inside the blinking time: it blinks.
        let mut now = start;
        let mut repaints = 0;
        while now < start + BLINK_FOR {
            now += ms(16);
            repaints += u32::from(blink.tick(now, Some((3, 4)), true));
        }
        assert!(repaints > 100, "{repaints}");
        // Past it, nothing repaints however long the window sits there.
        now = start + BLINK_FOR + ms(1);
        blink.tick(now, Some((3, 4)), true);
        assert_eq!(blink.alpha(), 1.0);
        let mut quiet = 0;
        for step in 1..=200 {
            quiet += u32::from(blink.tick(now + ms(16 * step), Some((3, 4)), true));
        }
        assert_eq!(quiet, 0);
        // Typing moves the cursor, which blinks again from solid.
        let later = now + ms(16 * 201);
        blink.tick(later, Some((4, 4)), true);
        assert_eq!(blink.alpha(), 1.0);
        assert!(blink.tick(later + HOLD_ON + FADE_OUT / 2, Some((4, 4)), true));
        assert!(blink.alpha() < 1.0);
    }

    #[test]
    fn a_background_window_or_reduced_motion_pins_the_cursor_solid() {
        let start = Instant::now();
        let mut blink = CursorBlink::new(start);
        blink.tick(start, Some((0, 0)), true);
        let cleared = start + HOLD_ON + FADE_OUT + HOLD_OFF / 2;
        blink.tick(cleared, Some((0, 0)), true);
        assert_eq!(blink.alpha(), 0.0);
        assert!(blink.tick(cleared, Some((0, 0)), false));
        assert_eq!(blink.alpha(), 1.0);
        assert!(!blink.tick(cleared + ms(700), Some((0, 0)), false));
    }
}
