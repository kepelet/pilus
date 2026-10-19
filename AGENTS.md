# Working in this repository

`pilus` is a native desktop client for a Luvus daemon. The daemon renders its
entire TUI surface to clients as a grid of styled cells; `pilus` attaches, folds
that grid into a `Surface`, and paints it in a GPUI window.

## The one rule that bites

`crates/pilus-protocol` mirrors a binary wire format encoded with
`bincode`'s positional serde encoding. In that encoding, an enum's variant
index and a struct's field order **are** the wire format. Adding a field in the
middle of a struct, reordering enum variants, or deleting a variant will
silently corrupt every message after the change.

If you must change a message:

1. append new variants and fields at the end;
2. bump `PROTOCOL_VERSION`;
3. extend `crates/pilus-client/tests/handshake.rs` so the change is exercised
   against a scripted peer.

The enabling feature in `Cargo.toml` (`runtime_shaders`) is also deliberate: it
keeps the macOS build from shelling out to the `metal` compiler, so a machine
with only the command-line tools can build the app.

## Invariants

- `FrameData::validate` and `FrameData::apply_diff` are the only places that
  mutate a grid. A malformed diff returns an error and leaves the frame
  untouched, so a client can request a full frame instead of painting a torn
  surface.
- `pilus-client` owns no rendering. It emits `ClientEvent`s and folds
  `ServerMessage`s into `Surface`; the window decides how to draw.
- The GUI thread never blocks on socket IO. Reads happen on a background reader
  thread; writes go through an unbounded channel to a writer thread.
- The macOS backdrop is `gui/glass.rs`'s own `NSVisualEffectView`, not GPUI's
  `Blurred` appearance (a weak, desaturated material by design). The window
  stays `Transparent`; the blur slider drives the view's `alphaValue`. The view is pinned to the dark
  appearance, so a light-mode Mac still frosts dark under the chrome.

- The GitHub Repositories section reads the signed-in account through `gh` on
  the machine `pilus` runs on, never through a daemon or SSH. It shows three
  repositories: `Settings::github_pinned` first, then the most recently pushed
  to. Every list, and each repository's issues, pull requests and workflow
  runs, goes through `pilus_client::GhAccount`'s cache (fresh lists are free,
  REST lists revalidate by `ETag`, a rate limit is waited out and persisted in
  `github-cache.json`); do not call `gh` for these from the GUI directly, or the
  cache and its limits are bypassed. `Settings::github` and
  `github_repositories` switch the feature off, which means no requests at
  all. The `visual-test` captures fill the feed with samples so a shot never
  reaches GitHub.

- Every sipil, whether the durable agent daemon (the `../sipil` project, a Node
  service over pi-durable) or a pilus window, is a node on one protocol over
  iroh: ALPN `pilus/sipil/1`, one JSON call per QUIC stream (`{"method","params"}`
  in, `{"result"}` or `{"error":{"code","message"}}` out; see
  `pilus_client::sipil::wire`). It is not the Luvus protocol, so
  `pilus-protocol` is not involved, and there is no HTTP: sipil binds no port.
  A node is addressed by its **device ID** (the iroh `EndpointId`, 64 hex
  characters) or a ticket, and the caller's identity is whatever the handshake
  proved, never a field in the frame. `wire::API_VERSION` is 3 (1 was the HTTP
  API); the daemon's `src/rpc.ts` and `wire.rs` are two ends of one contract, so
  change them together and extend `crates/pilus-client/tests/sipil.rs`, whose
  last test drives the real daemon when `../sipil` has its `node_modules`.
  `health` answers anyone and says what kind of node it is (`daemon` or `pilus`)
  and whether the asker is allowed; every other method needs an allow-list entry
  (a daemon's allow-list, filled by `npm run connect` once a pilus accepts or by
  `npm run allow`; a contact on a pilus). An
  allowed device on a daemon can run its agents' shell tools, so treat the list
  like SSH keys. `pilus_client::sipil::Node` owns one iroh endpoint and a small
  tokio runtime behind blocking calls; the GUI thread never touches it.
  `Node::ticket` also lists loopback, so a node on the same machine is reached
  directly. The identity key is `~/.pilus/sipil/identity.key`; losing it changes
  the device ID everywhere it was added.
