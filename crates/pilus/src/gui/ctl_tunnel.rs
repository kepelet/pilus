//! Keeps a `pilus ctl` tunnel open to each SSH machine the person has allowed.
//!
//! An agent that runs on a host cannot reach the window's `pilus.sock` on this
//! Mac. For a machine named in `Settings::ctl_tunnels`, one supervisor thread
//! holds a reverse tunnel (`pilus_client::CtlTunnel`) that publishes the socket
//! on the host, and brings it back when the link drops. The tunnels belong to
//! the app, not to a window: two windows would otherwise fight over the one
//! socket on the host. [`ensure`] reconciles them with the allowed list and is
//! safe to call as often as the list or the machine catalog changes.
//!
//! A supervisor never polls. It blocks while a tunnel is up (until ssh exits),
//! and between attempts it sleeps on a condition variable that stopping wakes.
//! A machine nobody allowed has no thread at all. The trust this extends, and
//! why it is opt-in, is in [security](../../../docs/agents/security.md#the-control-socket).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use pilus_client::{CtlKiller, CtlTunnel, SshTarget};

/// Where a machine's tunnel stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Status {
    /// ssh is connecting.
    Starting,
    /// The host has the socket.
    Up,
    /// The last attempt failed or the link dropped; another is waiting.
    Retrying(String),
}

/// A machine to hold a tunnel to.
pub(super) struct Spec {
    pub id: String,
    pub target: SshTarget,
}

/// Waits between attempts, by how many have failed in a row. The last holds.
const BACKOFF: [Duration; 5] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];

struct Running {
    target: SshTarget,
    stop: Arc<Stop>,
}

#[derive(Default)]
struct Registry {
    running: HashMap<String, Running>,
    status: HashMap<String, Status>,
}

static REGISTRY: LazyLock<Mutex<Registry>> = LazyLock::new(Mutex::default);
/// Bumped on every status change, so a window can tell when to repaint.
static REVISION: AtomicU64 = AtomicU64::new(0);

fn registry() -> MutexGuard<'static, Registry> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a supervisor waits on, and what stops it.
#[derive(Default)]
struct Stop {
    stopped: Mutex<bool>,
    wake: Condvar,
    killer: Mutex<Option<CtlKiller>>,
}

impl Stop {
    fn is_stopped(&self) -> bool {
        *self.stopped.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stop(&self) {
        *self.stopped.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.wake.notify_all();
        let killer = self
            .killer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(killer) = killer {
            killer.kill();
        }
    }

    /// Hand over the running tunnel's killer. If a stop came in while the
    /// tunnel was still starting, it ends now.
    fn set_killer(&self, killer: Option<CtlKiller>) {
        *self.killer.lock().unwrap_or_else(PoisonError::into_inner) = killer.clone();
        if let Some(killer) = killer
            && self.is_stopped()
        {
            killer.kill();
        }
    }

    /// Sleep for `wait`; false if stopped before it was over.
    fn sleep(&self, wait: Duration) -> bool {
        let guard = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
        let (guard, _) = self
            .wake
            .wait_timeout_while(guard, wait, |stopped| !*stopped)
            .unwrap_or_else(PoisonError::into_inner);
        !*guard
    }
}

/// How the status of machine `id` reads now, if it has a tunnel.
pub(super) fn status(id: &str) -> Option<Status> {
    registry().status.get(id).cloned()
}

/// A number that changes whenever any tunnel's status does.
pub(super) fn revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

fn set_status(id: &str, status: Status) {
    let mut registry = registry();
    // A machine stopped while its thread was mid-attempt must not reappear.
    if registry.running.contains_key(id) {
        registry.status.insert(id.to_string(), status);
        REVISION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Make the running tunnels the ones in `wanted`: start any that are missing,
/// restart one whose machine now points elsewhere, and end the rest.
pub(super) fn ensure(wanted: &[Spec], local_socket: &Path) {
    let mut registry = registry();
    let gone: Vec<String> = registry
        .running
        .iter()
        .filter(|(id, running)| {
            !wanted
                .iter()
                .any(|spec| &spec.id == *id && spec.target == running.target)
        })
        .map(|(id, _)| id.clone())
        .collect();
    for id in gone {
        if let Some(running) = registry.running.remove(&id) {
            running.stop.stop();
        }
        registry.status.remove(&id);
        REVISION.fetch_add(1, Ordering::Relaxed);
    }
    for spec in wanted {
        if registry.running.contains_key(&spec.id) {
            continue;
        }
        let stop = Arc::new(Stop::default());
        registry.running.insert(
            spec.id.clone(),
            Running {
                target: spec.target.clone(),
                stop: Arc::clone(&stop),
            },
        );
        registry.status.insert(spec.id.clone(), Status::Starting);
        REVISION.fetch_add(1, Ordering::Relaxed);
        let (id, target, local) = (
            spec.id.clone(),
            spec.target.clone(),
            local_socket.to_path_buf(),
        );
        let spawned = std::thread::Builder::new()
            .name("pilus-ctl-tunnel".to_string())
            .spawn(move || supervise(&id, &target, &local, &stop));
        if let Err(error) = spawned {
            tracing::warn!("ctl tunnel: could not start a thread: {error}");
            registry.running.remove(&spec.id);
            registry.status.remove(&spec.id);
        }
    }
}

/// End every tunnel, as a quit should: an `ssh -R` left running would keep the
/// host's socket pointing at a window that is gone.
pub(super) fn stop_all() {
    let mut registry = registry();
    for (_, running) in registry.running.drain() {
        running.stop.stop();
    }
    registry.status.clear();
    REVISION.fetch_add(1, Ordering::Relaxed);
}

fn supervise(id: &str, target: &SshTarget, local: &Path, stop: &Stop) {
    let mut failures = 0usize;
    while !stop.is_stopped() {
        set_status(id, Status::Starting);
        let why = match CtlTunnel::start(target, local) {
            Ok(tunnel) => {
                stop.set_killer(Some(tunnel.killer()));
                set_status(id, Status::Up);
                failures = 0;
                tracing::info!(machine = id, "ctl tunnel is up");
                let why = tunnel.wait_exit();
                stop.set_killer(None);
                why
            }
            Err(why) => {
                failures += 1;
                why
            }
        };
        if stop.is_stopped() {
            break;
        }
        tracing::info!(machine = id, %why, "ctl tunnel is down");
        set_status(id, Status::Retrying(why));
        let wait = BACKOFF[failures.min(BACKOFF.len() - 1)];
        if !stop.sleep(wait) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stop_wakes_a_sleeping_supervisor() {
        let stop = Arc::new(Stop::default());
        let sleeper = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || stop.sleep(Duration::from_secs(30)))
        };
        std::thread::sleep(Duration::from_millis(50));
        let started = std::time::Instant::now();
        stop.stop();
        assert!(!sleeper.join().expect("join"), "woken by the stop");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn an_undisturbed_sleep_runs_out() {
        let stop = Stop::default();
        assert!(stop.sleep(Duration::from_millis(20)));
        stop.stop();
        assert!(!stop.sleep(Duration::from_secs(30)), "already stopped");
    }

    #[test]
    fn nothing_allowed_means_nothing_running() {
        ensure(&[], Path::new("/nonexistent/pilus.sock"));
        assert!(status("nobody").is_none());
    }

    #[test]
    fn the_wait_grows_and_then_holds() {
        assert!(BACKOFF.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(BACKOFF[100usize.min(BACKOFF.len() - 1)], BACKOFF[4]);
    }
}
