# Persistence

Read this before you change what pilus stores, where it stores it, or the
schema of either SQLite file.

## `~/.pilus/pilus.db`: durable, migrated, never dropped

Durable state lives in `~/.pilus/pilus.db` (`crates/pilus-db`), not in a pile
of JSON files. The database holds the only copy of:

- the sipil conversations, contacts, channels and offline snapshots;
- the markdown notes and the GitHub cache;
- the window's per-item state, the `ui_state` document (`settings::UiState`):
  `Settings::window`, `session_labels`, `session_icons`, `github_pinned`,
  `sipil_order`, `last_session`, `last_endpoint`, `pinned_workspaces`,
  `recent_workspaces` and the pane sizes.

Because it is the only copy, it is **migrated, never dropped**:

- A schema change is a new entry at the end of `MIGRATIONS` in
  `pilus-db/src/schema.rs`. Never edit a released entry.
- pilus refuses a file written by a newer pilus (`Error::Newer`) and does not
  write to it.
- Open the database with `Db::at(dir)`, which shares one connection per folder.
- Each store keeps its working copy in memory and writes every change through.
- Lock order: take the store's lock before the database's, never the reverse.
- Inside `Db::transaction`, never call a `Db` method. Use the `tx`, as
  `put_doc_in` does.
- Small state that is always written whole (contacts, channels, offline,
  `ui_state`) is one `docs` row. State that grows or changes one row at a time
  (chat lines, notes, the GitHub cache) is a table.
- A store that used to be a flat file imports it once on first open
  (`Db::import_once`, tagged in `imports`) and moves it aside as
  `<name>.migrated` (`pilus_db::retire`). It never deletes the old file.

## `index.db`: a cache

`index.db` (`pilus-index`, beside the settings) works the other way. It is a
cache: a `SCHEMA` bump rebuilds it and `wipe` deletes it, so it has no
migrations and nothing durable goes in it. Its read rules are in
[features/search-and-insights.md](features/search-and-insights.md).

## `settings.json`: preferences

`settings.json` keeps the preferences. Two readers depend on it staying a plain
file: `pilus-agent` reads its `relay` block, and `--check-updates` reads
`Settings::preferences`, which does not open the database.

## Private files

Everything under `~/.pilus` is owner-only. The rules for creating and
replacing files there are in [security.md](security.md#private-files).

## The Database page

The Database page (`gui/database.rs`) is opened from the command palette, not
the sidebar, and is always available. It shows what `pilus.db` holds and never
writes:

- It shows a summary and 50 rows of one table at a time from
  `pilus_db::browse`. That function checks the table name against
  `sqlite_master` before the name reaches a query, and cuts every cell at 240
  characters.
- It reads when the page opens, when a table or page is chosen, and on Refresh.
  It never reads on a timer.
- It lets go of what it read when it closes, because a cell can hold
  someone's message.
- A new table shows up without any change to the page.
- The `visual-test` capture (`database_show_sample`) uses an in-memory database
  and never opens the real one.