- A pilus is a sipil by `peer::Local`: the node, the contacts
  (`contacts.json`), the conversations (`chats.jsonl`, an append-only log of
  JSON records replayed at launch; both under `~/.pilus/sipil`) and the `PeerService` that answers inbound calls. It answers
  `health` and `chat.send` only; a stranger's `chat.send` is refused unread and
  remembered as a *request* the person can accept, and a message is stored on the
  sender's side only once the receiver's pilus has taken it, so a refusal leaves
  the composer to retry. Adding a contact is mutual by hand (each side adds the
  other). `Settings::sipil_online` (default off) makes this pilus reachable and
  starts the node at launch; with it off the node still starts for a user action
  (connect, add a contact, send) but answers nobody. Contacts and threads are
  read from disk, so the sidebar's one SIPIL section lists them (user icon,
  with an unread count and an online dot) beside the daemon's agents (robot icon)
  with the network down. Messages arrive as `Event`s that `gui/sipil.rs::take_events` hands to the
  main window as toasts and, while it is not in front, system notifications;
  nothing is delivered while the window is closed (the node lives in the window,
  not in `pilus-agent`).
- A person can run several daemons (`SIPIL_NAME` names each one's primary agent:
  a Researcher, a CoS) and this window connects to all of them
  (`Settings::sipil_daemons`, the old single `sipil`/`sipil_address` read in once
  by `sipil_daemon_list`). A daemon's agent is not shared with a contact: no call
  hands anyone a session of it, `PeerService` answers `agent.*` with
  `unknown_method`, and `health` lists no agents. It takes part only in a channel
  its person invited it into (see Mentions). Contacts are renamed in the Sipil tab
  (`Directory::rename`); the name is the user's, never the peer's.
- Channels (`channels.json`, `pilus_client::sipil::Channels`) are named rooms of
  contacts with no server: the author's pilus fans `chan.send` out to every member
  that is a contact (in parallel, so one absent member holds nobody up), each
  member keeps its own copy under the thread `#<id>`, and a pilus learns a channel
  from the first message that names it. Only contacts are written to or listened
  to; a message from a stranger, or for a channel this pilus left, is dropped
  (`Learned::Left`). A message some members missed says who; there is no
  store-and-forward. A channel has an owner, the device that made it
  (`Channel::owner`, carried in every `chan.send`); only the owner can invite a
  contact (`Local::invite_to_channel`) or remove a member (`remove_from_channel`,
  both refused with `not_owner` otherwise). Each is a line to the members that
  carries `added` or `removed` device IDs beside the usual list. A pilus that has
  the channel believes only the owner's lists and changes, and refuses a message
  from someone who is not in its list (`not_member`), so a member taken out can
  neither read nor speak: it is told and forgets the channel, and one that was
  away finds out when every member refuses its next message. A removal is kept
  per channel (`Channels::remove_members`, `removed` in `channels.json`) so an
  older list does not bring the person back; only an `added` does. The info
  button on a channel's page opens `channel_panel`: members with their device IDs
  and who owns it; the owner also sees Remove (two clicks), who can be invited,
  and anyone can leave. The page closes itself when the channel is gone.
- Mentioning an agent (`@Researcher`, matched case-insensitively against the
  connected daemons' agent names by `mention::mentioned`) in a channel sends the
  line as usual and hands the conversation to that agent
  (`mention::answer`): its daemon gets the lines it has not seen, with who said
  each, in one session per conversation (the cursor lives in the chat store), and
  its reply is said back under its name (`as`), to everyone. An agent answers
  only in a channel its person invited it into, on the channel panel's "Your
  agents" list (`Channels::set_agent`, kept in `channels.json` on that device
  only and never sent; the invite asks once because every member can then run a
  model with file and shell tools on that machine with its API keys, and joining
  or leaving is said to the channel as the agent). Naming an agent that was not
  invited does nothing, for the person and for every member alike. For a line a
  member sends, `PeerService::invite_agents` turns the mention into
  `Event::Mentioned`, which `gui/sipil_social.rs::push_event` answers on the
  spot; for the person's own line `bring_in_agents` does the same. A line an agent
  said (`as`) never starts one of this pilus's agents, whatever it says. A direct
  chat between two people is never handed to an agent: no agent reads it or
  speaks in it, and naming one there does nothing, from either side
  (`bring_in_agents`, `answer_invited` and `answer_mention` all refuse a
  `Conversation::Direct`). Leaving a channel forgets its invitations.
