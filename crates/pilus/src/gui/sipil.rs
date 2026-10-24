//! The window's handle on sipil: the durable agent daemons it is connected to,
//! and the other pilus windows it talks to.
//!
//! Every sipil, daemon or pilus, is reached over iroh by device ID. This module
//! keeps one shared [`SipilState`] that a poller thread refreshes, and runs every
//! connect, save and send on a short-lived thread, so the GUI thread never waits
//! on a socket. The main window reads [`snapshot`] on each tick and the sipil
//! window reads it to paint; neither mutates it directly.
//!
//! There are two halves. The *daemon* half holds any number of daemons (a
//! Researcher, a Chief of Staff), each against its own saved address: a first
//! connect that fails gives up and says why, since the address is probably
//! wrong, while a connection that *was* up and drops, or a launch that finds a
//! daemon not yet running, keeps retrying quietly, but not forever: each failure
//! doubles the wait, and after a few minutes the daemon is shown as offline and
//! left alone until it calls, or the person opens or writes to it. The *people*
//! half ([`super::sipil_social`]) is this device's own side: the contacts, the
//! channels and their conversations, which live on disk and are listed whether
//! or not the network is up, and the iroh node, which starts when something
//! needs it.
//!
//! A conversation the window has open is [`Watch`]ed: for a daemon the poller
//! then reads its transcript every second instead of every few, and a send reads
//! it again at once, so the message appears without waiting for a tick. A daemon
//! that has the `watch` call pushes instead: a thread holds a stream open for the
//! open conversation and folds each event into the state, so the answer appears as
//! it is written, and polling steps aside for as long as the stream is up. A
//! daemon from before the call is polled as it always was. A contact's or a
//! channel's conversation is read from the local chat store, which the node
//! writes into as messages arrive.
//!
//! A daemon is often not there, so what it last said is kept
//! ([`pilus_client::sipil::Offline`]): its primary agent, so the sidebar row still
//! opens the chat, and the newest lines of the conversations looked at, which the
//! chat page shows until the daemon's own arrive. A message written meanwhile is
//! queued, shown beneath the log as waiting, and delivered in order as soon as the
//! daemon answers.
//!
//! State is swapped as a whole [`Arc`], and only when its contents changed, so
//! comparing two snapshots by pointer is enough for the window to know whether
//! to repaint.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pilus_client::sipil::history;
use pilus_client::sipil::{
    About, Activity, Agent, Backoff, Channel, ConversationInfo, Next, Policy, Queued, Quote,
    RememberedAgent, Request, Role, Routine, RunningTool, Session, Sipil, SipilError,
    TranscriptItem, Watched, Watching, WorkBoard, parse_target,
};
use pilus_client::{RelaySettings, pilus_config_dir};

use gpui::RenderImage;

use super::about::rasterize;
use super::activity;
use super::sipil_social as social;
#[cfg(not(feature = "visual-test"))]
pub use super::sipil_social::prepare;
pub use super::sipil_social::{
    Reach, accept_request, add_contact, create_channel, dismiss_request, leave_channel,
    remove_contact, remove_daemon, rename_contact, take_events,
};

/// How often the poller wakes while the app is in front and nothing is due
/// sooner. It only folds local files into the state, so it is cheap; what costs is
/// dialing, and that has its own schedule.
const TICK: Duration = Duration::from_secs(1);
/// The same while the app is behind another one. Anything that matters sooner
/// (a message, a contact calling, the app coming forward) wakes the poller itself.
const IDLE_TICK: Duration = Duration::from_secs(10);
/// How long a stream may say nothing, not even the daemon's ping every fifteen
/// seconds, before it is taken for dead and opened again.
const STREAM_SILENCE: Duration = Duration::from_secs(45);
/// How long to wait before opening a stream that broke again.
const STREAM_RETRY: Duration = Duration::from_secs(2);
/// Bumped whenever the open conversation changes, so a stream for one the window
/// has left ends.
static WATCH_GENERATION: AtomicU64 = AtomicU64::new(0);

/// How long a connected daemon is left alone between asking for its lists, while
/// none of its conversations is open.
const LIST_EVERY: Duration = Duration::from_secs(3);
/// The same for an open conversation that is not being streamed.
const OPEN_EVERY: Duration = Duration::from_secs(1);
/// How much longer both wait while the app is behind another one.
const BEHIND_FACTOR: u32 = 5;
/// How long a daemon given up on rests before the app coming forward may try it
/// again, so switching apps back and forth is not a dial per switch.
const FORWARD_REST: Duration = Duration::from_secs(120);

/// Where the connection to one daemon stands.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Phase {
    /// Not connected, and not trying: the first attempt failed.
    #[default]
    Off,
    /// Trying: a first connect, or getting a dropped one back.
    Connecting,
    /// Connected; the lists are live.
    Connected,
}

/// The conversation the chat page is showing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Watch {
    /// No chat page is open.
    #[default]
    Off,
    /// An existing conversation on a daemon: the daemon's device ID, then the
    /// conversation's id.
    On(String, String),
    /// A chat with a contact, by their device ID.
    Peer(String),
    /// A channel, by id.
    Channel(String),
}

impl Watch {
    /// Whether this conversation lives on one of this pilus's daemons.
    pub fn daemon(&self) -> Option<&str> {
        match self {
            Self::On(daemon, _) => Some(daemon),
            Self::Off | Self::Peer(_) | Self::Channel(_) => None,
        }
    }

    /// Whether this is a conversation with other people, which never depended on this pilus being connected to a daemon.
    pub fn is_social(&self) -> bool {
        matches!(self, Self::Peer(_) | Self::Channel(_))
    }
}

/// The conversation a chat-store thread key names: a channel's `#id`, or a
/// contact's device ID.
pub fn thread_watch(thread: &str) -> Watch {
    match pilus_client::sipil::channels::channel_of(thread) {
        Some(id) => Watch::Channel(id.to_string()),
        None => Watch::Peer(thread.to_string()),
    }
}

/// One daemon this window is connected to, or trying to be.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Daemon {
    /// Its device ID.
    pub id: String,
    /// The device ID or ticket it was added by.
    pub address: String,
    /// What its primary agent is called: how others see it and how it is
    /// mentioned.
    pub label: String,
    /// What this device's person calls it, once they have renamed it.
    pub alias: String,
    /// Off, connecting, or connected.
    pub phase: Phase,
    /// The daemon's release, once it has answered.
    pub version: Option<String>,
    /// The last failure, for the user to read.
    pub error: Option<String>,
    /// The daemon's agents.
    pub agents: Vec<Agent>,
    /// The top-level conversations.
    pub sessions: Vec<Session>,
    /// What it says about itself, once it has said.
    pub about: Option<About>,
    /// It is from before the `about` call, so asking again is no use.
    pub about_unsupported: bool,
    /// Its logo, drawn once from the SVG it advertises.
    pub logo: Option<Logo>,
    /// Its primary agent's board of open work, once it has said
    /// (`../sipil/src/work.ts`).
    pub work: Option<WorkBoard>,
    /// Its primary agent's routines, with when each runs next.
    pub routines: Vec<Routine>,
    /// It is from before the board and routines, so asking again is no use.
    pub board_unsupported: bool,
}

/// A daemon's logo, already drawn. Two are the same when they are the same
/// image, which is how a changed logo shows as a change.
#[derive(Clone)]
pub struct Logo(pub Arc<RenderImage>);

impl PartialEq for Logo {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for Logo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Logo")
    }
}

/// The most SVG a logo may be, whatever the daemon sends.
const LOGO_MAX_BYTES: usize = 256 * 1024;
/// Pixels a logo is drawn at: well above the largest it is painted.
const LOGO_EXTENT: u32 = 128;

impl Daemon {
    /// The logo this daemon advertised, if it has. Without one it wears the
    /// pilus mark in the colour of the icons around it, drawn by the caller:
    /// there is no picture of it, so there is nothing behind it.
    pub fn own_logo(&self) -> Option<Arc<RenderImage>> {
        self.logo.as_ref().map(|logo| Arc::clone(&logo.0))
    }

    /// Whether the sidebar should show this daemon's sections.
    pub fn connected(&self) -> bool {
        self.phase == Phase::Connected
    }

    /// What the daemon is called: the name it was given here, else its agent's
    /// name, else the start of its ID.
    pub fn name(&self) -> String {
        if !self.alias.is_empty() {
            self.alias.clone()
        } else if self.label.is_empty() {
            self.id.chars().take(8).collect()
        } else {
            self.label.clone()
        }
    }
}

/// A contact with what the window shows beside the name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Person {
    /// Who they are.
    pub contact: pilus_client::sipil::Contact,
    /// Whether they are there.
    pub reach: Reach,
    /// Lines of theirs not read yet.
    pub unread: u64,
}

/// A channel with its unread count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelRow {
    /// The channel.
    pub channel: Channel,
    /// Lines in it that nobody has read.
    pub unread: u64,
    /// The daemons (by device ID) whose agent this pilus invited into it.
    pub agents: Vec<String>,
}

