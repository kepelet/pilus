//! The people half of the Sipil tab state: contacts, channels, the
//! conversations in them, and the agents that join in when mentioned.
//!
//! Everything here is on this device's disk (contacts, channels and chats in
//! `~/.pilus/pilus.db`), so the lists are there before, and without, the
//! network. The iroh node starts when something needs it: a message
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
use std::time::{Duration, Instant};

use pilus_client::sipil::channels::thread_key;
use pilus_client::sipil::mention::{self, Mention};
use pilus_client::sipil::peer::local;
use pilus_client::sipil::{Backoff, Policy};
use pilus_client::sipil::{
    Channels, Chats, Contact, Conversation, Delivery, Directory, Event, ItemRole, Line, Local,
    LocalOptions, Offline, Quote, SipilError, TranscriptItem, parse_target,
};

use super::sipil::{
    Person, Shared, Watch, lock, nickname, publish, reachable_setting, relay_choice, request_id,
    snapshot, update,
};

/// How long a contact who answered is left alone before being asked again, while
/// the app is in front. Only the dot beside their name depends on it, and a
/// call from them (or opening their chat) refreshes it sooner.
const ONLINE_REFRESH: Duration = Duration::from_secs(120);
/// How long a contact given up on rests before the app coming forward may ask
/// again.
const FORWARD_REST: Duration = Duration::from_secs(120);

/// When one contact is next asked whether they are there.
pub(super) struct Probe {
    /// Spaces the asking of a contact who does not answer, and ends it.
    backoff: Backoff,
    /// When a contact who did answer is next asked; `None` means as soon as the
    /// app is in front.
    refresh_at: Option<Instant>,
}

impl Probe {
    fn new() -> Self {
        Self {
            backoff: Backoff::new(Policy::PERSON),
            refresh_at: None,
        }
    }

    /// Whether to ask this contact now. Nobody sees the dot while the app is
    /// behind another one, so nothing is asked then; coming forward asks.
    fn due(&self, now: Instant, front: bool, reach: Reach) -> bool {
        if !front || !self.backoff.due(now) {
            return false;
        }
        match reach {
            Reach::Online | Reach::Waiting => self.refresh_at.is_none_or(|at| now >= at),
            Reach::Unknown | Reach::Offline => true,
        }
    }
}

/// What `sync` last folded into the state. When none of it moved, there is
/// nothing to fold in, and the poller's wake costs a few atomic loads.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Synced {
    directory: u64,
    chats: u64,
    channels: u64,
    reach: u64,
    online: bool,
    reachable: bool,
    watch: Watch,
}

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

/// The stores the people half reads and writes.
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

