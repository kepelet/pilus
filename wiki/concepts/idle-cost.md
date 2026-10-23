---
title: Idle cost
description: Why an open window must let the CPU sleep, and the rules that keep it that way.
tags:
  - wiki
  - concept
---
# Idle cost

## Definition

The cost of an open but unused pilus window. The project treats “the CPU may sleep” as a feature with measurable rules: what runs per tick, what wakes it, and how often background work is allowed to try again.

## Why it matters

A desktop client that polls a daemon, dials peers, and repaints on a timer burns battery for nothing. Every regression here is visible with `PILUS_PERF=1`, which prints per-second loop ticks, repaints by cause, sipil poller wake-ups, calls, and dials. Idle cost is also why absence is cheap: dialing a switched-off device is the most expensive idle action, so the network waits for news instead.

## Where it lives

- [gui/mod.rs](../../crates/pilus/src/gui/mod.rs) — `next_tick`: frame rate while something moves, 50 ms in front, 250 ms behind another app (100/500 in energy mode).
- [activity.rs](../../crates/pilus/src/gui/activity.rs) — `scaled` stretches timer intervals fivefold behind another app; `set_front` is fed by every window.
- [cursor_blink.rs](../../crates/pilus/src/gui/cursor_blink.rs) — blinking is off by default and stops ten seconds after the last movement.
- [perf.rs](../../crates/pilus/src/gui/perf.rs) and [process.rs](../../crates/pilus/src/gui/process.rs) — counters and the live Process page.
- [sipil/backoff.rs](../../crates/pilus-client/src/sipil/backoff.rs) — capped, giving-up retry policies with a `wake` signal.
- [style.rs](../../crates/pilus/src/gui/style.rs) — skeleton pulses stop after six so a dead feed cannot repaint forever.

## Rules

- A new animation driven by the tick loop must count as “moving” there, or it stutters at the idle rate.
- A new background thread that wakes on a timer takes `activity::scaled`.
- A new network loop uses `Backoff`, not a constant interval.
- A new GPU-cost decoration checks `style::energy()`.
- A new outbound call records itself with `netlog::record` or `netlog::note`, or it is missing from the Process page.

## Related

- [Native chrome and GPUI](../architecture/native-chrome.md)
- [Sipil network](../architecture/sipil-network.md)
- [pilus (GUI)](../modules/gui.md)
