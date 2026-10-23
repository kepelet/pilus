# Search, Insights and the session page

Read this before you change `pilus-index`, the Search page (`gui/search.rs`),
the Insights page (`gui/stats.rs`) or the session page (`gui/session_view.rs`).

## Consent comes first

Search lists the Claude and Pi sessions on this Mac and on saved SSH machines,
from `index.db` (see [persistence.md](../persistence.md#indexdb-a-cache)).

- Nothing may read a transcript, locally or over SSH, until the person turns on
  `Settings::index_agents`.
- A remote machine is read only while its id is in `Settings::index_machines`.
- Never call `pilus_index::refresh` from code that skips these checks.
- Turning the setting off calls `pilus_index::wipe`. Unticking a machine forgets
  its rows.

## Search

- The index holds titles and metadata, never message text.
- Reads run on one background thread (`search_sync`), this Mac first. A machine
  that cannot be listed keeps its existing rows.
- A remote answer comes from `grep`/`tail` on the host, so keep
  `scan::evidence_script` and `read_local` choosing the same lines.
- Resume is `workspace.open` on the session's folder, then `agent.resume` on
  that machine's daemon. The daemon only knows the sessions its own hook
  recorded. On "no resumable session", an agent with `Agent::resume_flag`
  (Claude) is launched by `agent.start` with `--resume <id>` instead.
- The `visual-test` captures use `search_show_sample` and never touch the index.

## Insights (token use)

Insights totals the same index's token use. It has no permission of its own: it
shows Search's consent card, follows the same `index_agents` and
`index_machines` gates, and is fed by the same `search_sync` read.

- **Storage**: per-hour buckets in the `usage` table (machine, transcript, hour,
  model, four token counts, turns). `refresh` writes them in the same
  transaction as the session row, so a session never exists without the usage
  its size says was read.
- **Reading usage** (`usage.rs`): count assistant turns only. Claude repeats a
  streamed message's id on every line, so `dedupe` keeps the last line per id.
  Turns with zero tokens and `<synthetic>` turns are not usage.
- **Parity**: this Mac's lines are parsed as JSON, while a remote host's are
  cut out by the `awk` in `usage::REMOTE_AWK`. Keep the two producing the same
  `Turn`; a test runs both over one set of lines.
- **Counts** are what each turn reported, not a bill.
- **Cost**:
  - When a turn records its own cost (Pi), use that figure
    (`Turn::cost_micros`, kept per bucket with a `recorded` count).
  - When it does not (Claude), make a list-price estimate when a page asks for
    it, from the table in `pricing.rs` (taken from pi.dev/models as listed
    there). The estimate is never stored.
  - A model with neither is *unpriced*. Its tokens are counted in
    `Cost::unpriced` and left out of the dollar figures, never guessed.
  - Cache writes are priced at the 5-minute rate, because a transcript does not
    say which lifetime was used.
- **Changing things**: a price change needs no re-index. A change to what is
  stored, or to any table, means bumping `store::SCHEMA`, which rebuilds the
  cache.
- **Time**: hours are stored in UTC and grouped into days in the viewer's offset
  (`date +%z`) when the page opens.
- The heatmap cell and ranking row are `style::heat_cell` and `style::rank_row`,
  shown in the playground.

## The session page (`gui/session_view.rs`)

A session row (Home's Recent Sessions, Search's results) opens a page of the
conversation with Continue and Fork (`Reopen`). Fork is the agent's own flag
(`Agent::fork_args`), never the daemon's resume.

- The conversation comes from `pilus_index::read_transcript_local`, or from
  `read_transcript_remote` for a saved machine (the end of the file, cut by
  `tail` on the host, with the path sent on stdin).
- It is read on a worker thread, shown, and kept nowhere. It is read only when
  `Settings::index_agents` is on and the session's machine is in
  `index_machines`, the same gate Search uses.
- The page is a virtualized `list`. The first read is the last `WINDOW` (4 MB)
  of the file. Scrolling near the top reads a window twice as large (up to
  `MAX_WINDOW`) and puts the older entries in front (`SessionView::extend`), so
  a long session is never read whole unless the reader asks for it.
