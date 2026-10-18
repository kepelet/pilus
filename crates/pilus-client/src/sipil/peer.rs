//! A pilus window as a sipil: it answers other nodes, and it is how this window
//! talks to them.
//!
//! [`Local`] bundles what exists once per process: the iroh [`Node`], the
//! contact list, the channels, the chat store, and the [`PeerService`] that
//! answers inbound calls. A pilus answers `health` for anyone, and `chat.send`
//! and `chan.send` only for contacts; it has no agents, sessions or keys of its
//! own, and says so rather than pretending.
//!
//! A sipil agent is never reachable on its own: no call hands a contact a
//! session of it. It takes part only in a channel its person invited it into,
//! when a member names it (`@Researcher`), and its reply is said back to the
//! channel under its name. A person can run several daemons (a Researcher, a
//! Chief of Staff), and invites each separately.
//!
//! A stranger's message is refused and unread, but the stranger is remembered as
//! a request so the person can decide whether to add them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::channels::{Channel, Channels, Learned, channel_of, thread_key};
use super::chat::{Chats, MESSAGE_LIMIT};
use super::contacts::{Contact, Directory, Pairing};
use super::mention;
use super::node::{Node, NodeConfig, RelayChoice, Service, device_id};
use super::wire::{API_VERSION, Failure};

/// The longest ticket a daemon may send to be dialed back on.
const TICKET_LIMIT: usize = 4096;
use super::{Health, Quote, Sipil, SipilError, Target, parse_target, probe};

/// Something the window should tell the person about.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    /// Someone, or someone's agent, said something.
    Message {
        /// The conversation it belongs to: a contact's device ID, or a
        /// channel's `#<id>`.
        thread: String,
        /// The device that said it.
        from: String,
        /// Who said it, as the person knows them: a contact's name, or
        /// `name/Agent` when an agent of theirs did.
        name: String,
        /// The channel's name, when it was said in one.
        channel: Option<String>,
        /// What was said.
        text: String,
    },
    /// A member named one of this pilus's agents that was invited into the
    /// channel. The window answers it; it is never announced.
    Mentioned {
        /// The conversation it was said in.
        thread: String,
        /// The ID of the daemon the agent belongs to.
        daemon: String,
        /// The agent's name.
        agent: String,
    },
    /// A device that is not a contact tried to reach this pilus.
    Request {
        /// Their device ID.
        from: String,
    },
}

/// How to start [`Local`].
pub struct LocalOptions {
    /// Where the identity key lives.
    pub dir: PathBuf,
    /// The contacts. Opened by the caller, so they can be listed and edited
    /// before the network is up and are the same ones the node consults.
    pub directory: Arc<Directory>,
    /// The conversations, shared the same way.
    pub chats: Arc<Chats>,
    /// The channels, shared the same way.
    pub channels: Arc<Channels>,
    /// Which relays to use.
    pub relay: RelayChoice,
    /// What this pilus tells others to call its owner.
    pub nickname: String,
    /// Whether other nodes may call this one. Off, the node still dials out, but
    /// answers nobody: not even `health`.
    pub reachable: bool,
    /// Called for each [`Event`], on a network thread.
    pub notify: Arc<dyn Fn(Event) + Send + Sync>,
}

/// A sipil daemon whose agent can be invited into a channel.
#[derive(Clone)]
pub struct SharedDaemon {
    /// The daemon's device ID.
    pub id: String,
    /// What its primary agent is called.
    pub name: String,
    /// The connection to it.
    pub sipil: Sipil,
}

/// The daemons this pilus is connected to, by the ID they answer to.
type AgentLink = Arc<Mutex<Vec<SharedDaemon>>>;

/// Who a message goes to.
#[derive(Clone, Debug)]
pub enum Conversation {
    /// One contact.
    Direct(Contact),
    /// Every contact in a channel.
    Channel(Channel),
}

impl Conversation {
    /// The key of its thread in the chat store.
    pub fn thread(&self) -> String {
        match self {
            Self::Direct(contact) => contact.id.clone(),
            Self::Channel(channel) => channel.thread(),
        }
    }
}

/// What came of saying something to a conversation.
#[derive(Debug, Default)]
pub struct Delivery {
    /// The names of those it reached.
    pub delivered: Vec<String>,
    /// The names of those it did not, and why.
    pub failed: Vec<(String, SipilError)>,
}

