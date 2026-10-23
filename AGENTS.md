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

- Durable state is `~/.pilus/pilus.db` (`crates/pilus-db`), not a pile of JSON
  files. It holds the only copy of the sipil conversations, contacts, channels
  and offline snapshots, the markdown notes, the GitHub cache and the window's
  per-item state (`Settings::window`, `session_labels`, `session_icons`,
  `github_pinned`, `sipil_order`, `last_session`, `last_endpoint` and the pane
  sizes; the `ui_state` document, `settings::UiState`), so it is **migrated, never dropped**: a schema
  change is a new entry at the end of `pilus-db/src/schema.rs`'s `MIGRATIONS`
  and a released one is never edited, and a file from a newer pilus is refused
  (`Error::Newer`) rather than written to. `index.db` is the opposite, a cache
  that `SCHEMA` bumps rebuild and `wipe` deletes, so nothing durable goes there.
  A store keeps its working copy in memory and writes each change through; take
  the store's lock before the database's, never the reverse, and never call a
  `Db` method inside `Db::transaction` (use the `tx`, as `put_doc_in` does). Open
  it with `Db::at(dir)`, which shares one connection per folder. Small
  always-whole state (contacts, channels, offline, `ui_state`) is one `docs`
  row; anything that grows or changes a row at a time (chat lines, notes, the
  GitHub cache) is a table. A store that used to be a flat file imports it once
  on first open (`Db::import_once`, tagged in `imports`) and sets it aside as
  `<name>.migrated` (`pilus_db::retire`); it never deletes it. `settings.json`
  keeps the preferences, which `pilus-agent` (the `relay` block) and
  `--check-updates` (`Settings::preferences`, which does not open the database)
  also read.

- Pinned and recent workspaces (`gui/pinned.rs`) are folders on a machine, not
  open workspaces: `Settings::pinned_workspaces` and `recent_workspaces` (the
  newest `RECENT_WORKSPACES`, written when the picker or a pin opens one), both in
  the `ui_state` document, keyed by endpoint (`local` or a machine id) and path.
  "Pin this Workspace" is in a WORKSPACES row's context menu; the sidebar's PINNED
  WORKSPACES section (drawn only when something is pinned) and Home's Pinned and
  Recent blocks open one the same way: `workspace.open` on a worker thread through
  the control client, which reaches a saved machine over SSH whether or not the
  window is attached to it, then `activate_opened_endpoint`. The window leaves its
  page only when the daemon has answered, so a failed open can be retried. A pin
  and a path are compared after expanding `~` against the machine's home, so the
  same folder spelled two ways is one pin. The menu bar item lists the pins under
  the agents (`menubar::Pinned`, a `Pick::Pinned` click); with no window open the
  helper opens one and hands it the folder (`PilusView::saved_pick`), which waits
  for the machine catalog before opening a remote pin.

- The GitHub Repositories section reads the signed-in account through `gh` on
  the machine `pilus` runs on, never through a daemon or SSH. It shows three
  repositories: `Settings::github_pinned` first, then the most recently pushed
  to. Every list, and each repository's issues, pull requests and workflow
  runs, goes through `pilus_client::GhAccount`'s cache (fresh lists are free,
  REST lists revalidate by `ETag`, a rate limit is waited out and persisted in
  `pilus.db`); do not call `gh` for these from the GUI directly, or the
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
  (the `contacts` document), the conversations (the `chat_*` tables, read into
  memory at launch and written through per change; the identity key stays under
  `~/.pilus/sipil`, and the old `contacts.json` and `chats.jsonl` there are
  imported once) and the `PeerService` that answers inbound calls. It answers
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
- Channels (the `channels` document, `pilus_client::sipil::Channels`) are named rooms of
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
  per channel (`Channels::remove_members`, `removed` in the `channels` document) so an
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
  agents" list (`Channels::set_agent`, kept in the `channels` document on that device
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
- One sipil install can run several primary agents (its `role: primary`
  blueprints, see `../sipil/src/host.ts`). Each is a daemon of its own to pilus:
  its own device ID, contact and sidebar row, so nothing here changes for them. A
  daemon's transcript marks a line an agent said to a primary (a coding agent's
  report, `sipil tell`, another primary's question) with
  `speaker`, which the log draws as that agent's nick, never the person's.
- A daemon's primary agent keeps a board of its open work and a list of
  routines (`../sipil/src/work.ts`, `routines.ts`), read with the `work` and
  `routines` calls (`Sipil::work`, `Sipil::routines`) in the same poll as its
  agents and sessions. A failed read keeps the last good one and never calls the
  daemon down; a daemon that answers `unknown_method` is from before them
  (`Daemon::board_unsupported`) and is not asked again until it reconnects. The
  chat page's header has a Work button for a daemon that has a board, with a count
  of the items waiting on someone (`waiting` or `blocked`); it opens
  `work_panel` beside the log, which shares the side with the about panel and a
  thread. The agent keeps the board; pilus only shows it.
