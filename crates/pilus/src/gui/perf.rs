//! Opt-in terminal-rendering counters, enabled with `PILUS_PERF=1`.
//!
//! The GUI thread reports how often the grid element tree is rebuilt, what
//! the build costs, and how many daemon frames landed; one line per second
//! goes to stderr. Disabled, every call is a relaxed atomic load, so the
//! probes can stay in the render path and be compared across changes.
//!
//! A second line, from a thread of its own so it prints while nothing paints,
//! counts what the process does when nobody touches it: loop ticks, repaints
//! by cause, poller wake-ups, and the calls and connections the sipil node
//! made. Those are the costs that keep a laptop from sleeping.

use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("PILUS_PERF").is_some_and(|v| v != "0"))
}

#[derive(Default)]
struct Window {
    since: Option<Instant>,
    builds: u32,
    build_total: Duration,
    build_max: Duration,
    frames: u32,
    renders: u32,
}

thread_local! {
    static WINDOW: RefCell<Window> = RefCell::new(Window::default());
}

fn record(update: impl FnOnce(&mut Window)) {
    WINDOW.with(|cell| {
        let mut window = cell.borrow_mut();
        let now = Instant::now();
        let since = *window.since.get_or_insert(now);
        update(&mut window);
        if now.duration_since(since) >= Duration::from_secs(1) {
            let secs = now.duration_since(since).as_secs_f64();
            let average = window
                .build_total
                .checked_div(window.builds)
                .unwrap_or_default();
            eprintln!(
                "pilus-perf: {:.0} renders/s, {:.0} grid builds/s (avg {:?}, max {:?}), {:.0} frames/s",
                f64::from(window.renders) / secs,
                f64::from(window.builds) / secs,
                average,
                window.build_max,
                f64::from(window.frames) / secs,
            );
            *window = Window::default();
        }
    });
}

/// Time one grid element-tree build.
pub fn grid_build<R>(build: impl FnOnce() -> R) -> R {
    if !enabled() {
        return build();
    }
    let started = Instant::now();
    let result = build();
    let cost = started.elapsed();
    record(|w| {
        w.builds += 1;
        w.build_total += cost;
        w.build_max = w.build_max.max(cost);
    });
    result
}

/// Count one window render, whether or not it rebuilt the grid.
pub fn render() {
    count(Count::Render);
    if enabled() {
        record(|w| w.renders += 1);
    }
}

/// Count one daemon frame (full or diff) folded into the surface.
pub fn frame() {
    count(Count::Frame);
    if enabled() {
        record(|w| w.frames += 1);
    }
}

/// What the process did. The counts only ever grow: the idle line prints their
/// change each second, and the Process page differences them for its rates.
#[derive(Clone, Copy)]
pub enum Count {
    /// One pass of the window's tick loop.
    Tick,
    /// A repaint the tick loop asked for because polled state changed.
    RepaintState,
    /// A repaint for a sidebar tween, the cursor blink or the toolbar fade.
    RepaintMotion,
    /// One wake-up of the sipil poller thread.
    SipilWake,
    /// One render of a window.
    Render,
    /// One daemon frame folded into the surface.
    Frame,
}

const COUNTS: usize = 6;
const LABELS: [&str; COUNTS] = [
    "ticks",
    "repaint-state",
    "repaint-motion",
    "sipil-wakes",
    "renders",
    "frames",
];

static COUNTERS: [AtomicU64; COUNTS] = [const { AtomicU64::new(0) }; COUNTS];
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Count one occurrence of `what`. A relaxed add, cheap enough to leave in.
pub fn count(what: Count) {
    COUNTERS[what as usize].fetch_add(1, Ordering::Relaxed);
}

/// How many times `what` has happened since launch.
pub fn total(what: Count) -> u64 {
    COUNTERS[what as usize].load(Ordering::Relaxed)
}

/// Note whether the window is in front, so the idle line says which case it
/// measured.
pub fn window_active(active: bool) {
    ACTIVE.store(active, Ordering::Relaxed);
}

/// Start the idle line, once.
pub fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if !enabled() {
        return;
    }
    STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("pilus-perf".to_string())
            .spawn(|| {
                let mut traffic = pilus_client::sipil::node::traffic();
                let mut seen = [0u64; COUNTS];
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let now = pilus_client::sipil::node::traffic();
                    let mut line = String::from("pilus-idle:");
                    for ((counter, label), before) in COUNTERS.iter().zip(LABELS).zip(&mut seen) {
                        let total = counter.load(Ordering::Relaxed);
                        line.push_str(&format!(" {label}={}", total - *before));
                        *before = total;
                    }
                    line.push_str(&format!(
                        " sipil-calls={} sipil-dials={} active={}",
                        now.0 - traffic.0,
                        now.1 - traffic.1,
                        ACTIVE.load(Ordering::Relaxed),
                    ));
                    traffic = now;
                    eprintln!("{line}");
                }
            })
            .ok();
    });
}
