//! Live "something changed" signals for a paired device.
//!
//! A phone that polls pays for every look and sees changes late. The hub's
//! streams carry one request and one reply, so `hub.watch` is the one method
//! that keeps its stream open: it subscribes to this machine's Luvus event feed
//! and tells the device *that* something changed and *which lists* to refetch,
//! never *what* changed.
//!
//! Carrying topics instead of events is the point. An event holds a task's
//! prompt or a pane's cwd, and forwarding it would make this stream a second,
//! unaudited way to read everything. A topic signal reveals nothing, so any
//! enrolled device may open one; the device then refetches through the ordinary
//! calls, where its level and the gateway's allowlist apply as always.
//!
//! The feed is also noisy (`terminal.output_ready` fires on every burst of
//! output) and the gateway token is rate limited across all devices, so signals
//! are coalesced into at most one per [`FLUSH`], and a device holds a few
//! streams at most.
//!
//! Terminal output is the one event a device does care about when it is looking
//! at an agent's screen, so it becomes the `output` topic plus the ids of the
//! panes that produced it. That says *where* something was written, not what, and
//! the device reads the pane itself through `agent.read`, which its level governs.
//!
//! Frames, one JSON line each: `{"event":"changed","topics":[…],"panes":[…],"seq":N}`,
//! `{"event":"ping"}` while idle, and `{"event":"closed","reason":…}` last.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

/// Signals closer together than this are one.
pub const FLUSH: Duration = Duration::from_millis(300);
/// How often an idle stream proves it is alive, and the device is rechecked.
pub const HEARTBEAT: Duration = Duration::from_secs(20);
/// Streams open at once across every device.
const MAX_TOTAL: usize = 8;
/// Streams one device may hold. A reconnect briefly overlaps its predecessor.
const MAX_PER_DEVICE: usize = 2;

/// More panes than this in one signal are cut off; a device reading that many
/// at once is better served by refetching everything.
const MAX_PANES: usize = 32;

/// The lists an event invalidates. Empty for events a device never refetches
/// for, such as a terminal being created.
pub fn topics_for(event: &str) -> &'static [&'static str] {
    if event == "terminal.output_ready" {
        return &["output"];
    }
    let family = event.split('.').next().unwrap_or_default();
    match family {
        "task" => &["tasks", "mission"],
        "agent" | "pane" => &["agents", "mission"],
        "workspace" | "tab" => &["workspaces"],
        _ => &[],
    }
}

/// Who holds how many streams.
#[derive(Debug)]
pub struct Slots {
    total: Arc<Semaphore>,
    per_device: Arc<Mutex<HashMap<String, usize>>>,
}

impl Default for Slots {
    fn default() -> Self {
        Self {
            total: Arc::new(Semaphore::new(MAX_TOTAL)),
            per_device: Arc::default(),
        }
    }
}

/// A held stream. Dropping it frees the slot.
#[derive(Debug)]
pub struct Slot {
    _permit: OwnedSemaphorePermit,
    per_device: Arc<Mutex<HashMap<String, usize>>>,
    device: String,
}

impl Slots {
    /// Take a slot for `device`, or say why not.
    pub fn acquire(&self, device: &str) -> Result<Slot, &'static str> {
        let permit = Arc::clone(&self.total)
            .try_acquire_owned()
            .map_err(|_| "too many live streams are open on this machine")?;
        let mut held = lock(&self.per_device);
        let count = held.entry(device.to_string()).or_default();
        if *count >= MAX_PER_DEVICE {
            return Err("this device already holds its live streams");
        }
        *count += 1;
        drop(held);
        Ok(Slot {
            _permit: permit,
            per_device: Arc::clone(&self.per_device),
            device: device.to_string(),
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut held = lock(&self.per_device);
        if let Some(count) = held.get_mut(&self.device) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                held.remove(&self.device);
            }
        }
    }
}

