//! The people half of the Sipil tab state: contacts, channels, the
//! conversations in them, and the agents that join in when mentioned.
//!
//! Everything here is on this device's disk (`contacts.json`, `channels.json`,
//! `chats.jsonl` under `~/.pilus/sipil`), so the lists are there before, and
//! without, the network. The iroh node starts when something needs it: a message
//! to send, a contact to add, a daemon to connect, or the person having left
//! this pilus reachable. Like the daemon half, nothing here blocks the GUI
//! thread: whatever touches the network runs on a worker thread and reports
//! through [`super::sipil::SipilState`].
//!
//! A mention is the one place the two halves meet. Typing `@Researcher` in a
//! channel sends the line as usual and hands the conversation to that agent of
//! ours; its reply is said back into the channel under its name. Only our own
//! agents answer, and only in a channel we invited them into: naming one that
//! was not invited does nothing, and neither does an agent naming another. A
//! chat between two people is never handed to an agent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;

use pilus_client::sipil::channels::thread_key;
use pilus_client::sipil::mention::{self, Mention};
use pilus_client::sipil::peer::local;
use pilus_client::sipil::{
    Channels, Chats, Conversation, Delivery, Directory, Event, ItemRole, Line, Local, LocalOptions,
    Offline, Quote, SipilError, ToolCall, TranscriptItem, parse_target,
};

use super::sipil::{
    Person, Watch, lock, nickname, publish, reachable_setting, relay_choice, request_id, snapshot,
    update,
};

/// How many ticks pass between asking each contact whether they are there.
const PRESENCE_EVERY: u32 = 20;

/// What the last check of a contact found.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Reach {
    /// Not asked yet, or the network is not up.
    #[default]
    Unknown,
    /// Their pilus is running and has added this device.
    Online,
    /// Their pilus is running but has not added this device back, so it would
    /// refuse a message.
    Waiting,
    /// Nothing answered.
    Offline,
}

/// The files the people half reads and writes.
pub(super) struct Stores {
    pub(super) directory: Arc<Directory>,
    pub(super) chats: Arc<Chats>,
    pub(super) channels: Arc<Channels>,
    /// What is remembered of the daemons, for when they are away.
    pub(super) offline: Arc<Offline>,
}

static STORES: OnceLock<Stores> = OnceLock::new();
static EVENTS: std::sync::Mutex<Vec<Event>> = std::sync::Mutex::new(Vec::new());
static CHECKING: AtomicBool = AtomicBool::new(false);

/// The contact list, channels and conversations. Opening them reads three small
/// files and touches no network, so they are listed even while the node is down.
pub(super) fn stores() -> &'static Stores {
    STORES.get_or_init(|| {
        let dir = super::sipil::data_dir();
        Stores {
            directory: Arc::new(Directory::open(&dir)),
            chats: Arc::new(Chats::open(&dir)),
            channels: Arc::new(Channels::open(&dir)),
            offline: Arc::new(Offline::open(&dir)),
        }
    })
}

