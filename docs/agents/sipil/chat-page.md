# Sipil: sidebar, Sipil tab and chat page

Read this before you change how sipil is drawn: sidebar rows, the right-hand
Sipil tab, the chat page and its panels.

## Where state lives

- `gui/sipil.rs` owns the one shared `SipilState` and its poller, and holds the
  daemons' half.
- `gui/sipil_social.rs` holds the people's half: contacts, channels, mentions
  and presence.
- The chat page owns no conversation data. `sipil::watch` names the open
  conversation. The poller reads a daemon's transcript (every second) or a
  contact's thread from the chat store. Sends go through `sipil::send`.

## The sidebar's SIPIL section

The section is always there.

- It draws its primary agents from the daemons' half of the snapshot, and its
  people and channels from `SipilState::me`.
- Contacts and threads are read from disk, so they are listed even when the
  network is down. People get a user icon, an unread count and an online dot.
- Each daemon has its own row, which is where its status shows. There is no
  separate "sipil" row.
  - A daemon that is up shows its primary agent with a green dot at the right,
    like a contact who is there.
  - A daemon that is down stays listed with its logo and a grey offline dot,
    also while it connects. The reason is the tooltip, and a click opens the
    Sipil tab.
  - The row draws the daemon's logo (`Daemon::own_logo`) at `SIDEBAR_LOGO`,
    larger than a glyph. Without a logo, `daemon_mark` draws the pilus mark in
    the icon colour, with nothing behind it.
- There is no sessions list in the UI. A daemon is talked to through its
  agents.
- Rows are dragged into order and kept in `Settings::sipil_order` by key
  (`daemon:<id>`, `channel:<id>`, `person:<id>`). Knocks are not part of the
  order and stay first. Rows not named in the order follow in the default
  order (`move_sipil_row`, `sipil_rank`).

## The Sipil tab

There is no separate sipil window.

- The way in is the outlined pilus mark at the head of the right toolbar pill,
  before Browser (`right_sipil_toggle`). It opens the right sidebar's Sipil tab
  (`SidebarTab::Sipil`, the `SipilPanel` in `sipil_panel.rs`).
- The panel is made the first time it opens, and that is when the identity key
  is written (see [network.md](network.md#the-node-and-identity)).
- It takes the keyboard as `InputLayer::SipilPanel` while the column has focus.

## The chat page (`gui/sipil_chat.rs`)

Clicking an agent, a channel or a contact opens the chat page. It is drawn in
the terminal's card with its own `InputLayer::Sipil`, flush to the sidebar
(`flush_page_card`, no inset card), and laid out like an IRC client.

### The log

- The page has a topic bar, a log of `time nick: text` lines in the terminal
  font (no brackets), and an input line.
- On a daemon's page, another agent is never a speaker. A report from a pane
  (`sipil tell`), an answer to `ask_agent` or a routine's note arrives as a user
  line with a `speaker`; it is drawn as a muted `--` notice ("from Briefing")
  with no nick column (`is_report`). It is a reply to the nearest earlier line of
  the agent's that started the sender, else made a call naming it
  (`ask_agent briefing`). The daemon ends a `pilus_start_agent` result with
  `[pane 12]` and puts the pane a `sipil tell` or a watch report came from on the
  notice (`TranscriptItem::pane`), so a report goes under the spawn that made
  that pane and not under a later call about it (`started_agent`). A report with
  no pane is matched by the agent's name. So it
  leaves the log and that line shows "1 reply"; `group_threads` attaches it, and
  what the agent says next stays in the log. A report no call names stays in the
  log as a plain notice. In a chat with people the same shape is the
  person's own agent speaking, and stays a normal line.
- Tool calls are `*` lines, tool output `-` lines, and a failed turn a `!!`
  line. Nicks are colored from the theme's ANSI hues by name.
- A line's text is drawn as Markdown when it reads as Markdown
  (`looks_like_markdown`). `gui/markdown.rs` draws it in the terminal font and
  parses it once per text.
- A tool call is a folding line (`call_block`). Closed, it shows the tool and
  its summary. Open, it shows the arguments and the result. The result is drawn
  under its call by `call_id` and has no line of its own while that call is in
  view.
- The daemon's `transcript` also answers `conversation`, which names a
  conversation that neither the daemon's agents nor its sessions carry
  (`SipilState::watched`).
- The log is a virtualized gpui `list` held in `SipilChat`, so a long
  conversation costs the same as a short one. `SipilChat::sync` runs when the
  state snapshot changes, and re-measures only the rows from the first one
  whose `signature` changed. The list follows the end (`FollowMode::Tail`)
  until the reader scrolls up.
- Every line highlights on hover.
- Never show an error as an empty line. A failed turn arrives as `error` on the
  agent, the session and the transcript line.

### Replies and threads

- A line can be replied to with its hover button. The `Quote` (who said it, and
  the start of the text) travels as `reply` in `chat.send` and `chan.send` and
  is stored with the line on both sides. A daemon or a contact's agent has no
  field for a quote, so it gets the quote as Markdown in front of the message.
- A reply makes a thread. `group_threads` finds the line a reply quotes by its
  text, taking the nearest match before the reply. With a daemon, the quote is
  the Markdown prefix that `reply_of` reads back, and the agent's answers follow
  into the thread.
- Replies leave the log, and the quoted line shows "N replies", which opens
  `thread_panel` beside it. A message sent there is one more reply.
- A reply whose quoted line is out of view stays in the log.

### The composer

- The composer is a flat input line with no Send button, no status bar and no
  `[nick]` prefix. It is deliberately not a `style::field`; this is the one
  named exception to [ui.md](../ui.md#the-contract).
- It is cleared only when `SipilState::sent` grows, so a refused message stays
  in it for a retry.
- A contact's page has no Stop.

### Header and panels

- A daemon describes itself with the `about` call
  (`pilus_client::sipil::About`): name, host, workspace, model, the agents with
  their instructions and tools, and a logo as SVG. It is asked once per connect
  and kept on `Daemon::about`. An older daemon answers `unknown_method` and is
  not asked again.
- The logo is drawn by `about::rasterize` (parsed as untrusted, see
  [security.md](../security.md)) and appears in the header, the About panel and
  the sidebar row.
- The header's info button opens `about_panel` on a daemon's page and
  `contact_panel` on a contact's page. `contact_panel` shows the name with
  rename, the device ID, the channels in common, and remove. While the name is
  being edited, `SipilChat::active_input` gives the keyboard to the rename
  field.
- For a daemon with a board, the Work button opens `work_panel` (see
  [daemons.md](daemons.md#work-board-and-routines)).

### Notifications

A message that arrives while the window is not in front also posts a system
notification (`notify::post`, target `("sipil", thread)`). Clicking it opens
that conversation.

## Captures

The `visual-test` captures use `sipil::preview`. They never dial and never
write the identity.
