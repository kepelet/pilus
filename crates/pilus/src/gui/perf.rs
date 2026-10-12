//! Opt-in terminal-rendering counters, enabled with `PILUS_PERF=1`.
//!
//! The GUI thread reports how often the grid element tree is rebuilt, what
//! the build costs, and how many daemon frames landed; one line per second
//! goes to stderr. Disabled, every call is a relaxed atomic load, so the
//! probes can stay in the render path and be compared across changes.

use std::cell::RefCell;
use std::sync::OnceLock;
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
    if enabled() {
        record(|w| w.renders += 1);
    }
}

/// Count one daemon frame (full or diff) folded into the surface.
pub fn frame() {
    if enabled() {
        record(|w| w.frames += 1);
    }
}
