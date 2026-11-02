//! Who opened a Browser tab, and who may use it right now.
//!
//! Control is per tab, not per Browser. A tab an agent opens (`pilus ctl
//! browser new` or `open`) is the agent's, marked as such in the strip, and no
//! one has to say yes first. A tab the person opens is theirs: an agent that
//! reaches the control socket cannot read it, drive it, close it or select it,
//! until it *asks* for that one tab (`browser request N`) and the person
//! allows it. An allowed tab is lent: it is the person's still, and goes back
//! to them when the agent releases it or they take it back.
//!
//! While an agent works in a tab it *holds* it, and the window lays a native
//! shield over the page (`browser_shield`) so a click, a scroll or a key from
//! the person cannot interrupt a half-done form. The shield's Take over button
//! ends the hold. So does the agent's `release`. Nothing here runs on a timer:
//! a hold ends when someone ends it, and a refusal is a deadline compared when
//! the agent next asks.

use std::time::{Duration, Instant};

use crate::ctl::Replier;

/// How long an agent is turned away from a tab the person took back. Short,
/// since they may hand it over again, but not while their hands are on it.
const TAKEN_BACK_FOR: Duration = Duration::from_secs(15);
/// How long an agent that was told no is turned away from that tab.
const DECLINED_FOR: Duration = Duration::from_secs(60);
/// The longest reason shown.
const REASON_MAX: usize = 160;

/// Who opened a tab. It does not change when the person takes the tab back
/// or lends it: the strip keeps showing where it came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Opener {
    Person,
    Agent,
}

impl Opener {
    /// The word `pilus ctl browser tabs` prints.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Person => "person",
            Self::Agent => "agent",
        }
    }
}

/// A door shut on the agent until `until`, and what it is told.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Shut {
    until: Instant,
    why: &'static str,
}

/// What may be done to one tab, by whom.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Access {
    opener: Opener,
    /// An agent is working in it; the person is locked out.
    held: bool,
    /// The person allowed an agent into a tab they opened.
    lent: bool,
    shut: Option<Shut>,
}

/// What asking for a tab comes to.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum Ask {
    /// The agent already may use it; nothing to ask.
    Already,
    /// The person has to decide.
    Prompt,
    /// Turned away without bothering them.
    Refused(String),
}

impl Access {
    /// A tab the person opened.
    pub(super) fn person() -> Self {
        Self {
            opener: Opener::Person,
            held: false,
            lent: false,
            shut: None,
        }
    }

    /// A tab an agent opened. It is not held until the agent [`acquire`]s it,
    /// which opening it does.
    ///
    /// [`acquire`]: Self::acquire
    pub(super) fn agent() -> Self {
        Self {
            opener: Opener::Agent,
            ..Self::person()
        }
    }

    pub(super) fn opener(&self) -> Opener {
        self.opener
    }

    /// Whether the person is locked out: an agent is working in the tab.
    pub(super) fn held(&self) -> bool {
        self.held
    }

    /// Whether the tab is an agent's to use: it opened it, or was lent it.
    pub(super) fn agent_usable(&self) -> bool {
        self.opener == Opener::Agent || self.lent
    }

    /// Whether the person lent the tab to an agent.
    pub(super) fn lent(&self) -> bool {
        self.lent
    }

    /// The access of a tab a page here opened by itself (`window.open`, a
    /// link with a target). While an agent holds the page, the agent was the
    /// one clicking; otherwise only the person could have been.
    pub(super) fn child(&self) -> Self {
        if self.held {
            Self {
                held: true,
                ..Self::agent()
            }
        } else {
            Self::person()
        }
    }

    /// Whether an agent may use the tab at `now`, and if not, why, written for
    /// the agent that reads it.
    pub(super) fn permits(&self, now: Instant) -> Result<(), String> {
        if let Some(shut) = self.shut
            && now < shut.until
        {
            return Err(shut.why.to_string());
        }
        if self.agent_usable() {
            Ok(())
        } else {
            Err(
                "that tab was opened by the person, and an agent uses only the tabs it \
                 opened; ask for it with `pilus ctl browser request <tab> [why]`, or open \
                 your own with `pilus ctl browser new URL`"
                    .to_string(),
            )
        }
    }

