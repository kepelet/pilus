//! Push alerts for a phone that is not looking.
//!
//! The phone's own watch dies when iOS suspends the app, and the link to this
//! machine is peer to peer, so nothing can reach a sleeping phone from here
//! directly. This module closes the gap in three steps:
//!
//! 1. A phone tells the hub its APNs device token (`hub.push.register`, see
//!    [`crate::hub`]); [`PushStore`] keeps it, per enrolled device, in
//!    `push.json` beside `devices.json`.
//! 2. [`run`] follows this machine's Luvus event feed, but only while a token
//!    is registered, and reads `agent.list` after agent activity. An
//!    [`AttentionWatch`] folds each list into the agents that just started
//!    waiting or finished a turn, the host-side twin of the phone's
//!    `AttentionWatch`.
//! 3. Each such agent becomes one POST to the push relay (the Worker in
//!    `infra/push`), which holds the Apple key and talks to APNs.
//!
//! A saved SSH machine has no event feed (`luvus uhp proxy` carries one request
//! per SSH call), so [`run_remotes`] asks each one for `agent.list` on a slow
//! timer instead, as the desktop app does for a machine it is not showing. The
//! same [`AttentionWatch`] folds the replies, one per machine.
//!
//! The alert carries the agent's name and the session's title, nothing else.
//! A relay that answers 410 means the phone dropped its token, so the
//! registration goes too. Registrations also go when their device is revoked:
//! `devices.json` stays the only authority on who may be reached.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use pilus_client::{ControlClient, SshTarget};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::enroll::{AuthState, write_atomic};
use crate::hub::{LOCAL, gateway_request, subscribe_feed};
use crate::supervisor::GatewayLink;
use crate::watch::topics_for;

/// Where alerts are sent unless [`RELAY_ENV`] says otherwise.
pub const DEFAULT_RELAY: &str = "https://push.usepilus.dev/v1/push";

/// Overrides the relay URL. Set but empty turns push off, and the phone keeps
/// its own local alerts.
pub const RELAY_ENV: &str = "PILUS_PUSH_URL";

/// Events closer together than this are one look at `agent.list`.
const SETTLE: Duration = Duration::from_millis(400);

/// Backoff between attempts to open the event feed.
const RETRY_MIN: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(120);

/// How often a saved SSH machine is asked who is waiting while one of its
/// agents works or waits. Each ask is one SSH call, so this is slower than the
/// desktop app's 5 s; an alert that is ten seconds late still reaches a locked
/// phone.
const REMOTE_POLL: Duration = Duration::from_secs(10);

/// The same, while none of its agents works or waits. A block can only follow a
/// start, and a start is seen on the next ask, so this costs a late alert at
/// worst and keeps an idle machine, and a laptop host, from waking every ten
/// seconds all day.
const REMOTE_IDLE_POLL: Duration = Duration::from_secs(30);

/// The slowest an unreachable machine is asked. A machine that is asleep or
/// off the network is normal, so it backs off instead of giving up.
const REMOTE_RETRY_MAX: Duration = Duration::from_secs(300);

/// How often the saved machines are listed again, to follow one being added,
/// disabled or repointed.
const CATALOG_EVERY: Duration = Duration::from_secs(60);

/// A relay that does not answer in this long has failed. An alert that late is
/// not worth holding the next one up for.
const SEND_TIMEOUT: Duration = Duration::from_secs(15);

/// The relay to use, or `None` when push is turned off.
pub fn relay_url() -> Option<String> {
    match std::env::var(RELAY_ENV) {
        Ok(url) => {
            let url = url.trim();
            (!url.is_empty()).then(|| url.to_string())
        }
        Err(_) => Some(DEFAULT_RELAY.to_string()),
    }
}

/// An APNs device token as the phone spells it: lowercase hex. Apple may
/// lengthen tokens, so the bound is loose; the relay applies the same one.
pub fn valid_token(token: &str) -> bool {
    (64..=200).contains(&token.len())
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// One device's push address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Registration {
    pub token: String,
    /// Whether the token came from the APNs sandbox. The relay treats this as
    /// a hint and tries the other environment when it is wrong.
    pub sandbox: bool,
}