/// Relay `lines` (the gateway's event feed) to `out` as topic signals until one
/// side ends, and say why it ended.
///
/// `gone` resolves when the device stops listening; `alive` is asked on every
/// heartbeat and ends the stream when it turns false, which is how a revoked
/// device's open stream is cut.
pub async fn relay<W, G>(
    mut lines: mpsc::Receiver<String>,
    out: &mut W,
    flush: Duration,
    heartbeat: Duration,
    gone: G,
    alive: impl Fn() -> bool,
) -> &'static str
where
    W: AsyncWrite + Unpin,
    G: Future<Output = ()>,
{
    tokio::pin!(gone);
    let mut beat = tokio::time::interval(heartbeat);
    beat.tick().await;
    let mut pending: BTreeSet<&'static str> = BTreeSet::new();
    let mut panes: BTreeSet<String> = BTreeSet::new();
    let mut sequence = 0u64;
    let flush_timer = tokio::time::sleep(Duration::from_secs(3600));
    tokio::pin!(flush_timer);
    let mut armed = false;
    loop {
        tokio::select! {
            () = &mut gone => return "the device went away",
            line = lines.recv() => {
                let Some(line) = line else { return "the machine's event feed ended" };
                let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                let Some(name) = event.get("event").and_then(serde_json::Value::as_str) else { continue };
                if let Some(seen) = event.get("sequence").and_then(serde_json::Value::as_u64) {
                    sequence = sequence.max(seen);
                }
                // The feed lost events; every list may be stale.
                let topics: &[&str] = if name.ends_with("resync_required") {
                    &["tasks", "agents", "workspaces", "mission"]
                } else {
                    topics_for(name)
                };
                pending.extend(topics.iter().copied());
                if name == "terminal.output_ready"
                    && panes.len() < MAX_PANES
                    && let Some(pane) = event.pointer("/data/pane_id").and_then(serde_json::Value::as_str)
                {
                    panes.insert(pane.to_string());
                }
                if !pending.is_empty() && !armed {
                    armed = true;
                    flush_timer.as_mut().reset(tokio::time::Instant::now() + flush);
                }
            }
            () = &mut flush_timer, if armed => {
                armed = false;
                let topics: Vec<_> = std::mem::take(&mut pending).into_iter().collect();
                let mut frame = serde_json::json!({ "event": "changed", "topics": topics, "seq": sequence });
                let seen = std::mem::take(&mut panes);
                if !seen.is_empty() {
                    frame["panes"] = serde_json::json!(seen);
                }
                if send(out, &frame).await.is_err() {
                    return "the device went away";
                }
            }
            _ = beat.tick() => {
                if !alive() {
                    return "this device is no longer paired";
                }
                if send(out, &serde_json::json!({ "event": "ping" })).await.is_err() {
                    return "the device went away";
                }
            }
        }
    }
}