/// This process's sipil side.
pub struct Local {
    node: Arc<Node>,
    directory: Arc<Directory>,
    chats: Arc<Chats>,
    channels: Arc<Channels>,
    reachable: Arc<AtomicBool>,
    daemons: AgentLink,
}

static LOCAL: Mutex<Option<Arc<Local>>> = Mutex::new(None);

/// The process's [`Local`], once [`Local::start`] has succeeded.
pub fn local() -> Option<Arc<Local>> {
    LOCAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

impl Local {
    /// Bring the network side up for this process, or return the one already
    /// up. The first call's options win; a failed start can be retried.
    pub fn start(options: LocalOptions) -> Result<Arc<Self>, SipilError> {
        let mut slot = LOCAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(local) = slot.as_ref() {
            return Ok(Arc::clone(local));
        }
        let local = Self::build(options)?;
        *slot = Some(Arc::clone(&local));
        Ok(local)
    }

    /// Build a [`Local`] that is not the process's own, for tests.
    pub fn build(options: LocalOptions) -> Result<Arc<Self>, SipilError> {
        let (directory, chats, channels) = (options.directory, options.chats, options.channels);
        let own =
            device_id(&options.dir).map_err(|error| SipilError::Offline(error.to_string()))?;
        let reachable = Arc::new(AtomicBool::new(options.reachable));
        let daemons = AgentLink::default();
        let service = Arc::new(PeerService {
            own,
            daemons: Arc::clone(&daemons),
            reachable: Arc::clone(&reachable),
            directory: Arc::clone(&directory),
            chats: Arc::clone(&chats),
            channels: Arc::clone(&channels),
            nickname: options.nickname,
            notify: options.notify,
        });
        let node = Node::start(
            &NodeConfig {
                dir: options.dir,
                relay: options.relay,
            },
            service,
        )
        .map_err(|error| SipilError::Offline(error.to_string()))?;
        Ok(Arc::new(Self {
            node,
            directory,
            chats,
            channels,
            reachable,
            daemons,
        }))
    }

    /// Name the sipil daemons this pilus is connected to, which contacts with
    /// sharing on for one may talk to. Replaces the earlier list.
    pub fn set_daemons(&self, daemons: Vec<SharedDaemon>) {
        *self
            .daemons
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = daemons;
    }

    /// Let other nodes call this one, or stop answering them. Takes effect on
    /// the next call; connections already open are answered `unavailable`.
    pub fn set_reachable(&self, reachable: bool) {
        self.reachable.store(reachable, Ordering::Relaxed);
    }

    /// Whether other nodes may call this one.
    pub fn reachable(&self) -> bool {
        self.reachable.load(Ordering::Relaxed)
    }

    /// The iroh node.
    pub fn node(&self) -> Arc<Node> {
        Arc::clone(&self.node)
    }

    /// This device's ID.
    pub fn id(&self) -> &str {
        self.node.id()
    }

    /// The contacts and requests.
    pub fn directory(&self) -> &Arc<Directory> {
        &self.directory
    }

    /// The conversations.
    pub fn chats(&self) -> &Arc<Chats> {
        &self.chats
    }

    /// The channels.
    pub fn channels(&self) -> &Arc<Channels> {
        &self.channels
    }

    /// Say `text` to a contact. It is stored in their thread only once their
    /// pilus has taken it, so a refusal leaves nothing behind to retry around.
    /// `request_id` makes a retry the same message to them. `agent` names the
    /// sipil agent of this pilus that is speaking, if one is.
    pub fn send(
        &self,
        contact: &Contact,
        text: &str,
        request_id: &str,
        agent: Option<&str>,
    ) -> Result<(), SipilError> {
        self.send_replying(contact, text, request_id, agent, None)
    }

    /// [`Local::send`] for a line that answers another, which travels with it.
    pub fn send_replying(
        &self,
        contact: &Contact,
        text: &str,
        request_id: &str,
        agent: Option<&str>,
        reply: Option<&Quote>,
    ) -> Result<(), SipilError> {
        let target = contact_target(contact)?;
        self.node
            .call(
                target.addr(),
                "chat.send",
                &json!({ "requestId": request_id, "content": text, "as": agent, "reply": reply }),
            )
            .map_err(|error| super::call_error(&target, error))?;
        self.chats
            .push_mine_replying(&contact.id, text, agent, reply);
        Ok(())
    }

    /// Say `text` in a channel, to every member that is a contact. It is stored
    /// once, if it reached anyone (or there is no one to reach). Members that
    /// are offline miss it: there is nowhere to leave it for them.
    pub fn send_channel(
        &self,
        channel: &Channel,
        text: &str,
        request_id: &str,
        agent: Option<&str>,
    ) -> Result<Delivery, SipilError> {
        self.send_channel_replying(channel, text, request_id, agent, None)
    }

    /// [`Local::send_channel`] for a line that answers another.
    pub fn send_channel_replying(
        &self,
        channel: &Channel,
        text: &str,
        request_id: &str,
        agent: Option<&str>,
        reply: Option<&Quote>,
    ) -> Result<Delivery, SipilError> {
        let contacts = self.contacts_of(&channel.members);
        let said = Said {
            text,
            request_id,
            agent,
            reply,
            ..Said::default()
        };
        self.deliver_channel(channel, &contacts, &said)
    }

    /// Put `contact` in channel `id`, and tell the members, who learn the new
    /// list from the line that says so. The new member learns the channel from
    /// the same line. Only contacts hear it.
    pub fn invite_to_channel(
        &self,
        id: &str,
        contact: &Contact,
        request_id: &str,
    ) -> Result<Delivery, SipilError> {
        self.owned(id)?;
        if !self.channels.invite(id, &contact.id) {
            return Err(gone());
        }
        let channel = self.channels.get(id).ok_or_else(gone)?;
        let contacts = self.contacts_of(&channel.members);
        let text = format!("invited {}", contact.name);
        let added = [contact.id.clone()];
        let said = Said {
            text: &text,
            request_id,
            added: &added,
            ..Said::default()
        };
        self.deliver_channel(&channel, &contacts, &said)
    }

    /// Take `member` out of channel `id`: they are told, and leave it, and the
    /// others drop them from their lists and keep them out. Anyone in a channel
    /// may do this; there is no owner to ask. The change holds here even when
    /// nobody could be told.
    pub fn remove_from_channel(
        &self,
        id: &str,
        member: &str,
        request_id: &str,
    ) -> Result<Delivery, SipilError> {
        let before = self.owned(id)?;
        // The one removed is written to as well, so they hear it.
        let contacts = self.contacts_of(&before.members);
        let name = self
            .directory
            .contact(member)
            .map_or_else(|| member.chars().take(8).collect(), |contact| contact.name);
        self.channels.remove_members(id, &[member.to_string()]);
        let channel = self.channels.get(id).ok_or_else(gone)?;
        let text = format!("removed {name}");
        let removed = [member.to_string()];
        let said = Said {
            text: &text,
            request_id,
            removed: &removed,
            ..Said::default()
        };
        self.deliver_channel(&channel, &contacts, &said)
    }

    /// Channel `id`, when this pilus owns it: only its owner changes who is in it.
    fn owned(&self, id: &str) -> Result<Channel, SipilError> {
        let channel = self.channels.get(id).ok_or_else(gone)?;
        if channel.owner == self.id() {
            Ok(channel)
        } else {
            Err(SipilError::Refused {
                code: "not_owner".to_string(),
                message: "Only the owner of a channel can add or remove members.".to_string(),
            })
        }
    }

    /// The contacts among `members`.
    fn contacts_of(&self, members: &[String]) -> Vec<Contact> {
        members
            .iter()
            .filter_map(|member| self.directory.contact(member))
            .collect()
    }

    /// Send `said` to `contacts` as a line of `channel`, carrying the channel's
    /// name and members. It is stored once, if it reached anyone (or there is no
    /// one to reach).
    fn deliver_channel(
        &self,
        channel: &Channel,
        contacts: &[Contact],
        said: &Said<'_>,
    ) -> Result<Delivery, SipilError> {
        let mut everyone = channel.members.clone();
        everyone.push(self.id().to_string());
        let params = json!({
            "channel": channel.id,
            "name": channel.name,
            "members": everyone,
            "owner": channel.owner,
            "requestId": said.request_id,
            "content": said.text,
            "as": said.agent,
            "reply": said.reply,
            "added": said.added,
            "removed": said.removed,
        });
        // One slow or absent member must not hold the rest up.
        let outcomes: Vec<(String, Result<(), SipilError>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = contacts
                .iter()
                .map(|contact| {
                    let params = &params;
                    scope.spawn(move || {
                        let outcome = contact_target(contact).and_then(|target| {
                            self.node
                                .call(target.addr(), "chan.send", params)
                                .map(|_| ())
                                .map_err(|error| super::call_error(&target, error))
                        });
                        (contact.name.clone(), outcome)
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|handle| handle.join().ok())
                .collect()
        });
        let mut delivery = Delivery::default();
        for (name, outcome) in outcomes {
            match outcome {
                Ok(()) => delivery.delivered.push(name),
                Err(error) => delivery.failed.push((name, error)),
            }
        }
        // Every member says this pilus is not in the channel: it was taken out
        // while it was away, so it is forgotten here as if it had left.
        if delivery.delivered.is_empty()
            && !delivery.failed.is_empty()
            && delivery.failed.iter().all(|(_, error)| {
                matches!(error, SipilError::Refused { code, .. } if code == "not_member")
            })
        {
            let _ = self.channels.remove(&channel.id);
            self.chats.forget(&channel.thread());
            return Err(SipilError::Refused {
                code: "not_member".to_string(),
                message: "You are no longer in that channel.".to_string(),
            });
        }
        if delivery.delivered.is_empty()
            && let Some((_, error)) = delivery.failed.pop()
        {
            return Err(error);
        }
        self.chats
            .push_mine_replying(&channel.thread(), said.text, said.agent, said.reply);
        Ok(delivery)
    }

    /// Say `text` to a conversation, whichever kind it is.
    pub fn say(
        &self,
        conversation: &Conversation,
        text: &str,
        request_id: &str,
        agent: Option<&str>,
    ) -> Result<Delivery, SipilError> {
        self.say_replying(conversation, text, request_id, agent, None)
    }

    /// [`Local::say`] for a line that answers another.
    pub fn say_replying(
        &self,
        conversation: &Conversation,
        text: &str,
        request_id: &str,
        agent: Option<&str>,
        reply: Option<&Quote>,
    ) -> Result<Delivery, SipilError> {
        match conversation {
            Conversation::Direct(contact) => {
                self.send_replying(contact, text, request_id, agent, reply)?;
                Ok(Delivery {
                    delivered: vec![contact.name.clone()],
                    failed: Vec::new(),
                })
            }
            Conversation::Channel(channel) => {
                self.send_channel_replying(channel, text, request_id, agent, reply)
            }
        }
    }

    /// Ask a contact's pilus how it is: online, and whether it has added this
    /// device back.
    pub fn check(&self, contact: &Contact) -> Result<Health, SipilError> {
        probe(&self.node, &contact_target(contact)?)
    }
}

/// Where to dial a contact.
pub fn contact_target(contact: &Contact) -> Result<Target, SipilError> {
    parse_target(contact.ticket.as_deref().unwrap_or(&contact.id))
}

/// Answers calls from other nodes on behalf of this pilus.
struct PeerService {
    own: String,
    daemons: AgentLink,
    reachable: Arc<AtomicBool>,
    directory: Arc<Directory>,
    chats: Arc<Chats>,
    channels: Arc<Channels>,
    nickname: String,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
}

impl std::fmt::Debug for PeerService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerService").finish_non_exhaustive()
    }
}

impl Service for PeerService {
    fn handle(&self, from: &str, method: &str, params: Value) -> Result<Value, Failure> {
        if !self.reachable.load(Ordering::Relaxed) {
            return Err(Failure::new(
                "unavailable",
                "That pilus is not taking calls.",
            ));
        }
        match method {
            "health" => Ok(json!({
                "name": "sipil",
                "kind": "pilus",
                "version": env!("CARGO_PKG_VERSION"),
                "api": API_VERSION,
                "allowed": self.directory.is_contact(from),
                "nickname": self.nickname,
            })),
            "pair.request" => self.pair_request(from, &params),
            "chat.send" => self.chat_send(from, &params),
            "chan.send" => self.chan_send(from, &params),
            _ => Err(Failure::new(
                "unknown_method",
                "This is a pilus, not a sipil agent: it only takes chat messages.",
            )),
        }
    }
}

impl PeerService {
    fn daemons(&self) -> Vec<SharedDaemon> {
        self.daemons
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// A sipil daemon asks to be connected. Nothing is added until the person
    /// answers in the window; the daemon asks again until it hears.
    fn pair_request(&self, from: &str, params: &Value) -> Result<Value, Failure> {
        if params.get("kind").and_then(Value::as_str) != Some("daemon") {
            return Err(Failure::new(
                "bad_request",
                "Only a sipil agent can ask to be connected.",
            ));
        }
        let label = params.get("label").and_then(Value::as_str).unwrap_or("");
        // A ticket is how this pilus will dial it back, so it must name the
        // device that asked: another device's ticket would aim this pilus at it.
        let ticket = params
            .get("ticket")
            .and_then(Value::as_str)
            .filter(|ticket| ticket.len() <= TICKET_LIMIT)
            .filter(|ticket| parse_target(ticket).is_ok_and(|target| target.id() == from))
            .map(str::to_string);
        let (state, fresh) = self.directory.pair(from, label, ticket);
        if fresh {
            (self.notify)(Event::Request {
                from: from.to_string(),
            });
        }
        Ok(json!({
            "status": match state {
                Pairing::Pending => "pending",
                Pairing::Accepted => "accepted",
                Pairing::Declined => "declined",
            },
            "nickname": self.nickname,
        }))
    }

    fn knock(&self, from: &str) {
        if self.directory.knock(from) {
            (self.notify)(Event::Request {
                from: from.to_string(),
            });
        }
    }

    /// The text of a message, checked.
    fn content(params: &Value) -> Result<&str, Failure> {
        let text = params
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| Failure::new("missing_field", "A message needs content."))?;
        if text.len() > MESSAGE_LIMIT {
            return Err(Failure::new(
                "too_large",
                format!("A message can be at most {} KB.", MESSAGE_LIMIT / 1024),
            ));
        }
        Ok(text)
    }

    /// The agent a message was said by, if it names one: short, one line.
    fn agent_name(params: &Value) -> Option<String> {
        let name: String = params
            .get("as")
            .and_then(Value::as_str)?
            .chars()
            .filter(|c| !c.is_control())
            .take(40)
            .collect();
        let name = name.trim().to_string();
        (!name.is_empty()).then_some(name)
    }

    fn chat_send(&self, from: &str, params: &Value) -> Result<Value, Failure> {
        let Some(contact) = self.directory.contact(from) else {
            self.knock(from);
            return Err(unauthorized());
        };
        let text = Self::content(params)?;
        let request_id = params
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let agent = Self::agent_name(params);
        let reply = Quote::from_params(params);
        if self.chats.push_theirs_replying(
            from,
            text,
            request_id,
            None,
            agent.as_deref(),
            reply.as_ref(),
        ) {
            (self.notify)(Event::Message {
                thread: from.to_string(),
                from: from.to_string(),
                name: speaker_name(&contact.name, agent.as_deref()),
                channel: None,
                text: text.to_string(),
            });
            // Nothing here starts an agent: a chat between two people is theirs
            // alone. Agents join a channel, never a direct chat.
        }
        Ok(json!({}))
    }

    /// Hand `thread`, a channel, to each agent of ours that `text` names, when
    /// that agent was invited into it. Being a member is what lets someone name
    /// it: the invitation is its person's word that the channel may run it. An
    /// agent's own line never starts another, or two could answer each other for
    /// ever.
    fn invite_agents(&self, thread: &str, text: &str, said_by_agent: Option<&str>) {
        if said_by_agent.is_some() {
            return;
        }
        let Some(channel) = channel_of(thread) else {
            return;
        };
        let invited = self.channels.agents(channel);
        let present: Vec<SharedDaemon> = self
            .daemons()
            .into_iter()
            .filter(|daemon| invited.contains(&daemon.id))
            .collect();
        let names: Vec<String> = present.iter().map(|daemon| daemon.name.clone()).collect();
        for name in mention::mentioned(text, &names) {
            if let Some(daemon) = present.iter().find(|daemon| daemon.name == name) {
                (self.notify)(Event::Mentioned {
                    thread: thread.to_string(),
                    daemon: daemon.id.clone(),
                    agent: daemon.name.clone(),
                });
            }
        }
    }

    fn chan_send(&self, from: &str, params: &Value) -> Result<Value, Failure> {
        let Some(contact) = self.directory.contact(from) else {
            self.knock(from);
            return Err(unauthorized());
        };
        let text = Self::content(params)?;
        let id = params
            .get("channel")
            .and_then(Value::as_str)
            .filter(|id| id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| Failure::new("missing_field", "A channel message names its channel."))?
            .to_ascii_lowercase();
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("channel");
        let mut members: Vec<String> = params
            .get("members")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        members.push(from.to_string());
        let owner = params
            .get("owner")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let known = self.channels.get(&id);
        // Someone who is not in a channel this pilus is in cannot speak in it:
        // a member who was taken out hears nothing, and says nothing, here.
        if known
            .as_ref()
            .is_some_and(|channel| !channel.members.iter().any(|member| *member == from))
        {
            return Err(Failure::new("not_member", "You are not in that channel."));
        }
        // Who is in a channel changes only at the owner's word.
        let from_owner = known
            .as_ref()
            .is_some_and(|channel| !channel.owner.is_empty() && channel.owner == from);
        let ids = |key: &str| -> Vec<String> {
            params
                .get(key)
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_ascii_lowercase)
                        .collect()
                })
                .unwrap_or_default()
        };
        // Invited back to a channel this pilus left or was taken out of.
        if ids("added").contains(&self.own.to_ascii_lowercase()) {
            self.channels.rejoin(&id);
        }
        if from_owner {
            let removed = ids("removed");
            if removed.contains(&self.own.to_ascii_lowercase()) {
                // Taken out: the channel is forgotten here, as if it had been left.
                let _ = self.channels.remove(&id);
                self.chats.forget(&thread_key(&id));
                return Ok(json!({}));
            }
            self.channels.restore(&id, &ids("added"));
            if !removed.is_empty() {
                self.channels.remove_members(&id, &removed);
            }
        }
        let learned = self
            .channels
            .learn(&id, name, &members, &self.own, from, &owner);
        if learned == Learned::Left {
            // They are told it went through: leaving is this pilus's own business.
            return Ok(json!({}));
        }
        let channel = self.channels.get(&id);
        let thread = thread_key(&id);
        let request_id = params
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let agent = Self::agent_name(params);
        let reply = Quote::from_params(params);
        if self.chats.push_theirs_replying(
            &thread,
            text,
            request_id,
            Some(from),
            agent.as_deref(),
            reply.as_ref(),
        ) {
            (self.notify)(Event::Message {
                thread: thread.clone(),
                from: from.to_string(),
                name: speaker_name(&contact.name, agent.as_deref()),
                channel: channel.map(|channel| channel.name),
                text: text.to_string(),
            });
            self.invite_agents(&thread, text, agent.as_deref());
        }
        Ok(json!({}))
    }
}

/// How a contact is named in an announcement: `Rina`, or `Rina/Researcher`.
fn speaker_name(contact: &str, agent: Option<&str>) -> String {
    match agent {
        Some(agent) => format!("{contact}/{agent}"),
        None => contact.to_string(),
    }
}

fn unauthorized() -> Failure {
    Failure::new(
        "unauthorized",
        "That pilus has not added this device as a contact, so it does not take your messages yet.",
    )
}

/// The channel id a thread key names, if it names one.
/// What a line of a channel says and carries.
#[derive(Default)]
struct Said<'a> {
    text: &'a str,
    request_id: &'a str,
    agent: Option<&'a str>,
    reply: Option<&'a Quote>,
    /// Members the line invites, whom its readers may list again.
    added: &'a [String],
    /// Members the line removes, who leave and whom the others drop.
    removed: &'a [String],
}

/// The channel is not in this pilus's list.
fn gone() -> SipilError {
    SipilError::Address("That channel is no longer in the list.".to_string())
}

pub fn thread_channel(thread: &str) -> Option<&str> {
    channel_of(thread)
}
