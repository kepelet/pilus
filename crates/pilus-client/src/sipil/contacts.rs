//! The people this pilus will talk to: contacts, and strangers who knocked.
//!
//! A contact is a device ID, which the QUIC handshake proves, so being on this
//! list is the whole of authorization: a contact's message is taken, anyone
//! else's is refused. A refused device is remembered as a *request* so the
//! window can offer to accept it; nothing it sent is kept.
//!
//! A sipil daemon is a contact too, of a different kind: it asks to be added
//! (`sipil connect` on its machine), the person confirms here, and from then on
//! this pilus talks to its agents. A daemon contact is never a person: it has no
//! chat, is never invited into a channel, and none of the checks below that admit
//! a person admit it.
//!
//! Adding someone is mutual by design. They must add this device too before
//! their messages are taken here, exactly as this device must be on a sipil
//! daemon's allow-list: nobody can start talking to a pilus that has not
//! agreed to listen.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::store;

/// How many unanswered requests are remembered; the oldest go first, so a
/// device that keeps knocking cannot grow the file.
const MAX_REQUESTS: usize = 20;
/// The longest name a contact can be given.
const NAME_LIMIT: usize = 60;

/// Someone this pilus talks to.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Contact {
    /// Their device ID: 64 lowercase hexadecimal characters.
    pub id: String,
    /// What this person is called here. Chosen by the user, never by the peer.
    pub name: String,
    /// A ticket carrying their addresses, when one was pasted. Without it they
    /// are found through iroh's discovery by ID alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    /// Unix seconds when they were added.
    #[serde(default)]
    pub added: u64,
    /// Whether this is a sipil daemon rather than a person.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub daemon: bool,
}

/// A device that tried to reach this pilus without being a contact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Request {
    /// Their device ID.
    pub id: String,
    /// Unix seconds when they last tried.
    pub at: u64,
    /// For a sipil daemon asking to connect, what it calls itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// For a sipil daemon, the ticket to dial it back on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    /// Whether this is a sipil daemon asking to connect, not a person.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub daemon: bool,
}

/// What a sipil daemon is told when it asks to connect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pairing {
    /// Waiting for the person here to answer.
    Pending,
    /// Added as a contact.
    Accepted,
    /// Turned down. Said once; asking again starts over.
    Declined,
}

#[derive(Default, Deserialize, Serialize)]
struct File {
    #[serde(default)]
    contacts: Vec<Contact>,
    #[serde(default)]
    requests: Vec<Request>,
}

/// The contact list and request inbox, shared between the window and the
/// node's inbound handler.
pub struct Directory {
    path: PathBuf,
    inner: Mutex<File>,
    version: AtomicU64,
    /// Daemons that were refused and have not yet been told so.
    declined: Mutex<BTreeSet<String>>,
}