    /// An agent starts or keeps working in the tab: it takes the hold, and the
    /// person is locked out until it lets go.
    pub(super) fn acquire(&mut self, now: Instant) -> Result<(), String> {
        self.permits(now)?;
        self.held = true;
        Ok(())
    }

    /// An agent asks for the tab.
    pub(super) fn ask(&self, now: Instant) -> Ask {
        if let Some(shut) = self.shut
            && now < shut.until
        {
            return Ask::Refused(shut.why.to_string());
        }
        if self.agent_usable() {
            Ask::Already
        } else {
            Ask::Prompt
        }
    }

    /// The person said yes: the tab is lent, and the agent holds it.
    pub(super) fn allow(&mut self) {
        self.lent = true;
        self.held = true;
        self.shut = None;
    }

    /// The person said no. Returns what the agent is told.
    pub(super) fn deny(&mut self, now: Instant) -> &'static str {
        let why = "the person declined; do not ask for this tab again for a minute";
        self.shut = Some(Shut {
            until: now + DECLINED_FOR,
            why,
        });
        why
    }

    /// The agent is done: the hold ends and a lent tab goes back to the
    /// person. True if the agent held it.
    pub(super) fn release(&mut self) -> bool {
        let held = self.held;
        self.held = false;
        self.lent = false;
        held
    }

    /// The person takes the tab back. True if an agent held it.
    pub(super) fn take_over(&mut self, now: Instant) -> bool {
        if !self.held {
            return false;
        }
        self.held = false;
        self.lent = false;
        self.shut = Some(Shut {
            until: now + TAKEN_BACK_FOR,
            why: "the person took this tab back; wait a little, or open another with \
                  `pilus ctl browser new URL`",
        });
        true
    }
}

/// An agent's request for a tab, waiting on the person.
pub(super) struct Pending {
    reason: String,
    /// Every client that asked meanwhile, answered together.
    waiting: Vec<Replier>,
}

impl Pending {
    pub(super) fn new(reason: Option<&str>, replier: &Replier) -> Self {
        Self {
            reason: clean(reason),
            waiting: vec![replier.clone()],
        }
    }

    /// Another client asks for the same tab.
    pub(super) fn join(&mut self, replier: &Replier) {
        self.waiting.push(replier.clone());
    }

    /// The reason the agent gave, if it gave one: the agent's word, shown as
    /// plain text.
    pub(super) fn reason(&self) -> Option<&str> {
        Some(self.reason.as_str()).filter(|reason| !reason.is_empty())
    }

