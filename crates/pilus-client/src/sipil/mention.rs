//! Bringing a sipil agent into a conversation by naming it.
//!
//! Typing `@Researcher` in a channel sends the line as usual and also hands the
//! conversation to that agent: its daemon gets the lines it has not seen, in one
//! session per conversation, and what the agent answers is said back into the
//! channel under its name, to everyone in it. Only a channel does this: a chat
//! between two people is theirs alone, so no agent reads it or speaks in it. In a
//! channel only the author's own agents answer, and only to the author's own lines
//! (or those of a contact the agent was shared with): a message from anyone else
//! never starts one of your agents, whatever it says.
//!
//! The agent is a participant, not a command: it sees who said what, so "what do
//! you think of Rani's plan" works in a channel of Rina, Rani and you.

use std::time::{Duration, Instant};

use super::chat::{Chats, Cursor, Line};
use super::{Activity, ItemRole, Sipil, SipilError};

/// The most earlier lines an agent is shown when first mentioned.
const FIRST_CONTEXT: usize = 40;
/// How long an agent may take before the mention is given up on.
const PATIENCE: Duration = Duration::from_secs(600);
/// How long an idle daemon may show nothing new before the mention is given up
/// on: a turn that never started.
const SILENCE: Duration = Duration::from_secs(45);
/// How often the daemon is asked how the turn is going.
const POLL: Duration = Duration::from_millis(1000);

/// The agents `text` names, in the order they are named. A mention is `@` and
/// a name, case-insensitively, and must end at a character that cannot continue
/// a name, so `@Researcher2` is not `@Researcher`. Names may contain spaces; the
/// longest name wins where one is the start of another.
pub fn mentioned<'a>(text: &str, names: &'a [String]) -> Vec<&'a str> {
    let lower = text.to_lowercase();
    let mut order: Vec<&String> = names.iter().collect();
    order.sort_by_key(|name| std::cmp::Reverse(name.chars().count()));
    let mut found: Vec<(usize, &String)> = Vec::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find('@') {
        let start = from + at;
        let rest = &lower[start + 1..];
        let before_ok = lower[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok
            && let Some(name) = order.iter().find(|name| {
                let name = name.to_lowercase();
                rest.starts_with(&name)
                    && rest[name.len()..]
                        .chars()
                        .next()
                        .is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '-'))
            })
            && !found.iter().any(|(_, seen)| seen == name)
        {
            found.push((start, name));
        }
        from = start + 1;
    }
    found.into_iter().map(|(_, name)| name.as_str()).collect()
}

/// What the agent is told: where it is, who was there, and what was said since it
/// last looked. `speaker` names the author of a line for the agent.
pub fn prompt(
    agent: &str,
    conversation: &str,
    lines: &[Line],
    speaker: &dyn Fn(&Line) -> String,
) -> String {
    let mut text = format!(
        "You are {agent}, taking part in {conversation}. You were mentioned. \
         Everything below was said there since you last looked. Reply to what was asked of you; \
         your reply is posted in the conversation for everyone in it, so write it to them, briefly.\n\n"
    );
    for line in lines {
        match &line.reply {
            Some(quote) => text.push_str(&format!(
                "{} (replying to {}: \"{}\"): {}\n",
                speaker(line),
                quote.who,
                quote.text,
                line.text
            )),
            None => text.push_str(&format!("{}: {}\n", speaker(line), line.text)),
        }
    }
    text
}

/// Where and for whom an agent is answering.
pub struct Mention<'a> {
    /// The chat store the conversation lives in.
    pub chats: &'a Chats,
    /// The agent's daemon.
    pub daemon: &'a Sipil,
    /// The daemon's device ID, which keys the agent's session and cursor.
    pub daemon_id: &'a str,
    /// What the agent is called.
    pub agent: &'a str,
    /// The conversation's key in the chat store.
    pub thread: &'a str,
    /// Describes the place ("a chat with Rina", "#project").
    pub conversation: &'a str,
    /// Makes a line retryable without a second message.
    pub request_id: &'a str,
}

