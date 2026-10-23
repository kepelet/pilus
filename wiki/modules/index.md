---
title: pilus-index
description: Transcript indexing, cache schema, usage parsing, and pricing.
tags:
  - wiki
  - module
---
# pilus-index

## Summary

`pilus-index` is a local index of coding-agent sessions on this machine and on saved remote ones, so the window can search them, show token usage, and reopen a conversation. The data flow is one way: scan transcripts, parse the few lines worth reading into a title and metadata, and keep the result in a SQLite cache.

The index holds no message text. It is a cache of what the agents wrote, reproducible from transcripts. Nothing here asks whether it may run: the caller owns that decision (`Settings::index_agents`) and must not call `refresh` before it is made.

## Responsibilities

- List transcript files and read evidence only from files that changed.
- Parse titles, timestamps, models, and assistant usage into typed rows.
- Keep sessions and usage in `index.db`; write both in one transaction.
- Answer Search, Insights, and session replay queries.
- Read the end of a transcript for a session page, bounded by windows.
- Estimate list-price cost for turns without a recorded figure.

## Public API / entry points

From [lib.rs](../../crates/pilus-index/src/lib.rs): `Agent`, `Session`, `Reply`; `refresh`, `Report`, `Source`; `Index`, `Filter`, `wipe`; `read_transcript_local`, `read_transcript_remote`, `Transcript`, `Entry`, `WINDOW`, `MAX_WINDOW`; `last_reply_local`; stats types `Cost`, `Hourly`, `Overview`, `Ranked`, `Scope`, `Tokens`; `Bucket`, `Turn`; `Price`, `estimate_cost`, `price_of`.

## Key files

| Concern | File |
| --- | --- |
| Scan and remote evidence script | [scan.rs](../../crates/pilus-index/src/scan.rs) |
| Parsing titles/metadata | [parse.rs](../../crates/pilus-index/src/parse.rs) |
| Refresh orchestration | [refresh.rs](../../crates/pilus-index/src/refresh.rs) |
| SQLite cache | [store.rs](../../crates/pilus-index/src/store.rs) |
| Usage extraction | [usage.rs](../../crates/pilus-index/src/usage.rs) |
| Pricing table and estimates | [pricing.rs](../../crates/pilus-index/src/pricing.rs) |
| Stats queries | [stats.rs](../../crates/pilus-index/src/stats.rs) |
| Transcript reading | [transcript.rs](../../crates/pilus-index/src/transcript.rs) |
| Row types | [model.rs](../../crates/pilus-index/src/model.rs) |

## Cache schema

[store.rs](../../crates/pilus-index/src/store.rs) uses `SCHEMA = 3` and drops/rebuilds on mismatch. Two tables:

- `sessions` — title, machine, agent, path, updated time, metadata; indexed by `updated`.
- `usage` — machine, transcript, hour, model, four token counts, turns, recorded cost; indexed by `hour`.

## Consent and remote reads

- `Settings::index_agents` gates every read of a transcript, locally and remotely.
- `Settings::index_machines` gates each remote machine; unticking forgets its rows.
- Turning the feature off calls `wipe`, which deletes the cache.
- Reads run on one background thread, this Mac first; a machine that cannot be listed keeps its rows.
- A remote answer comes from `grep`/`tail` on the host, and `scan::evidence_script` must choose the same lines as `read_local`.

## Usage and cost

A `Turn` is one assistant line's model and token counts. Claude repeats a streamed message id on every line, so `dedupe` keeps the last per id; zero-token and `<synthetic>` turns are not usage. The local reader parses JSON; `usage::REMOTE_AWK` cuts the same fields on the host, and a test runs both over one set of lines. Cost is Pi's own recorded figure where present; otherwise `pricing.rs` estimates at list price when a page asks, never stored. A model with neither is *unpriced*: counted but excluded from dollars.

## Dependencies

`rusqlite` (bundled), `pilus-client` (`SshTarget`), `serde`/`serde_json`, `thiserror`. No GPUI.

## Design decisions

- **Cache, not truth.** Dropping and rebuilding on schema change is cheaper and safer than migrating a derived file.
- **Evidence, not transcripts.** Only the lines that name a session cross the wire from a remote host; the rest of a session is read on demand.
- **One reader, bounded.** A session page reads the last 4 MB and grows to 8 MB on demand, so a long transcript is never read whole unasked.
- **No permission of its own.** Consent lives in settings and is checked by the GUI; the library refuses to guess.

## Participates in

- [Search, stats, and resume](../flows/session-search.md)

## Related

- [Persistence and data ownership](../architecture/persistence.md)
- [pilus-db](./db.md)