    /// Hand the answer to everyone who asked.
    pub(super) fn answer(self, reply: &crate::ctl::Reply) {
        for replier in self.waiting {
            replier.send(reply.clone());
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctl::Reply;

    #[test]
    fn an_agent_cannot_use_a_tab_the_person_opened() {
        let now = Instant::now();
        let mut tab = Access::person();
        let why = tab.acquire(now).expect_err("refused");
        assert!(why.contains("opened by the person"), "{why}");
        assert!(why.contains("browser request"), "it says how to ask: {why}");
        assert!(!tab.held(), "a refused ask holds nothing");
        assert!(tab.permits(now).is_err());
    }

    #[test]
    fn working_in_an_agent_tab_locks_the_person_out_until_release() {
        let now = Instant::now();
        let mut tab = Access::agent();
        assert!(!tab.held(), "opened, not yet used");
        assert!(tab.acquire(now).is_ok());
        assert!(tab.held());
        assert!(tab.acquire(now).is_ok(), "asking again is fine");
        assert!(tab.release());
        assert!(!tab.held());
        assert!(!tab.release(), "nothing left to release");
        assert!(tab.acquire(now).is_ok(), "a released tab can be used again");
    }

    #[test]
    fn taking_a_tab_back_turns_the_agent_away_briefly() {
        let now = Instant::now();
        let mut tab = Access::agent();
        tab.acquire(now).expect("held");
        assert!(tab.take_over(now));
        assert!(!tab.held());
        let why = tab.acquire(now).expect_err("refused");
        assert!(why.contains("took this tab back"), "{why}");
        assert!(tab.permits(now).is_err());
        let later = now + TAKEN_BACK_FOR + Duration::from_secs(1);
        assert!(tab.acquire(later).is_ok(), "the door reopens by itself");
        // Nothing to take back from a tab nobody holds.
        assert!(!Access::agent().take_over(now));
        assert!(!Access::person().take_over(now));
    }

    #[test]
    fn the_strip_keeps_saying_who_opened_a_tab_after_it_was_taken_back() {
        let now = Instant::now();
        let mut tab = Access::agent();
        tab.acquire(now).expect("held");
        tab.take_over(now);
        assert_eq!(tab.opener(), Opener::Agent);
        assert_eq!(tab.opener().label(), "agent");
        assert_eq!(Access::person().opener().label(), "person");
    }

    #[test]
    fn a_tab_a_held_page_opens_is_the_agents_and_any_other_is_the_persons() {
        let now = Instant::now();
        let mut tab = Access::agent();
        tab.acquire(now).expect("held");
        let child = tab.child();
        assert_eq!(child.opener(), Opener::Agent);
        assert!(child.held());
        // A link the person clicked in a tab nobody holds is theirs.
        tab.release();
        assert_eq!(tab.child(), Access::person());
        assert_eq!(Access::person().child(), Access::person());
    }

    #[test]
    fn the_person_lends_a_tab_and_it_goes_back_on_release() {
        let now = Instant::now();
        let mut tab = Access::person();
        assert_eq!(tab.ask(now), Ask::Prompt);
        tab.allow();
        assert!(tab.lent() && tab.held() && tab.agent_usable());
        assert_eq!(tab.opener(), Opener::Person, "still opened by the person");
        assert!(tab.permits(now).is_ok());
        assert_eq!(tab.ask(now), Ask::Already, "nothing more to ask");
        assert!(tab.release());
        assert!(!tab.lent() && !tab.held());
        assert!(tab.permits(now).is_err(), "the agent must ask again");
        assert_eq!(tab.ask(now), Ask::Prompt);
    }

    #[test]
    fn taking_a_lent_tab_back_ends_the_loan() {
        let now = Instant::now();
        let mut tab = Access::person();
        tab.allow();
        assert!(tab.take_over(now));
        assert!(!tab.lent());
        assert!(matches!(tab.ask(now), Ask::Refused(why) if why.contains("took this tab back")));
        let later = now + TAKEN_BACK_FOR + Duration::from_secs(1);
        assert_eq!(tab.ask(later), Ask::Prompt, "it may ask again afterwards");
    }

    #[test]
    fn a_no_is_remembered_without_a_timer() {
        let now = Instant::now();
        let mut tab = Access::person();
        tab.deny(now);
        let soon = now + Duration::from_secs(30);
        assert!(matches!(tab.ask(soon), Ask::Refused(why) if why.contains("declined")));
        let why = tab.permits(soon).expect_err("still shut");
        assert!(why.contains("declined"), "not the generic message: {why}");
        let later = now + DECLINED_FOR + Duration::from_secs(1);
        assert_eq!(tab.ask(later), Ask::Prompt);
        // A yes after a no opens the door at once.
        tab.deny(now);
        tab.allow();
        assert!(tab.permits(now).is_ok());
    }

    #[test]
    fn everyone_who_asked_is_answered_together() {
        let (first, first_answer) = Replier::pair();
        let (second, second_answer) = Replier::pair();
        let mut pending = Pending::new(Some("check the todo app"), &first);
        pending.join(&second);
        assert_eq!(pending.reason(), Some("check the todo app"));
        assert!(first_answer.try_recv().is_err(), "nobody is answered yet");
        pending.answer(&Reply::err("no"));
        assert!(!first_answer.try_recv().expect("told").ok);
        assert!(!second_answer.try_recv().expect("told").ok);
    }

    #[test]
    fn dropping_a_request_tells_the_client_at_once() {
        let (replier, answer) = Replier::pair();
        let pending = Pending::new(None, &replier);
        drop(replier);
        drop(pending);
        assert!(matches!(
            answer.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn a_reason_is_one_short_line_of_plain_text() {
        assert_eq!(clean(Some("  a\n\tb\u{7}  c ")), "a b c");
        assert_eq!(clean(None), "");
        let long = clean(Some(&"x".repeat(1000)));
        assert_eq!(long.chars().count(), REASON_MAX + 1);
        assert!(long.ends_with('…'));
        assert_eq!(Pending::new(None, &Replier::pair().0).reason(), None);
    }
}