/// The persisted push addresses, keyed by the device's endpoint id.
///
/// Losing the file costs nothing: a phone registers again on its next
/// connect. That is why a file that does not decode starts an empty store
/// instead of failing the daemon, unlike `devices.json`.
#[derive(Debug)]
pub struct PushStore {
    path: PathBuf,
    entries: Mutex<BTreeMap<String, Registration>>,
    /// Wakes whoever waits in [`PushStore::registered`] when a registration arrives.
    wake: Notify,
}

impl PushStore {
    pub fn load(path: PathBuf) -> Self {
        let entries = match std::fs::read(&path) {
            Ok(bytes) => parse(&bytes).unwrap_or_else(|| {
                eprintln!("pilus-agent: ignoring unreadable push registrations");
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        };
        Self {
            path,
            entries: Mutex::new(entries),
            wake: Notify::new(),
        }
    }

    /// Remember `token` for `device`, replacing any earlier one.
    pub fn register(&self, device: &str, token: &str, sandbox: bool) -> Result<(), &'static str> {
        if !valid_token(token) {
            return Err("that is not a device token");
        }
        let mut entries = lock(&self.entries);
        let mut candidate = entries.clone();
        candidate.insert(
            device.to_string(),
            Registration {
                token: token.to_string(),
                sandbox,
            },
        );
        self.save(&candidate)
            .map_err(|_| "could not save the registration")?;
        *entries = candidate;
        drop(entries);
        self.wake.notify_waiters();
        Ok(())
    }

    /// Forget `device`'s token. Returns whether it had one.
    pub fn unregister(&self, device: &str) -> bool {
        let mut entries = lock(&self.entries);
        let mut candidate = entries.clone();
        if candidate.remove(device).is_none() {
            return false;
        }
        // A failed write leaves the file stale but the memory right, and the
        // stale entry is dropped again the next time it fails to deliver.
        if self.save(&candidate).is_err() {
            eprintln!("pilus-agent: could not save push registrations");
        }
        *entries = candidate;
        true
    }

    /// Forget every device `keep` rejects, such as the revoked ones.
    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        let stale: Vec<String> = lock(&self.entries)
            .keys()
            .filter(|device| !keep(device))
            .cloned()
            .collect();
        for device in stale {
            self.unregister(&device);
        }
    }

    pub fn snapshot(&self) -> Vec<(String, Registration)> {
        lock(&self.entries)
            .iter()
            .map(|(device, registration)| (device.clone(), registration.clone()))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        lock(&self.entries).is_empty()
    }

    /// Resolve once at least one device is registered. [`run`] and
    /// [`run_remotes`] both wait here, so the registration wakes every waiter.
    async fn registered(&self) {
        loop {
            // Enabled before the check, so a registration landing between the
            // check and the await is not lost: `notify_waiters` keeps no permit.
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.is_empty() {
                return;
            }
            notified.await;
        }
    }

    fn save(&self, entries: &BTreeMap<String, Registration>) -> crate::Result<()> {
        let list: Vec<Value> = entries
            .iter()
            .map(|(device, registration)| {
                json!({
                    "device": device,
                    "token": registration.token,
                    "sandbox": registration.sandbox,
                })
            })
            .collect();
        write_atomic(
            &self.path,
            json!({ "registrations": list }).to_string().as_bytes(),
        )
    }
}

fn parse(bytes: &[u8]) -> Option<BTreeMap<String, Registration>> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let list = value.get("registrations")?.as_array()?;
    Some(
        list.iter()
            .filter_map(|item| {
                let device = item.get("device")?.as_str()?;
                let token = item.get("token")?.as_str()?;
                if !valid_token(token) {
                    return None;
                }
                let sandbox = item.get("sandbox").and_then(Value::as_bool) == Some(true);
                Some((
                    device.to_string(),
                    Registration {
                        token: token.to_string(),
                        sandbox,
                    },
                ))
            })
            .collect(),
    )
}

