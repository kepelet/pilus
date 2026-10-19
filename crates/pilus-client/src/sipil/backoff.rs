//! When to try a peer that did not answer again, and when to stop trying.
//!
//! Dialing a device that is switched off is the most expensive thing an idle
//! pilus does: it resolves the device through discovery, tries a relay and then
//! direct paths, and waits out a ten second deadline, all to learn what it learned
//! three seconds earlier. A [`Backoff`] spaces those attempts further apart after
//! each failure and, after enough of them, gives up. A given-up peer is not
//! forgotten: it is retried when something says it may be back (it called us, the
//! person opened its conversation or sent to it, the app came to the front), via
//! [`Backoff::wake`].
//!
//! Nothing here reads a clock or sleeps; the caller passes `now`, so the schedule
//! is unit-testable and the poller decides how long to sleep.

use std::time::{Duration, Instant};

/// How a kind of peer is retried.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Policy {
    /// The wait after the first failure.
    pub first: Duration,
    /// The longest wait between two attempts.
    pub longest: Duration,
    /// Failures in a row after which the peer is given up on.
    pub attempts: u32,
}

impl Policy {
    /// A sipil daemon the person asked to be connected to. It is usually
    /// started right after the laptop is, so the first retries are quick; about
    /// six minutes of failures in all.
    pub const DAEMON: Self = Self {
        first: Duration::from_secs(3),
        longest: Duration::from_secs(5 * 60),
        attempts: 8,
    };

    /// A contact's pilus, asked only so a dot can be shown: nobody is waiting on
    /// it, so it starts slower. About a quarter of an hour of failures in all.
    pub const PERSON: Self = Self {
        first: Duration::from_secs(30),
        longest: Duration::from_secs(10 * 60),
        attempts: 6,
    };

    /// The wait after the `failures`-th failure in a row (the first is 1).
    fn delay(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(16);
        self.first.saturating_mul(1 << doublings).min(self.longest)
    }
}

/// Where one peer stands in being retried.
#[derive(Clone, Debug)]
pub struct Backoff {
    policy: Policy,
    failures: u32,
    /// When the next attempt is due; `None` means now.
    next: Option<Instant>,
    given_up: bool,
    /// When the last attempt failed.
    last_failure: Option<Instant>,
}

