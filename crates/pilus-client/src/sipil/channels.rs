//! Channels: named conversations among several contacts.
//!
//! There is no server to hold a channel, so each member keeps its own copy: its
//! name, its members, and (in the chat store, under `#<id>`) its lines. A
//! message is sent to every member by its author's pilus, and every pilus that
//! takes one learns the member list it carries, so a channel one person made
//! shows up, with its name, in everyone's list as soon as they are first
//! written to. Only contacts are written to or listened to, so a member this
//! pilus has not added is listed but silent.
//!
//! A channel has an owner, the device that made it, and only the owner can
//! invite a contact to it or remove a member. Every message carries the owner's
//! ID, a pilus that has the channel believes only the owner's lists and changes
//! of members, and a message from someone who is not a member is refused. A
//! removal is remembered per channel (`removed`), so a lagging member's older
//! list does not bring the person back, and only an invitation does.
//!
//! An agent of this pilus's own is in a channel only once its person invited it
//! (`agents`). That list is this device's alone and is never sent: the other
//! members learn of the agent from the line it says on joining, and naming an
//! agent that was not invited does nothing.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use pilus_db::Db;
use serde::{Deserialize, Serialize};

use super::store;

/// The document the channels are kept in.
const DOC: &str = "channels";

/// The longest channel name kept.
const NAME_LIMIT: usize = 40;
/// The most members a channel lists, so a message cannot name a crowd.
const MEMBER_LIMIT: usize = 50;

/// A named conversation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Channel {
    /// 16 hexadecimal characters, chosen by whoever made the channel and the
    /// same for every member.
    pub id: String,
    /// What it is called here, without the `#`.
    pub name: String,
    /// The device that made the channel, and the only one that can change who
    /// is in it. Empty for a channel whose first message did not say.
    #[serde(default)]
    pub owner: String,
    /// The other members' device IDs, the owner's among them unless this pilus
    /// is the owner. This pilus is never in its own list.
    pub members: Vec<String>,
    /// Unix seconds when this pilus learned of it.
    #[serde(default)]
    pub created: u64,
}

impl Channel {
    /// The key of this channel's thread in the chat store.
    pub fn thread(&self) -> String {
        thread_key(&self.id)
    }
}

/// The chat-store key of channel `id`.
pub fn thread_key(id: &str) -> String {
    format!("#{id}")
}

/// The channel id in a chat-store key, if it is a channel's.
pub fn channel_of(thread: &str) -> Option<&str> {
    thread.strip_prefix('#')
}

#[derive(Default, Deserialize, Serialize)]
struct File {
    #[serde(default)]
    channels: Vec<Channel>,
    /// Channels this pilus left, so a late message does not bring one back.
    #[serde(default)]
    left: Vec<String>,
    /// Members taken out of a channel, by channel id, so a list that still
    /// names them does not add them back.
    #[serde(default)]
    removed: std::collections::HashMap<String, Vec<String>>,
    /// The sipil daemons (by device ID) whose agent this pilus's person invited
    /// into a channel, by channel id.
    #[serde(default)]
    agents: std::collections::HashMap<String, Vec<String>>,
}

/// What learning of a channel came to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Learned {
    /// A channel this pilus did not have.
    New,
    /// A channel it already had; its member list was brought up to date.
    Known,
    /// A channel it left: nothing was kept.
    Left,
}

/// Every channel this pilus is in.
pub struct Channels {
    db: Db,
    /// Where the identity key lives, which names the owner of a new channel.
    dir: PathBuf,
    inner: Mutex<File>,
    version: AtomicU64,
}

impl Channels {
    /// Open the channels in `db`, importing `dir`'s `channels.json` the first
    /// time. `dir` also holds the identity key.
    pub fn open(db: &Db, dir: &Path) -> Self {
        Self {
            inner: Mutex::new(store::load(db, DOC, &dir.join("channels.json"))),
            db: db.clone(),
            dir: dir.to_path_buf(),
            version: AtomicU64::new(0),
        }
    }

    /// Counts every change, so a window can repaint only when it moved.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// The channels, oldest first.
    pub fn list(&self) -> Vec<Channel> {
        self.lock().channels.clone()
    }

