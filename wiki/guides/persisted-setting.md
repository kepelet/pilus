---
title: Add a persisted setting
description: Choose settings.json or ui_state, then wire load and save.
tags:
  - wiki
  - guide
---
# Add a persisted setting

## Goal

Store a new preference or piece of per-item window state so it survives relaunch, without putting durable data in a cache or sensitive state in the wrong file.

## Steps

1. **Choose the home.** A preference (font, theme, toggle, update channel, relay) goes in `settings.json`. State the window accumulates per item (geometry, labels, pins, order, last selection, pane sizes) goes in `pilus.db`'s single `ui_state` document. Never put either in `index.db`.
2. **Add the field.** Add it to `Settings` in [settings.rs](../../crates/pilus/src/settings.rs) with a serde default, and give it a value in `impl Default for Settings`.
3. **If it is window state, register the key.** Add the field name to `STATE_KEYS` and to the `UiState` struct that `Settings::save` writes and `Settings::load` reads. A key not in `STATE_KEYS` is never copied back.
4. **Load and save.** `Settings::load` fills the value from the database (or JSON) and `Settings::save` writes it back; both are already called on the normal lifecycle. Do not write the database from a render function.
5. **Mirror only if the daemon needs it.** A client-only toggle stays local; a value the daemon also projects (e.g. workspace path visibility) is additionally pushed through the relevant control/`ShellDockLayout` field.
6. **Test the round trip.** The settings tests rebuild a `Settings` from a temporary database and assert the key survives; extend them for a new `ui_state` field.

## Relevant code

- [settings.rs](../../crates/pilus/src/settings.rs) — `Settings`, `UiState`, `STATE_DOC`, `STATE_KEYS`, `load`, `save`, `Default`.
- [pilus-db/lib.rs](../../crates/pilus-db/src/lib.rs) — `doc`, `put_doc`, `transaction`.
- [Persistence and data ownership](../architecture/persistence.md) — the file split and lock order.
- [Migrations](../concepts/migrations.md) — when a schema step is needed instead.

## Gotchas

- `settings.json` is read by `pilus-agent`'s `relay` block and by `--check-updates`, which do not open the database. A field those paths need must stay in JSON.
- Adding a new database table is a new `MIGRATIONS` entry, not a `ui_state` key; a released migration is never edited.
- `Settings::save` must not be called inside a `Db::transaction`; the store takes its lock first, then the database's.
- A field with no serde default breaks every existing file. Give it one.
- Secrets do not belong in either file: the relay secret is a documented exception kept owner-only.

## Related

- [Persistence and data ownership](../architecture/persistence.md)
- [Migrations](../concepts/migrations.md)
- [pilus-db](../modules/db.md)
