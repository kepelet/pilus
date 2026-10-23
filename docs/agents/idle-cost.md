# Idle cost

Read this before you add a timer, a poll, a background thread, an animation or
anything that calls out of the machine.

An open window must let the CPU sleep. Judge every change here with
`PILUS_PERF=1`. It prints a `pilus-idle:` line each second (loop ticks,
repaints by cause, sipil poller wake-ups, calls and dials) next to the render
counters.

## The tick loop

- The window's tick loop (`next_tick` in `gui/mod.rs`) runs at the frame rate
  only while something moves: a tween, a toast, the cursor's fade, or the
  sidebar freeze. The rest of the time it ticks every 50 ms while pilus is in
  front and every 250 ms while it is behind another app.
- Daemon frames wake the loop through `Arrival`, so output does not wait for
  the tick.
- A new animation driven from the loop must count as "moving" there, or it
  stutters at the idle rate.

## Transitions and repaints

Every GPUI frame re-renders the whole window: the sidebar, the tab strip and
every terminal row. A frame costs a few milliseconds, so anything that asks
for one at the display's refresh rate costs about half a core.

- An element that eases on its own (`hover_fill`, `state_fill`, the toggle
  knob, meters, the sidebar's hover pill, pane frames) calls
  `request_animation_frame` until the value it paints equals its target. Get
  the eased step from `style::ease_step`. It returns `None` once the time is
  up, and the element then paints the target itself. An interpolated end can
  miss the target: `style::mix` fades to a clear color in the other end's
  hue, so a hover fill that faded out never matched its resting
  `transparent_black`. After any hover the window then redrew at the refresh
  rate until the next one.
- Nothing animates for as long as a state lasts. A working agent, or anything
  loading, is a still dot in its status color (`style::busy_dot`). The
  spinner it replaced redrew the window 30 times a second while any agent
  worked.
- The terminal repaints for the pointer only when its shape changes, over a
  link or off one (`set_hover_cell`). The move itself still goes to the
  daemon.
- `PILUS_PERF=1` shows such a loop as `renders` well above
  `repaint-state + repaint-motion + frames` while nothing is moving.

## The cursor

The cursor does not blink unless `Settings::cursor_blink` is on. It is off by
default and set in Appearances. When it is on, blinking stops ten seconds after
the cursor last moved (`cursor_blink::BLINK_FOR`). The fade repaints the whole
grid about twenty times a second, which used to be most of an idle window's
cost. Moving the cursor, or returning to the window, starts it again.

## Timers and polling

- A poll that no event drives goes through `activity::scaled`. While the app is
  behind another one, it stretches the interval five times
  (`activity::set_front`, fed by every window's tick loop from
  `dock::app_active`). Coming forward polls at once because the elapsed time
  has already passed. A new background thread that wakes on a timer uses it
  too.
- The one exception is the sipil poller, which paces itself. It sleeps on a
  condvar until the next thing is due (`next_wake`: one second in front, ten
  behind), and `nudge()` wakes it for a message, a call or the app coming
  forward.
- Network dials use `Backoff`, never a constant interval, and give up after a
  set number of failures. See [sipil/daemons.md](sipil/daemons.md#absence-is-cheap).
  A read over a link that is already open is not a dial. The per-second
  transcript read of a connected daemon's open conversation is an example.

## Energy mode

Energy mode (`Settings::energy_mode`) is switched from the pilus menu, the menu
bar item (`menubar::Pick::Energy`) or the command palette. It overrides the
person's choices in what gets drawn without changing the choices themselves:

- `Settings::drawn_radius` is square, except `Radius::Full` and `Soft` (the
  pill and the tab), which stay round as they would at a zero radius.
  `drawn_opacity` is solid, so the blur view is dropped. These two replace
  `radius` and `glass_opacity` everywhere they are used. The Appearances
  sliders keep reading the originals.
- Motion follows GPUI's `reduce_motion`, set by `PilusView::sync_energy`. A new
  animation must honor `cx.reduce_motion()`, or it keeps running in energy
  mode.
- Decorative paint that costs GPU time (the focused pane's shadow) checks
  `style::energy()`.
- The tick loop idles at 100 ms in front and 500 ms behind.

## The Process page

The Process page (`gui/process.rs`) is opened from the command palette, not the
sidebar, and is always available. It shows the same costs live:

- CPU, memory and per-thread CPU, from `pilus_client::process` (macOS
  `proc_pidinfo`);
- the `perf::Count` totals as rates;
- `pilus_client::netlog`, a ring of the last 300 calls out of the machine.

It samples once a second, and only while it is open. Any new code that calls
out of the machine (a request, or a subprocess that reaches a host) records
itself with `netlog::record` or `netlog::note`, or it will be missing from the
page. Local-only subprocesses (`git`, the daemon's socket) are not network
calls and stay out.