- A daemon being away does not close its agent. `pilus_client::sipil::Offline`
  (the `offline` document) keeps each daemon's primary agent, the newest
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
  SIPIL section draws its primary agents from the daemons' half of that snapshot,
  and its people and channels from `SipilState::me`. There is no subagent: every
  agent of a daemon is a primary with a row of its own, and `Role` has no other
  value. There
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
  `call_id` and has no line of its own while that call is in view. The daemon's
  `transcript` answers `conversation` too, which names a conversation neither list
  carries (`SipilState::watched`). A line can be replied to (the
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
  The composer has
  no status bar or `[nick]` prefix. The composer is cleared only when
  `SipilState::sent` grows, so a refused message stays to retry. A contact's page has no Stop. Never show an error as an empty line: a failed
  turn arrives as `error` on the agent, the session and the transcript line.

- Notes on excerpts of markdown files (`gui/md_notes.rs`) are the app's own,
  kept per machine and file in the `md_notes` table of `pilus.db`. They are not
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
- Quick open (⌘P, `gui/quick_open.rs`) and find in file (⌘F, `gui/file_find.rs`)
  belong to the Files tab. Quick open reads its own listing of the tree's root
  with `pilus_client::list_files` (`git ls-files` in a repository, `find`
  elsewhere, bounded by `MAX_FILES`; local or over SSH, on a worker thread),
  because the daemon's tree only carries open folders; it is read again each time
  the picker opens, and a pick goes through `files_open_path` like a tree click.
  Find matches the editor's text (smart-case, ASCII folding so byte offsets stay
  valid), repaints only when `TextInput::revision` or the query changed, paints
  its matches through `TextInput::set_marks` and shows the current one as the
  editor's selection. On a markdown preview ⌘F switches to the source first.
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

- A session row (Home's Recent Sessions, Search's results) opens
  `gui/session_view.rs`, a page of the conversation with Continue and Fork
  (`Reopen`; Fork is the agent's own flag, `Agent::fork_args`, never the daemon's
  resume). The conversation is `pilus_index::read_transcript_local`, or
  `read_transcript_remote` for a saved machine (the end of the file, cut by
  `tail` on the host, the path sent on stdin), read on a worker thread, shown and
  kept nowhere, and only with `Settings::index_agents` and that session's machine
  in `index_machines`, as Search reads it. The page is a virtualized `list`; the
  first read is the last `WINDOW` (4 MB) of the file, and scrolling near the top
  reads a window twice as large (up to `MAX_WINDOW`) and puts the older entries in
  front (`SessionView::extend`), so a long session is never read whole unasked.
- Home (`gui/overview.rs`, the row below Open Workspace; the new-agent composer
  is `home.rs` and is called "New Agent") is read-only and owns no data: the
  every agent, in a Working now card and an Idle agents card (blocked, done, idle; the sipil daemons' agents are not listed, they have their own sidebar rows), each with its workspace, folder and branch, the
  newest ten rows of Search's index (same `Settings::index_agents` gate and
  consent hint; the file is opened per read, never held), open pull requests of
  the sidebar's repositories read through `GhAccount` on a worker thread (none
  at all with GitHub off), and the newest line in any chat thread or remembered
  daemon conversation (`sipil_social::latest`, local stores only, never dials).
  An idle agent's row opens a panel (`gui/overview_peek.rs`): its newest reply as
  Markdown, a reply field (`agent.prompt`) and Stop, which closes the agent's pane
  after a second press (`pane.close`). A terminal agent's reply is
  the end of its transcript (`pilus_index::last_reply_local`, found through
  `Index::session_for`), read on demand, shown and kept nowhere, and only with
  `Settings::index_agents` and this Mac ticked; otherwise, over SSH, or for an
  agent waiting on a question, the panel shows the tail of its screen
  (`agent.read`). The latest sipil
  message is drawn by the chat page's own Markdown path (`chat_text`). The
  composer has no Issue picker; the page's Issues block lists them.
  Between the composer and the columns sit the latest sipil message and, when a connected daemon
  has a board, an Open work block (`overview_work`: each board's open items, what
  waits on someone first, read from `Daemon::work` and never asked for here); a
  row opens that daemon's primary agent. The page is a flush card, so the hero
  runs to the window's edges.
  The hero is `assets/hero-factory.png`, cropped from the site's image. The page
  embeds the Cmd+Shift+N composer (`home_composer`, the same `PilusView::home`
  state): opening Home starts it (`home_begin`) and leaving either one closes
  both (`close_home_page`), so the page has no key layer of its own. GitHub's
  blocks (pull requests, issues) are hidden with GitHub off or `gh` missing, and
  the sipil message while sipil is not set up and switched on (`sipil_listed`).