/// Hand the conversation to the agent and wait for its answer, which the caller
/// says back into the conversation. `speaker` names who said a line.
pub fn answer(
    mention: &Mention<'_>,
    speaker: &dyn Fn(&Line) -> String,
) -> Result<String, SipilError> {
    let Mention {
        chats,
        daemon,
        daemon_id,
        agent,
        thread,
        conversation,
        request_id,
    } = *mention;
    let cursor = chats.cursor(thread, daemon_id);
    let lines = chats.lines(thread);
    let seen = cursor.as_ref().map_or(0, |cursor| cursor.through);
    let mut fresh: Vec<Line> = lines
        .iter()
        .filter(|line| line.id > seen)
        .cloned()
        .collect();
    if cursor.is_none() && fresh.len() > FIRST_CONTEXT {
        fresh.drain(..fresh.len() - FIRST_CONTEXT);
    }
    let through = lines.last().map_or(seen, |line| line.id);

    let session = match &cursor {
        Some(cursor) => cursor.session.clone(),
        None => daemon.create_session()?.id,
    };
    let before = daemon
        .transcript(&session)?
        .iter()
        .map(|item| item.id)
        .max()
        .unwrap_or(0);
    daemon.send_message(
        &session,
        &prompt(agent, conversation, &fresh, speaker),
        request_id,
    )?;
    chats.set_cursor(
        thread,
        daemon_id,
        Cursor {
            session: session.clone(),
            through,
        },
    );

    let started = Instant::now();
    loop {
        std::thread::sleep(POLL);
        let items = daemon.transcript(&session)?;
        let new: Vec<_> = items.iter().filter(|item| item.id > before).collect();
        let working = daemon
            .sessions()?
            .iter()
            .any(|candidate| candidate.id == session && candidate.status == Activity::Working);
        let answered = new.iter().any(|item| item.role == ItemRole::Assistant);
        if !working && answered {
            if let Some(failed) = new.iter().find_map(|item| item.error.clone()) {
                return Err(SipilError::Refused {
                    code: "agent_failed".to_string(),
                    message: format!("{agent} could not answer: {failed}"),
                });
            }
            let reply = new
                .iter()
                .rev()
                .filter(|item| item.role == ItemRole::Assistant && !item.text.trim().is_empty())
                .map(|item| item.text.trim().to_string())
                .next();
            return reply.ok_or_else(|| SipilError::Refused {
                code: "agent_silent".to_string(),
                message: format!("{agent} finished without saying anything."),
            });
        }
        let waited = started.elapsed();
        if waited > PATIENCE || (!working && !answered && waited > SILENCE) {
            return Err(SipilError::Refused {
                code: "agent_timeout".to_string(),
                message: format!("{agent} did not answer in time."),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Quote;
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_mention_is_an_at_and_a_name_in_any_case() {
        let agents = names(&["Researcher", "CoS"]);
        assert_eq!(
            mentioned("hey @researcher what now", &agents),
            ["Researcher"]
        );
        assert_eq!(mentioned("@CoS, plan it", &agents), ["CoS"]);
        assert_eq!(
            mentioned("@cos then @Researcher", &agents),
            ["CoS", "Researcher"]
        );
    }

    #[test]
    fn a_name_that_merely_starts_like_one_is_not_a_mention() {
        let agents = names(&["Researcher"]);
        assert!(mentioned("@Researcher2 hi", &agents).is_empty());
        assert!(mentioned("mail me@researcher.example", &agents).is_empty());
        assert!(mentioned("no at sign", &agents).is_empty());
    }

    #[test]
    fn the_longest_name_wins_and_a_name_counts_once() {
        let agents = names(&["Chief", "Chief of Staff"]);
        assert_eq!(
            mentioned("ask @Chief of Staff please", &agents),
            ["Chief of Staff"]
        );
        assert_eq!(mentioned("@Chief and @chief again", &agents), ["Chief"]);
    }

    #[test]
    fn the_prompt_says_where_it_is_and_who_said_what() {
        let line = |id, text: &str| Line {
            id,
            mine: false,
            text: text.to_string(),
            at: 0,
            who: None,
            agent: None,
            reply: None,
        };
        let text = prompt(
            "Researcher",
            "a chat with Rina",
            &[line(1, "hi"), line(2, "@Researcher summarise")],
            &|line| {
                if line.id == 1 {
                    "Rina".to_string()
                } else {
                    "alex".to_string()
                }
            },
        );
        assert!(text.contains("You are Researcher, taking part in a chat with Rina"));
        assert!(text.contains("Rina: hi\nalex: @Researcher summarise\n"));

        // A reply says what it answers.
        let mut answering = line(3, "yes");
        answering.reply = Some(Quote::of("Rina", "ship it?"));
        let text = prompt("CoS", "a chat", &[answering], &|_| "alex".to_string());
        assert!(text.contains("alex (replying to Rina: \"ship it?\"): yes\n"));
    }
}