- A daemon being away does not close its agent. `pilus_client::sipil::Offline`
  (`offline.json`, same directory) keeps each daemon's primary agent, the newest
  200 lines of the 8 conversations last looked at, and an outbox. `sipil::begin`
  seeds the `Daemon` from it, so the sidebar row (muted dot instead of green)
  opens the chat before, and without, a connection; `sipil::watch` shows the
  snapshot until the daemon's own lines replace it (`history::newest` merges
  them). `sipil::send` to a daemon that is not `connected()` queues the message
  (`hold`, written to disk at once, shown under the log as "waiting" with
  Discard) and `deliver_queued` hands the outbox over in order after the next
  successful connect, under the request id it was queued with so a cut-off
  delivery is not said twice. A device the daemon has not allowed keeps the
  queue; any other refusal drops that message and says so. Snapshot writes are
  throttled and flushed by the poller; the outbox is not. `remove_daemon` forgets
  all of it. A daemon that is away is never shown as working.
- Absence is cheap. Dialing a device that is switched off is the costliest thing an
  idle pilus does, so neither a daemon nor a contact is dialed on a fixed beat.
  `pilus_client::sipil::Backoff` spaces the attempts after each failure
  (`Policy::DAEMON`: 3 s doubling, eight failures, about six minutes;
  `Policy::PERSON`: 30 s doubling, six failures) and then *gives up*: a daemon
  shows `Phase::Off` with the reason, a contact stays offline, and nothing dials
  until something says they may be back, via `Backoff::wake`: the peer called us
  (`Local::take_heard`: a call from any contact, person or daemon, `health`
  included, and even one a not-reachable pilus refuses, counts as that device
  announcing itself; `Local::on_heard` nudges the poller), the person opened the
  conversation, wrote to it, or opened the Sipil tab (`wake_daemons`), or the app
  came to the front after resting `FORWARD_REST` (`wake_if_rested`, so switching
  apps is not a dial per switch). The daemon announces itself the same way: on
  start `../sipil/src/announce.ts` calls `health` on every allowed device, three
  tries; an older daemon or pilus just never does, and is found on the person's
  next look. Presence checks (`Probe` in `sipil_social.rs`) run only while the app
  is in front, since nobody sees the dot otherwise, and a contact who answered is
  re-asked every two minutes, not twenty seconds. The poller thread sleeps on a
  condvar until the next due thing (`next_wake`, one second in front and ten
  behind another app) and `nudge()` wakes it for a message, a call or the app
  coming forward; `sync` returns at once when the stores' `version()`s have not
  moved.
- The SIPIL rows (daemons, channels, people; not knocks, which stay first) are
  dragged into order and kept in `Settings::sipil_order` by key (`daemon:<id>`,
  `channel:<id>`, `person:<id>`); rows not named there follow in the default
  order (`move_sipil_row`, `sipil_rank`).
