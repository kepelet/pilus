---
title: pilus-db
description: The durable SQLite file, migrations, and private-file helpers.
tags:
  - wiki
  - module
---
# pilus-db

## Summary

`pilus-db` is the durable SQLite file behind pilus's own state, `~/.pilus/pilus.db`. It is not the search index: this file holds the only copy of conversations, notes, contacts, channels, offline snapshots, the GitHub cache, and the window's per-item state. It is migrated in place and never deleted.

## Responsibilities

- Open (and create, owner-only) the database, enabling WAL and a busy timeout.
- Share one connection per folder across the process (`Db::at`).
- Run append-only migrations and refuse a file from a newer pilus.
- Provide document reads/writes and transactions with a defined lock order.
- Import legacy flat files once and retire them.
- Offer a read-only browse API for the Database page.
- Own the private-file helpers (`create_private`, `write_private`, `private_dir`, `retire`).

## Public API / entry points

From [lib.rs](../../crates/pilus-db/src/lib.rs): `Db::open`, `Db::at`, `Db::get_doc`/`put_doc`/`transaction`, `Db::import_once`, `FILE`, `create_private`, `write_private`, `private_dir`, `retire`, `Error`, `Result`, and a re-export of `rusqlite`. [browse.rs](../../crates/pilus-db/src/browse.rs) exposes `tables`, `TableInfo`, `Cell`, and a paged row read with `CELL_LIMIT = 240`.

## Key files

- [lib.rs](../../crates/pilus-db/src/lib.rs) — connection sharing, docs, transactions, private files.
- [schema.rs](../../crates/pilus-db/src/schema.rs) — `MIGRATIONS`, the append-only schema.
- [browse.rs](../../crates/pilus-db/src/browse.rs) — read-only table inspection.
- [error.rs](../../crates/pilus-db/src/error.rs) — including `Error::Newer`.

## Schema

Migration 1 creates:

| Table | Holds |
| --- | --- |
| `docs` | Whole-document state: contacts, channels, offline memory, `ui_state` |
| `imports` | Legacy flat files already imported |
| `chat_threads`, `chat_lines`, `chat_requests`, `chat_cursors` | sipil conversations and daemon cursors |
| `md_notes` | Notes on markdown excerpts, per endpoint and file |
| `github_cache` | ETag/body cache for GitHub lists |

A new table appears in the Database page without a change there. The page reads only on open, table/page choice, and Refresh, and never writes.

## Dependencies

`rusqlite` with `bundled` (SQLite compiles from source with the command-line C compiler, so no system library is needed), `serde`, `serde_json`, `thiserror`.

## Design decisions

- **One writer path.** `Db::at` shares a connection per folder so a store and the settings cannot race through two writers.
- **Durable, therefore migrated.** `MIGRATIONS` is append-only, `user_version` moves with the step, and a newer file is `Error::Newer` rather than a silent downgrade.
- **Stores write through.** The working copy is in memory; each change is committed immediately. A crash leaves the old document, not half of a new one.
- **Legacy files are retired, not deleted.** `import_once` plus `retire` makes downgrading a rename away.
- **Browsing cannot leak SQL.** Table names are checked against `sqlite_master`, and every cell is cut to 240 characters.

## Related

- [Persistence and data ownership](../architecture/persistence.md)
- [Migrations](../concepts/migrations.md)
- [pilus-index](./index.md)