    /// The channel `id`.
    pub fn get(&self, id: &str) -> Option<Channel> {
        self.lock()
            .channels
            .iter()
            .find(|channel| channel.id == id)
            .cloned()
    }

    /// Make a channel of `members` and return it, owned by this device. `name`
    /// is cleaned the way a channel name must be: lowercase, no spaces.
    pub fn create(&self, name: &str, members: Vec<String>) -> std::io::Result<Channel> {
        let owner = super::node::device_id(&self.dir)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let channel = Channel {
            id: fresh_id(),
            name: clean_name(name),
            owner,
            members: bounded(members, &[]),
            created: store::now(),
        };
        self.change(|file| file.channels.push(channel.clone()))?;
        Ok(channel)
    }

    /// Learn of a channel `from` wrote to: make it if it is new, owned by
    /// whoever `owner` says, and otherwise add the members `from` names only if
    /// `from` is its owner. `own` is this device, which is never listed. A
    /// channel that was left stays left.
    pub fn learn(
        &self,
        id: &str,
        name: &str,
        members: &[String],
        own: &str,
        from: &str,
        owner: &str,
    ) -> Learned {
        if self.lock().left.iter().any(|left| left == id) {
            return Learned::Left;
        }
        let mut learned = Learned::Known;
        let _ =
            self.change(
                |file| match file.channels.iter_mut().find(|channel| channel.id == id) {
                    // Only the owner's list is believed.
                    Some(channel) if channel.owner.is_empty() || channel.owner != from => {}
                    Some(channel) => {
                        let known = std::mem::take(&mut channel.members);
                        let joined: Vec<String> =
                            known.into_iter().chain(members.iter().cloned()).collect();
                        let mut excluded = file.removed.get(id).cloned().unwrap_or_default();
                        excluded.push(own.to_string());
                        channel.members = bounded(joined, &excluded);
                    }
                    None => {
                        learned = Learned::New;
                        file.channels.push(Channel {
                            id: id.to_string(),
                            name: clean_name(name),
                            owner: bounded(vec![owner.to_string()], &[])
                                .pop()
                                .unwrap_or_default(),
                            members: bounded(members.to_vec(), &[own.to_string()]),
                            created: store::now(),
                        });
                    }
                },
            );
        learned
    }

    /// Put `member` in channel `id`, and take them off its removed list if they
    /// were on it. False when there is no such channel.
    pub fn invite(&self, id: &str, member: &str) -> bool {
        self.edit_members(id, |file, channel| {
            if let Some(removed) = file.removed.get_mut(id) {
                removed.retain(|gone| gone != member);
            }
            let joined: Vec<String> = channel
                .members
                .iter()
                .cloned()
                .chain([member.to_string()])
                .collect();
            channel.members = bounded(joined, &[]);
        })
    }

    /// Take `members` out of channel `id` and remember it, so no list adds them
    /// back. False when there is no such channel.
    pub fn remove_members(&self, id: &str, members: &[String]) -> bool {
        self.edit_members(id, |file, channel| {
            channel.members.retain(|member| !members.contains(member));
            let removed = file.removed.entry(id.to_string()).or_default();
            for member in members {
                if !removed.contains(member) {
                    removed.push(member.to_ascii_lowercase());
                }
            }
        })
    }

    /// Take channel `id` off the list of those this pilus left, because a
    /// member invited it back.
    pub fn rejoin(&self, id: &str) {
        let _ = self.change(|file| file.left.retain(|left| left != id));
    }

    /// Let `members` be added to channel `id` again, because someone in it
    /// invited them.
    pub fn restore(&self, id: &str, members: &[String]) {
        let _ = self.change(|file| {
            if let Some(removed) = file.removed.get_mut(id) {
                removed.retain(|gone| !members.contains(gone));
            }
        });
    }

    fn edit_members(&self, id: &str, edit: impl FnOnce(&mut File, &mut Channel)) -> bool {
        let mut found = false;
        let _ = self.change(|file| {
            let Some(at) = file.channels.iter().position(|channel| channel.id == id) else {
                return;
            };
            let mut channel = file.channels.remove(at);
            edit(file, &mut channel);
            file.channels.insert(at, channel);
            found = true;
        });
        found
    }