/// What an alert is about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    /// The agent started waiting on the user.
    Blocked,
    /// The agent finished a turn it was running.
    Finished,
}

/// An agent that has just started waiting on the user, or just finished.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Alert {
    /// Stable per pane and kind. The phone uses it to replace an older alert
    /// and to take the alert down once the agent moves on, so it must match
    /// the tag in `AttentionWatch.swift`.
    pub tag: String,
    pub title: String,
    pub body: String,
}

impl Alert {
    fn from_agent(kind: Kind, machine: &str, pane: &str, agent: &Value) -> Self {
        let agent_kind = text(agent, "agent");
        let who = if agent_kind.is_empty() {
            "An agent".to_string()
        } else {
            pilus_client::kind_label(&agent_kind)
        };
        // The session's own title when the agent reports one, then the pane's
        // alias, then the workspace: the same order the phone lists them in.
        let session = ["agent_session_title", "name", "workspace_name"]
            .iter()
            .map(|key| text(agent, key))
            .find(|value| !value.is_empty())
            .unwrap_or_default();
        let title = match kind {
            Kind::Blocked => format!("{who} is waiting your input"),
            Kind::Finished => format!("{who} finished"),
        };
        Self {
            tag: tag_for(kind, machine, pane),
            title,
            body: session,
        }
    }
}

/// The collapse id for one pane, the tag `AttentionWatch.swift` builds too.
/// Blocked and finished alerts have their own, so one never replaces the other.
fn tag_for(kind: Kind, machine: &str, pane: &str) -> String {
    let word = match kind {
        Kind::Blocked => "blocked",
        Kind::Finished => "done",
    };
    let mut tag = format!("pilus-{word}-{machine}-{pane}");
    // `apns-collapse-id` allows 64 bytes. A machine id is the user's to name,
    // so cut on a character boundary rather than panic in the middle of one.
    let mut end = tag.len().min(64);
    while !tag.is_char_boundary(end) {
        end -= 1;
    }
    tag.truncate(end);
    tag
}

/// Spots the moment an agent starts waiting or finishes, from successive
/// `agent.list` replies. It touches no network, so the rules are testable on
/// their own.
#[derive(Debug)]
pub struct AttentionWatch {
    /// The machine the lists come from; it names the alert's tag.
    machine: String,
    /// An agent already blocked or done when the first list arrives is old
    /// news, not an alert.
    seeded: bool,
    /// Last seen status per pane.
    seen: BTreeMap<String, String>,
}

impl Default for AttentionWatch {
    fn default() -> Self {
        Self::new(LOCAL)
    }
}

impl AttentionWatch {
    pub fn new(machine: impl Into<String>) -> Self {
        Self {
            machine: machine.into(),
            seeded: false,
            seen: BTreeMap::new(),
        }
    }

    /// Fold one `agent.list` reply and return the agents that newly block or
    /// finish. Only a turn that was running counts as finishing: an agent that
    /// first shows up `done` is history.
    pub fn observe(&mut self, list: &Value) -> Vec<Alert> {
        let agents = list
            .get("agents")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let mut now = BTreeMap::new();
        let mut fresh = Vec::new();
        for agent in agents {
            let pane = text(agent, "pane");
            if pane.is_empty() {
                continue;
            }
            let status = text(agent, "status");
            if self.seeded {
                let was = self.seen.get(&pane).map(String::as_str);
                if status == "blocked" && was != Some("blocked") {
                    fresh.push(Alert::from_agent(Kind::Blocked, &self.machine, &pane, agent));
                } else if status == "done" && was == Some("working") {
                    fresh.push(Alert::from_agent(Kind::Finished, &self.machine, &pane, agent));
                }
            }
            now.insert(pane, status);
        }
        self.seen = now;
        self.seeded = true;
        fresh
    }

    /// Forget everything, so the next list is old news again.
    pub fn reset(&mut self) {
        self.seeded = false;
        self.seen.clear();
    }
}