/// Messages and knocks that arrived since the last call, oldest first, for the
/// main window to announce.
pub fn take_events() -> Vec<Event> {
    std::mem::take(
        &mut *EVENTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

fn push_event(event: Event) {
    // A member naming an agent we invited is a request to run it, not news.
    if let Event::Mentioned {
        thread,
        daemon,
        agent,
        ..
    } = event
    {
        thread::spawn(move || answer_invited(&thread, &daemon, &agent));
        return;
    }
    EVENTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(event);
}

/// Bring the network side up, or return the one already up. Starting binds a
/// UDP socket and reads a key, so callers run it on a worker thread.
pub(super) fn go_online() -> Result<Arc<Local>, SipilError> {
    let stores = stores();
    let started = Local::start(LocalOptions {
        dir: super::sipil::data_dir(),
        directory: Arc::clone(&stores.directory),
        chats: Arc::clone(&stores.chats),
        channels: Arc::clone(&stores.channels),
        relay: relay_choice(),
        nickname: nickname(),
        reachable: reachable_setting(),
        notify: Arc::new(push_event),
    });
    match &started {
        Ok(local) => {
            let id = local.id().to_string();
            update(|state| {
                state.me.online = true;
                state.me.device_id = Some(id);
                state.me.error = None;
                state.me.nickname = nickname();
            });
            // Daemons that connected before the network did are offered now.
            super::sipil::share_daemons();
        }
        Err(error) => update(|state| state.me.error = Some(error.to_string())),
    }
    started
}

/// Read this device's ID off disk, creating the identity on first use, so the
/// window can show it before any network is up. A key that cannot be made is
/// reported as the network's error.
#[cfg_attr(feature = "visual-test", allow(dead_code))]
pub fn prepare() {
    let reachable = reachable_setting();
    match pilus_client::sipil::device_id(&super::sipil::data_dir()) {
        Ok(id) => update(|state| {
            state.me.device_id = Some(id);
            state.me.reachable = reachable;
            state.me.nickname = nickname();
        }),
        Err(error) => update(|state| state.me.error = Some(error.to_string())),
    }
}

/// Add a contact by device ID or ticket. A mistake in what was typed comes back
/// at once; the rest is done on a worker thread and reported in the state.
pub fn add_contact(input: &str, name: &str) -> Result<(), String> {
    let target = parse_target(input).map_err(|error| error.to_string())?;
    if snapshot().me.device_id.as_deref() == Some(target.id().as_str()) {
        return Err("That is this device's own ID.".to_string());
    }
    let (name, ticket) = (name.to_string(), target.ticket());
    thread::spawn(move || {
        // The node is what makes a contact reachable, so adding one brings it up.
        let online = go_online();
        // A daemon is not a person: it connects by asking (`sipil connect`),
        // and it would answer a chat message with "unknown method". If it cannot
        // be asked right now (offline), it is added and the first message says.
        if let Ok(local) = &online
            && let Ok(health) = pilus_client::sipil::probe(&local.node(), &target)
            && health.kind == pilus_client::sipil::NodeKind::Daemon
        {
            update(|state| {
                state.me.contact_error = Some(
                    "That is a sipil agent, not a person. On its machine, run: npm run connect -- --device-id <your device ID>"
                        .to_string(),
                );
            });
            return;
        }
        let added = stores().directory.add(&target.id(), &name, ticket);
        update(|state| match added {
            Ok(()) => {
                state.me.added += 1;
                state.me.contact_error = None;
            }
            Err(error) => {
                state.me.contact_error = Some(format!("Could not save the contact: {error}"));
            }
        });
        check_presence();
    });
    Ok(())
}

/// Give a contact a new name.
pub fn rename_contact(id: &str, name: &str) {
    let _ = stores().directory.rename(id, name);
}

/// Invite the agent of the daemon `daemon` into the channel `channel`, or take it
/// out. Inviting says so to the members, as the agent, since they cannot see the
/// invitation otherwise and would not know to name it.
pub fn set_channel_agent(channel: &str, daemon: &str, invited: bool) {
    let stores = stores();
    if !stores.channels.set_agent(channel, daemon, invited) {
        return;
    }
    let (Some(found), Some(name)) = (
        stores.channels.get(channel),
        snapshot().daemon(daemon).map(|daemon| daemon.name()),
    ) else {
        return;
    };
    let text = if invited {
        format!("{name} joined the channel. Name it with @{name} to ask it something.")
    } else {
        format!("{name} left the channel.")
    };
    thread::spawn(move || {
        let outcome = go_online().and_then(|local| {
            local.say(
                &Conversation::Channel(found),
                &text,
                &request_id(),
                Some(&name),
            )
        });
        if let Err(error) = outcome {
            update(|state| state.send_error = Some(peer_error(&error)));
        }
        refresh_thread();
    });
}

/// Forget a contact.
pub fn remove_contact(id: &str) {
    let _ = stores().directory.remove(id);
    // Leaving a chat with someone no longer listed would show a name that is gone.
    let watching = snapshot().watch.clone();
    if matches!(&watching, Watch::Peer(open) if open == id) {
        super::sipil::watch(Watch::Off);
    }
}

/// Forget a sipil daemon: it is no longer a contact and is disconnected.
pub fn remove_daemon(id: &str) {
    let _ = stores().directory.remove(id);
    // Its snapshots and anything still waiting to be sent go with it.
    stores().offline.forget(id);
    super::sipil::disconnect(id);
}

/// Turn a device that knocked into a contact. A sipil daemon is connected to at
/// once.
pub fn accept_request(id: &str) {
    let id = id.to_string();
    thread::spawn(move || match stores().directory.accept(&id) {
        Ok(Some(true)) => {
            if let Some(contact) = stores()
                .directory
                .daemons()
                .into_iter()
                .find(|c| c.id == id)
            {
                super::sipil::connect_daemon(&contact.ticket.unwrap_or(contact.id));
            }
        }
        _ => check_presence(),
    });
}

/// Ignore a device that knocked.
pub fn dismiss_request(id: &str) {
    let _ = stores().directory.dismiss(id);
}

/// Make a channel of `members` (device IDs of contacts) named `name`. Nobody is
/// told until the first message, which carries the name and the members.
pub fn create_channel(name: &str, members: Vec<String>) -> Result<(), String> {
    if name.trim().trim_start_matches('#').is_empty() {
        return Err("Name the channel.".to_string());
    }
    match stores().channels.create(name, members) {
        Ok(_) => {
            update(|state| {
                state.me.added += 1;
                state.me.contact_error = None;
            });
            Ok(())
        }
        Err(error) => Err(format!("Could not save the channel: {error}")),
    }
}

/// Leave a channel: it and its conversation are forgotten here, and a late
/// message does not bring it back.
pub fn leave_channel(id: &str) {
    let _ = stores().channels.remove(id);
    stores().chats.forget(&thread_key(id));
    if snapshot().watch == Watch::Channel(id.to_string()) {
        super::sipil::watch(Watch::Off);
    }
}

/// Invite the contact `contact` to channel `channel`. The members hear it in a
/// line that carries the new list, and the contact learns the channel from it.
pub fn invite_to_channel(channel: &str, contact: &str) {
    let (channel, contact) = (channel.to_string(), contact.to_string());
    thread::spawn(move || {
        let outcome = go_online().and_then(|local| {
            let found = stores().directory.contact(&contact).ok_or_else(|| {
                SipilError::Address("That contact is no longer in the list.".to_string())
            })?;
            local.invite_to_channel(&channel, &found, &request_id())
        });
        membership_changed(&outcome);
    });
}

/// Remove `member` (a device ID) from channel `channel`: they are told, and the
/// others drop them. It holds here even if nobody could be told.
pub fn remove_from_channel(channel: &str, member: &str) {
    let (channel, member) = (channel.to_string(), member.to_string());
    thread::spawn(move || {
        let outcome = go_online()
            .and_then(|local| local.remove_from_channel(&channel, &member, &request_id()));
        membership_changed(&outcome);
    });
}

/// Show what a change of members came to, and the channel as it now is.
fn membership_changed(outcome: &Result<Delivery, SipilError>) {
    update(|state| {
        state.send_error = match outcome {
            Ok(delivery) => missed(delivery),
            Err(error) => Some(peer_error(error)),
        };
    });
    sync(0);
}

/// The conversation `watch` is, if it is one with people in it.
fn conversation(watch: &Watch) -> Option<Conversation> {
    match watch {
        Watch::Peer(id) => stores().directory.contact(id).map(Conversation::Direct),
        Watch::Channel(id) => stores().channels.get(id).map(Conversation::Channel),
        Watch::Off | Watch::On(..) => None,
    }
}

/// Say `text` in the open chat or channel, and bring in any agent of ours it
/// mentions. Nothing is stored unless it reached someone, so a refusal leaves the
/// composer to retry.
pub(super) fn send_to_conversation(watching: &Watch, text: String, reply: Option<Quote>) {
    update(|state| {
        state.sending = true;
        state.send_error = None;
    });
    let watching = watching.clone();
    thread::spawn(move || {
        let outcome = go_online().and_then(|local| match conversation(&watching) {
            Some(conversation) => local
                .say_replying(&conversation, &text, &request_id(), None, reply.as_ref())
                .map(|delivery| (local, conversation, delivery)),
            None => Err(SipilError::Address(
                "That conversation is no longer in the list.".to_string(),
            )),
        });
        let note = match &outcome {
            Ok((_, _, delivery)) => missed(delivery),
            Err(_) => None,
        };
        update(|state| {
            state.sending = false;
            match &outcome {
                Ok(_) => {
                    state.sent += 1;
                    state.send_error = note;
                }
                Err(error) => state.send_error = Some(peer_error(error)),
            }
        });
        if let Ok((local, conversation, _)) = outcome {
            refresh_thread();
            bring_in_agents(&local, &conversation, &text);
        }
    });
}

/// "Not delivered to Ayu, Dimas." for a channel message some members missed.
fn missed(delivery: &Delivery) -> Option<String> {
    if delivery.failed.is_empty() {
        return None;
    }
    let names: Vec<&str> = delivery
        .failed
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    Some(format!("Not delivered to {}.", names.join(", ")))
}

/// Start each of our agents that `text` mentions on its own thread, when it was
/// invited into this channel.
fn bring_in_agents(local: &Arc<Local>, conversation: &Conversation, text: &str) {
    // Only a channel has room for an agent. A chat between two people is theirs
    // alone: nothing in it is read by an agent, and no agent speaks in it.
    if !matches!(conversation, Conversation::Channel(_)) {
        return;
    }
    let Conversation::Channel(channel) = conversation else {
        return;
    };
    let invited = stores().channels.agents(&channel.id);
    let candidates: Vec<(String, String)> = snapshot()
        .mentionable()
        .into_iter()
        .filter(|(daemon, _)| invited.contains(daemon))
        .collect();
    let names: Vec<String> = candidates.iter().map(|(_, name)| name.clone()).collect();
    for name in mention::mentioned(text, &names) {
        let Some((daemon, _)) = candidates.iter().find(|(_, label)| label == name) else {
            continue;
        };
        let (local, conversation) = (Arc::clone(local), conversation.clone());
        let (daemon, name) = (daemon.clone(), name.to_string());
        thread::spawn(move || answer_mention(&local, &conversation, &daemon, &name));
    }
}

/// Answer a contact who named one of our agents in `thread`. The conversation is
/// found again here, since the contact may have been removed, or the channel left,
/// between the line arriving and this thread starting.
fn answer_invited(thread: &str, daemon: &str, agent: &str) {
    let stores = stores();
    // A channel only: a contact naming an agent in a chat between two people
    // starts nothing.
    let conversation = pilus_client::sipil::channels::channel_of(thread)
        .and_then(|id| stores.channels.get(id))
        .map(Conversation::Channel);
    let (Some(conversation), Some(local)) = (conversation, local()) else {
        return;
    };
    // The invitation may have been withdrawn since the line arrived.
    if let Conversation::Channel(channel) = &conversation
        && !stores
            .channels
            .agents(&channel.id)
            .iter()
            .any(|invited| invited == daemon)
    {
        return;
    }
    answer_mention(&local, &conversation, daemon, agent);
}

/// Let `agent` read the conversation and say what it makes of it.
fn answer_mention(local: &Local, conversation: &Conversation, daemon: &str, agent: &str) {
    if !matches!(conversation, Conversation::Channel(_)) {
        return;
    }
    let thread = conversation.thread();
    let key = (thread.clone(), agent.to_string());
    update(|state| state.thinking.push(key.clone()));
    let outcome = reply_as(local, conversation, daemon, agent);
    update(|state| {
        if let Some(at) = state.thinking.iter().position(|entry| *entry == key) {
            state.thinking.remove(at);
        }
        if let Err(error) = &outcome {
            // Say who failed, in the conversation it was asked in.
            if state.watch_thread().as_deref() == Some(thread.as_str()) {
                state.send_error = Some(error.to_string());
            }
        }
    });
    refresh_thread();
}

fn reply_as(
    local: &Local,
    conversation: &Conversation,
    daemon: &str,
    agent: &str,
) -> Result<(), SipilError> {
    let sipil = {
        let shared = lock();
        shared
            .links
            .iter()
            .find(|link| link.id == daemon)
            .and_then(|link| link.sipil.clone())
    }
    .ok_or_else(|| SipilError::Offline(format!("{agent} is not connected.")))?;
    let me = nickname();
    let directory = &stores().directory;
    let (place, speaker): (String, Speaker) = match conversation {
        Conversation::Direct(contact) => {
            let name = contact.name.clone();
            (
                format!("a chat between {me} and {name}"),
                Box::new({
                    let me = me.clone();
                    move |line: &Line| match (&line.agent, line.mine) {
                        (Some(agent), true) => agent.clone(),
                        (Some(agent), false) => format!("{name}/{agent}"),
                        (None, true) => me.clone(),
                        (None, false) => name.clone(),
                    }
                }),
            )
        }
        Conversation::Channel(channel) => {
            let names: Vec<String> = channel
                .members
                .iter()
                .map(|member| member_name(directory, member))
                .collect();
            let directory = Arc::clone(directory);
            (
                format!(
                    "the channel #{} with {}",
                    channel.name,
                    if names.is_empty() {
                        "no one else yet".to_string()
                    } else {
                        names.join(", ")
                    }
                ),
                Box::new({
                    let me = me.clone();
                    move |line: &Line| {
                        let who = match (&line.who, line.mine) {
                            (_, true) => me.clone(),
                            (Some(who), false) => member_name(&directory, who),
                            (None, false) => "someone".to_string(),
                        };
                        match &line.agent {
                            Some(agent) if line.mine => agent.clone(),
                            Some(agent) => format!("{who}/{agent}"),
                            None => who,
                        }
                    }
                }),
            )
        }
    };
    let thread = conversation.thread();
    let request = request_id();
    let reply = mention::answer(
        &Mention {
            chats: &stores().chats,
            daemon: &sipil,
            daemon_id: daemon,
            agent,
            thread: &thread,
            conversation: &place,
            request_id: &request,
        },
        &*speaker,
    )?;
    local.say(conversation, &reply, &request_id(), Some(agent))?;
    Ok(())
}

/// Names who said a line, for an agent reading a conversation.
type Speaker = Box<dyn Fn(&Line) -> String>;

/// A member's name: the contact's, else the start of their ID.
pub(super) fn member_name(directory: &Directory, id: &str) -> String {
    directory
        .contact(id)
        .map_or_else(|| id.chars().take(8).collect(), |contact| contact.name)
}

/// What to tell the person when a message to a contact did not go.
pub(super) fn peer_error(error: &SipilError) -> String {
    if matches!(error, SipilError::Refused { code, .. } if code == "unknown_method") {
        // The contact is a sipil daemon, which has agents rather than a chat.
        "That device is a sipil agent, not a person. Remove it from Contacts and connect to it from the Sipil tab to talk to it."
            .to_string()
    } else if error.is_unauthorized() {
        "They have not added this device as a contact yet, so their pilus does not take your messages. Send them your device ID from the Sipil tab."
            .to_string()
    } else {
        error.to_string()
    }
}

/// Show the open chat's or channel's thread from the chat store, and mark it
/// read: the page being open on it is reading it.
pub(super) fn refresh_thread() {
    let watching = snapshot().watch.clone();
    let thread = match &watching {
        Watch::Peer(id) => id.clone(),
        Watch::Channel(id) => thread_key(id),
        Watch::Off | Watch::On(..) => return,
    };
    let stores = stores();
    let items: Vec<TranscriptItem> = stores
        .chats
        .lines(&thread)
        .into_iter()
        .map(|line| item_of(&stores.directory, &watching, line))
        .collect();
    stores.chats.mark_read(&thread);
    let mut shared = lock();
    if shared.state.watch != watching {
        return;
    }
    publish(&mut shared, |state| {
        state.transcript = items;
        state.transcript_loaded = true;
        state.chat_error = None;
    });
}

/// One stored line as a transcript line. The speaker is left out when the role
/// says it all ("you", or the one person in a direct chat).
fn item_of(directory: &Directory, watching: &Watch, line: Line) -> TranscriptItem {
    let channel = matches!(watching, Watch::Channel(_));
    let person = line.who.as_deref().map(|who| member_name(directory, who));
    let speaker = match (&line.agent, line.mine) {
        (Some(agent), true) => Some(agent.clone()),
        (Some(agent), false) => {
            let owner = person.clone().or_else(|| {
                if let Watch::Peer(id) = watching {
                    directory.contact(id).map(|contact| contact.name)
                } else {
                    None
                }
            });
            Some(owner.map_or_else(|| agent.clone(), |owner| format!("{owner}/{agent}")))
        }
        (None, false) if channel => person,
        (None, _) => None,
    };
    // A note that an agent of ours handed work off reads as the call it was.
    if let Some(delegation) = &line.delegation {
        return TranscriptItem {
            id: line.id,
            role: ItemRole::Assistant,
            text: String::new(),
            tools: vec!["subagent".to_string()],
            calls: vec![ToolCall {
                id: delegation.call.clone(),
                name: "subagent".to_string(),
                summary: delegation.task.clone(),
                args: String::new(),
                subagent: Some(delegation.subagent()),
            }],
            call_id: None,
            is_error: false,
            reply: None,
            error: None,
            at: Some(line.at),
            speaker,
        };
    }
    TranscriptItem {
        id: line.id,
        role: if line.mine {
            ItemRole::User
        } else {
            ItemRole::Peer
        },
        text: line.text,
        tools: Vec::new(),
        calls: Vec::new(),
        call_id: None,
        is_error: false,
        reply: line.reply,
        error: None,
        at: Some(line.at),
        speaker,
    }
}

/// Fold the contacts, the channels, the requests and the open thread into the
/// state, and now and then ask the contacts whether they are there. All of it is
/// local except the asking, which runs on its own thread.
pub(super) fn sync(ticks: u32) {
    let stores = stores();
    let node = local();
    let daemon_names: Vec<(String, String)> = stores
        .directory
        .daemons()
        .into_iter()
        .map(|contact| (contact.id, contact.name))
        .collect();
    let (contacts, channels, requests) = (
        stores.directory.contacts(),
        stores.channels.list(),
        stores.directory.requests(),
    );
    if node.is_some() && !contacts.is_empty() && ticks % PRESENCE_EVERY == 1 {
        check_presence();
    }
    {
        let mut shared = lock();
        let reach = shared.reach.clone();
        let people: Vec<Person> = contacts
            .into_iter()
            .map(|contact| {
                let found = reach.get(&contact.id);
                Person {
                    reach: found.copied().unwrap_or_default(),
                    unread: stores.chats.unread(&contact.id),
                    contact,
                }
            })
            .collect();
        let rows = channels
            .into_iter()
            .map(|channel| super::sipil::ChannelRow {
                unread: stores.chats.unread(&channel.thread()),
                agents: stores.channels.agents(&channel.id),
                channel,
            })
            .collect();
        publish(&mut shared, |state| {
            state.me.people = people;
            for daemon in &mut state.daemons {
                daemon.alias = daemon_names
                    .iter()
                    .find(|(id, _)| *id == daemon.id)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_default();
            }
            state.me.channels = rows;
            state.me.requests = requests;
            state.me.online = node.is_some();
            if let Some(node) = &node {
                state.me.device_id = Some(node.id().to_string());
                state.me.reachable = node.reachable();
            }
        });
    }
    refresh_thread();
}

/// Ask every contact whether their pilus is there, once at a time.
pub(super) fn check_presence() {
    if CHECKING.swap(true, Ordering::AcqRel) {
        return;
    }
    thread::spawn(|| {
        if let Some(node) = local() {
            for contact in stores().directory.contacts() {
                let found = match node.check(&contact) {
                    Ok(health) if health.allowed => Reach::Online,
                    Ok(_) => Reach::Waiting,
                    Err(_) => Reach::Offline,
                };
                lock().reach.insert(contact.id, found);
            }
        }
        CHECKING.store(false, Ordering::Release);
    });
}

impl super::sipil::SipilState {
    /// The chat-store key of the conversation with people that is open, if one is.
    pub(super) fn watch_thread(&self) -> Option<String> {
        match &self.watch {
            Watch::Peer(id) => Some(id.clone()),
            Watch::Channel(id) => Some(thread_key(id)),
            Watch::Off | Watch::On(..) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_a_contact_will_not_take_says_how_to_fix_it() {
        let refused = SipilError::Refused {
            code: "unauthorized".to_string(),
            message: "That pilus has not added this device as a contact".to_string(),
        };
        assert!(peer_error(&refused).contains("device ID"));
        let daemon = SipilError::Refused {
            code: "unknown_method".to_string(),
            message: "Unknown method chat.send.".to_string(),
        };
        assert!(peer_error(&daemon).contains("sipil agent"));
        let offline = SipilError::Unreachable {
            target: "26e9ba62".to_string(),
            reason: "no answer".to_string(),
        };
        assert_eq!(peer_error(&offline), offline.to_string());
    }

    #[test]
    fn channel_members_who_missed_a_message_are_named() {
        let mut delivery = Delivery::default();
        assert_eq!(missed(&delivery), None);
        delivery
            .failed
            .push(("Ayu".to_string(), SipilError::Malformed));
        delivery
            .failed
            .push(("Dimas".to_string(), SipilError::Malformed));
        assert_eq!(
            missed(&delivery).as_deref(),
            Some("Not delivered to Ayu, Dimas.")
        );
    }

    fn line(mine: bool, agent: Option<&str>, who: Option<&str>) -> Line {
        Line {
            id: 1,
            mine,
            text: "x".to_string(),
            at: 0,
            who: who.map(str::to_string),
            agent: agent.map(str::to_string),
            reply: None,
            delegation: None,
        }
    }

    #[test]
    fn a_reply_keeps_its_quote_and_a_handed_off_task_reads_as_the_call_it_was() {
        let dir = std::env::temp_dir().join(format!("pilus-social-note-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let directory = Directory::open(&dir);
        let chat = Watch::Channel("0123456789abcdef".to_string());

        let mut answering = line(true, None, None);
        answering.reply = Some(Quote::of("Rina", "ship it"));
        assert_eq!(
            item_of(&directory, &chat, answering).reply,
            Some(Quote::of("Rina", "ship it"))
        );

        let mut note = line(true, Some("CoS"), None);
        note.delegation = Some(pilus_client::sipil::Delegation {
            call: "c1".to_string(),
            daemon: "d".to_string(),
            conversation: "9".to_string(),
            agent: "researcher".to_string(),
            task: "How does X work?".to_string(),
        });
        let item = item_of(&directory, &chat, note);
        assert_eq!(item.role, ItemRole::Assistant);
        assert_eq!(item.speaker.as_deref(), Some("CoS"));
        assert!(
            item.text.is_empty(),
            "it is drawn as the call, not as words"
        );
        let subagent = item.calls[0].subagent.as_ref().unwrap();
        assert_eq!(
            (subagent.conversation.as_deref(), subagent.daemon.as_deref()),
            (Some("9"), Some("d")),
            "it names the daemon, since the open conversation is a channel"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_names_its_speaker_only_when_the_role_does_not() {
        let dir = std::env::temp_dir().join(format!("pilus-social-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let directory = Directory::open(&dir);
        let rina = "a".repeat(64);
        directory.add(&rina, "Rina", None).unwrap();

        let chat = Watch::Peer(rina.clone());
        // A direct chat: my own line and her plain line need no name.
        assert_eq!(
            item_of(&directory, &chat, line(true, None, None)).speaker,
            None
        );
        assert_eq!(
            item_of(&directory, &chat, line(false, None, None)).speaker,
            None
        );
        // An agent of mine, and an agent of hers, do.
        assert_eq!(
            item_of(&directory, &chat, line(true, Some("Researcher"), None))
                .speaker
                .as_deref(),
            Some("Researcher")
        );
        assert_eq!(
            item_of(&directory, &chat, line(false, Some("CoS"), None))
                .speaker
                .as_deref(),
            Some("Rina/CoS")
        );
        // A channel names every speaker who is not me.
        let channel = Watch::Channel("0123456789abcdef".to_string());
        assert_eq!(
            item_of(&directory, &channel, line(false, None, Some(&rina)))
                .speaker
                .as_deref(),
            Some("Rina")
        );
        assert_eq!(
            item_of(&directory, &channel, line(false, Some("CoS"), Some(&rina)))
                .speaker
                .as_deref(),
            Some("Rina/CoS")
        );
        assert_eq!(
            item_of(&directory, &channel, line(true, None, None)).speaker,
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