/// Write one frame.
pub async fn send<W: AsyncWrite + Unpin>(
    out: &mut W,
    frame: &serde_json::Value,
) -> std::io::Result<()> {
    out.write_all(frame.to_string().as_bytes()).await?;
    out.write_all(b"\n").await?;
    out.flush().await
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    fn feed(lines: &[&str]) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel(64);
        for line in lines {
            tx.try_send((*line).to_string()).expect("queue");
        }
        // Keep the feed open: the relay should end for the reason a test names.
        std::mem::forget(tx);
        rx
    }

    /// Run a relay over `lines` for `run`, returning every frame it wrote.
    async fn frames(
        lines: mpsc::Receiver<String>,
        run: Duration,
        alive: impl Fn() -> bool,
    ) -> (Vec<serde_json::Value>, Option<&'static str>) {
        let (mut ours, theirs) = tokio::io::duplex(64 * 1024);
        let ended = tokio::time::timeout(
            run,
            relay(
                lines,
                &mut ours,
                Duration::from_millis(40),
                Duration::from_millis(200),
                std::future::pending(),
                alive,
            ),
        )
        .await
        .ok();
        drop(ours);
        let mut out = Vec::new();
        let mut reader = BufReader::new(theirs).lines();
        while let Some(line) = reader.next_line().await.expect("read") {
            out.push(serde_json::from_str(&line).expect("json"));
        }
        (out, ended)
    }

    #[test]
    fn terminal_output_signals_only_where_and_other_terminal_events_do_not() {
        assert_eq!(topics_for("terminal.output_ready"), ["output"]);
        assert!(topics_for("terminal.created").is_empty());
        assert!(topics_for("terminal.frame").is_empty());
        assert!(topics_for("subscription_started").is_empty());
        assert_eq!(topics_for("task.updated"), ["tasks", "mission"]);
        assert_eq!(
            topics_for("pane.agent_status_changed"),
            ["agents", "mission"]
        );
        assert_eq!(topics_for("tab.created"), ["workspaces"]);
    }

    #[tokio::test]
    async fn a_burst_is_one_signal_without_payloads() {
        let secret = r#""prompt":"do not leak me""#;
        let burst = [
            format!(r#"{{"event":"task.updated","sequence":5,"data":{{{secret}}}}}"#),
            r#"{"event":"task.updated","sequence":6,"data":{}}"#.to_string(),
            r#"{"event":"pane.agent_status_changed","sequence":7,"data":{}}"#.to_string(),
            r#"{"event":"terminal.output_ready","sequence":8,"data":{"pane_id":"7","text":"do not leak me either"}}"#.to_string(),
        ];
        let refs: Vec<&str> = burst.iter().map(String::as_str).collect();
        let (frames, _) = frames(feed(&refs), Duration::from_millis(150), || true).await;
        let changed: Vec<_> = frames.iter().filter(|f| f["event"] == "changed").collect();
        assert_eq!(changed.len(), 1, "{frames:?}");
        assert_eq!(
            changed[0]["topics"],
            serde_json::json!(["agents", "mission", "output", "tasks"])
        );
        assert_eq!(changed[0]["panes"], serde_json::json!(["7"]));
        assert_eq!(changed[0]["seq"], 8);
        assert!(!serde_json::to_string(&frames).unwrap().contains("leak"));
    }

    #[tokio::test]
    async fn terminal_noise_is_one_output_signal_that_never_refetches_lists() {
        let noise =
            [r#"{"event":"terminal.output_ready","sequence":1,"data":{"pane_id":"3"}}"#; 20];
        let (frames, _) = frames(feed(&noise), Duration::from_millis(500), || true).await;
        let changed: Vec<_> = frames.iter().filter(|f| f["event"] == "changed").collect();
        assert_eq!(changed.len(), 1, "twenty writes, one signal: {frames:?}");
        assert_eq!(changed[0]["topics"], serde_json::json!(["output"]));
        assert_eq!(changed[0]["panes"], serde_json::json!(["3"]));
    }

    #[tokio::test]
    async fn an_idle_stream_still_proves_it_is_alive() {
        let (frames, _) = frames(feed(&[]), Duration::from_millis(500), || true).await;
        assert!(
            !frames.is_empty() && frames.iter().all(|f| f["event"] == "ping"),
            "{frames:?}"
        );
    }

    #[tokio::test]
    async fn a_lost_feed_asks_for_everything() {
        let (frames, _) = frames(
            feed(&[r#"{"event":"events.resync_required","sequence":9}"#]),
            Duration::from_millis(150),
            || true,
        )
        .await;
        let changed = frames
            .iter()
            .find(|f| f["event"] == "changed")
            .expect("signal");
        assert_eq!(changed["topics"].as_array().expect("topics").len(), 4);
    }

    #[tokio::test]
    async fn revoking_the_device_ends_its_stream() {
        let revoked = std::sync::atomic::AtomicBool::new(false);
        let flag = &revoked;
        let (_, ended) = frames(feed(&[]), Duration::from_secs(2), move || {
            !flag.swap(true, std::sync::atomic::Ordering::SeqCst)
        })
        .await;
        assert_eq!(ended, Some("this device is no longer paired"));
    }

    #[tokio::test]
    async fn the_end_of_the_feed_ends_the_stream() {
        let (tx, rx) = mpsc::channel::<String>(1);
        drop(tx);
        let (_, ended) = frames(rx, Duration::from_secs(2), || true).await;
        assert_eq!(ended, Some("the machine's event feed ended"));
    }

    #[test]
    fn slots_cap_a_device_and_the_machine() {
        let slots = Slots::default();
        let a = slots.acquire("a").expect("first");
        let _b = slots.acquire("a").expect("second");
        assert!(slots.acquire("a").is_err(), "per-device cap");
        drop(a);
        let _again = slots.acquire("a").expect("freed by drop");
        let held: Vec<_> = (0..MAX_TOTAL - 2)
            .map(|n| slots.acquire(&format!("d{n}")).expect("room"))
            .collect();
        assert_eq!(held.len(), MAX_TOTAL - 2);
        assert!(slots.acquire("late").is_err(), "machine-wide cap");
    }
}