/// A field as text, whether the daemon sent a string or a number.
fn text(value: &Value, key: &str) -> String {
    match value.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    }
}

/// What the relay said about one alert.
#[derive(Debug, Eq, PartialEq)]
enum Delivery {
    Sent,
    /// The phone no longer has this token.
    Gone,
    Failed(String),
}

/// POST one alert to the relay. Blocking: callers run it off the reactor.
fn post(relay: &str, registration: &Registration, alert: &Alert) -> Delivery {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(SEND_TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into();
    let body = json!({
        "token": registration.token,
        "sandbox": registration.sandbox,
        "title": alert.title,
        "body": alert.body,
        "collapse_id": alert.tag,
    });
    match agent.post(relay).send_json(&body) {
        Ok(response) => match response.status().as_u16() {
            200..=299 => Delivery::Sent,
            410 => Delivery::Gone,
            status => Delivery::Failed(format!("the relay answered {status}")),
        },
        Err(error) => Delivery::Failed(error.to_string()),
    }
}

/// Send `alert` to every registered device that is still paired.
async fn notify(store: &Arc<PushStore>, auth: &AuthState, relay: &str, alert: &Alert) {
    store.retain(|device| auth.level_of(device).is_some());
    for (device, registration) in store.snapshot() {
        let (relay, alert) = (relay.to_string(), alert.clone());
        let outcome = tokio::task::spawn_blocking(move || post(&relay, &registration, &alert))
            .await
            .unwrap_or_else(|error| Delivery::Failed(error.to_string()));
        match outcome {
            Delivery::Sent => {}
            Delivery::Gone => {
                store.unregister(&device);
            }
            Delivery::Failed(why) => eprintln!("pilus-agent: push to {device} failed: {why}"),
        }
    }
}

/// How one pass over the event feed ended.
enum Ended {
    /// Nobody is registered any more, so there is nothing to follow.
    Idle,
    /// The feed was open and then closed.
    Lost,
    /// The feed could not be opened.
    Unavailable(String),
}

/// Watch for agents that start waiting and push each one, for as long as the
/// daemon runs. The event feed is held only while a device is registered, so
/// an unused feature costs no connection and no wakeups.
pub async fn run(
    link: Arc<GatewayLink>,
    auth: Arc<AuthState>,
    store: Arc<PushStore>,
    relay: String,
) {
    let mut watch = AttentionWatch::default();
    let mut delay = RETRY_MIN;
    loop {
        if store.is_empty() {
            store.registered().await;
            watch.reset();
            delay = RETRY_MIN;
        }
        match follow(&link, &auth, &store, &relay, &mut watch).await {
            Ended::Idle => continue,
            Ended::Lost => delay = RETRY_MIN,
            Ended::Unavailable(why) => {
                eprintln!("pilus-agent: push cannot follow the event feed: {why}");
                delay = (delay * 2).min(RETRY_MAX);
            }
        }
        tokio::time::sleep(delay).await;
    }
}

async fn follow(
    link: &GatewayLink,
    auth: &Arc<AuthState>,
    store: &Arc<PushStore>,
    relay: &str,
    watch: &mut AttentionWatch,
) -> Ended {
    let mut feed = match subscribe_feed(link, auth).await {
        Ok(feed) => feed,
        Err(error) => {
            let why = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the gateway refused the event feed");
            return Ended::Unavailable(why.to_string());
        }
    };
    // Read the list once up front: it seeds a fresh watch, and after a gap it
    // catches an agent that blocked while the feed was down.
    look(link, auth, store, relay, watch).await;
    loop {
        let Some(line) = feed.recv().await else {
            return Ended::Lost;
        };
        if store.is_empty() {
            return Ended::Idle;
        }
        if !concerns_agents(&line) {
            continue;
        }
        // A status change arrives with a burst of siblings; one look covers them.
        let settled = tokio::time::sleep(SETTLE);
        tokio::pin!(settled);
        loop {
            tokio::select! {
                () = &mut settled => break,
                more = feed.recv() => if more.is_none() { return Ended::Lost },
            }
        }
        look(link, auth, store, relay, watch).await;
    }
}

/// Read `agent.list` and push whatever newly blocked or finished.
async fn look(
    link: &GatewayLink,
    auth: &Arc<AuthState>,
    store: &Arc<PushStore>,
    relay: &str,
    watch: &mut AttentionWatch,
) {
    let Ok(list) = gateway_request(link, auth, "agent.list", json!({})).await else {
        return;
    };
    for alert in watch.observe(&list) {
        notify(store, auth, relay, &alert).await;
    }
}

/// Whether a feed line is about agents. The feed is mostly terminal output,
/// which says nothing about who is waiting.
fn concerns_agents(line: &str) -> bool {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    let Some(name) = event.get("event").and_then(Value::as_str) else {
        return false;
    };
    name.ends_with("resync_required") || topics_for(name).contains(&"agents")
}

/// How long to wait before asking a machine again, given its last `agent.list`.
fn next_poll(list: &Value) -> Duration {
    let active = list
        .get("agents")
        .and_then(Value::as_array)
        .is_some_and(|agents| {
            agents
                .iter()
                .any(|agent| matches!(text(agent, "status").as_str(), "working" | "blocked"))
        });
    if active {
        REMOTE_POLL
    } else {
        REMOTE_IDLE_POLL
    }
}

/// A machine's poll task, stopped when it is dropped so that dropping
/// [`run_remotes`] stops them all.
struct Poller(JoinHandle<()>);

impl Drop for Poller {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Watch every saved SSH machine for agents that start waiting, for as long as
/// the daemon runs. Like [`run`], it costs nothing until a device registers:
/// no catalog read, no SSH call. The catalog is read again every
/// [`CATALOG_EVERY`], and only machines the hub itself would reach (enabled,
/// with a verified remote binary) are polled.
pub async fn run_remotes(
    auth: Arc<AuthState>,
    store: Arc<PushStore>,
    relay: String,
    session: Option<String>,
) {
    let mut pollers: BTreeMap<String, (SshTarget, Poller)> = BTreeMap::new();
    loop {
        if store.is_empty() {
            pollers.clear();
            store.registered().await;
        }
        let catalog = {
            let session = session.clone();
            tokio::task::spawn_blocking(move || {
                pilus_client::list_machines(&pilus_client::config_dir(), session.as_deref())
            })
            .await
            .unwrap_or_default()
        };
        let wanted: BTreeMap<String, SshTarget> = catalog
            .into_iter()
            .filter(|machine| machine.id != LOCAL && machine.enabled)
            .filter_map(|machine| Some((machine.id.clone(), machine.ssh_target()?)))
            .collect();
        // A poller that returned (nobody was registered) is started afresh.
        pollers.retain(|id, (target, poller)| {
            !poller.0.is_finished() && wanted.get(id) == Some(target)
        });
        for (id, target) in wanted {
            pollers.entry(id.clone()).or_insert_with(|| {
                let task = tokio::spawn(poll_machine(
                    id,
                    target.clone(),
                    Arc::clone(&auth),
                    Arc::clone(&store),
                    relay.clone(),
                ));
                (target, Poller(task))
            });
        }
        tokio::time::sleep(CATALOG_EVERY).await;
    }
}

/// Ask one machine for `agent.list` until nobody is registered, and push what
/// newly blocks. The first reply only seeds the watch.
async fn poll_machine(
    machine: String,
    target: SshTarget,
    auth: Arc<AuthState>,
    store: Arc<PushStore>,
    relay: String,
) {
    let mut watch = AttentionWatch::new(machine.clone());
    let mut delay = REMOTE_POLL;
    let mut unreachable = false;
    while !store.is_empty() {
        let probe = target.clone();
        let asked = tokio::task::spawn_blocking(move || {
            ControlClient::connect_ssh(probe)?.request("agent.list", json!({}))
        })
        .await;
        match asked {
            Ok(Ok(list)) => {
                unreachable = false;
                delay = next_poll(&list);
                for alert in watch.observe(&list) {
                    notify(&store, &auth, &relay, &alert).await;
                }
            }
            failed => {
                // Said once per outage, not once per try.
                if !unreachable {
                    let why = match failed {
                        Ok(Err(error)) => error.to_string(),
                        Err(error) => error.to_string(),
                        Ok(Ok(_)) => String::new(),
                    };
                    eprintln!("pilus-agent: push cannot reach machine {machine}: {why}");
                }
                unreachable = true;
                delay = (delay * 2).min(REMOTE_RETRY_MAX);
            }
        }
        tokio::time::sleep(delay).await;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use std::io::{Read, Write};

    const TOKEN: &str = "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12";

    fn agent(pane: &str, status: &str) -> Value {
        json!({
            "pane": pane,
            "agent": "claude",
            "name": "fixer",
            "agent_session_title": "fix the flaky test",
            "status": status,
            "workspace_name": "pilus",
        })
    }

    fn list(agents: &[Value]) -> Value {
        json!({ "agents": agents })
    }

    #[test]
    fn an_agent_blocked_at_the_first_look_is_old_news() {
        let mut watch = AttentionWatch::default();
        assert!(watch.observe(&list(&[agent("1", "blocked")])).is_empty());
        assert!(watch.observe(&list(&[agent("1", "blocked")])).is_empty());
    }

    #[test]
    fn a_newly_blocked_agent_is_one_alert_with_the_agreed_text() {
        let mut watch = AttentionWatch::default();
        watch.observe(&list(&[agent("1", "working")]));
        let fresh = watch.observe(&list(&[agent("1", "blocked")]));
        assert_eq!(
            fresh,
            [Alert {
                tag: "pilus-blocked-local-1".to_string(),
                title: "Claude is waiting your input".to_string(),
                body: "fix the flaky test".to_string(),
            }]
        );
        assert!(
            watch.observe(&list(&[agent("1", "blocked")])).is_empty(),
            "still blocked is not a second alert"
        );
    }

    #[test]
    fn unblocking_then_blocking_again_alerts_again() {
        let mut watch = AttentionWatch::default();
        watch.observe(&list(&[]));
        assert_eq!(watch.observe(&list(&[agent("1", "blocked")])).len(), 1);
        assert!(watch.observe(&list(&[agent("1", "working")])).is_empty());
        assert_eq!(watch.observe(&list(&[agent("1", "blocked")])).len(), 1);
    }

    #[test]
    fn a_pane_that_vanished_while_blocked_is_forgotten() {
        let mut watch = AttentionWatch::default();
        watch.observe(&list(&[]));
        watch.observe(&list(&[agent("1", "blocked")]));
        watch.observe(&list(&[]));
        assert_eq!(watch.observe(&list(&[agent("1", "blocked")])).len(), 1);
    }

    #[test]
    fn the_body_falls_back_from_session_title_to_alias_to_workspace() {
        let mut bare = agent("1", "blocked");
        bare["agent_session_title"] = json!("");
        assert_eq!(Alert::from_agent(Kind::Blocked, "local", "1", &bare).body, "fixer");
        bare["name"] = Value::Null;
        assert_eq!(Alert::from_agent(Kind::Blocked, "local", "1", &bare).body, "pilus");
    }

    #[test]
    fn numeric_pane_ids_and_unknown_kinds_still_read_well() {
        let mut watch = AttentionWatch::default();
        watch.observe(&list(&[]));
        let mut odd = agent("", "blocked");
        odd["pane"] = json!(7);
        odd["agent"] = json!("opencode");
        let fresh = watch.observe(&list(&[odd]));
        assert_eq!(fresh[0].tag, "pilus-blocked-local-7");
        assert_eq!(fresh[0].title, "OpenCode is waiting your input");
    }

    #[test]
    fn a_turn_that_was_running_and_is_done_is_one_finished_alert() {
        let mut watch = AttentionWatch::default();
        watch.observe(&list(&[agent("1", "working")]));
        let fresh = watch.observe(&list(&[agent("1", "done")]));
        assert_eq!(
            fresh,
            [Alert {
                tag: "pilus-done-local-1".to_string(),
                title: "Claude finished".to_string(),
                body: "fix the flaky test".to_string(),
            }]
        );
        assert!(
            watch.observe(&list(&[agent("1", "done")])).is_empty(),
            "still done is not a second alert"
        );
        // The next turn finishing alerts again.
        watch.observe(&list(&[agent("1", "working")]));
        assert_eq!(watch.observe(&list(&[agent("1", "done")])).len(), 1);
    }

    #[test]
    fn an_agent_first_seen_done_or_idle_then_done_is_not_a_finish() {
        let mut watch = AttentionWatch::default();
        assert!(watch.observe(&list(&[agent("1", "done")])).is_empty());
        assert!(watch.observe(&list(&[agent("2", "done")])).is_empty());
        watch.observe(&list(&[agent("3", "idle")]));
        assert!(watch.observe(&list(&[agent("3", "done")])).is_empty());
        // A pane that appears already done after the first look is history too.
        assert!(watch.observe(&list(&[agent("4", "done")])).is_empty());
    }

    #[test]
    fn blocked_and_finished_alerts_never_share_a_tag() {
        let blocked = tag_for(Kind::Blocked, "local", "1");
        let finished = tag_for(Kind::Finished, "local", "1");
        assert_eq!(blocked, "pilus-blocked-local-1");
        assert_eq!(finished, "pilus-done-local-1");
        assert_ne!(blocked, finished);
    }

    #[test]
    fn a_remote_machine_names_its_own_tag() {
        let mut watch = AttentionWatch::new("mac-mini");
        assert!(watch.observe(&list(&[agent("4", "working")])).is_empty());
        let fresh = watch.observe(&list(&[agent("4", "blocked")]));
        assert_eq!(fresh[0].tag, "pilus-blocked-mac-mini-4");
        // Two machines may use the same pane id without sharing an alert.
        let mut other = AttentionWatch::new("studio");
        other.observe(&list(&[agent("4", "working")]));
        assert_ne!(other.observe(&list(&[agent("4", "blocked")]))[0].tag, fresh[0].tag);
    }

    #[test]
    fn a_long_or_non_ascii_tag_is_cut_on_a_character_boundary() {
        let tag = tag_for(Kind::Blocked, "ünïcödé-machine-with-a-long-name-ünïcödé", "pane-12345678");
        assert!(tag.len() <= 64);
        assert!(tag.starts_with("pilus-blocked-ünïcödé"));
        assert_eq!(tag_for(Kind::Blocked, "mac-mini", "7"), "pilus-blocked-mac-mini-7");
    }

    #[test]
    fn a_machine_is_asked_often_only_while_an_agent_works_or_waits() {
        assert_eq!(next_poll(&list(&[agent("1", "working")])), REMOTE_POLL);
        assert_eq!(
            next_poll(&list(&[agent("1", "done"), agent("2", "blocked")])),
            REMOTE_POLL
        );
        assert_eq!(next_poll(&list(&[agent("1", "done")])), REMOTE_IDLE_POLL);
        assert_eq!(next_poll(&list(&[])), REMOTE_IDLE_POLL);
        assert_eq!(next_poll(&json!({})), REMOTE_IDLE_POLL);
    }

    #[test]
    fn only_agent_events_trigger_a_look() {
        assert!(concerns_agents(r#"{"event":"pane.agent_status_changed"}"#));
        assert!(concerns_agents(r#"{"event":"events.resync_required"}"#));
        assert!(!concerns_agents(r#"{"event":"terminal.output_ready"}"#));
        assert!(!concerns_agents(r#"{"event":"task.updated"}"#));
        assert!(!concerns_agents("not json"));
    }

    #[test]
    fn tokens_are_lowercase_hex_of_a_sane_length() {
        assert!(valid_token(TOKEN));
        assert!(!valid_token(&TOKEN.to_uppercase()));
        assert!(!valid_token(&TOKEN[..63]));
        assert!(!valid_token(&format!("{TOKEN}zz")));
        assert!(!valid_token(&"a".repeat(201)));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pilus-agent-push-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir.join("push.json")
    }

    #[test]
    fn registrations_survive_a_restart_and_unregister_removes_them() {
        let path = scratch("persist");
        let store = PushStore::load(path.clone());
        assert!(store.register("phone", "not a token", false).is_err());
        store.register("phone", TOKEN, true).expect("register");
        let again = PushStore::load(path.clone());
        assert_eq!(
            again.snapshot(),
            [(
                "phone".to_string(),
                Registration {
                    token: TOKEN.to_string(),
                    sandbox: true
                }
            )]
        );
        assert!(again.unregister("phone"));
        assert!(!again.unregister("phone"));
        assert!(PushStore::load(path).is_empty());
    }

    #[tokio::test]
    async fn one_registration_wakes_every_waiter() {
        let store = Arc::new(PushStore::load(scratch("waiters")));
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let store = Arc::clone(&store);
                tokio::spawn(async move { store.registered().await })
            })
            .collect();
        // Let both reach the wait before the registration lands.
        tokio::time::sleep(Duration::from_millis(50)).await;
        store.register("phone", TOKEN, false).expect("register");
        for waiter in waiters {
            tokio::time::timeout(Duration::from_secs(2), waiter)
                .await
                .expect("woken")
                .expect("joined");
        }
    }

    #[test]
    fn a_revoked_device_loses_its_registration() {
        let store = PushStore::load(scratch("revoke"));
        store.register("phone", TOKEN, false).expect("register");
        store.register("tablet", TOKEN, false).expect("register");
        store.retain(|device| device == "phone");
        assert_eq!(store.snapshot().len(), 1);
    }

    #[test]
    fn a_damaged_file_starts_empty_instead_of_failing() {
        let path = scratch("damaged");
        std::fs::write(&path, b"{ nope").expect("write");
        assert!(PushStore::load(path).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_registration_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch("mode");
        let store = PushStore::load(path.clone());
        store.register("phone", TOKEN, false).expect("register");
        let mode = std::fs::metadata(path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// A relay that answers one request with `status`, and hands back what it read.
    fn fake_relay(status: u16) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/v1/push", listener.local_addr().expect("addr"));
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let read = stream.read(&mut chunk).expect("read");
                request.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&request);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
            }
            write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            )
            .expect("reply");
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, server)
    }

    #[test]
    fn the_relay_gets_the_token_the_text_and_the_collapse_id() {
        let (url, server) = fake_relay(200);
        let registration = Registration {
            token: TOKEN.to_string(),
            sandbox: false,
        };
        let alert = Alert::from_agent(Kind::Blocked, "local", "3", &agent("3", "blocked"));
        assert_eq!(post(&url, &registration, &alert), Delivery::Sent);
        let request = server.join().expect("server");
        let body: Value =
            serde_json::from_str(request.split_once("\r\n\r\n").expect("body").1).expect("json");
        assert_eq!(body["token"], TOKEN);
        assert_eq!(body["title"], "Claude is waiting your input");
        assert_eq!(body["body"], "fix the flaky test");
        assert_eq!(body["collapse_id"], "pilus-blocked-local-3");
    }

    #[test]
    fn a_410_means_the_phone_is_gone_and_other_failures_are_not() {
        let registration = Registration {
            token: TOKEN.to_string(),
            sandbox: false,
        };
        let alert = Alert::from_agent(Kind::Blocked, "local", "3", &agent("3", "blocked"));
        let (url, server) = fake_relay(410);
        assert_eq!(post(&url, &registration, &alert), Delivery::Gone);
        server.join().expect("server");
        let (url, server) = fake_relay(502);
        assert!(matches!(
            post(&url, &registration, &alert),
            Delivery::Failed(_)
        ));
        server.join().expect("server");
    }
}