## Untrusted input and local files

pilus runs outside the sandbox, so the rules below are what keep a pane, a
daemon or a contact from reaching past what it was given.

- What the daemon asks to open (`ServerMessage::OpenUrl`, from whatever runs in a
  pane, on a remote host too) reaches the system opener only when
  `surface::openable_url` passes it: http, https and mailto. `open` launches any
  registered scheme, and a `file:` URL starts an application.
- An SVG from another device (a sipil's logo) is parsed with
  `about::untrusted_svg_options`, which resolves only `data:` images: usvg would
  otherwise read any path an `<image>` names.
- A remote command reaches the host's login shell, which may be fish. Quote for it
  with `transport::quote_for_login_shell` (backslashes and quotes outside the
  quoted runs, which `sh` and fish read alike) and quote the script's own words
  with `git::sh_quote`; never build a remote command from unquoted input.
- Files under `~/.pilus` are owner-only. Create one with `pilus_db::create_private`
  and replace one with `pilus_db::write_private` (staged, synced, renamed), so a
  crash never leaves half a file; `pilus_db::private_dir` keeps the folder `0700`.
  A remote save is staged beside the file and copied over it only when every
  byte arrived (`write::remote_save_script`).
- Warnings and panics go to `~/.pilus/logs/pilus.log` (`logging.rs`, owner-only,
  cut over at 4 MB; `PILUS_LOG=info|debug|trace`). Report trouble with `tracing`,
  not `eprintln!`, or a bundle launched from Finder loses it.

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
- The cursor does not blink unless `Settings::cursor_blink` is on (off by default,
  Appearances). When it is, it stops blinking ten seconds after it last moved
  (`cursor_blink::BLINK_FOR`); the fade repaints the whole grid about twenty times
  a second, which was most of an idle window's cost. Moving it, or returning to the
  window, starts it again.
- A poll that is not driven by an event goes through `activity::scaled`, which
  stretches its interval five times while the app is behind another one
  (`activity::set_front`, fed by every window's tick loop from
  `dock::app_active`). Coming forward polls at once because the elapsed time has
  already passed. A new background thread that wakes on a timer takes it too.
- Network polls use `Backoff`, not a constant: see the sipil notes above.
- Energy mode (`Settings::energy_mode`, switched from the pilus menu, the menu bar
  item (`menubar::Pick::Energy`) or the command palette) draws over the person's
  choices without changing them: `Settings::drawn_radius` (square, except `Radius::Full`
  and `Soft`, the pill and the tab, which stay round as at a zero radius) and
  `drawn_opacity` (solid, so the blur view is dropped) replace `radius` and
  `glass_opacity` at every point of use, while the Appearances sliders keep
  reading the originals. Motion is GPUI's `reduce_motion`, set by
  `PilusView::sync_energy`; a new animation must honor `cx.reduce_motion()` or it
  will keep running under energy mode. Decorative paint with a GPU cost (the
  focused pane's shadow) checks `style::energy()`, and the tick loop idles at
  100 ms in front and 500 ms behind.
- The Process page (`gui/process.rs`; always available, reached from the command palette, not the sidebar) shows the same costs
  live: CPU, memory and per-thread CPU from `pilus_client::process` (macOS
  `proc_pidinfo`, the one other `unsafe` block, scoped and commented), the
  `perf::Count` totals as rates, and `pilus_client::netlog`, a ring of the last 300
  calls out of the machine. It samples once a second only while it is open. A new
  place that calls out (a request, a subprocess that reaches a host) records itself
  with `netlog::record` or `netlog::note`, or it will be missing from the page;
  local-only subprocesses (`git`, the daemon's socket) are not network and stay out.
- The Database page (`gui/database.rs`; always available, reached from the command
  palette, not the sidebar) shows what
  `pilus.db` holds, read-only: a summary and 50 rows of one table at a time from
  `pilus_db::browse`, which checks a table name against `sqlite_master` before it
  reaches a query, cuts every cell at 240 characters and never writes. It reads
  on open, on a table or page choice and on Refresh, never on a timer, and lets
  go of what it read when it closes, since a cell can be someone's message. A new
  table appears in it without a change here. The `visual-test` capture
  (`database_show_sample`) uses an in-memory database and never opens the real
  one.

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