/// This device's side of sipil: who it is, who it talks to, and whether the
/// network is up. It is kept across a disconnect from any daemon.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Me {
    /// What this device is called in a conversation.
    pub nickname: String,
    /// This device's ID, once its identity exists.
    pub device_id: Option<String>,
    /// The iroh node is running.
    pub online: bool,
    /// Other pilus windows may call this one.
    pub reachable: bool,
    /// Why the network could not start.
    pub error: Option<String>,
    /// The contacts, in the order they were added.
    pub people: Vec<Person>,
    /// The channels, in the order they were made or learned.
    pub channels: Vec<ChannelRow>,
    /// Devices that tried to reach this one without being contacts.
    pub requests: Vec<Request>,
    /// Why the last contact or channel could not be made.
    pub contact_error: Option<String>,
    /// Why the last daemon address could not be used.
    pub daemon_error: Option<String>,
    /// How many contacts and channels have been made. The window clears its
    /// forms when this grows, so a refusal leaves what was typed.
    pub added: u64,
}

/// What sipil last told us, for painting.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SipilState {
    /// This device, its contacts and its channels.
    pub me: Me,
    /// The daemons, in the order they were added.
    pub daemons: Vec<Daemon>,
    /// A conversation, and a note a person is waiting on, for the chat page.
    pub watch: Watch,
    /// Its newest lines, oldest first.
    pub transcript: Vec<TranscriptItem>,
    /// The transcript of `watch` has arrived, so an empty one is empty rather
    /// than still loading.
    pub transcript_loaded: bool,
    /// The daemon has lines before the first of `transcript`: older ones can be
    /// loaded.
    pub transcript_more: bool,
    /// Older lines are being fetched.
    pub loading_older: bool,
    /// The answer being written in `watch`, which is not yet a line of the
    /// transcript. Only a daemon that streams fills it.
    pub live: String,
    /// The calls running in the watched conversation, with what each has
    /// printed. Only a daemon that streams fills it.
    pub running: Vec<RunningTool>,
    /// What the daemon says the watched conversation is: its name and status.
    pub watched: Option<ConversationInfo>,
    /// Agents of ours that are answering a mention: the thread, then the agent.
    pub thinking: Vec<(String, String)>,
    /// Messages written to the watched conversation that its daemon has not taken
    /// yet, oldest first: it was away when they were written.
    pub queued: Vec<Queued>,
    /// A message is on its way.
    pub sending: bool,
    /// Why the last message was not taken, or who it did not reach.
    pub send_error: Option<String>,
    /// Why the open conversation could not be read.
    pub chat_error: Option<String>,
    /// How many messages have been taken. The page clears its composer when
    /// this grows, so a message that was refused stays in it to retry.
    pub sent: u64,
}

impl SipilState {
    /// The daemon with device ID `id`.
    pub fn daemon(&self, id: &str) -> Option<&Daemon> {
        self.daemons.iter().find(|daemon| daemon.id == id)
    }

    /// The daemon `watch` lives on, if it is a daemon conversation.
    pub fn watched_daemon(&self) -> Option<&Daemon> {
        self.watch.daemon().and_then(|id| self.daemon(id))
    }

    /// The contact `watch` names, when it is a chat with them.
    pub fn watched_person(&self) -> Option<&Person> {
        match &self.watch {
            Watch::Peer(id) => self
                .me
                .people
                .iter()
                .find(|person| person.contact.id == *id),
            Watch::Off | Watch::On(..) | Watch::Channel(_) => None,
        }
    }

    /// The channel `watch` names.
    pub fn watched_channel(&self) -> Option<&Channel> {
        match &self.watch {
            Watch::Channel(id) => self
                .me
                .channels
                .iter()
                .map(|row| &row.channel)
                .find(|channel| channel.id == *id),
            _ => None,
        }
    }

    /// The names of our agents that are answering a mention in the open
    /// conversation.
    pub fn thinking_here(&self) -> Vec<String> {
        let Some(thread) = self.watch_thread() else {
            return Vec::new();
        };
        self.thinking
            .iter()
            .filter(|(at, _)| *at == thread)
            .map(|(_, agent)| agent.clone())
            .collect()
    }

    /// Lines nobody has read, in contacts' chats and channels.
    pub fn unread(&self) -> u64 {
        self.me
            .people
            .iter()
            .map(|person| person.unread)
            .sum::<u64>()
            + self.me.channels.iter().map(|row| row.unread).sum::<u64>()
    }

    /// The names of this pilus's agents that can be invited into a channel: the
    /// connected daemons' agents, by the daemon they belong to.
    pub fn mentionable(&self) -> Vec<(String, String)> {
        self.daemons
            .iter()
            .filter(|daemon| daemon.connected())
            .map(|daemon| (daemon.id.clone(), daemon.name()))
            .collect()
    }
}

/// How this window keeps up with one daemon.
pub(super) struct Link {
    /// The daemon's device ID.
    pub(super) id: String,
    /// Bumped by every attempt, so a slow call that outlives its attempt cannot
    /// overwrite the next one.
    pub(super) epoch: u64,
    /// The connection, once the node can make one. While it is `None` nothing is
    /// polled.
    pub(super) sipil: Option<Sipil>,
    /// A poll of this daemon is running.
    polling: Arc<AtomicBool>,
    /// A stream is delivering the open conversation, so it is not polled.
    streaming: Arc<AtomicBool>,
    /// The outbox is being handed over, which only one thread does at a time.
    delivering: Arc<AtomicBool>,
    /// When to dial this daemon again after it failed to answer, and whether to
    /// have stopped: a daemon that is switched off is not worth a dial every few
    /// seconds, and answers when it comes back.
    backoff: Backoff,
    /// When a connected daemon is next asked for its lists.
    next_poll: Instant,
}

#[derive(Default)]
pub(super) struct Shared {
    pub(super) state: Arc<SipilState>,
    /// The daemons to keep up with, while the user wants them connected.
    pub(super) links: Vec<Link>,
    next_epoch: u64,
    /// What the last presence check found, by device ID.
    pub(super) reach: BTreeMap<String, Reach>,
    /// Counts every change to `reach`, so the sync knows to fold it in.
    pub(super) reach_rev: u64,
    /// When each contact is next asked whether they are there.
    pub(super) probes: BTreeMap<String, social::Probe>,
    /// What the last sync folded in.
    pub(super) synced: Option<social::Synced>,
}

impl Shared {
    fn link(&self, id: &str) -> Option<&Link> {
        self.links.iter().find(|link| link.id == id)
    }

    fn link_mut(&mut self, id: &str) -> Option<&mut Link> {
        self.links.iter_mut().find(|link| link.id == id)
    }

    fn fresh_epoch(&mut self) -> u64 {
        self.next_epoch += 1;
        self.next_epoch
    }

    /// Whether attempt `epoch` is still the one `id` is on.
    fn current(&self, id: &str, epoch: u64) -> bool {
        self.link(id).is_some_and(|link| link.epoch == epoch)
    }
}

static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
static POLLER: OnceLock<()> = OnceLock::new();
/// Whether other windows may call this one, as the settings say.
static REACHABLE: AtomicBool = AtomicBool::new(false);

