//! The in-app toast queue.
//!
//! Every notice pilus raises, whether a daemon `Notify` push or the outcome of
//! something the user just did, lands here instead of the OS notification
//! center. The view pushes into [`Toasts`], the poll loop calls
//! [`Toasts::tick`] each frame, and `PilusView::toast_stack` paints whatever
//! is still live through `style::toast`. Nothing here touches GPUI, so the
//! timing rules are unit-testable on their own.

use std::time::{Duration, Instant};

/// Toasts on screen at once; a burst past this retires the oldest first.
const MAX_VISIBLE: usize = 4;

/// How long a toast fades out before it leaves the queue.
pub(super) const EXIT: Duration = Duration::from_millis(180);

/// What a toast reports, which sets its glyph and how long it lingers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToastKind {
    /// A daemon push or a neutral status.
    Info,
    /// An action finished.
    Success,
    /// An action failed; it stays longest because the user has to act on it.
    Error,
}

impl ToastKind {
    fn lifetime(self) -> Duration {
        match self {
            Self::Info => Duration::from_secs(5),
            Self::Success => Duration::from_millis(3500),
            Self::Error => Duration::from_secs(8),
        }
    }
}

/// One notice.
#[derive(Clone, Debug)]
pub(super) struct Toast {
    pub(super) id: u64,
    pub(super) kind: ToastKind,
    pub(super) title: Option<String>,
    pub(super) body: String,
    /// Time left on screen. It only counts down while the window is in front,
    /// so a push that arrives in the background is still there to read.
    remaining: Duration,
    /// When the fade-out began, once the toast is on its way out.
    leaving: Option<Instant>,
}

impl Toast {
    /// Opacity multiplier for the fade-out: 1 while live, easing to 0.
    pub(super) fn exit_opacity(&self, now: Instant) -> f32 {
        match self.leaving {
            None => 1.0,
            Some(since) => {
                let step = now.saturating_duration_since(since).as_secs_f32() / EXIT.as_secs_f32();
                (1.0 - step.clamp(0.0, 1.0)).powi(3)
            }
        }
    }
}

/// The live toasts, oldest first.
pub(super) struct Toasts {
    items: Vec<Toast>,
    next_id: u64,
    last_tick: Instant,
    /// A push happened since the last tick, so the view must repaint.
    fresh: bool,
}

impl Toasts {
    pub(super) fn new() -> Self {
        Self {
            items: Vec::new(),
            next_id: 0,
            last_tick: Instant::now(),
            fresh: false,
        }
    }

    /// Raise a toast. Repeating the notice already on screen restarts its
    /// clock instead of stacking a copy, so a daemon that pushes the same line
    /// in a loop stays one card.
    pub(super) fn push(&mut self, kind: ToastKind, title: Option<String>, body: String) {
        self.fresh = true;
        if let Some(existing) = self.items.iter_mut().find(|toast| {
            toast.leaving.is_none()
                && toast.kind == kind
                && toast.title == title
                && toast.body == body
        }) {
            existing.remaining = kind.lifetime();
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(Toast {
            id,
            kind,
            title,
            body,
            remaining: kind.lifetime(),
            leaving: None,
        });
        let live = self.items.iter().filter(|t| t.leaving.is_none()).count();
        let now = Instant::now();
        for toast in self
            .items
            .iter_mut()
            .filter(|toast| toast.leaving.is_none())
            .take(live.saturating_sub(MAX_VISIBLE))
        {
            toast.leaving = Some(now);
        }
    }

    /// Start fading `id` out. A toast already leaving keeps its start time.
    pub(super) fn dismiss(&mut self, id: u64, now: Instant) {
        if let Some(toast) = self.items.iter_mut().find(|toast| toast.id == id) {
            toast.leaving.get_or_insert(now);
        }
    }

    /// Advance every clock to `now`. Returns whether the view must repaint:
    /// a new push, a fade in progress, or a toast that just left.
    ///
    /// `active` is whether the window is in front; time spent behind it does
    /// not count against a toast's life. `settle` skips the fade (reduce
    /// motion).
    pub(super) fn tick(&mut self, now: Instant, active: bool, settle: bool) -> bool {
        let elapsed = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;
        let mut changed = std::mem::take(&mut self.fresh);
        for toast in &mut self.items {
            match toast.leaving {
                Some(_) => changed = true,
                None if active => {
                    toast.remaining = toast.remaining.saturating_sub(elapsed);
                    if toast.remaining.is_zero() {
                        toast.leaving = Some(now);
                        changed = true;
                    }
                }
                None => {}
            }
        }
        let before = self.items.len();
        self.items.retain(|toast| match toast.leaving {
            Some(since) => !settle && now.saturating_duration_since(since) < EXIT,
            None => true,
        });
        changed | (self.items.len() != before)
    }

    /// Whether any toast is on screen, in which case its clock and fade want a
    /// tick every frame.
    pub(super) fn any(&self) -> bool {
        !self.items.is_empty()
    }

    pub(super) fn items(&self) -> &[Toast] {
        &self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(toasts: &mut Toasts, body: &str) {
        toasts.push(ToastKind::Info, None, body.to_string());
    }

    #[test]
    fn expires_after_its_lifetime_then_fades_out() {
        let mut toasts = Toasts::new();
        push(&mut toasts, "hello");
        let start = toasts.last_tick;
        assert!(toasts.tick(start, true, false), "a push repaints");
        assert!(!toasts.tick(start + Duration::from_secs(1), true, false));
        // Past its lifetime it starts leaving but is still listed.
        assert!(toasts.tick(start + Duration::from_secs(6), true, false));
        assert_eq!(toasts.items().len(), 1);
        assert!(toasts.items()[0].exit_opacity(start + Duration::from_secs(6)) > 0.99);
        // After the fade it is gone.
        assert!(toasts.tick(start + Duration::from_secs(7), true, false));
        assert!(toasts.items().is_empty());
    }

    #[test]
    fn does_not_age_while_the_window_is_in_the_background() {
        let mut toasts = Toasts::new();
        push(&mut toasts, "hello");
        let start = toasts.last_tick;
        toasts.tick(start + Duration::from_secs(60), false, false);
        assert_eq!(toasts.items().len(), 1);
        assert!(toasts.items()[0].leaving.is_none());
    }

    #[test]
    fn repeating_a_notice_restarts_it_instead_of_stacking() {
        let mut toasts = Toasts::new();
        push(&mut toasts, "same");
        let start = toasts.last_tick;
        toasts.tick(start + Duration::from_secs(4), true, false);
        push(&mut toasts, "same");
        assert_eq!(toasts.items().len(), 1);
        toasts.tick(start + Duration::from_secs(8), true, false);
        assert!(toasts.items()[0].leaving.is_none(), "clock restarted");
    }

    #[test]
    fn a_burst_retires_the_oldest_beyond_the_cap() {
        let mut toasts = Toasts::new();
        for n in 0..MAX_VISIBLE + 2 {
            push(&mut toasts, &format!("notice {n}"));
        }
        let leaving: Vec<_> = toasts
            .items()
            .iter()
            .map(|toast| toast.leaving.is_some())
            .collect();
        assert_eq!(leaving, [true, true, false, false, false, false]);
    }

    #[test]
    fn dismiss_fades_the_toast_and_reduce_motion_drops_it_at_once() {
        let mut toasts = Toasts::new();
        push(&mut toasts, "bye");
        let start = toasts.last_tick;
        let id = toasts.items()[0].id;
        toasts.dismiss(id, start);
        assert!(toasts.tick(start, true, true));
        assert!(toasts.items().is_empty());
    }
}