impl Directory {
    /// Open the store under `dir` (`contacts.json`), starting empty if there is none.
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("contacts.json");
        Self {
            inner: Mutex::new(store::load(&path)),
            path,
            version: AtomicU64::new(0),
            declined: Mutex::new(BTreeSet::new()),
        }
    }

    /// Counts every change, so a window can repaint only when it moved.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// The people who are contacts, oldest first.
    pub fn contacts(&self) -> Vec<Contact> {
        self.lock()
            .contacts
            .iter()
            .filter(|contact| !contact.daemon)
            .cloned()
            .collect()
    }

    /// The sipil daemons that are contacts, oldest first.
    pub fn daemons(&self) -> Vec<Contact> {
        self.lock()
            .contacts
            .iter()
            .filter(|contact| contact.daemon)
            .cloned()
            .collect()
    }

    /// The strangers who knocked, newest first.
    pub fn requests(&self) -> Vec<Request> {
        self.lock().requests.clone()
    }

    /// Whether `id` is a person who is a contact. A sipil daemon is not: nothing
    /// that admits a person admits it.
    pub fn is_contact(&self, id: &str) -> bool {
        self.contact(id).is_some()
    }

    /// Whether `id` is a contact of either kind, a person or a sipil daemon.
    /// Only for deciding whose call is worth noting: it admits no one.
    pub fn knows(&self, id: &str) -> bool {
        self.lock().contacts.iter().any(|contact| contact.id == id)
    }

    /// The person for `id`.
    pub fn contact(&self, id: &str) -> Option<Contact> {
        self.lock()
            .contacts
            .iter()
            .find(|contact| contact.id == id && !contact.daemon)
            .cloned()
    }

    /// Whether `id` is a sipil daemon that is a contact.
    pub fn is_daemon(&self, id: &str) -> bool {
        self.lock()
            .contacts
            .iter()
            .any(|contact| contact.id == id && contact.daemon)
    }

    /// Add `id` as `name`, or update the name and ticket of an existing contact.
    /// Adding someone answers any request they made.
    pub fn add(&self, id: &str, name: &str, ticket: Option<String>) -> std::io::Result<()> {
        let name = clean_name(name, id);
        self.change(|file| {
            file.requests.retain(|request| request.id != id);
            match file.contacts.iter_mut().find(|contact| contact.id == id) {
                Some(contact) => {
                    contact.name = name;
                    if ticket.is_some() {
                        contact.ticket = ticket;
                    }
                }
                None => file.contacts.push(Contact {
                    id: id.to_string(),
                    name,
                    ticket,
                    added: store::now(),
                    daemon: false,
                }),
            }
        })
    }

    /// Turn a request into a contact: a person as they are, a sipil daemon under
    /// the name it gave and with the ticket to dial it by. Returns whether it
    /// was a daemon, or `None` when there was no such request.
    pub fn accept(&self, id: &str) -> std::io::Result<Option<bool>> {
        let Some(request) = self
            .lock()
            .requests
            .iter()
            .find(|request| request.id == id)
            .cloned()
        else {
            return Ok(None);
        };
        if !request.daemon {
            self.add(id, "", None)?;
            return Ok(Some(false));
        }
        let name = clean_name(request.label.as_deref().unwrap_or(""), id);
        self.change(|file| {
            file.requests.retain(|request| request.id != id);
            file.contacts.retain(|contact| contact.id != id);
            file.contacts.push(Contact {
                id: id.to_string(),
                name,
                ticket: request.ticket.clone(),
                added: store::now(),
                daemon: true,
            });
        })?;
        Ok(Some(true))
    }

    /// A sipil daemon asks to be connected. Tells it where that stands, and
    /// whether this is news to the person (a first ask, not a repeat).
    pub fn pair(&self, id: &str, label: &str, ticket: Option<String>) -> (Pairing, bool) {
        if self.is_daemon(id) {
            return (Pairing::Accepted, false);
        }
        if self
            .declined
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id)
        {
            return (Pairing::Declined, false);
        }
        let label = clean_name(label, id);
        let fresh = !self.lock().requests.iter().any(|request| request.id == id);
        let _ = self.change(|file| {
            // A repeat ask without a usable ticket keeps the one already known.
            let earlier = file
                .requests
                .iter()
                .find(|request| request.id == id)
                .and_then(|request| request.ticket.clone());
            file.requests.retain(|request| request.id != id);
            file.requests.insert(
                0,
                Request {
                    id: id.to_string(),
                    at: store::now(),
                    label: Some(label),
                    ticket: ticket.or(earlier),
                    daemon: true,
                },
            );
            file.requests.truncate(MAX_REQUESTS);
        });
        (Pairing::Pending, fresh)
    }

    /// Give an existing contact a new name. Returns false if there is none.
    pub fn rename(&self, id: &str, name: &str) -> std::io::Result<bool> {
        let name = clean_name(name, id);
        let mut found = false;
        self.change(|file| {
            if let Some(contact) = file.contacts.iter_mut().find(|contact| contact.id == id) {
                contact.name = name;
                found = true;
            }
        })?;
        Ok(found)
    }

    /// Forget a contact. Their messages are refused from now on; the thread
    /// stays in the chat store.
    pub fn remove(&self, id: &str) -> std::io::Result<()> {
        self.change(|file| file.contacts.retain(|contact| contact.id != id))
    }

    /// Remember that `id` knocked. Returns whether this is news.
    pub fn knock(&self, id: &str) -> bool {
        let fresh = !self.lock().requests.iter().any(|request| request.id == id);
        let _ = self.change(|file| {
            file.requests.retain(|request| request.id != id);
            file.requests.insert(
                0,
                Request {
                    id: id.to_string(),
                    at: store::now(),
                    label: None,
                    ticket: None,
                    daemon: false,
                },
            );
            file.requests.truncate(MAX_REQUESTS);
        });
        fresh
    }

    /// Drop a request without adding the device. A sipil daemon is told so the
    /// next time it asks.
    pub fn dismiss(&self, id: &str) -> std::io::Result<()> {
        let daemon = self
            .lock()
            .requests
            .iter()
            .any(|request| request.id == id && request.daemon);
        if daemon {
            self.declined
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.to_string());
        }
        self.change(|file| file.requests.retain(|request| request.id != id))
    }

    fn change(&self, edit: impl FnOnce(&mut File)) -> std::io::Result<()> {
        let mut file = self.lock();
        edit(&mut file);
        self.version.fetch_add(1, Ordering::Relaxed);
        store::save(&self.path, &*file)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, File> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A contact's name, trimmed and bounded, falling back to the start of the ID.
fn clean_name(name: &str, id: &str) -> String {
    let name: String = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let name: String = name.chars().take(NAME_LIMIT).collect();
    if name.is_empty() {
        id.chars().take(8).collect()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pilus-contacts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn contacts_survive_a_restart() {
        let dir = scratch("restart");
        let id = "a".repeat(64);
        let directory = Directory::open(&dir);
        directory.add(&id, "  Rina   P ", None).unwrap();
        let again = Directory::open(&dir);
        assert_eq!(again.contact(&id).unwrap().name, "Rina P");
        assert!(again.is_contact(&id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stranger_is_a_request_until_added_and_never_a_contact() {
        let dir = scratch("knock");
        let id = "b".repeat(64);
        let directory = Directory::open(&dir);
        assert!(directory.knock(&id), "the first knock is news");
        assert!(!directory.knock(&id), "the second is not");
        assert!(!directory.is_contact(&id));
        assert_eq!(directory.requests().len(), 1);
        directory.add(&id, "", None).unwrap();
        assert!(directory.requests().is_empty());
        assert_eq!(directory.contact(&id).unwrap().name, "bbbbbbbb");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_contact_can_be_renamed() {
        let dir = scratch("rename");
        let id = "e".repeat(64);
        let directory = Directory::open(&dir);
        directory
            .add(&id, "Rina", Some("ticket".to_string()))
            .unwrap();
        assert!(directory.rename(&id, "  Rina  P ").unwrap());
        assert!(!directory.rename(&"f".repeat(64), "x").unwrap());
        let again = Directory::open(&dir).contact(&id).unwrap();
        assert_eq!(again.name, "Rina P");
        assert_eq!(
            again.ticket.as_deref(),
            Some("ticket"),
            "a rename keeps the ticket"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_daemon_is_a_contact_only_after_the_person_accepts_and_is_never_a_person() {
        let dir = scratch("daemon");
        let id = "d".repeat(64);
        let directory = Directory::open(&dir);
        let (first, fresh) = directory.pair(&id, "  Researcher ", Some("ticket".to_string()));
        assert_eq!((first, fresh), (Pairing::Pending, true));
        assert_eq!(
            directory.pair(&id, "Researcher", None),
            (Pairing::Pending, false)
        );
        assert!(!directory.is_daemon(&id), "asking is not being added");
        let request = &directory.requests()[0];
        assert!(request.daemon && request.label.as_deref() == Some("Researcher"));

        assert_eq!(directory.accept(&id).unwrap(), Some(true));
        assert!(directory.requests().is_empty());
        assert!(directory.is_daemon(&id));
        assert_eq!(directory.pair(&id, "", None).0, Pairing::Accepted);
        let kept = Directory::open(&dir).daemons();
        assert_eq!(kept[0].name, "Researcher");
        assert_eq!(kept[0].ticket.as_deref(), Some("ticket"));
        // Nothing that admits a person admits a daemon.
        assert!(!directory.is_contact(&id) && directory.contact(&id).is_none());
        assert!(directory.contacts().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_daemon_can_be_renamed_and_stays_a_daemon() {
        let dir = scratch("rename-daemon");
        let id = "c".repeat(64);
        let directory = Directory::open(&dir);
        directory.pair(&id, "Researcher", None);
        directory.accept(&id).unwrap();
        assert!(directory.rename(&id, "  Chief ").unwrap());
        let again = Directory::open(&dir);
        assert!(again.is_daemon(&id));
        assert_eq!(again.daemons()[0].name, "Chief");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_declined_daemon_is_told_once_and_may_ask_again() {
        let dir = scratch("decline");
        let id = "c".repeat(64);
        let directory = Directory::open(&dir);
        directory.pair(&id, "Researcher", None);
        directory.dismiss(&id).unwrap();
        assert!(directory.requests().is_empty());
        assert_eq!(directory.pair(&id, "Researcher", None).0, Pairing::Declined);
        assert!(
            directory.requests().is_empty(),
            "the decline is not a new ask"
        );
        assert_eq!(directory.pair(&id, "Researcher", None).0, Pairing::Pending);
        assert_eq!(directory.accept(&"9".repeat(64)).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn requests_are_bounded() {
        let dir = scratch("bound");
        let directory = Directory::open(&dir);
        for n in 0..(MAX_REQUESTS + 5) {
            directory.knock(&format!("{n:064}"));
        }
        assert_eq!(directory.requests().len(), MAX_REQUESTS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_file_starts_empty_instead_of_failing() {
        let dir = scratch("broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("contacts.json"), "{ nope").unwrap();
        assert!(Directory::open(&dir).contacts().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
