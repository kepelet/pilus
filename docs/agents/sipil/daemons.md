# Sipil: daemons

Read this before you change how pilus connects to sipil daemons, polls them, or
keeps their state while they are away.

## Several daemons, several primaries

- A person can run several daemons. `SIPIL_NAME` names each daemon's primary
  agent (a Researcher, a CoS), and the window connects to all of them
  (`Settings::sipil_daemons`). The old single `sipil`/`sipil_address` is read
  in once by `sipil_daemon_list`.
- One sipil install can run several primary agents (its `role: primary`
  blueprints, see `../sipil/src/host.ts`). pilus treats each one as a daemon of
  its own, with its own device ID, contact and sidebar row.
- There is no subagent. Every agent of a daemon is a primary with its own row,
  and `Role` has no other value.
- When an agent says a line to a primary (a coding agent's report,
  `sipil tell`, another primary's question), the daemon's transcript marks it
  with `speaker`. The log draws `speaker` as that agent's nick, never as the
  person's.

## Work board and routines

A daemon's primary agent keeps a board of its open work and a list of routines
(`../sipil/src/work.ts`, `routines.ts`). The agent keeps the board; pilus only
shows it.

- They are read with the `work` and `routines` calls (`Sipil::work`,
  `Sipil::routines`) in the same poll as the daemon's agents and sessions.
- A failed read keeps the last good one and never marks the daemon down.
- A daemon that answers `unknown_method` predates the board
  (`Daemon::board_unsupported`) and is not asked again until it reconnects.
- The chat page's header has a Work button for a daemon that has a board. It
  shows a count of the items waiting on someone (`waiting` or `blocked`) and
  opens `work_panel` beside the log. That side is shared with the about panel
  and a thread.

## Offline: an away daemon still has a chat

A daemon being away does not close its agent.
`pilus_client::sipil::Offline` (the `offline` document) keeps each daemon's
primary agent, the newest 200 lines of the 8 conversations looked at most
recently, and an outbox.

- `sipil::begin` seeds the `Daemon` from that document, so the sidebar row (a
  muted dot instead of green) opens the chat before a connection exists, and
  without one.
- `sipil::watch` shows the snapshot until the daemon's own lines replace it
  (`history::newest` merges them).
- `sipil::send` to a daemon that is not `connected()` queues the message
  (`hold`). The message is written to disk at once and shown under the log as
  "waiting", with Discard.
- After the next successful connect, `deliver_queued` hands the outbox over in
  order, each message under the request id it was queued with, so a delivery
  that was cut off is not said twice.
- A device the daemon has not allowed keeps its queue. Any other refusal drops
  that message and says so.
- Snapshot writes are throttled and flushed by the poller. Outbox writes are
  not throttled.
- `remove_daemon` forgets all of it.
- The outbox is keyed by device ID, so it also holds the lines written to a
  contact whose pilus is away (see
  [people-and-channels.md](people-and-channels.md#contacts-and-direct-messages)).
- A daemon that is away is never shown as working.

## Absence is cheap

Dialing a device that is switched off is the costliest thing an idle pilus does,
so no daemon or contact is dialed on a fixed beat.

- `pilus_client::sipil::Backoff` spaces the attempts after each failure:
  - `Policy::DAEMON`: starts at 3 s and doubles, for eight failures (about six
    minutes);
  - `Policy::PERSON`: starts at 30 s and doubles, for six failures.

  After that it *gives up*. A daemon shows `Phase::Off` with the reason, a
  contact stays offline, and nothing dials until something suggests they may be
  back.
- `Backoff::wake` restarts dialing when:
  - the peer called us (`Local::take_heard`). Any call from any contact,
    person or daemon, counts as that device announcing itself. That includes
    `health`, and a call that a not-reachable pilus refuses.
    `Local::on_heard` nudges the poller;
  - the person opened the conversation, wrote to it, or opened the Sipil tab
    (`wake_daemons`);
  - the app came to the front after resting `FORWARD_REST` (`wake_if_rested`),
    so switching apps does not cost a dial each time.
- The daemon announces itself the same way. On start,
  `../sipil/src/announce.ts` calls `health` on every allowed device, up to
  three tries. An older daemon or pilus never does this and is found the next
  time the person looks.
- Presence checks (`Probe` in `sipil_social.rs`) run only while the app is in
  front, since nobody sees the dot otherwise. A contact who answered is asked
  again every two minutes, not every twenty seconds.
- Reads over an open connection are not dials. The poller reads a connected
  daemon's open conversation every second, because sipil has no push channel
  yet.

## The poller

`gui/sipil.rs` owns the one shared `SipilState` and its poller.

- The poller thread sleeps on a condvar until the next thing is due
  (`next_wake`: one second in front, ten behind another app). `nudge()` wakes it
  for a message, a call or the app coming forward. This replaces
  `activity::scaled` for this thread (see [idle-cost.md](../idle-cost.md)).
- `sync` returns at once when the stores' `version()`s have not moved.
- If the first connect to a daemon fails, the poller stops and reports. At
  launch, and after a dropped connection, it keeps retrying quietly.
