//! Whether an agent may drive the Browser, and who said so.
//!
//! An agent that reaches the control socket (from this Mac or through a
//! tunnel) asks with `pilus ctl browser control`. The request waits on a banner
//! in the Browser: Allow, or Deny. Until the person allows it, every verb that
//! opens, reads or drives a page is refused with a message that tells the agent
//! how to ask. While it is allowed the banner says so and offers Take over, and
//! a real click or keypress in the page takes over too (the page's script can
//! tell those from the agent's synthetic events).
//!
//! No timer runs here. A refusal is remembered as a deadline and compared when
//! the next request arrives, so a nagging agent meets a closed door without the
//! window polling for anything. The reason an agent gives is shown as plain
//! text and is only ever a hint: it is the agent's word.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::ctl::{Replier, Reply};

/// How long an agent that was turned down is turned away without a banner.
const DECLINED_FOR: Duration = Duration::from_secs(60);
/// The same after the person took the Browser back: shorter, since they are
/// likely to hand it over again, but not while their hands are on it.
const TAKEN_OVER_FOR: Duration = Duration::from_secs(15);
/// The longest reason shown.
const REASON_MAX: usize = 160;

/// Where the Browser stands with the agents.
#[derive(Default)]
pub(super) enum Control {
    /// Nobody asked, or the last grant ended.
    #[default]
    Idle,
    /// An agent asked and the banner is waiting for the person. Every client
    /// that asked meanwhile is answered together.
    Asking {
        reason: String,
        waiting: Vec<Replier>,
    },
    /// The person said yes.
    Granted { reason: String },
    /// Turned away until `until`.
    Refused { until: Instant, why: &'static str },
}

/// What the person sees of an agent's reason: one line of printable text.
fn clean(reason: Option<&str>) -> String {
    let line: String = reason
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(REASON_MAX) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line,
    }
}

fn granted() -> Reply {
    Reply::data(json!({ "granted": true }))
}

impl Control {
    /// Whether verbs that drive the Browser may run.
    pub(super) fn permits(&self) -> bool {
        matches!(self, Self::Granted { .. })
    }