- `gui/sipil.rs` owns the one shared `SipilState` and its poller; the sidebar's
  SIPIL section draws its primary agents from the daemons' half of that snapshot
  (a subagent has no row: it is listed in the subthread panel of the conversation
  that handed it work), and its people and channels from `SipilState::me`. There
  is no sessions list: a daemon is talked to through its agents. `gui/sipil_social.rs` holds the people half (contacts,
  channels, mentions, presence); `gui/sipil.rs` holds the daemons and the poller. A first
  connect that fails stops and reports; a launch or a dropped connection keeps
  retrying quietly. A daemon is never typed in here: `sipil connect` on its
  machine sends `pair.request`, which `PeerService` records as a `Request` with
  `daemon: true`; accepting it (`Directory::accept`) makes a `Contact` with
  `daemon: true`, and `sipil::ensure` dials the daemon contacts at launch. A
  daemon contact is never a person (`Directory::contacts`, `contact` and
  `is_contact` skip it), so nothing that admits a person admits it. Provider keys and the model belong to the daemon: it reads
  them from its own environment (`OPENCODE_API_KEY`, `SIPIL_MODEL`), and the
  protocol has no call to read, set or choose either, so pilus has no key form
  and no model picker. The chat page only shows the model a conversation runs on.
  Opening the window creates the identity key, nothing else. The `visual-test` captures use `sipil::preview` and never
  dial or write the identity.
  Clicking an agent, session or contact opens `gui/sipil_chat.rs`, a page in the
  terminal's card with its own `InputLayer::Sipil`, laid out like an IRC client:
  a topic bar, a log of `HH:MM <nick> text` lines in the terminal font (tool
  calls as `*`, tool output as `-`, a failed turn as `!!`, nicks colored from the
  theme's ANSI hues by name), and a flat input
  line with no Send button. It is deliberately not a `style::field`. A line is
  `nick: text`, with no brackets, and its text is Markdown when it reads as
  Markdown (`looks_like_markdown`), drawn by `gui/markdown.rs` in the terminal
  font and parsed once per text. A message that arrives while the window is not in
  front also posts a system notification (`notify::post`, target `("sipil",
  thread)`) whose click opens that conversation. It owns no conversation
  data: `sipil::watch` names the open conversation and the poller reads a
  daemon's transcript every second (sipil has no push channel yet) or a
  contact's thread from the chat store, and a send goes through `sipil::send`,
  A tool call is a folding line (`call_block`): closed it shows the tool and its
  summary, open the arguments and the result, which is drawn under its call by
  `call_id` and has no line of its own while that call is in view. A `subagent`
  call reads "<agent> talking with <researcher> about “<task>”" and that sentence
  opens the child's page (`step_into_sipil`, with a way back: the header's back
  button and Escape), by `Watch::On` on the daemon it ran on; a daemon not
  connected, or a contact's agent (the relay strips the conversation id), leaves
  it plain text. The daemon's `transcript` answers `conversation` too, which
  names a subagent of a session that neither list carries (`SipilState::watched`).
  In a channel or chat, `mention::answer` notes each hand-off as a `Line` with a
  `Delegation` as it happens; that note is kept on the owner's pilus only and is
  never sent, since nobody else could open it. A line can be replied to (the
  hover button): the `Quote` (who, and the start of the text) travels as `reply`
  in `chat.send` and `chan.send` and is stored with the line on both sides; to a
  daemon or a contact's agent, which have no field for one, it goes as a Markdown
  quote in front of the message. The log is a virtualized gpui `list` held in
  `SipilChat`, flush to the sidebar (`flush_page_card`, no inset card), so a long
  conversation costs what a short one does: `SipilChat::sync` runs when the state
  snapshot changes and re-measures only the rows from the first one whose
  `signature` changed, and the list follows the end (`FollowMode::Tail`) until the
  reader scrolls up. A reply (the hover button on a line) is a thread: `group_threads` finds the line a
  reply quotes (by its text, nearest before; with a daemon the quote is the Markdown
  prefix `reply_of` reads back, and the agent's answers follow into the thread), the
  replies leave the log, and the line shows "N replies" that opens `thread_panel`
  beside it, where a message sent is one more reply. A reply whose line is out of
  view stays in the log. Every line highlights on hover. A daemon describes itself with
  the `about` call (`pilus_client::sipil::About`: name, host, workspace, model, the
  agents with their instructions and tools, and a logo as SVG), asked once per
  connect and kept on `Daemon::about`; an older daemon answers `unknown_method`
  and is not asked again. The header's info button opens `about_panel`; the logo
  (`Daemon::own_logo`: the advertised SVG drawn by `about::rasterize`; without
  one `daemon_mark` draws the pilus mark in the icon colour, with nothing behind
  it) is in the header, the About panel and
  the daemon's sidebar row. The sidebar's SIPIL section is always
  there. There is no separate sipil window: the way in is the outlined pilus mark at the
  head of the right toolbar pill, before Browser (`right_sipil_toggle`), which
  opens the right sidebar's Sipil tab (`SidebarTab::Sipil`, the `SipilPanel` in
  `sipil_panel.rs`, made on first open so no identity key is written before
  then; it takes the keyboard as `InputLayer::SipilPanel` while the column has
  focus). There is no separate "sipil" row. Each daemon has its own row, which is where its status
  is: one that is up shows its primary agent with a green dot at the right, like
  a contact who is there; one that is not stays listed with its logo and a grey offline dot (a
  spinner while connecting), the reason as the tooltip, and a click opens the
  Sipil tab. A daemon's logo is drawn larger than a glyph (`SIDEBAR_LOGO`). The info button
  also opens `contact_panel` on a contact's page (name with rename, device ID,
  channels in common, remove); while the name is
  edited `SipilChat::active_input` gives the keyboard to the rename field. A header button
  (`subthreads_toggle`, with the count) opens `subthread_panel`
  beside the log, listing every `subagent` call of the open conversation, each
  opening that child's page. The composer has
  no status bar or `[nick]` prefix. The composer is cleared only when
  `SipilState::sent` grows, so a refused message stays to retry. A contact's page has no Stop. Never show an error as an empty line: a failed
  turn arrives as `error` on the agent, the session and the transcript line.

