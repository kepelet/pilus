//! Whether anyone is looking at pilus, for the loops that poll.
//!
//! Most of what pilus polls (a daemon's lists, a contact's presence, the git
//! status behind a tab) is only worth the battery while someone can see it.
//! Each window's tick loop reports whether the app is in front; the pollers ask
//! here how long to wait, and the moment the app comes forward they are woken to
//! catch up, so what was skipped behind another app is fresh by the time it is
//! seen. Nothing here touches GPUI, so it is unit-testable on its own.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// How much longer a poll waits while the app is behind another one. Results
/// still arrive; they are just not fetched for a screen nobody is reading.
const BEHIND: u32 = 5;

/// How much slower the agent feeds poll with the app behind another one or
/// windowless. Less than [`BEHIND`], since they also deliver the "waiting on you"
/// notifications, which should not lag by a quarter of a minute.
const FEEDS_BEHIND: u32 = 3;

static FRONT: AtomicBool = AtomicBool::new(true);
static FRONT_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Report whether the app is in front. Called by every window's tick loop with
/// the same answer, so the first call after a change is the one that counts.
pub fn set_front(front: bool) {
    let was = FRONT.swap(front, Ordering::AcqRel);
    if was == front {
        return;
    }
    pilus_client::set_feed_slowdown(if front { 1 } else { FEEDS_BEHIND });
    if front {
        FRONT_EPOCH.fetch_add(1, Ordering::AcqRel);
        super::sipil::nudge();
    }
}

/// Whether the app is in front.
pub fn in_front() -> bool {
    FRONT.load(Ordering::Acquire)
}

/// Counts every time the app came to the front, so a poller can tell that it
/// did since it last looked.
pub fn front_epoch() -> u64 {
    FRONT_EPOCH.load(Ordering::Acquire)
}

/// `base` while the app is in front, and a multiple of it while it is behind
/// another.
pub fn scaled(base: Duration) -> Duration {
    scaled_for(base, in_front())
}

fn scaled_for(base: Duration, front: bool) -> Duration {
    if front { base } else { base * BEHIND }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poll_waits_longer_while_the_app_is_behind() {
        let base = Duration::from_secs(2);
        assert_eq!(scaled_for(base, true), base);
        assert_eq!(scaled_for(base, false), Duration::from_secs(10));
    }
}