pub(super) fn lock() -> MutexGuard<'static, Shared> {
    SHARED
        .get_or_init(|| Mutex::new(Shared::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Change the state in place, publishing a new snapshot only if it differs.
pub(super) fn update(change: impl FnOnce(&mut SipilState)) {
    let mut shared = lock();
    publish(&mut shared, change);
}

pub(super) fn publish(shared: &mut Shared, change: impl FnOnce(&mut SipilState)) {
    let mut next = (*shared.state).clone();
    change(&mut next);
    if next != *shared.state {
        shared.state = Arc::new(next);
    }
}

/// Change one daemon's state, if it is still listed.
#[cfg(test)]
fn update_daemon(id: &str, change: impl FnOnce(&mut Daemon)) {
    update(|state| {
        if let Some(daemon) = state.daemons.iter_mut().find(|daemon| daemon.id == id) {
            change(daemon);
        }
    });
}

/// The current state. Cheap; call it every frame.
pub fn snapshot() -> Arc<SipilState> {
    lock().state.clone()
}

/// What is remembered of the daemons.
fn offline() -> &'static pilus_client::sipil::Offline {
    &social::stores().offline
}

/// The primary agent of a daemon that is not answering, as it was last seen.
fn remembered_agent(agent: RememberedAgent) -> Agent {
    Agent {
        id: agent.id,
        name: agent.name,
        role: Role::Primary,
        status: Activity::Idle,
        model: None,
        error: None,
    }
}

/// The messages waiting for the conversation `watch` is. A contact's chat is
/// named by their device ID, which is also where their outbox is kept.
pub(super) fn queue_of(watch: &Watch) -> Vec<Queued> {
    match watch {
        Watch::On(daemon, id) => offline().queued(daemon, id),
        Watch::Peer(id) => offline().queued(id, id),
        // A channel has no store-and-forward: members who are away miss the line.
        Watch::Off | Watch::Channel(_) => Vec::new(),
    }
}

/// Where this device's identity, contacts, channels and chats live.
pub(super) fn data_dir() -> std::path::PathBuf {
    pilus_config_dir().join("sipil")
}

/// What this device is called in a conversation.
pub(super) fn nickname() -> String {
    std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .unwrap_or_else(|| "pilus".to_string())
}

/// Whether other windows may call this one.
pub(super) fn reachable_setting() -> bool {
    REACHABLE.load(Ordering::Relaxed)
}

/// The relays the node should use: the settings' own, else the public ones.
pub(super) fn relay_choice() -> pilus_client::sipil::RelayChoice {
    use pilus_client::sipil::RelayChoice;
    match RelaySettings::load(&pilus_config_dir()) {
        Ok(settings) if !settings.url.is_empty() => RelayChoice::Urls(settings.url),
        // A relay block that does not parse is the agent's to report; the
        // window falls back to the public relays rather than stay offline.
        _ => RelayChoice::Default,
    }
}

/// The daemons that are contacts, each by the ticket it gave or else its device
/// ID: what to dial.
pub fn daemon_addresses() -> Vec<String> {
    social::stores()
        .directory
        .daemons()
        .into_iter()
        .map(|contact| contact.ticket.unwrap_or(contact.id))
        .collect()
}

/// Connect to a daemon that has just become a contact. It may not have put this
/// device on its allow-list yet, so a refusal is retried by the poller instead
/// of being reported.
pub fn connect_daemon(address: &str) {
    ensure(&[address.to_string()], REACHABLE.load(Ordering::Relaxed));
}

/// Start up: reconnect to the daemons that are contacts, and bring the network
/// up if the user left themselves reachable.
///
/// A daemon that is not up yet is not an error: the poller keeps trying, and
/// its sections appear whenever it answers.
pub fn ensure(addresses: &[String], reachable: bool) {
    REACHABLE.store(reachable, Ordering::Relaxed);
    update(|state| state.me.reachable = reachable);
    start_poller();
    // An address that does not parse (the old `http://` URL) is not one to wait on.
    let wanted: Vec<(String, String)> = addresses
        .iter()
        .filter_map(|address| {
            parse_target(address)
                .ok()
                .map(|target| (target.id(), address.clone()))
        })
        .collect();
    if wanted.is_empty() && !reachable {
        return;
    }
    let attempts: Vec<(String, String, u64)> = wanted
        .into_iter()
        .map(|(id, address)| {
            let epoch = begin(&id, &address);
            (id, address, epoch)
        })
        .collect();
    thread::spawn(move || {
        let Ok(local) = social::go_online() else {
            return;
        };
        for (id, address, epoch) in attempts {
            let Ok(target) = Sipil::with_node(local.node(), &address) else {
                continue;
            };
            let polling = {
                let mut shared = lock();
                let Some(link) = shared.links.iter_mut().find(|link| link.id == id) else {
                    continue;
                };
                if link.epoch != epoch {
                    continue;
                }
                // From here the poller owns the retry. It is told this first
                // attempt is under way, or it would dial again at once.
                link.sipil = Some(target.clone());
                link.polling.store(true, Ordering::Release);
                Arc::clone(&link.polling)
            };
            // Failing quietly is the point: it is noted, so the poller's first
            // turn waits out the backoff instead of dialing again at once.
            let id = id.clone();
            thread::spawn(move || {
                match target.connect() {
                    Ok(health) => adopt(&id, &target, epoch, &health),
                    Err(error) => {
                        let mut shared = lock();
                        if shared.current(&id, epoch) {
                            failed(&mut shared, &id, &error.to_string());
                        }
                    }
                }
                polling.store(false, Ordering::Release);
                nudge();
            });
        }
    });
}

/// Stop talking to a daemon and drop everything it told us.
pub fn disconnect(id: &str) {
    let mut shared = lock();
    shared.links.retain(|link| link.id != id);
    publish(&mut shared, |state| {
        state.daemons.retain(|daemon| daemon.id != id);
        // A page open on that daemon's conversation has nothing left to show.
        if state.watch.daemon() == Some(id) {
            state.watch = Watch::Off;
            state.transcript.clear();
            state.transcript_loaded = false;
        }
    });
    drop(shared);
    share_daemons();
}

/// Start an attempt on a daemon, forgetting whatever came before about it.
/// Returns the epoch the attempt owns. The connection is filled in once the
/// node can make it.
fn begin(id: &str, address: &str) -> u64 {
    let mut shared = lock();
    let epoch = shared.fresh_epoch();
    shared.links.retain(|link| link.id != id);
    shared.links.push(Link {
        id: id.to_string(),
        epoch,
        sipil: None,
        polling: Arc::default(),
        streaming: Arc::default(),
        delivering: Arc::default(),
        backoff: Backoff::new(Policy::DAEMON),
        next_poll: Instant::now(),
    });
    publish(&mut shared, |state| {
        // Until it answers, it is listed as the agent it was last time, so the
        // chat opens while the daemon is away.
        let (label, agents) = match offline().agent(id) {
            Some((label, agent)) => (label, vec![remembered_agent(agent)]),
            None => (String::new(), Vec::new()),
        };
        let fresh = Daemon {
            id: id.to_string(),
            address: address.to_string(),
            phase: Phase::Connecting,
            label,
            agents,
            ..Daemon::default()
        };
        match state.daemons.iter_mut().find(|daemon| daemon.id == id) {
            Some(daemon) => *daemon = fresh,
            None => state.daemons.push(fresh),
        }
    });
    epoch
}

/// Tell the network side which daemons are connected, so a channel an agent was
/// invited into can run it. A daemon that connects before the network is
/// up has nothing to share yet; it is offered when [`social::go_online`] runs.
pub(super) fn share_daemons() {
    let shared = lock();
    let daemons: Vec<pilus_client::sipil::SharedDaemon> = shared
        .state
        .daemons
        .iter()
        .filter(|daemon| daemon.connected())
        .filter_map(|daemon| {
            let link = shared.link(&daemon.id)?;
            Some(pilus_client::sipil::SharedDaemon {
                id: daemon.id.clone(),
                name: daemon.name(),
                sipil: link.sipil.clone()?,
            })
        })
        .collect();
    drop(shared);
    if let Some(local) = pilus_client::sipil::peer::local() {
        local.set_daemons(daemons);
    }
}

/// Let other pilus windows reach this one, or stop answering them.
pub fn set_reachable(reachable: bool) {
    REACHABLE.store(reachable, Ordering::Relaxed);
    update(|state| state.me.reachable = reachable);
    thread::spawn(move || {
        if reachable {
            if let Ok(local) = social::go_online() {
                local.set_reachable(true);
            }
        } else if let Some(local) = pilus_client::sipil::peer::local() {
            local.set_reachable(false);
        }
    });
}

/// Open a conversation in the chat page, or close it with [`Watch::Off`].
pub fn watch(watch: Watch) {
    let daemon_side = match &watch {
        Watch::On(daemon, id) => Some((daemon.clone(), id.clone())),
        _ => None,
    };
    let social_side = watch.is_social();
    let generation = WATCH_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let (target, epoch, streaming) = {
        let mut shared = lock();
        // The stream of the conversation left behind is over, so nothing it
        // delivered counts any more.
        for link in &shared.links {
            link.streaming.store(false, Ordering::Release);
        }
        publish(&mut shared, |state| {
            state.watch = watch.clone();
            state.transcript.clear();
            state.transcript_loaded = false;
            state.transcript_more = false;
            state.loading_older = false;
            state.live.clear();
            state.running.clear();
            state.watched = None;
            state.send_error = None;
            state.chat_error = None;
            state.queued = queue_of(&watch);
            // What was last seen stands in until the daemon's own lines arrive;
            // with the daemon away they are all there is.
            if let Some((daemon, id)) = &daemon_side
                && let Some((lines, more)) = offline().transcript(daemon, id)
            {
                state.transcript = lines;
                state.transcript_more = more;
                state.transcript_loaded = true;
            }
        });
        if social_side {
            drop(shared);
            // A conversation with people is on disk: show it now rather than at
            // the tick.
            social::refresh_thread();
            if let Watch::Peer(id) = &watch {
                social::wake_presence(id);
            }
            return;
        }
        let Some((daemon, _)) = &daemon_side else {
            return;
        };
        // Someone opening a daemon's conversation is a reason to ask whether it
        // is there, and to read the conversation without waiting for its turn.
        wake_link(&mut shared, daemon);
        nudge();
        match shared
            .link(daemon)
            .and_then(|link| Some((link.sipil.clone()?, link.epoch, link.streaming.clone())))
        {
            Some(found) => found,
            None => return,
        }
    };
    let Some((daemon, id)) = daemon_side else {
        return;
    };
    thread::spawn(move || {
        stream_conversation(&daemon, &target, epoch, &id, generation, &streaming)
    });
}

/// Say `text` in the open conversation, with `reply` the line it answers.
pub fn send(text: String, reply: Option<Quote>) {
    let watching = {
        let shared = lock();
        if shared.state.sending {
            return;
        }
        shared.state.watch.clone()
    };
    send_in(watching, text, reply);
}

/// Say `text` in `watching`, which need not be the open conversation: the Home
/// page answers an agent without opening its chat.
pub fn send_in(watching: Watch, text: String, reply: Option<Quote>) {
    match watching {
        Watch::Peer(_) | Watch::Channel(_) => {
            return social::send_to_conversation(&watching, text, reply);
        }
        Watch::Off => return,
        Watch::On(..) => {}
    }
    // A daemon's conversation has no field for a quote, so the model reads it
    // in front of the message.
    let text = quoted(text, reply.as_ref());
    let Some(daemon) = watching.daemon().map(str::to_string) else {
        return;
    };
    let (target, epoch) = {
        let mut shared = lock();
        let up = shared.state.daemon(&daemon).is_some_and(Daemon::connected);
        let found = shared
            .link(&daemon)
            .and_then(|link| Some((link.sipil.clone()?, link.epoch)))
            .filter(|_| up);
        let Some(found) = found else {
            // The daemon is away: the message waits for it.
            if let Watch::On(_, id) = &watching {
                hold(&mut shared, &daemon, id, &text);
            }
            // Writing to it is a reason to ask whether it is back.
            wake_link(&mut shared, &daemon);
            drop(shared);
            nudge();
            return;
        };
        publish(&mut shared, |state| {
            state.sending = true;
            state.send_error = None;
        });
        found
    };
    thread::spawn(move || {
        let outcome = match &watching {
            Watch::On(_, id) => target
                .send_message(id, &text, &request_id())
                .map(|()| id.clone()),
            _ => Err(SipilError::Malformed),
        };
        let mut shared = lock();
        if !shared.current(&daemon, epoch) {
            return;
        }
        publish(&mut shared, |state| {
            state.sending = false;
            match &outcome {
                Ok(_) => state.sent += 1,
                Err(error) => state.send_error = Some(error.to_string()),
            }
        });
        let streaming = shared
            .link(&daemon)
            .is_some_and(|link| link.streaming.load(Ordering::Acquire));
        drop(shared);
        // A stream brings the message itself; a read here could land after, and
        // show less than the stream has.
        if let (Ok(id), false) = (outcome, streaming) {
            refresh_transcript(&daemon, &target, epoch, &id);
        }
    });
}

/// Put `text` in the outbox of `conversation` on `daemon`, which is away, and
/// count it as taken so the composer clears.
fn hold(shared: &mut Shared, daemon: &str, conversation: &str, text: &str) {
    let queued = offline().queue(daemon, conversation, text, &request_id());
    publish(shared, |state| match queued {
        Some(_) => {
            state.queued = queue_of(&state.watch);
            state.send_error = None;
            state.sent += 1;
        }
        None => {
            state.send_error =
                Some("Too many messages are waiting for it to come back".to_string());
        }
    });
}

/// Give up a message that is waiting for its daemon.
pub fn discard_queued(request: &str) {
    let mut shared = lock();
    let owner = match shared.state.watch.clone() {
        Watch::On(daemon, _) => daemon,
        Watch::Peer(contact) => contact,
        Watch::Off | Watch::Channel(_) => return,
    };
    offline().settle(&owner, request);
    publish(&mut shared, |state| state.queued = queue_of(&state.watch));
}

/// Hand a daemon the messages written while it was away, oldest first, each
/// under the request id it was queued with so a delivery that is cut off and
/// retried is not said twice. It stops at the first message that does not go,
/// to keep them in order, and the next connect tries again.
fn deliver_queued(daemon: &str, target: &Sipil, epoch: u64) {
    let delivering = {
        let shared = lock();
        if !shared.current(daemon, epoch) || offline().outbox(daemon).is_empty() {
            return;
        }
        match shared.link(daemon) {
            Some(link) => link.delivering.clone(),
            None => return,
        }
    };
    if delivering.swap(true, Ordering::AcqRel) {
        return;
    }
    let target = target.clone();
    let daemon = daemon.to_string();
    thread::spawn(move || {
        for queued in offline().outbox(&daemon) {
            let outcome = target.send_message(&queued.conversation, &queued.text, &queued.request);
            let mut shared = lock();
            if !shared.current(&daemon, epoch) {
                break;
            }
            match outcome {
                Ok(()) => {
                    offline().settle(&daemon, &queued.request);
                    publish(&mut shared, |state| state.queued = queue_of(&state.watch));
                }
                // The daemon is away again, slow, or does not know this device
                // yet (the person may allow it): keep the rest, in order.
                Err(error) if error.is_unauthorized() => break,
                Err(SipilError::Refused { .. }) => {
                    // It will not take this one, and waiting will not change that.
                    // It is said, not dropped silently, so the person can write it
                    // again.
                    offline().settle(&daemon, &queued.request);
                    publish(&mut shared, |state| {
                        state.queued = queue_of(&state.watch);
                        state.send_error = Some(format!(
                            "A message written while it was away was not taken: {}",
                            queued.text
                        ));
                    });
                }
                Err(_) => break,
            }
        }
        delivering.store(false, Ordering::Release);
        let shared = lock();
        let streaming = shared
            .link(&daemon)
            .is_some_and(|link| link.streaming.load(Ordering::Acquire));
        let watched = match &shared.state.watch {
            Watch::On(at, id) if *at == daemon => Some(id.clone()),
            _ => None,
        };
        drop(shared);
        // A stream brings the lines itself; otherwise read them now.
        if let (Some(id), false) = (watched, streaming) {
            refresh_transcript(&daemon, &target, epoch, &id);
        }
    });
}

/// Stop the open conversation's current work.
pub fn abort() {
    abort_in(snapshot().watch.clone());
}

/// Stop the current work of `watching`, which need not be the open conversation.
pub fn abort_in(watching: Watch) {
    let Watch::On(daemon, id) = watching else {
        return;
    };
    let (target, epoch) = {
        let shared = lock();
        let Some(found) = shared
            .link(&daemon)
            .and_then(|link| Some((link.sipil.clone()?, link.epoch)))
        else {
            return;
        };
        found
    };
    thread::spawn(move || {
        if let Err(error) = target.abort(&id) {
            let mut shared = lock();
            if shared.current(&daemon, epoch) {
                publish(&mut shared, |state| {
                    state.send_error = Some(error.to_string())
                });
            }
        }
    });
}

/// Read a conversation's transcript and keep it if the page is still on it.
fn refresh_transcript(daemon: &str, target: &Sipil, epoch: u64, id: &str) {
    let outcome = target.transcript_page(id, None);
    let mut shared = lock();
    if !shared.current(daemon, epoch)
        || shared.state.watch != Watch::On(daemon.to_string(), id.to_string())
    {
        return;
    }
    publish(&mut shared, |state| match outcome {
        Ok(page) => {
            show_newest(state, page.items, page.more);
            state.transcript_loaded = true;
            state.watched = page.conversation;
            state.chat_error = None;
        }
        Err(error) => state.chat_error = Some(error.to_string()),
    });
    keep_snapshot(&shared.state, daemon, id);
}

/// Remember the lines the page now shows, for when the daemon is away.
fn keep_snapshot(state: &SipilState, daemon: &str, id: &str) {
    if state.transcript_loaded && state.watch == Watch::On(daemon.to_string(), id.to_string()) {
        offline().remember_transcript(daemon, id, &state.transcript, state.transcript_more);
    }
}

/// Show the newest page, keeping any older lines the person loaded.
fn show_newest(state: &mut SipilState, page: Vec<TranscriptItem>, page_more: bool) {
    let (lines, more) = history::newest(&state.transcript, state.transcript_more, page, page_more);
    state.transcript = lines;
    state.transcript_more = more;
}

/// Load the lines before the first one the open conversation shows. Does
/// nothing if there are none, or if they are already coming.
pub fn load_older() {
    let (daemon, id, before) = {
        let shared = lock();
        let state = &shared.state;
        let (Watch::On(daemon, id), Some(first)) = (&state.watch, state.transcript.first()) else {
            return;
        };
        if !state.transcript_more || state.loading_older {
            return;
        }
        (daemon.clone(), id.clone(), first.id)
    };
    let (target, epoch) = {
        let mut shared = lock();
        let Some(found) = shared
            .link(&daemon)
            .and_then(|link| Some((link.sipil.clone()?, link.epoch)))
        else {
            return;
        };
        publish(&mut shared, |state| state.loading_older = true);
        found
    };
    thread::spawn(move || {
        let outcome = target.transcript_page(&id, Some(before));
        let mut shared = lock();
        if !shared.current(&daemon, epoch)
            || shared.state.watch != Watch::On(daemon.clone(), id.clone())
        {
            return;
        }
        publish(&mut shared, |state| {
            state.loading_older = false;
            match outcome {
                Ok(page) => {
                    let (lines, more) = history::older(&state.transcript, page.items, page.more);
                    state.transcript = lines;
                    state.transcript_more = more;
                }
                Err(error) => state.chat_error = Some(error.to_string()),
            }
        });
    });
}

/// Keep the open conversation up to date from a stream, for as long as the
/// window is on it.
///
/// The stream is opened again when it breaks. A daemon from before `watch`, or one
/// that refuses it, is read once and left to the poller, which also covers the
/// moments between streams.
fn stream_conversation(
    daemon: &str,
    target: &Sipil,
    epoch: u64,
    id: &str,
    generation: u64,
    streaming: &AtomicBool,
) {
    let open =
        || WATCH_GENERATION.load(Ordering::Acquire) == generation && lock().current(daemon, epoch);
    while open() {
        let mut watching = match target.watch(id) {
            Ok(watching) => watching,
            Err(_) => {
                pause_between_streams(&open);
                continue;
            }
        };
        let mut watched = Watched::default();
        let mut heard = Instant::now();
        let mut delivered = false;
        let refused = loop {
            if !open() {
                streaming.store(false, Ordering::Release);
                return;
            }
            match watching.next(Duration::from_secs(1)) {
                Ok(Next::Event(event)) => {
                    heard = Instant::now();
                    delivered = true;
                    watched.apply(*event);
                    if show_watched(daemon, epoch, id, generation, &watched) {
                        streaming.store(true, Ordering::Release);
                    }
                }
                Ok(Next::Idle) if heard.elapsed() > STREAM_SILENCE => break None,
                Ok(Next::Idle) => {}
                // A node that finishes without having said anything is not
                // streaming; asking again would only get the same.
                Ok(Next::Ended) if !delivered => {
                    streaming.store(false, Ordering::Release);
                    refresh_transcript(daemon, target, epoch, id);
                    return;
                }
                Ok(Next::Ended) => break None,
                Err(error) if matches!(error, SipilError::Refused { .. }) => break Some(error),
                Err(_) => break None,
            }
        };
        streaming.store(false, Ordering::Release);
        if let Some(error) = refused {
            // This daemon will not stream this conversation: read it, and say why
            // if even that fails. The poller keeps it current from here.
            if !Watching::unsupported(&error) {
                let mut shared = lock();
                if open_still(&shared, daemon, epoch, generation) {
                    publish(&mut shared, |state| {
                        state.chat_error = Some(error.to_string())
                    });
                }
                return;
            }
            refresh_transcript(daemon, target, epoch, id);
            return;
        }
        pause_between_streams(&open);
    }
}

/// Wait out `STREAM_RETRY`, a second at a time, so leaving the conversation ends
/// the thread promptly.
fn pause_between_streams(open: &impl Fn() -> bool) {
    let until = Instant::now() + STREAM_RETRY;
    while Instant::now() < until && open() {
        thread::sleep(Duration::from_millis(250));
    }
}

fn open_still(shared: &Shared, daemon: &str, epoch: u64, generation: u64) -> bool {
    WATCH_GENERATION.load(Ordering::Acquire) == generation && shared.current(daemon, epoch)
}

/// Put what a stream has told so far on show, if the window is still on it.
/// Whether it was.
fn show_watched(daemon: &str, epoch: u64, id: &str, generation: u64, watched: &Watched) -> bool {
    let mut shared = lock();
    if !open_still(&shared, daemon, epoch, generation)
        || shared.state.watch != Watch::On(daemon.to_string(), id.to_string())
    {
        return false;
    }
    publish(&mut shared, |state| {
        show_newest(state, watched.items.clone(), watched.more);
        state.transcript_loaded = true;
        state.live.clone_from(&watched.live);
        state.running.clone_from(&watched.tools);
        state.watched.clone_from(&watched.info);
        state.chat_error = None;
    });
    keep_snapshot(&shared.state, daemon, id);
    true
}

/// `text` with the line it answers in front, when it answers one.
fn quoted(text: String, reply: Option<&Quote>) -> String {
    match reply {
        Some(quote) => quote.prefix(&text),
        None => text,
    }
}

/// A fresh id for each message, so a retried request is recognisable to the
/// other side and a second message never is mistaken for one.
pub(super) fn request_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("pilus-{nanos}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Take a daemon that answered: load its lists and fold them in.
fn adopt(id: &str, target: &Sipil, epoch: u64, health: &pilus_client::sipil::Health) {
    let lists = Lists::fetch(target, true, true);
    let mut shared = lock();
    if !shared.current(id, epoch) {
        return;
    }
    settle(&mut shared, id, health, lists);
    let connected = shared.state.daemon(id).is_some_and(Daemon::connected);
    drop(shared);
    share_daemons();
    if connected {
        deliver_queued(id, target, epoch);
    }
}

/// Fold a daemon's answer into the state.
///
/// A daemon that answers its health check but fails a list is not one to call
/// connected, so the sidebar does not show half of it.
fn settle(shared: &mut Shared, id: &str, health: &pilus_client::sipil::Health, lists: Lists) {
    let failure = lists.failure();
    let refused = failure.clone();
    let version = health.version.clone();
    let label = health.label.clone().unwrap_or_default();
    publish(shared, |state| {
        let Some(daemon) = state.daemons.iter_mut().find(|daemon| daemon.id == id) else {
            return;
        };
        match failure {
            None => {
                daemon.phase = Phase::Connected;
                daemon.version = Some(version);
                daemon.label = label;
                daemon.error = None;
                lists.apply(daemon);
                if let Some(agent) = daemon
                    .agents
                    .iter()
                    .find(|agent| agent.role == Role::Primary)
                {
                    offline().remember_agent(
                        id,
                        &daemon.label,
                        RememberedAgent {
                            id: agent.id.clone(),
                            name: agent.name.clone(),
                        },
                    );
                }
            }
            Some(error) => {
                daemon.phase = Phase::Connecting;
                daemon.error = Some(error);
                // It may come back as a restarted daemon with other keys and
                // another model, so it is asked again.
                daemon.about = None;
                daemon.about_unsupported = false;
                daemon.board_unsupported = false;
            }
        }
    });
    match refused {
        None => {
            if let Some(link) = shared.link_mut(id) {
                link.backoff.succeeded();
            }
        }
        Some(error) => failed(shared, id, &error),
    }
}

/// The agent's board and its routines, as the daemon answered each.
type Board = (
    Result<WorkBoard, SipilError>,
    Result<Vec<Routine>, SipilError>,
);

/// Everything the sidebar and the window list, fetched together.
struct Lists {
    agents: Result<Vec<Agent>, SipilError>,
    sessions: Result<Vec<Session>, SipilError>,
    /// What the daemon says about itself, asked for when it has not yet. It is
    /// not a reason to call the daemon down: an older one has no such call.
    about: Option<Result<About, SipilError>>,
    /// The agent's board and routines, asked for unless the daemon is too old
    /// to have them. Like `about`, a failure here never calls the daemon down.
    board: Option<Board>,
}

impl Lists {
    fn fetch(target: &Sipil, with_about: bool, with_board: bool) -> Self {
        Self {
            agents: target.agents(),
            sessions: target.sessions(),
            about: with_about.then(|| target.about()),
            board: with_board.then(|| (target.work(), target.routines())),
        }
    }

    /// The first failure, if any call failed: a daemon that answers one call and
    /// not the next is not one to call connected.
    fn failure(&self) -> Option<String> {
        self.agents
            .as_ref()
            .err()
            .or(self.sessions.as_ref().err())
            .map(ToString::to_string)
    }

    fn apply(self, daemon: &mut Daemon) {
        if let Ok(agents) = self.agents {
            daemon.agents = agents;
        }
        if let Ok(sessions) = self.sessions {
            daemon.sessions = sessions;
        }
        match self.about {
            Some(Ok(about)) => {
                daemon.logo = about
                    .logo
                    .as_deref()
                    .filter(|svg| svg.len() <= LOGO_MAX_BYTES)
                    .and_then(|svg| rasterize(svg.as_bytes(), LOGO_EXTENT))
                    .map(Logo);
                daemon.about = Some(about);
            }
            Some(Err(SipilError::Refused { code, .. })) if code == "unknown_method" => {
                daemon.about_unsupported = true;
            }
            Some(Err(_)) | None => {}
        }
        match self.board {
            Some((Err(SipilError::Refused { code, .. }), _)) if code == "unknown_method" => {
                daemon.board_unsupported = true;
            }
            Some((work, routines)) => {
                if let Ok(work) = work {
                    daemon.work = Some(work);
                }
                if let Ok(routines) = routines {
                    daemon.routines = routines;
                }
            }
            None => {}
        }
    }
}

fn start_poller() {
    POLLER.get_or_init(|| {
        thread::Builder::new()
            .name("pilus-sipil".to_string())
            .spawn(poll_forever)
            .ok();
    });
}

/// Set when something happened that the poller should not wait a tick to see.
static NUDGE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// Wake the poller now. A message arrived, a contact called, the app came
/// forward, or the person asked: it would find out at its next tick, but not
/// before.
pub(super) fn nudge() {
    let (flag, wake) = &NUDGE;
    *flag.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
    wake.notify_one();
}

/// Wait up to `limit`, or until nudged.
fn rest(limit: Duration) {
    let (flag, wake) = &NUDGE;
    let mut nudged = flag.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !*nudged {
        nudged = wake
            .wait_timeout(nudged, limit)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0;
    }
    *nudged = false;
}

/// The shortest the poller sleeps, so a deadline that has just passed cannot
/// turn the loop into a spin.
const MIN_REST: Duration = Duration::from_millis(100);

fn poll_forever() {
    let mut forward = activity::front_epoch();
    loop {
        rest(next_wake(Instant::now()));
        super::perf::count(super::perf::Count::SipilWake);
        let now = Instant::now();
        let came = activity::front_epoch();
        if came != forward {
            forward = came;
            catch_up(now);
        }
        let front = activity::in_front();
        social::sync(now, front);
        poll_daemons(now, front);
        offline().flush();
    }
}

/// How long the poller may sleep: until the app-wide tick, or a daemon's turn
/// if that comes first.
fn next_wake(now: Instant) -> Duration {
    let tick = if activity::in_front() {
        TICK
    } else {
        IDLE_TICK
    };
    let shared = lock();
    let soonest = shared
        .links
        .iter()
        // A daemon being polled now reports back by nudging, so it is not a
        // reason to wake before then.
        .filter(|link| link.sipil.is_some() && !link.polling.load(Ordering::Acquire))
        .filter_map(|link| {
            if shared.state.daemon(&link.id).is_some_and(Daemon::connected) {
                Some(link.next_poll.saturating_duration_since(now))
            } else {
                link.backoff.wait(now)
            }
        })
        .min();
    soonest.map_or(tick, |wait| wait.min(tick)).max(MIN_REST)
}

/// The app has come to the front: read what was left to go stale, and try the
/// daemons that were given up on once more, if they have rested.
fn catch_up(now: Instant) {
    let mut shared = lock();
    let mut woke = Vec::new();
    for link in &mut shared.links {
        link.next_poll = now;
        let was = link.backoff.given_up();
        link.backoff.wake_if_rested(now, FORWARD_REST);
        if was && !link.backoff.given_up() {
            woke.push(link.id.clone());
        }
    }
    for id in woke {
        trying_again(&mut shared, &id);
    }
    drop(shared);
    social::catch_up(now);
}

/// How long to leave a connected daemon alone before asking again.
fn poll_every(open: bool, front: bool) -> Duration {
    let base = if open { OPEN_EVERY } else { LIST_EVERY };
    if front { base } else { base * BEHIND_FACTOR }
}

/// A daemon to poll: its ID, connection, attempt, open conversation, and the
/// flag that keeps one poll running at a time.
type Due = (String, Sipil, u64, Option<String>, Arc<AtomicBool>);

/// One round of keeping up with each daemon. A daemon that is slow to answer
/// holds up only its own poll. One that is up is asked for its lists every few
/// seconds; one that is not is dialed on its backoff, which stretches after each
/// failure and ends in giving up.
fn poll_daemons(now: Instant, front: bool) {
    let due: Vec<Due> = {
        let mut shared = lock();
        let Shared { links, state, .. } = &mut *shared;
        links
            .iter_mut()
            .filter_map(|link| {
                let target = link.sipil.clone()?;
                let up = state.daemon(&link.id).is_some_and(Daemon::connected);
                // A conversation a stream is delivering is not read here.
                let open = match &state.watch {
                    Watch::On(daemon, id)
                        if *daemon == link.id && !link.streaming.load(Ordering::Acquire) =>
                    {
                        Some(id.clone())
                    }
                    _ => None,
                };
                let ready = if up {
                    now >= link.next_poll
                } else {
                    link.backoff.due(now)
                };
                ready.then(|| {
                    link.next_poll = now + poll_every(open.is_some(), front);
                    (
                        link.id.clone(),
                        target,
                        link.epoch,
                        open,
                        link.polling.clone(),
                    )
                })
            })
            .collect()
    };
    for (id, target, epoch, open, polling) in due {
        if polling.swap(true, Ordering::AcqRel) {
            continue;
        }
        thread::spawn(move || {
            poll_daemon(&id, &target, epoch, open);
            polling.store(false, Ordering::Release);
            nudge();
        });
    }
}

fn poll_daemon(id: &str, target: &Sipil, epoch: u64, open: Option<String>) {
    // A reconnect starts from the health check, so a daemon that came back as
    // something else is caught rather than trusted.
    let (with_about, with_board) = lock().state.daemon(id).map_or((false, false), |daemon| {
        (
            daemon.about.is_none() && !daemon.about_unsupported,
            !daemon.board_unsupported,
        )
    });
    let outcome = target
        .connect()
        .map(|health| (health, Lists::fetch(target, with_about, with_board)));
    let connected = {
        let mut shared = lock();
        if !shared.current(id, epoch) {
            return;
        }
        match outcome {
            Ok((health, lists)) => settle(&mut shared, id, &health, lists),
            Err(error) => failed(&mut shared, id, &error.to_string()),
        }
        shared.state.daemon(id).is_some_and(Daemon::connected)
    };
    // The link follows the connection through drops and returns.
    share_daemons();
    if connected {
        deliver_queued(id, target, epoch);
    }
    if let Some(conversation) = open.filter(|_| connected) {
        refresh_transcript(id, target, epoch, &conversation);
    }
}

/// A daemon did not answer: say so, and step its backoff. When that was the
/// last attempt, stop calling it connecting and say it will not be tried again
/// until something suggests it is back.
fn failed(shared: &mut Shared, id: &str, error: &str) {
    publish(shared, |state| {
        if let Some(daemon) = state.daemons.iter_mut().find(|daemon| daemon.id == id) {
            daemon.phase = Phase::Connecting;
            daemon.error = Some(error.to_string());
        }
    });
    let gave_up = shared
        .link_mut(id)
        .is_some_and(|link| link.backoff.failed(Instant::now()));
    if gave_up {
        publish(shared, |state| {
            if let Some(daemon) = state.daemons.iter_mut().find(|daemon| daemon.id == id) {
                daemon.phase = Phase::Off;
                daemon.error = Some(format!(
                    "{error}. Pilus has stopped retrying; it tries again when this device calls, or when you open it."
                ));
            }
        });
    }
}

/// Show a daemon that was given up on as trying again.
fn trying_again(shared: &mut Shared, id: &str) {
    publish(shared, |state| {
        if let Some(daemon) = state.daemons.iter_mut().find(|daemon| daemon.id == id)
            && daemon.phase == Phase::Off
        {
            daemon.phase = Phase::Connecting;
            // The reason it was given up on is no longer the news.
            daemon.error = None;
        }
    });
}

/// Try a daemon again now, because something says it may be there: the person
/// opened it or wrote to it, or it called.
pub(super) fn wake_daemon(id: &str) {
    {
        let mut shared = lock();
        wake_link(&mut shared, id);
    }
    nudge();
}

/// Try every daemon that is not connected again now.
pub fn wake_daemons() {
    {
        let mut shared = lock();
        let ids: Vec<String> = shared.links.iter().map(|link| link.id.clone()).collect();
        for id in ids {
            wake_link(&mut shared, &id);
        }
    }
    nudge();
}

fn wake_link(shared: &mut Shared, id: &str) {
    let up = shared.state.daemon(id).is_some_and(Daemon::connected);
    let Some(link) = shared.link_mut(id) else {
        return;
    };
    link.next_poll = Instant::now();
    if up {
        return;
    }
    let was = link.backoff.given_up();
    link.backoff.wake();
    if was {
        trying_again(shared, id);
    }
}

/// Whether `id` is a daemon this window is connected to or trying to be.
pub(super) fn is_daemon(id: &str) -> bool {
    lock().link(id).is_some()
}

/// Make `sipil` the connected daemon `id`, for the tests: the same as a connect
/// that succeeded.
#[cfg(test)]
fn attach(id: &str, sipil: Sipil) -> u64 {
    let epoch = begin(id, id);
    let mut shared = lock();
    if let Some(link) = shared.links.iter_mut().find(|link| link.id == id) {
        link.sipil = Some(sipil);
    }
    publish(&mut shared, |state| {
        if let Some(daemon) = state.daemons.iter_mut().find(|daemon| daemon.id == id) {
            daemon.phase = Phase::Connected;
        }
    });
    epoch
}

/// Show `state` as if the daemons had reported it, for the visual-test shots.
/// The poller never runs, so nothing overwrites it.
#[cfg(feature = "visual-test")]
pub fn preview(state: SipilState) {
    let mut shared = lock();
    shared.links.clear();
    shared.state = Arc::new(state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pilus_client::sipil::node::Service;
    use pilus_client::sipil::{Failure, Node, NodeConfig, RelayChoice};

    /// The state is process-wide, so the tests that drive it take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn take_turn() -> MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Forget every daemon and conversation.
    fn reset() {
        let mut shared = lock();
        shared.links.clear();
        shared.state = Arc::default();
    }

    #[test]
    fn a_daemon_has_a_logo_only_when_it_advertised_one() {
        let mut daemon = Daemon::default();
        assert!(daemon.own_logo().is_none());
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><rect width="10" height="10" fill="#0f0"/></svg>"##;
        daemon.logo = rasterize(svg.as_bytes(), LOGO_EXTENT).map(Logo);
        assert!(daemon.own_logo().is_some());
    }

    #[test]
    fn what_a_daemon_says_about_itself_is_kept_and_an_older_daemon_is_not_asked_again() {
        let about = About {
            name: "CoS".to_string(),
            logo: Some("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 1 1\"><rect width=\"1\" height=\"1\"/></svg>".to_string()),
            ..About::default()
        };
        let lists = |about| Lists {
            agents: Ok(Vec::new()),
            sessions: Ok(Vec::new()),
            about,
            board: None,
        };
        let mut daemon = Daemon::default();
        lists(Some(Ok(about))).apply(&mut daemon);
        assert_eq!(daemon.about.as_ref().map(|a| a.name.as_str()), Some("CoS"));
        assert!(daemon.logo.is_some(), "the advertised SVG is drawn");

        let mut old = Daemon::default();
        let unknown = SipilError::Refused {
            code: "unknown_method".to_string(),
            message: String::new(),
        };
        lists(Some(Err(unknown))).apply(&mut old);
        assert!(old.about.is_none() && old.about_unsupported);
        // A failure that is not that one is just tried again.
        let mut flaky = Daemon::default();
        lists(Some(Err(SipilError::Malformed))).apply(&mut flaky);
        assert!(flaky.about.is_none() && !flaky.about_unsupported);
    }

    #[test]
    fn the_board_is_kept_from_a_daemon_that_has_one_and_not_asked_of_one_that_does_not() {
        let lists = |board| Lists {
            agents: Ok(Vec::new()),
            sessions: Ok(Vec::new()),
            about: None,
            board: Some(board),
        };
        let item = pilus_client::sipil::WorkItem {
            id: 1,
            title: "Rotate the keys".to_string(),
            state: "blocked".to_string(),
            ..Default::default()
        };
        let mut daemon = Daemon::default();
        lists((
            Ok(WorkBoard {
                open: vec![item.clone()],
                closed: Vec::new(),
            }),
            Ok(vec![Routine {
                id: 1,
                name: "PR check".to_string(),
                ..Default::default()
            }]),
        ))
        .apply(&mut daemon);
        assert_eq!(
            daemon.work.as_ref().map(|w| w.open.clone()),
            Some(vec![item])
        );
        assert_eq!(daemon.routines.len(), 1);

        // A failed read keeps the last good board.
        lists((Err(SipilError::Malformed), Err(SipilError::Malformed))).apply(&mut daemon);
        assert_eq!(daemon.work.as_ref().map(|w| w.open.len()), Some(1));
        assert_eq!(daemon.routines.len(), 1);
        assert!(!daemon.board_unsupported);

        // A daemon from before the board says so once, and is not asked again.
        let mut old = Daemon::default();
        let unknown = || SipilError::Refused {
            code: "unknown_method".to_string(),
            message: String::new(),
        };
        lists((Err(unknown()), Err(unknown()))).apply(&mut old);
        assert!(old.board_unsupported && old.work.is_none());
    }

    #[test]
    fn an_unchanged_state_keeps_the_same_snapshot() {
        let mut shared = Shared::default();
        let before = shared.state.clone();
        publish(&mut shared, |_| {});
        assert!(Arc::ptr_eq(&before, &shared.state));
        publish(&mut shared, |state| state.sent += 1);
        assert!(!Arc::ptr_eq(&before, &shared.state));
    }

    /// A daemon that answers `health` and then each of `replies` in turn, and
    /// remembers the methods it was asked.
    #[derive(Debug)]
    struct Script {
        replies: Mutex<std::collections::VecDeque<Result<serde_json::Value, Failure>>>,
        seen: Mutex<Vec<String>>,
    }

    impl Service for Script {
        fn handle(
            &self,
            _from: &str,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<serde_json::Value, Failure> {
            if method == "health" {
                return Ok(serde_json::json!(
                    {"name": "sipil", "kind": "daemon", "label": "Researcher", "version": "t", "api": 2, "allowed": true}
                ));
            }
            self.seen.lock().expect("seen").push(method.to_string());
            self.replies
                .lock()
                .expect("replies")
                .pop_front()
                .unwrap_or_else(|| Err(Failure::new("script_ended", "No reply was scripted.")))
        }
    }

    #[derive(Debug)]
    struct Deaf;

    impl Service for Deaf {
        fn handle(
            &self,
            _: &str,
            _: &str,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, Failure> {
            Err(Failure::new("deaf", "not listening"))
        }
    }

    fn node(name: &str, service: Arc<dyn Service>) -> Arc<Node> {
        let dir =
            std::env::temp_dir().join(format!("pilus-gui-sipil-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Node::start(
            &NodeConfig {
                dir,
                relay: RelayChoice::Disabled,
            },
            service,
        )
        .expect("a node")
    }

    /// A client for a daemon that replies as scripted, and the methods it saw.
    fn scripted(
        name: &str,
        replies: Vec<Result<serde_json::Value, Failure>>,
    ) -> (Sipil, Arc<Script>, Arc<Node>) {
        let script = Arc::new(Script {
            replies: Mutex::new(replies.into()),
            seen: Mutex::new(Vec::new()),
        });
        let server = node(&format!("{name}-daemon"), script.clone());
        let client = node(&format!("{name}-client"), Arc::new(Deaf));
        let sipil = Sipil::with_node(client, &server.ticket()).expect("a ticket");
        (sipil, script, server)
    }

    fn wait_until(what: &str, done: impl Fn(&SipilState) -> bool) -> Arc<SipilState> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let state = snapshot();
            if done(&state) {
                return state;
            }
            assert!(Instant::now() < deadline, "{what}: {state:?}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_message_sipil_refuses_stays_to_be_retried() {
        let _turn = take_turn();
        let (sipil, _script, _server) = scripted(
            "refused",
            vec![Err(Failure::new(
                "no_provider",
                "Set OPENCODE_API_KEY in the environment sipil starts in.",
            ))],
        );
        reset();
        let daemon = sipil.target().id();
        attach(&daemon, sipil);
        // Set directly: `watch` would also read the transcript, which this
        // peer is not scripted to answer.
        update(|state| state.watch = Watch::On(daemon.clone(), "1".to_string()));
        send("hi".to_string(), None);
        let state = wait_until("no error arrived", |state| state.send_error.is_some());
        assert_eq!(
            state.send_error.as_deref(),
            Some("Set OPENCODE_API_KEY in the environment sipil starts in.")
        );
        // Nothing was taken, so the page has no reason to clear its composer.
        assert_eq!(state.sent, 0);
        assert!(!state.sending);
        reset();
    }

    #[test]
    fn a_daemon_that_never_answers_is_given_up_on_and_woken_by_the_person() {
        let _turn = take_turn();
        reset();
        begin("away", "away");
        let mut shared = lock();
        for failure in 1..Policy::DAEMON.attempts {
            failed(&mut shared, "away", "no answer");
            let daemon = shared.state.daemon("away").expect("listed");
            assert_eq!(daemon.phase, Phase::Connecting, "failure {failure}");
        }
        failed(&mut shared, "away", "no answer");
        let daemon = shared.state.daemon("away").expect("listed");
        assert_eq!(daemon.phase, Phase::Off);
        let note = daemon.error.clone().unwrap_or_default();
        assert!(
            note.starts_with("no answer.") && note.contains("stopped retrying"),
            "{note}"
        );
        let later = Instant::now() + Duration::from_secs(24 * 3600);
        assert!(
            !shared
                .link("away")
                .is_some_and(|link| link.backoff.due(later)),
            "a day later it is still left alone"
        );

        // Opening it is a reason to ask once more, and it says so.
        wake_link(&mut shared, "away");
        let daemon = shared.state.daemon("away").expect("listed");
        assert_eq!(
            (daemon.phase, daemon.error.as_deref()),
            (Phase::Connecting, None)
        );
        assert!(
            shared
                .link("away")
                .is_some_and(|link| link.backoff.due(Instant::now()))
        );
        // One more failure puts it back to rest rather than starting the ladder over.
        failed(&mut shared, "away", "still no answer");
        assert_eq!(
            shared.state.daemon("away").map(|daemon| daemon.phase),
            Some(Phase::Off)
        );
        drop(shared);
        reset();
    }

    #[test]
    fn a_daemon_that_answers_starts_the_ladder_over() {
        let _turn = take_turn();
        reset();
        begin("flaky", "flaky");
        let mut shared = lock();
        for _ in 0..3 {
            failed(&mut shared, "flaky", "no answer");
        }
        let failures = |shared: &Shared| shared.link("flaky").map(|link| link.backoff.failures());
        assert_eq!(failures(&shared), Some(3));
        let health = pilus_client::sipil::Health {
            name: "sipil".to_string(),
            kind: pilus_client::sipil::NodeKind::Daemon,
            version: "0".to_string(),
            api: pilus_client::sipil::API_VERSION,
            allowed: true,
            label: None,
            nickname: None,
        };
        let lists = Lists {
            agents: Ok(Vec::new()),
            sessions: Ok(Vec::new()),
            about: None,
            board: None,
        };
        settle(&mut shared, "flaky", &health, lists);
        assert_eq!(failures(&shared), Some(0));
        assert!(shared.state.daemon("flaky").is_some_and(Daemon::connected));
        drop(shared);
        reset();
    }

    #[test]
    fn the_poller_sleeps_to_the_next_deadline_and_not_to_a_daemon_it_is_already_asking() {
        let _turn = take_turn();
        reset();
        let (sipil, _script, _server) = scripted("sleepy", vec![]);
        let daemon = sipil.target().id();
        attach(&daemon, sipil);
        let now = Instant::now();
        {
            let mut shared = lock();
            let link = shared.link_mut(&daemon).expect("attached");
            link.next_poll = now + Duration::from_millis(400);
        }
        let wake = next_wake(now);
        assert!(
            wake >= MIN_REST && wake <= Duration::from_millis(400),
            "{wake:?}: a connected daemon's turn comes before the tick"
        );
        {
            let mut shared = lock();
            let link = shared.link_mut(&daemon).expect("attached");
            link.next_poll = now;
            link.polling.store(true, Ordering::Release);
        }
        let wake = next_wake(now);
        assert!(
            wake >= IDLE_TICK.min(TICK),
            "{wake:?}: a poll that is under way reports back by itself"
        );
        reset();
    }

    #[test]
    fn a_connected_daemon_is_asked_less_often_behind_another_app() {
        assert_eq!(poll_every(false, true), LIST_EVERY);
        assert_eq!(poll_every(true, true), OPEN_EVERY);
        assert_eq!(poll_every(false, false), LIST_EVERY * BEHIND_FACTOR);
        assert!(poll_every(true, false) > poll_every(true, true));
    }

    #[test]
    fn each_daemon_keeps_its_own_lists_and_one_going_away_leaves_the_other() {
        let _turn = take_turn();
        reset();
        let (a, _sa, _na) = scripted("many-a", vec![]);
        let (b, _sb, _nb) = scripted("many-b", vec![]);
        let (ida, idb) = (a.target().id(), b.target().id());
        attach(&ida, a);
        attach(&idb, b);
        update_daemon(&ida, |daemon| daemon.label = "Researcher".to_string());
        update_daemon(&idb, |daemon| daemon.label = "CoS".to_string());
        let state = snapshot();
        assert_eq!(
            state
                .mentionable()
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>(),
            ["Researcher", "CoS"]
        );
        update(|state| state.watch = Watch::On(ida.clone(), "1".to_string()));
        disconnect(&ida);
        let state = snapshot();
        assert_eq!(state.daemons.len(), 1);
        assert_eq!(state.daemons[0].name(), "CoS");
        assert_eq!(
            state.watch,
            Watch::Off,
            "a page open on the daemon that left closes"
        );
        reset();
    }

    #[test]
    fn a_request_id_is_never_reused() {
        let ids: std::collections::BTreeSet<_> = (0..50).map(|_| request_id()).collect();
        assert_eq!(ids.len(), 50);
    }

    #[test]
    fn a_list_that_failed_leaves_the_last_good_one() {
        let mut daemon = Daemon {
            sessions: vec![Session {
                id: "7".to_string(),
                title: "kept".to_string(),
                status: Activity::Idle,
                model: None,
                error: None,
            }],
            ..Daemon::default()
        };
        let lists = Lists {
            about: None,
            board: None,
            agents: Ok(vec![Agent {
                id: "1".to_string(),
                name: "sipil".to_string(),
                role: Role::Primary,
                status: Activity::Idle,
                model: None,
                error: None,
            }]),
            sessions: Err(SipilError::Malformed),
        };
        assert!(lists.failure().is_some());
        lists.apply(&mut daemon);
        assert_eq!(daemon.agents.len(), 1);
        assert_eq!(daemon.sessions[0].title, "kept");
    }

    #[test]
    fn unread_lines_add_up_across_contacts_and_channels() {
        let person = |unread| Person {
            contact: pilus_client::sipil::Contact {
                id: "a".repeat(64),
                name: "A".to_string(),
                ticket: None,
                added: 0,
                daemon: false,
            },
            reach: Reach::Online,
            unread,
        };
        let state = SipilState {
            me: Me {
                people: vec![person(2), person(3)],
                channels: vec![ChannelRow {
                    channel: Channel {
                        id: "0123456789abcdef".to_string(),
                        name: "x".to_string(),
                        members: Vec::new(),
                        owner: String::new(),
                        created: 0,
                    },
                    unread: 4,
                    agents: Vec::new(),
                }],
                ..Me::default()
            },
            ..SipilState::default()
        };
        assert_eq!(state.unread(), 9);
    }

    #[test]
    fn only_a_connected_daemon_shows_sections() {
        let mut daemon = Daemon::default();
        assert!(!daemon.connected());
        daemon.phase = Phase::Connecting;
        assert!(!daemon.connected());
        daemon.phase = Phase::Connected;
        assert!(daemon.connected());
        assert_eq!(
            daemon.name(),
            "",
            "an unnamed daemon with no id has no name yet"
        );
        daemon.id = "26e9ba62c5fedd08".to_string();
        assert_eq!(daemon.name(), "26e9ba62");
        daemon.label = "CoS".to_string();
        assert_eq!(daemon.name(), "CoS");
        daemon.alias = "Chief".to_string();
        assert_eq!(daemon.name(), "Chief", "the name given here wins");
    }

    #[test]
    fn a_stream_shows_only_while_the_window_is_still_on_its_conversation() {
        let _turn = take_turn();
        reset();
        let epoch = begin("d1", "d1");
        publish(&mut lock(), |state| {
            state.watch = Watch::On("d1".to_string(), "7".to_string());
        });
        let generation = WATCH_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
        let mut watched = Watched {
            live: "Hel".to_string(),
            tools: vec![RunningTool {
                call_id: "c1".to_string(),
                name: "bash".to_string(),
                output: "one\n".to_string(),
            }],
            ..Watched::default()
        };
        assert!(show_watched("d1", epoch, "7", generation, &watched));
        let shown = snapshot();
        assert_eq!(
            (shown.live.as_str(), shown.transcript_loaded),
            ("Hel", true)
        );
        assert_eq!(
            shown.running.len(),
            1,
            "what a running call printed is shown"
        );

        // Another conversation is open now, or the same one was reopened (a new
        // generation), or the daemon was reconnected (a new epoch): what the old
        // stream still says is not shown.
        watched.live = "stale".to_string();
        assert!(!show_watched("d1", epoch, "8", generation, &watched));
        assert!(!show_watched("d1", epoch + 1, "7", generation, &watched));
        WATCH_GENERATION.fetch_add(1, Ordering::AcqRel);
        assert!(!show_watched("d1", epoch, "7", generation, &watched));
        assert_eq!(snapshot().live, "Hel");
        reset();
    }

    /// Kills the daemon process when the test ends.
    struct Reaper(std::process::Child);

    impl Drop for Reaper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn the_open_conversation_follows_a_real_daemon_by_stream_and_the_poller_steps_aside() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        let checkout = std::env::var_os("SIPIL_DIR").map_or_else(
            || std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../sipil"),
            std::path::PathBuf::from,
        );
        let usable = checkout.join("node_modules/@number0/iroh").exists()
            && Command::new("node")
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success());
        if !usable {
            eprintln!("skipped: no sipil checkout with node_modules beside this repository");
            return;
        }
        let _turn = take_turn();
        let home =
            std::env::temp_dir().join(format!("pilus-gui-stream-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let mut child = Command::new("node")
            .arg("src/main.ts")
            .current_dir(&checkout)
            .env("SIPIL_HOME", &home)
            .env("SIPIL_CWD", &home)
            .env("SIPIL_RELAY", "off")
            .env_remove("OPENCODE_API_KEY")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start sipil");
        let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let _reaper = Reaper(child);
        let ticket = loop {
            match lines.next() {
                Some(Ok(line)) => {
                    if let Some(ticket) = line.strip_prefix("ticket:") {
                        break ticket.trim().to_string();
                    }
                }
                _ => panic!("sipil exited before printing its ticket"),
            }
        };
        thread::spawn(move || lines.for_each(drop));

        let client = node("stream-client", Arc::new(Deaf));
        let allowed = Command::new("node")
            .args(["src/main.ts", "allow", client.id(), "test"])
            .current_dir(&checkout)
            .env("SIPIL_HOME", &home)
            .output()
            .expect("run allow");
        assert!(allowed.status.success(), "{allowed:?}");

        // The daemon's ticket lists loopback, so a test machine reaches it directly.
        let sipil = Sipil::with_node(client, &ticket).expect("a target");
        let session = sipil.create_session().expect("a session");
        reset();
        let daemon = sipil.target().id();
        attach(&daemon, sipil);
        watch(Watch::On(daemon.clone(), session.id.clone()));

        // The stream's snapshot is what loads the conversation, and while it
        // delivers, the poller is told to leave the transcript alone.
        let streaming = |daemon: &str| {
            lock()
                .link(daemon)
                .is_some_and(|link| link.streaming.load(Ordering::Acquire))
        };
        let state = wait_until("the snapshot", |state| state.transcript_loaded);
        assert_eq!(
            state.watched.as_ref().map(|info| info.id.as_str()),
            Some(session.id.as_str())
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !streaming(&daemon) {
            assert!(Instant::now() < deadline, "the stream never reported in");
            thread::sleep(Duration::from_millis(20));
        }

        // Leaving the conversation ends its stream.
        watch(Watch::Off);
        let deadline = Instant::now() + Duration::from_secs(10);
        while streaming(&daemon) {
            assert!(
                Instant::now() < deadline,
                "the stream outlived its conversation"
            );
            thread::sleep(Duration::from_millis(20));
        }
        reset();
    }

    fn said(id: u64) -> TranscriptItem {
        serde_json::from_value(serde_json::json!(
            {"id": id, "role": "user", "text": format!("m{id}"), "tools": []}
        ))
        .expect("a line")
    }

    fn ids(state: &SipilState) -> Vec<u64> {
        state.transcript.iter().map(|item| item.id).collect()
    }

    #[test]
    fn older_lines_are_loaded_in_front_and_only_while_there_are_some() {
        let _turn = take_turn();
        let page = serde_json::json!({
            "items": [said(3), said(4)], "more": true
        });
        let (sipil, script, _server) = scripted("older", vec![Ok(page)]);
        reset();
        let daemon = sipil.target().id();
        attach(&daemon, sipil);
        publish(&mut lock(), |state| {
            state.watch = Watch::On(daemon.clone(), "7".to_string());
            state.transcript = vec![said(5), said(6)];
            state.transcript_loaded = true;
        });

        // Nothing is said to lie before the first line, so nothing is asked.
        load_older();
        thread::sleep(Duration::from_millis(200));
        assert!(script.seen.lock().expect("seen").is_empty());

        publish(&mut lock(), |state| state.transcript_more = true);
        load_older();
        let state = wait_until("the older lines", |state| state.transcript.len() == 4);
        assert_eq!(ids(&state), [3, 4, 5, 6]);
        assert!(state.transcript_more && !state.loading_older);
        assert_eq!(*script.seen.lock().expect("seen"), ["transcript"]);

        // A newer page does not throw the older lines away.
        publish(&mut lock(), |state| {
            show_newest(state, vec![said(6), said(7)], true);
        });
        assert_eq!(ids(&snapshot()), [3, 4, 5, 6, 7]);
        reset();
    }

    #[test]
    fn a_failure_to_load_older_lines_is_shown_and_does_not_lose_the_ones_held() {
        let _turn = take_turn();
        let (sipil, _script, _server) =
            scripted("older-fails", vec![Err(Failure::new("boom", "No luck."))]);
        reset();
        let daemon = sipil.target().id();
        attach(&daemon, sipil);
        publish(&mut lock(), |state| {
            state.watch = Watch::On(daemon.clone(), "7".to_string());
            state.transcript = vec![said(5)];
            state.transcript_more = true;
        });
        load_older();
        let state = wait_until("the failure", |state| state.chat_error.is_some());
        assert_eq!(state.chat_error.as_deref(), Some("No luck."));
        assert_eq!(ids(&state), [5]);
        assert!(
            state.transcript_more && !state.loading_older,
            "it can be tried again"
        );
        reset();
    }
}