- Notes on excerpts of markdown files (`gui/md_notes.rs`) are the app's own,
  kept per machine and file in `md-notes.json` beside the settings. They are not
  the daemon's review notes: those are anchored to a Git layer, which an unchanged
  file has not. Sending ticked notes is one `agent.prompt` carrying every excerpt,
  so an agent never receives a note it cannot place.

- The Files tab edits text files in place (`files_tab.rs`), saved through
  `pilus_client::write_text` (local or over SSH), never through the daemon. A
  click opens a file in the editor when `highlight::is_text` says it is text
  (markdown, a file with a grammar, or a known plain-text kind); anything else,
  and a double click, still goes to the daemon. Markdown (`md`, `markdown`,
  `mdown`, `mkd`, `mdx`, by `md_view::is_markdown`) opens as a document with
  Source, Preview and side-by-side modes; any other file opens straight into
  the source editor, with no preview and an `MdDoc` that holds its text
  unparsed. A truncated read, or a file over `md_view::EDIT_LIMIT`, is not
  opened for editing.
- The editor field (`md_editor_input`) is colored by `highlight::source`.
  Markdown gets its marks, front matter as YAML, fenced blocks in their own
  language, and for `.mdx` the `import`/`export` blocks as JavaScript and JSX
  tags as markup. The bundled grammars lack TypeScript and JSX, so
  `highlight::ALIASES` reads them as JavaScript; TOML and a few others open
  uncolored. The preview reads `.mdx` with MDX's constructs
  (`markdown::mdx_options`) and falls back to plain markdown when MDX rejects
  the text, since a stray `<` or `{` is an error there; do not make that parse
  stricter, or a half-typed file blanks the preview.
- Editor chords are scoped by key context, never by a check in the handler: the
  editor div carries `MdEditor`, so Cmd+S saves instead of toggling the sidebar.
  A deeper context wins over the global binding, so a new editor chord is
  another `KeyBinding` in that context.

