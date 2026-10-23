---
title: Migrations
description: Append-only schema for durable state, rebuild for caches.
tags:
  - wiki
  - concept
---
# Migrations

## Definition

The schema policy for pilus's two SQLite files. Durable state uses append-only migrations; caches are dropped and rebuilt.

## Why it matters

`pilus.db` holds the only copy of conversations, notes, contacts, and window state. Editing a released migration or reordering a step would corrupt an installed user's file, and silently downgrading would lose data. `index.db` holds nothing that cannot be re-derived from transcripts, so rebuilding is cheaper and safer than migrating. The two policies are intentionally different and easy to confuse.

## Where it lives

- [pilus-db/schema.rs](../../crates/pilus-db/src/schema.rs) — `MIGRATIONS`, oldest first; `PRAGMA user_version` says how many have run. A newer file is `Error::Newer`.
- [pilus-db/lib.rs](../../crates/pilus-db/src/lib.rs) — `import_once` and `retire` for legacy flat files.
- [pilus-index/store.rs](../../crates/pilus-index/src/store.rs) — `SCHEMA = 3`; a mismatch drops and rebuilds.
- [AGENTS.md](../../AGENTS.md) — the invariant, stated for contributors.

## Rules

- A durable change is a new entry at the end of `MIGRATIONS`; a released one is never edited.
- A cache change bumps `store::SCHEMA` and expects a rebuild.
- Never call a `Db` method inside `Db::transaction`; use the `tx`.
- Take the store's lock before the database's, never the reverse.
- Import a legacy flat file once, then retire it as `<name>.migrated`; never delete it.

## Related

- [Persistence and data ownership](../architecture/persistence.md)
- [pilus-db](../modules/db.md), [pilus-index](../modules/index.md)
- [Add a persisted setting](../guides/persisted-setting.md)