    /// A word for `pilus ctl state`.
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Idle | Self::Refused { .. } => "none",
            Self::Asking { .. } => "asking",
            Self::Granted { .. } => "granted",
        }
    }

    /// The reason on the banner, if there is one.
    pub(super) fn reason(&self) -> Option<&str> {
        match self {
            Self::Asking { reason, .. } | Self::Granted { reason } => {
                Some(reason.as_str()).filter(|reason| !reason.is_empty())
            }
            _ => None,
        }
    }

    /// Why a driving verb was refused, written for the agent that reads it.
    pub(super) fn refusal(&self, now: Instant) -> String {
        match self {
            Self::Granted { .. } => String::new(),
            Self::Asking { .. } => {
                "the person has not answered yet; wait for `pilus ctl browser control` to return"
                    .to_string()
            }
            Self::Refused { until, why } if now < *until => why.to_string(),
            _ => "the person has not given you the Browser; ask with \
                  `pilus ctl browser control <why you need it>` and wait for them to allow it"
                .to_string(),
        }
    }

    /// An agent asks. `Some` is the answer now; `None` means the banner is up
    /// and `replier` is answered when the person decides.
    pub(super) fn ask(
        &mut self,
        reason: Option<&str>,
        replier: &Replier,
        now: Instant,
    ) -> Option<Reply> {
        match self {
            Self::Granted { .. } => Some(granted()),
            Self::Refused { until, why } if now < *until => Some(Reply::err(*why)),
            Self::Asking { waiting, .. } => {
                waiting.push(replier.clone());
                None
            }
            Self::Idle | Self::Refused { .. } => {
                *self = Self::Asking {
                    reason: clean(reason),
                    waiting: vec![replier.clone()],
                };
                None
            }
        }
    }

    /// The person allowed it. True if something was waiting.
    pub(super) fn allow(&mut self) -> bool {
        let Self::Asking { reason, waiting } = std::mem::take(self) else {
            return false;
        };
        for replier in waiting {
            replier.send(granted());
        }
        *self = Self::Granted { reason };
        true
    }

    /// The person said no. True if something was waiting.
    pub(super) fn deny(&mut self, now: Instant) -> bool {
        let Self::Asking { waiting, .. } = std::mem::take(self) else {
            return false;
        };
        let why = "the person declined; do not ask again for a minute";
        for replier in waiting {
            replier.send(Reply::err(why));
        }
        *self = Self::Refused {
            until: now + DECLINED_FOR,
            why,
        };
        true
    }

    /// The person took the Browser back. True if an agent had it.
    pub(super) fn take_over(&mut self, now: Instant) -> bool {
        if !matches!(self, Self::Granted { .. }) {
            return false;
        }
        *self = Self::Refused {
            until: now + TAKEN_OVER_FOR,
            why: "the person took over the Browser; wait a little before asking again",
        };
        true
    }

    /// The agent hands it back. True if it had it, or was still asking.
    pub(super) fn release(&mut self) -> bool {
        if matches!(self, Self::Idle | Self::Refused { .. }) {
            return false;
        }
        // Anyone still waiting is dropped, which tells their client.
        *self = Self::Idle;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asker() -> (Replier, std::sync::mpsc::Receiver<Reply>) {
        Replier::pair()
    }

    #[test]
    fn nothing_is_permitted_until_the_person_allows_it() {
        let now = Instant::now();
        let mut control = Control::default();
        assert!(!control.permits());
        assert!(control.refusal(now).contains("browser control"));
        let (replier, answer) = asker();
        assert!(control.ask(Some("check the app"), &replier, now).is_none());
        assert_eq!(control.label(), "asking");
        assert_eq!(control.reason(), Some("check the app"));
        assert!(!control.permits(), "asking is not permission");
        assert!(control.refusal(now).contains("not answered"));
        assert!(answer.try_recv().is_err(), "nobody is answered yet");
        assert!(control.allow());
        assert!(control.permits());
        assert_eq!(
            answer.try_recv().expect("answered").data,
            Some(json!({"granted": true}))
        );
    }

    #[test]
    fn everyone_who_asked_is_answered_together() {
        let now = Instant::now();
        let mut control = Control::default();
        let (first, first_answer) = asker();
        let (second, second_answer) = asker();
        control.ask(Some("one"), &first, now);
        control.ask(Some("two"), &second, now);
        // The first reason stays on the banner.
        assert_eq!(control.reason(), Some("one"));
        control.deny(now);
        assert!(!first_answer.try_recv().expect("told").ok);
        assert!(!second_answer.try_recv().expect("told").ok);
    }

    #[test]
    fn a_no_is_remembered_without_a_timer() {
        let now = Instant::now();
        let mut control = Control::default();
        let (replier, _answer) = asker();
        control.ask(None, &replier, now);
        assert!(control.deny(now));
        let (again, _) = asker();
        // Within the minute: refused at once, and no banner.
        let soon = now + Duration::from_secs(30);
        let reply = control.ask(None, &again, soon).expect("refused");
        assert!(!reply.ok);
        assert_eq!(control.label(), "none");
        assert!(control.refusal(soon).contains("declined"));
        // After it: asking raises the banner again.
        let later = now + DECLINED_FOR + Duration::from_secs(1);
        assert!(control.ask(None, &again, later).is_none());
        assert_eq!(control.label(), "asking");
    }

    #[test]
    fn taking_over_ends_the_grant_and_closes_the_door_briefly() {
        let now = Instant::now();
        let mut control = Control::default();
        let (replier, _answer) = asker();
        control.ask(Some("x"), &replier, now);
        control.allow();
        assert!(control.take_over(now));
        assert!(!control.permits());
        assert!(control.refusal(now).contains("took over"));
        let (again, _) = asker();
        assert!(control.ask(None, &again, now).is_some(), "refused at once");
        assert!(
            control
                .ask(None, &again, now + TAKEN_OVER_FOR + Duration::from_secs(1))
                .is_none(),
            "asks again afterwards"
        );
        // Nothing to take over from.
        assert!(!Control::default().take_over(now));
    }

    #[test]
    fn asking_when_already_allowed_just_says_yes() {
        let now = Instant::now();
        let mut control = Control::default();
        let (replier, _answer) = asker();
        control.ask(None, &replier, now);
        control.allow();
        let (again, _) = asker();
        let reply = control.ask(Some("again"), &again, now).expect("yes");
        assert_eq!(reply.data, Some(json!({"granted": true})));
    }

    #[test]
    fn releasing_hands_it_back_and_drops_anyone_waiting() {
        let now = Instant::now();
        let mut control = Control::default();
        let (replier, answer) = asker();
        control.ask(None, &replier, now);
        drop(replier);
        assert!(control.release());
        assert!(matches!(
            answer.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        assert!(!control.release(), "nothing left to release");
        assert!(!control.permits());
    }

    #[test]
    fn a_reason_is_one_short_line_of_plain_text() {
        assert_eq!(clean(Some("  a\n\tb\u{7}  c ")), "a b c");
        assert_eq!(clean(None), "");
        let long = clean(Some(&"x".repeat(1000)));
        assert_eq!(long.chars().count(), REASON_MAX + 1);
        assert!(long.ends_with('…'));
    }
}