- Search (`gui/search.rs`, the row below Open Workspace) lists the Claude and Pi
  sessions on this Mac and on saved SSH machines, from `pilus-index`: a SQLite
  file, `index.db` beside the settings. It is a rebuildable cache of titles and
  metadata (no message text); a `SCHEMA` bump drops and rebuilds it, so there are
  no migrations. Nothing may read a transcript, locally or over SSH, until the
  person turns on `Settings::index_agents`, and a remote machine is read only
  while its id is in `Settings::index_machines`; do not call
  `pilus_index::refresh` from anywhere that skips that check. Turning it off
  calls `pilus_index::wipe`, and unticking a machine forgets its rows. Reads run
  on one background thread, this Mac first, and a machine that cannot be listed
  keeps its rows. A remote answer comes from `grep`/`tail` on the host, so keep
  `scan::evidence_script` and `read_local` choosing the same lines. Resume is
  `workspace.open` on the session's folder then `agent.resume` on its machine's
  daemon; the daemon only knows the sessions its own hook recorded, so on
  "no resumable session" an agent with `Agent::resume_flag` (Claude) is launched
  by `agent.start` with `--resume <id>` instead. The `visual-test` captures use `search_show_sample` and never touch
  the index.
- Stats (`gui/stats.rs`, the row below Search) totals the same index's token
  use: per-hour buckets (`usage` table: machine, transcript, hour, model, four
  token counts, turns) that `refresh` writes in the same transaction as the
  session row, so a session never exists without the usage its size says was
  read. It has no permission of its own: it shows Search's consent card and is
  gated by the same `Settings::index_agents` and `index_machines`, and the same
  background read (`search_sync`) feeds both pages. Usage is read from assistant
  turns only (`usage.rs`): Claude repeats a streamed message's id on every line,
  so `dedupe` keeps the last per id; zero-token and `<synthetic>` turns are not
  usage. This Mac's lines are parsed as JSON and a remote host's are cut out by
  the `awk` in `usage::REMOTE_AWK`, so keep the two producing the same `Turn`
  (a test runs both over one set of lines). Counts are what each turn reported,
  not a bill. Cost is Pi's own recorded figure for each turn (`Turn::cost_micros`,
  kept per bucket with a `recorded` count) and, for turns that have none
  (Claude's), a list-price estimate made when a page asks, never stored, from the
  table in `pricing.rs`, taken from pi.dev/models as it lists them. A model
  with neither is *unpriced*: its tokens are counted in `Cost::unpriced` and left
  out of the dollars, never guessed. Cache writes are priced at the 5-minute rate,
  since a transcript does not say which lifetime was used. Changing a price needs
  no re-index; changing what is stored does (`store::SCHEMA`). Hours are stored in UTC and grouped into days in the viewer's
  offset (`date +%z`) when the page opens. Bump `store::SCHEMA` when a table
  changes, which rebuilds the cache. The heatmap cell and ranking row are
  `style::heat_cell` and `style::rank_row`, shown in the playground.

## Idle cost

An open window must be able to let the CPU sleep; `PILUS_PERF=1` prints a
`pilus-idle:` line per second (loop ticks, repaints by cause, sipil poller
wake-ups, calls and dials) next to the render counters, and is how a change here
is judged. The rules that keep it low:

- The window's tick loop (`next_tick` in `gui/mod.rs`) runs at the frame rate
  only while something moves (a tween, a toast, the cursor's fade, the sidebar
  freeze); otherwise every 50 ms in front and 250 ms behind another app. Daemon
  frames wake it through `Arrival`, so output does not wait for the tick. New
  animation driven from the loop must count as "moving" there, or it will stutter
  at the idle rate.
- The cursor stops blinking ten seconds after it last moved
  (`cursor_blink::BLINK_FOR`); the fade repaints the whole grid about twenty times
  a second, which was most of an idle window's cost. Moving it, or returning to the
  window, starts it again.
- A poll that is not driven by an event goes through `activity::scaled`, which
  stretches its interval five times while the app is behind another one
  (`activity::set_front`, fed by every window's tick loop from
  `dock::app_active`). Coming forward polls at once because the elapsed time has
  already passed. A new background thread that wakes on a timer takes it too.
- Network polls use `Backoff`, not a constant: see the sipil notes above.
- The Process page (sidebar, below Stats, only while `Settings::developer_mode` is on: Settings → Developer, or the palette; `gui/process.rs`) shows the same costs
  live: CPU, memory and per-thread CPU from `pilus_client::process` (macOS
  `proc_pidinfo`, the one other `unsafe` block, scoped and commented), the
  `perf::Count` totals as rates, and `pilus_client::netlog`, a ring of the last 300
  calls out of the machine. It samples once a second only while it is open. A new
  place that calls out (a request, a subprocess that reaches a host) records itself
  with `netlog::record` or `netlog::note`, or it will be missing from the page;
  local-only subprocesses (`git`, the daemon's socket) are not network and stay out.

## UI

`crates/pilus/src/gui/ui_playground.rs` is the visual contract: a working
catalogue of every native element the app draws. `crates/pilus/src/gui/style.rs`
is the only implementation of it. Draw controls through `style::button`,
`style::small_button`, `style::field`, and `style::section_label`; do not
hand-roll a button, field, or section label in another window. When an element
is missing, add it to `style.rs` and to the playground first, then use it.

- **Type scale**: 13 body, 12 secondary, 11 label. Section labels are bold,
  muted, and uppercase. Resolve sizes with `style::Fonts::resolve` or
  `style::scaled` so every surface follows the Interface size setting; a fixed
  `text_size(px(...))` for chrome text is a bug even when the default happens
  to match.
- **Geometry**: 28px regular controls, 24px compact ones, `px_3`/`px_2`
  padding, `rounded_md` for controls, cards, and overlays, `rounded_lg` for
  large floating panels. The Home composer's `rounded_t_lg`/`rounded_b_lg`
  follows that rule rather than excepting it.
- **Color** comes from `Theme` roles; literals belong in `theme.rs` and
  `scheme.rs` only. Every border and hairline, chrome frames and interactive
  content alike, is `theme.border`: the color scheme owns it, so never
  hardcode a frame color. The selected tab is raised by its panel fill alone; it takes
  no border. Accent means progress or focus, not a panel edge; the edge of a focused
  field, panel, or card is `theme.border_focus`.
- **Selection** in the sidebar (workspace, worktree, agent) is the `theme.panel`
  pill with `panel_fg`/`muted` text and a bold name. `selection_bg`/`selection_fg`
  belong to text selection and list highlighting, not sidebar rows.
- **Motion**: every state change eases; nothing snaps.
  - A floating panel (menu, picker, palette, popover, tooltip) or a page
    opens through `style::enter`, wrapped on its open branch only so the fade
    replays each time it appears.
  - Hover and selection backgrounds go through `FillExt::hover_fill` /
    `state_fill`, never `.hover(|s| s.bg(..))` or a selection-dependent
    `.bg(..)`. Call them before adding children so the fill sits beneath.
  - Disclosure uses `style::chevron`, switches `style::toggle`, and working
    status `style::spinner`.
  - The terminal grid never takes an opacity fade (a translucent window would
    show the desktop through it); `reflow_alpha` fades its text instead. Do
    not resize the daemon while the sidebar tweens: it reflows once when the
    dock moves, and in-between sizes make it reflow again.
- Every window root goes through `apply_chrome_font`, so the Interface family
  and size reach the whole tree. A new window opts in by taking the shared
  `Settings` and calling it in `render`.

## Commands

```sh
cargo test                    # unit + handshake integration tests
cargo run -p pilus              # the window (needs a running daemon)
cargo run -p pilus -- --probe   # headless: print the first frame
```

Before shipping a UI change, render it. The `visual-test` feature (not part of
a normal build) captures every surface to `/tmp/pilus-shots`:

```sh
cargo run -p pilus --features visual-test -- --ui-shots       # standalone windows
cargo run -p pilus --features visual-test -- --ui-shots-main  # main window + overlays
```

`PILUS_SHOT_FONT_SIZE` overrides the Interface size for those captures, which is
the quickest way to catch chrome that ignores the setting.

## Style

- Rust 2024, `unsafe_code` and `unwrap_used` are denied at the workspace level.
  The single `unsafe` call (`discovery::current_uid`) carries a `SAFETY` comment
  and a scoped `allow`.
- Comments state *why*, not *what*. Prefer a name that removes the need for the
  comment.
- Every module has a `//!` header explaining its role in the data flow.