impl Backoff {
    /// A peer not yet tried: due at once.
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            failures: 0,
            next: None,
            given_up: false,
            last_failure: None,
        }
    }

    /// Whether to try now. A peer that was given up on is never due until woken.
    pub fn due(&self, now: Instant) -> bool {
        !self.given_up && self.next.is_none_or(|at| now >= at)
    }

    /// How long until the next attempt, or `None` when none is planned (the peer
    /// was given up on). Zero when it is due.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        if self.given_up {
            return None;
        }
        Some(
            self.next
                .map_or(Duration::ZERO, |at| at.saturating_duration_since(now)),
        )
    }

    /// The attempt failed. Returns whether this was the last one.
    pub fn failed(&mut self, now: Instant) -> bool {
        self.failures = self.failures.saturating_add(1);
        self.last_failure = Some(now);
        if self.failures >= self.policy.attempts {
            self.given_up = true;
            self.next = None;
        } else {
            self.next = Some(now + self.policy.delay(self.failures));
        }
        self.given_up
    }

    /// The peer answered: start over, so a later drop begins at the short waits.
    pub fn succeeded(&mut self) {
        self.failures = 0;
        self.next = None;
        self.given_up = false;
    }

    /// Something says the peer may be back, so try now. A peer that was given up
    /// on gets one attempt, not the whole ladder again: if that fails it is
    /// given up on at once.
    pub fn wake(&mut self) {
        self.given_up = false;
        self.next = None;
        self.failures = self.failures.min(self.policy.attempts.saturating_sub(1));
    }

    /// Wake a peer that was given up on, if its last failure is at least `rest`
    /// old. For triggers that can fire again and again (the app coming to the
    /// front), which must not turn into a dial each time.
    pub fn wake_if_rested(&mut self, now: Instant, rest: Duration) {
        if self.given_up
            && self
                .last_failure
                .is_none_or(|at| now.saturating_duration_since(at) >= rest)
        {
            self.wake();
        }
    }

    /// Whether the peer was given up on and is waiting to be woken.
    pub fn given_up(&self) -> bool {
        self.given_up
    }

    /// Failures in a row so far.
    pub fn failures(&self) -> u32 {
        self.failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// Every attempt a peer that never answers would get in `span`, as
    /// offsets from the start, if the caller tried the moment each was due.
    fn attempts_within(policy: Policy, span: Duration) -> Vec<Duration> {
        let start = Instant::now();
        let mut backoff = Backoff::new(policy);
        let mut now = start;
        let mut tried = Vec::new();
        while now - start <= span {
            if backoff.due(now) {
                tried.push(now - start);
                backoff.failed(now);
            }
            match backoff.wait(now) {
                Some(wait) => now += wait.max(Duration::from_millis(1)),
                None => break,
            }
        }
        tried
    }

    #[test]
    fn a_new_peer_is_due_at_once() {
        let backoff = Backoff::new(Policy::DAEMON);
        assert!(backoff.due(Instant::now()));
        assert_eq!(backoff.wait(Instant::now()), Some(Duration::ZERO));
    }

    #[test]
    fn each_failure_doubles_the_wait_up_to_the_longest() {
        let policy = Policy {
            first: secs(3),
            longest: secs(20),
            attempts: 99,
        };
        let waits: Vec<_> = (1..=6).map(|failures| policy.delay(failures)).collect();
        assert_eq!(
            waits,
            [secs(3), secs(6), secs(12), secs(20), secs(20), secs(20)]
        );
    }

    #[test]
    fn a_daemon_that_never_answers_is_given_up_on_after_a_few_minutes() {
        let tried = attempts_within(Policy::DAEMON, secs(24 * 3600));
        // The old poller asked every three seconds: 28,800 attempts in a day.
        assert_eq!(tried.len(), 8, "{tried:?}");
        assert!(*tried.last().unwrap_or(&Duration::ZERO) < secs(7 * 60));
    }

    #[test]
    fn a_contact_who_never_answers_is_asked_six_times_not_four_thousand() {
        let tried = attempts_within(Policy::PERSON, secs(24 * 3600));
        // The old presence check asked every twenty seconds: 4,320 attempts a day.
        assert_eq!(tried.len(), 6, "{tried:?}");
    }

    #[test]
    fn giving_up_is_reported_on_the_last_failure_only() {
        let now = Instant::now();
        let mut backoff = Backoff::new(Policy {
            first: secs(1),
            longest: secs(1),
            attempts: 3,
        });
        assert!(!backoff.failed(now));
        assert!(!backoff.failed(now));
        assert!(backoff.failed(now));
        assert!(backoff.given_up());
        assert!(!backoff.due(now + secs(3600)));
        assert_eq!(backoff.wait(now), None);
    }

    #[test]
    fn a_woken_peer_gets_one_attempt_and_then_rests_again() {
        let now = Instant::now();
        let mut backoff = Backoff::new(Policy::PERSON);
        for _ in 0..Policy::PERSON.attempts {
            backoff.failed(now);
        }
        assert!(backoff.given_up());
        backoff.wake();
        assert!(backoff.due(now), "waking makes it due now");
        assert!(backoff.failed(now), "one failure gives it up again");
    }

    #[test]
    fn waking_a_peer_that_is_still_being_retried_does_not_forgive_its_failures() {
        let now = Instant::now();
        let mut backoff = Backoff::new(Policy::DAEMON);
        backoff.failed(now);
        backoff.failed(now);
        backoff.wake();
        assert!(backoff.due(now));
        assert_eq!(backoff.failures(), 2);
    }

    #[test]
    fn coming_back_to_the_app_over_and_over_does_not_dial_over_and_over() {
        let start = Instant::now();
        let mut backoff = Backoff::new(Policy::PERSON);
        for _ in 0..Policy::PERSON.attempts {
            backoff.failed(start);
        }
        let rest = secs(120);
        // Back within the rest: left alone.
        backoff.wake_if_rested(start + secs(30), rest);
        assert!(backoff.given_up());
        // Back after it: one more attempt, which fails and rests again.
        backoff.wake_if_rested(start + secs(150), rest);
        assert!(backoff.due(start + secs(150)));
        backoff.failed(start + secs(150));
        backoff.wake_if_rested(start + secs(160), rest);
        assert!(backoff.given_up());
    }

    #[test]
    fn an_answer_starts_the_ladder_over() {
        let now = Instant::now();
        let mut backoff = Backoff::new(Policy::DAEMON);
        for _ in 0..4 {
            backoff.failed(now);
        }
        backoff.succeeded();
        assert_eq!(backoff.failures(), 0);
        assert!(backoff.due(now));
        backoff.failed(now);
        assert_eq!(backoff.wait(now), Some(Policy::DAEMON.first));
    }
}
