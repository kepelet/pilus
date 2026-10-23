# Home

Read this before you change the Home page (`gui/overview.rs`), its idle-agent
panel, or the New Agent composer it embeds.

The sidebar's top rows are Open Workspace, then Home, Search and Insights, in
that order.

## Home owns no data

Home stores nothing of its own. Everything on it is read from state that
already exists, and its actions (the composer, a reply, Stop) go to the daemon.

The page is a flush card, so the hero image runs to the window's edges. From
top to bottom it shows:

1. **The New Agent composer** (`home_composer`). This is the Cmd+Shift+N
   composer (`home.rs`), sharing the same `PilusView::home` state. Opening Home
   starts it (`home_begin`), and leaving either one closes both
   (`close_home_page`), so the page has no key layer of its own. It has no Issue
   picker; the page's Issues block lists issues.
2. **The latest sipil message.** This is the newest line in any chat thread or
   remembered daemon conversation (`sipil_social::latest`, which reads local
   stores only and never dials). It is drawn by the chat page's own Markdown
   path (`chat_text`) and hidden unless sipil is set up and switched on
   (`sipil_listed`).
3. **Open work** (`overview_work`). When a connected daemon has a board, this
   block shows each board's open items, with what waits on someone first. It is
   read from `Daemon::work` and never fetched from here. A row opens that
   daemon's primary agent.
4. **Agents**: every agent, in a Working now card and an Idle agents card
   (blocked, done, idle), each with its workspace, folder and branch. The sipil
   daemons' agents are not listed here; they have their own sidebar rows.
5. **Recent sessions**: the newest ten rows of Search's index, under the same
   `Settings::index_agents` gate and consent hint. The file is opened for each
   read and never held open.
6. **GitHub**: open pull requests and issues of the sidebar's repositories,
   read through `GhAccount` on a worker thread. These blocks are hidden when
   GitHub is off or `gh` is missing.

## The idle-agent panel (`gui/overview_peek.rs`)

Clicking an idle agent's row opens a panel with its newest reply as Markdown,
a reply field (`agent.prompt`), and Stop. Stop closes the agent's pane
(`pane.close`) on a second press.

- A terminal agent's reply is the end of its transcript
  (`pilus_index::last_reply_local`, found through `Index::session_for`). It is
  read on demand, shown and kept nowhere, and read only when
  `Settings::index_agents` is on and this Mac is ticked.
- Otherwise the panel shows the tail of the agent's screen (`agent.read`). That
  covers agents over SSH and agents waiting on a question.