/// The contact list, channels and conversations. Opening them reads `pilus.db`
/// and touches no network, so they are listed even while the node is down.
pub(super) fn stores() -> &'static Stores {
    STORES.get_or_init(|| {
        let dir = super::sipil::data_dir();
        let db = pilus_db::Db::at(&pilus_client::pilus_config_dir());
        Stores {
            directory: Arc::new(Directory::open(&db, &dir)),
            chats: Arc::new(Chats::open(&db, &dir)),
            channels: Arc::new(Channels::open(&db, &dir)),
            offline: Arc::new(Offline::open(&db, &dir)),
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
    // The unread count and the list order are the poller's to fold in; it
    // may be asleep for a long while behind another app.
    super::sipil::nudge();
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
            // A contact calling is the best news of them there is.
            local.on_heard(super::sipil::nudge);
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
    // Lines still waiting for them are not sent to a contact who is gone.
    stores().offline.forget(id);
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
    sync(Instant::now(), true);
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
/// composer to retry. A chat with a contact whose pilus is away is the exception:
/// the line waits in their outbox and goes when they are back.
pub(super) fn send_to_conversation(watching: &Watch, text: String, reply: Option<Quote>) {
    update(|state| {
        state.sending = true;
        state.send_error = None;
    });
    let watching = watching.clone();
    thread::spawn(move || {
        let away = match &watching {
            Watch::Peer(id) => stores().directory.contact(id).filter(|_| is_away(id)),
            _ => None,
        };
        // A contact known to be away is not dialed: that would hold the composer
        // for the whole dial deadline just to learn it again.
        if let Some(contact) = away {
            let first = stores()
                .offline
                .queued(&contact.id, &contact.id)
                .is_empty();
            hold(&contact, &text, reply.as_ref(), first);
            return;
        }
        let outcome = go_online().and_then(|local| match conversation(&watching) {
            Some(conversation) => local
                .say_replying(&conversation, &text, &request_id(), None, reply.as_ref())
                .map(|delivery| (local, conversation, delivery)),
            None => Err(SipilError::Address(
                "That conversation is no longer in the list.".to_string(),
            )),
        });
        // The dial found nobody: remember that, so the next line does not dial
        // again, and keep this one for when they are back.
        if let (Watch::Peer(id), Err(SipilError::Unreachable { .. })) = (&watching, &outcome)
            && let Some(contact) = stores().directory.contact(id)
        {
            note_away(id);
            hold(&contact, &text, reply.as_ref(), false);
            return;
        }
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

/// Whether a line to `id` waits instead of being dialed: the last check found
/// them away, or earlier lines are already waiting, which this one must not pass.
fn is_away(id: &str) -> bool {
    lock().reach.get(id) == Some(&Reach::Offline) || !stores().offline.queued(id, id).is_empty()
}

/// Record that a dial to `id` just found nobody, as a presence check would.
fn note_away(id: &str) {
    let mut shared = lock();
    if shared.reach.insert(id.to_string(), Reach::Offline) != Some(Reach::Offline) {
        shared.reach_rev += 1;
    }
    shared
        .probes
        .entry(id.to_string())
        .or_insert_with(Probe::new)
        .backoff
        .failed(Instant::now());
}

/// Keep `text` in `contact`'s outbox and count it as taken, so the composer
/// clears and the line shows under the log as waiting. `wake` asks whether they
/// are back at once, which writing to an absent contact is a reason for; it is
/// left off when a dial or a check has just said they are not, and for lines
/// that join ones already waiting.
fn hold(contact: &Contact, text: &str, reply: Option<&Quote>, wake: bool) {
    let queued = stores().offline.queue_replying(
        &contact.id,
        &contact.id,
        text,
        &request_id(),
        reply,
    );
    let taken = queued.is_some();
    update(|state| {
        state.sending = false;
        if taken {
            state.queued = super::sipil::queue_of(&state.watch);
            state.send_error = None;
            state.sent += 1;
        } else {
            state.send_error =
                Some("Too many messages are waiting for them to come back".to_string());
        }
    });
    if !taken {
        return;
    }
    // They may have come back since the outbox last emptied; the check that said
    // so only delivered what was waiting then.
    if lock().reach.get(&contact.id) == Some(&Reach::Online) {
        deliver_waiting(contact.clone());
    } else if wake {
        wake_presence(&contact.id);
    }
}

/// Contacts whose outbox is being handed over, one delivery each at a time so
/// the lines keep their order.
static DELIVERING: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Hand `contact` the lines written while they were away, oldest first, each
/// under the request id it was queued with so a delivery that was cut off and
/// retried is not said twice. It stops at the first line that does not go, to
/// keep them in order; the next sign of life tries again.
fn deliver_waiting(contact: Contact) {
    if stores().offline.queued(&contact.id, &contact.id).is_empty() {
        return;
    }
    {
        let mut busy = DELIVERING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if busy.contains(&contact.id) {
            return;
        }
        busy.push(contact.id.clone());
    }
    thread::spawn(move || {
        if let Some(node) = local() {
            while let Some(next) = stores()
                .offline
                .queued(&contact.id, &contact.id)
                .into_iter()
                .next()
            {
                let outcome = node.send_replying(
                    &contact,
                    &next.text,
                    &next.request,
                    None,
                    next.reply.as_ref(),
                );
                match outcome {
                    Ok(()) => {
                        stores().offline.settle(&contact.id, &next.request);
                    }
                    // Away again, slow, or yet to add this device (they may):
                    // keep the rest, in order.
                    Err(error) if error.is_unauthorized() => break,
                    Err(SipilError::Refused { .. }) => {
                        // Waiting will not change that. Say so, so it can be
                        // written again.
                        stores().offline.settle(&contact.id, &next.request);
                        update(|state| {
                            state.send_error = Some(format!(
                                "A message written while {} was away was not taken: {}",
                                contact.name, next.text
                            ));
                        });
                    }
                    Err(_) => break,
                }
                update(|state| state.queued = super::sipil::queue_of(&state.watch));
            }
        }
        DELIVERING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|id| *id != contact.id);
        update(|state| state.queued = super::sipil::queue_of(&state.watch));
        refresh_thread();
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
/// state, and ask the contacts who are due whether they are there. All of it is
/// local except the asking, which runs on its own thread.
pub(super) fn sync(now: Instant, front: bool) {
    let stores = stores();
    let node = local();
    if let Some(node) = &node {
        for id in node.take_heard() {
            heard(&id);
        }
    }
    if node.is_some() {
        check_presence_at(now, front);
    }
    let mut signature = Synced {
        directory: stores.directory.version(),
        chats: stores.chats.version(),
        channels: stores.channels.version(),
        online: node.is_some(),
        reachable: node.as_ref().is_some_and(|node| node.reachable()),
        watch: snapshot().watch.clone(),
        ..Synced::default()
    };
    {
        let mut shared = lock();
        signature.reach = shared.reach_rev;
        if shared.synced.as_ref() == Some(&signature) {
            return;
        }
        shared.synced = Some(signature);
    }
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

/// A contact called this node. A daemon is tried again now, a person is asked
/// about now: either is up, and nothing else says so sooner.
fn heard(id: &str) {
    if super::sipil::is_daemon(id) {
        super::sipil::wake_daemon(id);
    } else {
        wake_presence(id);
        // The call proves they are up, even while the app is behind another one
        // and no check would run, so lines waiting for them go now.
        if let Some(contact) = stores().directory.contact(id) {
            deliver_waiting(contact);
        }
    }
}

/// Ask `id` whether they are there at the next chance, whatever the backoff
/// says: the person opened their chat, or they called.
pub(super) fn wake_presence(id: &str) {
    {
        let mut shared = lock();
        let probe = shared
            .probes
            .entry(id.to_string())
            .or_insert_with(Probe::new);
        probe.backoff.wake();
        probe.refresh_at = None;
    }
    check_presence();
}

/// The app came to the front: what it knew about the contacts is stale, so ask
/// again, and give those who were given up on one more try if they have rested.
pub(super) fn catch_up(now: Instant) {
    let mut shared = lock();
    for probe in shared.probes.values_mut() {
        probe.backoff.wake_if_rested(now, FORWARD_REST);
        probe.refresh_at = None;
    }
}

/// Ask the contacts who are due whether their pilus is there, once at a time.
pub(super) fn check_presence() {
    check_presence_at(Instant::now(), super::activity::in_front());
}

fn check_presence_at(now: Instant, front: bool) {
    if CHECKING.load(Ordering::Acquire) {
        return;
    }
    let due: Vec<Contact> = {
        let mut shared = lock();
        let Shared { probes, reach, .. } = &mut *shared;
        stores()
            .directory
            .contacts()
            .into_iter()
            .filter(|contact| {
                let found = reach.get(&contact.id).copied().unwrap_or_default();
                probes
                    .entry(contact.id.clone())
                    .or_insert_with(Probe::new)
                    .due(now, front, found)
            })
            .collect()
    };
    if due.is_empty() || CHECKING.swap(true, Ordering::AcqRel) {
        return;
    }
    thread::spawn(move || {
        if let Some(node) = local() {
            for contact in due {
                let found = match node.check(&contact) {
                    Ok(health) if health.allowed => Reach::Online,
                    Ok(_) => Reach::Waiting,
                    Err(_) => Reach::Offline,
                };
                let now = Instant::now();
                let mut shared = lock();
                if shared.reach.insert(contact.id.clone(), found) != Some(found) {
                    shared.reach_rev += 1;
                }
                let probe = shared
                    .probes
                    .entry(contact.id.clone())
                    .or_insert_with(Probe::new);
                if found == Reach::Offline {
                    probe.backoff.failed(now);
                } else {
                    probe.backoff.succeeded();
                    probe.refresh_at = Some(now + ONLINE_REFRESH);
                }
                drop(shared);
                // They are back: what was written while they were away goes now.
                if found == Reach::Online {
                    deliver_waiting(contact);
                }
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

/// The newest thing anyone said in any conversation this pilus has, for the
/// Home page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Latest {
    /// The conversation it was said in, which is what a click opens.
    pub(super) watch: Watch,
    /// What the conversation is called: a contact, `#channel`, or an agent.
    pub(super) place: String,
    /// Who said it.
    pub(super) who: String,
    pub(super) text: String,
    /// Unix seconds; 0 for a remembered line the daemon never dated.
    pub(super) at: u64,
    /// Lines in that conversation nobody has read.
    pub(super) unread: u64,
}

/// The newest line across the contacts' and channels' threads and the primary
/// agents' remembered conversations. All of it is read from this device's own
/// store, so it works with the network down and never dials anyone.
pub(super) fn latest(state: &super::sipil::SipilState) -> Option<Latest> {
    let stores = stores();
    let mut found: Vec<Latest> = Vec::new();
    let named = |id: &str| {
        state
            .me
            .people
            .iter()
            .find(|person| person.contact.id == id)
            .map(|person| person.contact.name.clone())
    };
    for person in &state.me.people {
        let Some(line) = stores.chats.last(&person.contact.id) else {
            continue;
        };
        let who = if line.mine {
            "you".to_string()
        } else {
            line.agent
                .clone()
                .unwrap_or_else(|| person.contact.name.clone())
        };
        found.push(Latest {
            watch: Watch::Peer(person.contact.id.clone()),
            place: person.contact.name.clone(),
            who,
            text: line.text,
            at: line.at,
            unread: person.unread,
        });
    }
    for row in &state.me.channels {
        let Some(line) = stores.chats.last(&row.channel.thread()) else {
            continue;
        };
        let who = if line.mine {
            "you".to_string()
        } else if let Some(agent) = line.agent.clone() {
            agent
        } else {
            line.who
                .as_deref()
                .map(|id| named(id).unwrap_or_else(|| id.chars().take(8).collect()))
                .unwrap_or_default()
        };
        found.push(Latest {
            watch: Watch::Channel(row.channel.id.clone()),
            place: format!("#{}", row.channel.name),
            who,
            text: line.text,
            at: line.at,
            unread: row.unread,
        });
    }
    for daemon in &state.daemons {
        for agent in &daemon.agents {
            let Some((items, _)) = stores.offline.transcript(&daemon.id, &agent.id) else {
                continue;
            };
            let said = items.into_iter().rev().find(|item| {
                item.role == ItemRole::Assistant
                    && item.error.is_none()
                    && !item.text.trim().is_empty()
            });
            if let Some(item) = said {
                found.push(Latest {
                    watch: Watch::On(daemon.id.clone(), agent.id.clone()),
                    place: daemon.name(),
                    who: agent.name.clone(),
                    text: item.text,
                    at: item.at.unwrap_or(0),
                    unread: 0,
                });
            }
        }
    }
    found.into_iter().max_by_key(|latest| latest.at)
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
        }
    }

    #[test]
    fn a_reply_keeps_its_quote() {
        let dir = std::env::temp_dir().join(format!("pilus-social-note-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let directory = Directory::open(&pilus_db::Db::in_memory(), &dir);
        let chat = Watch::Channel("0123456789abcdef".to_string());

        let mut answering = line(true, None, None);
        answering.reply = Some(Quote::of("Rina", "ship it"));
        assert_eq!(
            item_of(&directory, &chat, answering).reply,
            Some(Quote::of("Rina", "ship it"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_names_its_speaker_only_when_the_role_does_not() {
        let dir = std::env::temp_dir().join(format!("pilus-social-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let directory = Directory::open(&pilus_db::Db::in_memory(), &dir);
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