    /// The daemons whose agent is invited into channel `id`.
    pub fn agents(&self, id: &str) -> Vec<String> {
        self.lock().agents.get(id).cloned().unwrap_or_default()
    }

    /// Invite the agent of `daemon` into channel `id`, or take it out. False when
    /// there is no such channel.
    pub fn set_agent(&self, id: &str, daemon: &str, invited: bool) -> bool {
        if !self.lock().channels.iter().any(|channel| channel.id == id) {
            return false;
        }
        let _ = self.change(|file| {
            let agents = file.agents.entry(id.to_string()).or_default();
            agents.retain(|invited| invited != daemon);
            if invited {
                agents.push(daemon.to_string());
            }
            if agents.is_empty() {
                file.agents.remove(id);
            }
        });
        true
    }

    /// Leave a channel: it is forgotten here and the others are not told.
    pub fn remove(&self, id: &str) -> std::io::Result<()> {
        self.change(|file| {
            file.channels.retain(|channel| channel.id != id);
            file.agents.remove(id);
            if !file.left.iter().any(|left| left == id) {
                file.left.push(id.to_string());
            }
        })
    }

    fn change(&self, edit: impl FnOnce(&mut File)) -> std::io::Result<()> {
        let mut file = self.lock();
        edit(&mut file);
        self.version.fetch_add(1, Ordering::Relaxed);
        store::save(&self.db, DOC, &*file)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, File> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A channel name: lowercase, spaces as dashes, nothing that breaks a line.
pub fn clean_name(name: &str) -> String {
    let name: String = name
        .trim()
        .trim_start_matches('#')
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| !c.is_control() && *c != '@' && *c != ':')
        .flat_map(char::to_lowercase)
        .take(NAME_LIMIT)
        .collect();
    if name.is_empty() {
        "channel".to_string()
    } else {
        name
    }
}

/// `members` without duplicates, without `exclude`, and without anything that is
/// not a device ID, up to the limit.
fn bounded(members: Vec<String>, exclude: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for member in members {
        let valid = member.len() == 64 && member.bytes().all(|b| b.is_ascii_hexdigit());
        if valid
            && !exclude.contains(&member)
            && !kept.contains(&member)
            && kept.len() < MEMBER_LIMIT
        {
            kept.push(member.to_ascii_lowercase());
        }
    }
    kept
}

/// Sixteen random hexadecimal characters.
fn fresh_id() -> String {
    let key = iroh::SecretKey::generate().to_bytes();
    key[..8].iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pilus-channels-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The store on the database that lives in `dir`, as a launch would open it.
    fn open(dir: &Path) -> Channels {
        Channels::open(&Db::at(dir), dir)
    }

    fn id(seed: char) -> String {
        seed.to_string().repeat(64)
    }

    #[test]
    fn a_name_is_made_fit_for_a_mention_and_a_sidebar() {
        assert_eq!(clean_name("  #Project Alpha: @x "), "project-alpha-x");
        assert_eq!(clean_name("###"), "channel");
        assert_eq!(clean_name(&"a".repeat(100)).len(), NAME_LIMIT);
    }

    #[test]
    fn a_channel_survives_a_restart_and_never_lists_junk_or_repeats() {
        let dir = scratch("restart");
        let channels = open(&dir);
        let made = channels
            .create(
                "Alerts",
                vec![id('a'), id('a'), "nope".to_string(), id('b')],
            )
            .unwrap();
        assert_eq!(made.members, [id('a'), id('b')]);
        assert_eq!(made.id.len(), 16);
        assert_eq!(open(&dir).get(&made.id), Some(made.clone()));
        assert_eq!(made.thread(), format!("#{}", made.id));
        assert_eq!(channel_of(&made.thread()), Some(made.id.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn learning_a_channel_creates_it_once_and_believes_only_its_owner() {
        let dir = scratch("learn");
        let channels = open(&dir);
        let me = id('m');
        let (owner, other) = (id('a'), id('c'));
        assert_eq!(
            channels.learn(
                "0123456789abcdef",
                "Project",
                &[owner.clone(), me.clone()],
                &me,
                &owner,
                &owner
            ),
            Learned::New
        );
        let channel = channels.get("0123456789abcdef").unwrap();
        assert_eq!(channel.owner, owner);
        // The owner's later list adds members; nobody renames it.
        assert_eq!(
            channels.learn(
                "0123456789abcdef",
                "Renamed",
                &[id('b')],
                &me,
                &owner,
                &owner
            ),
            Learned::Known
        );
        // A member who is not the owner does not add anyone.
        channels.learn("0123456789abcdef", "x", &[id('d')], &me, &other, &other);
        let channel = channels.get("0123456789abcdef").unwrap();
        assert_eq!(channel.name, "project");
        assert_eq!(channel.owner, owner, "nor takes the channel over");
        assert_eq!(
            channel.members,
            [id('a'), id('b')],
            "this device is never listed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_channel_made_here_is_owned_by_this_device() {
        let dir = scratch("owner");
        let channels = open(&dir);
        let made = channels.create("x", vec![id('a')]).unwrap();
        assert_eq!(
            made.owner,
            super::super::node::device_id(&dir).unwrap(),
            "the identity of the directory it lives in"
        );
        assert_eq!(open(&dir).get(&made.id).unwrap().owner, made.owner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_member_invited_is_listed_and_one_removed_stays_out_until_invited_again() {
        let dir = scratch("members");
        let channels = open(&dir);
        let me = id('m');
        let made = channels.create("x", vec![id('a')]).unwrap();
        assert!(channels.invite(&made.id, &id('b')));
        assert_eq!(channels.get(&made.id).unwrap().members, [id('a'), id('b')]);
        assert!(!channels.invite("nope", &id('b')), "no such channel");

        assert!(channels.remove_members(&made.id, &[id('a')]));
        assert_eq!(channels.get(&made.id).unwrap().members, [id('b')]);
        // A member who has not heard yet still lists them; that does not
        // bring them back.
        channels.learn(
            &made.id,
            "x",
            &[id('a'), id('c')],
            &me,
            &made.owner,
            &made.owner,
        );
        assert_eq!(channels.get(&made.id).unwrap().members, [id('b'), id('c')]);
        // An invitation does, here and on a pilus told of it.
        channels.invite(&made.id, &id('a'));
        assert_eq!(
            channels.get(&made.id).unwrap().members,
            [id('b'), id('c'), id('a')]
        );
        channels.remove_members(&made.id, &[id('c')]);
        channels.restore(&made.id, &[id('c')]);
        channels.learn(&made.id, "x", &[id('c')], &me, &made.owner, &made.owner);
        assert!(channels.get(&made.id).unwrap().members.contains(&id('c')));

        // It all survives a restart.
        channels.remove_members(&made.id, &[id('b')]);
        let again = open(&dir);
        again.learn(&made.id, "x", &[id('b')], &me, &made.owner, &made.owner);
        assert!(!again.get(&made.id).unwrap().members.contains(&id('b')));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaving_forgets_the_channel() {
        let dir = scratch("leave");
        let channels = open(&dir);
        let made = channels.create("x", vec![]).unwrap();
        channels.remove(&made.id).unwrap();
        assert!(channels.list().is_empty());
        let me = id('m');
        assert_eq!(
            channels.learn(&made.id, "x", &[id('a')], &me, &id('a'), &id('a')),
            Learned::Left,
            "a late message does not bring a left channel back"
        );
        assert!(channels.list().is_empty());
        // An invitation does.
        channels.rejoin(&made.id);
        assert_eq!(
            channels.learn(&made.id, "x", &[id('a')], &me, &id('a'), &id('a')),
            Learned::New
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_agent_is_in_a_channel_only_once_invited_and_leaves_with_it() {
        let dir = scratch("agents");
        let channels = open(&dir);
        let made = channels.create("x", vec![id('a')]).unwrap();
        assert!(channels.agents(&made.id).is_empty());
        assert!(channels.set_agent(&made.id, "daemon-a", true));
        assert!(channels.set_agent(&made.id, "daemon-b", true));
        assert!(channels.set_agent(&made.id, "daemon-b", false));
        assert!(
            !channels.set_agent("nope", "daemon-a", true),
            "no such channel"
        );
        assert_eq!(open(&dir).agents(&made.id), ["daemon-a"]);
        channels.remove(&made.id).unwrap();
        channels.rejoin(&made.id);
        assert!(
            channels.agents(&made.id).is_empty(),
            "an invitation does not outlive the channel here"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
