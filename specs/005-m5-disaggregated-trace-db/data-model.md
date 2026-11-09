# Data model: M5 additions

Proposed; follows the design register ([design.md](design.md)). Files on storage are unchanged. All additions are
catalog migrations that run the same on SQLite and Postgres (constitution V), in one `schema_version` step.

## Catalog

| | Table / column | Fields | Written by | Read by | Decision |
|---|---|---|---|---|---|
| 🟠 | `state.catalog_version` | `BIGINT NOT NULL DEFAULT 0` | every publishing transaction: flush, compaction swap, text-index sidecar, feedback write | readers, at the start of each query | KD-2 |
| 🟢 | `writer_lease` (one row) | `holder TEXT` (host, pid, start time), `number BIGINT`, `expires_at` | writer: take (raises `number`), renew | writer at every publish (`WHERE number = $mine`); `/healthz` | KD-7 |
| 🟢 | `reader_checkins` | `id TEXT PRIMARY KEY`, `address`, `catalog_version`, `data_up_to`, `seen_at` | each reader every 10 s | writer's status panel (rows seen in the last minute); stale rows pruned after a day | KD-10 |
| 🟠 | `feedback.updated_version` | `BIGINT`, the `catalog_version` of its last write | feedback writes | readers reload rows with `updated_version > their version` | KD-5 |
| 🟠 | `segments.replaced_at` (if M4 doesn't add an equivalent) | time compaction replaced the file | compaction swap | writer's delete loop (after `--grace`) | KD-6 |

## Rules

- **One version per query.** A reader answers a request from exactly one `catalog_version`; data not yet in a
  published file is absent, never partial.
- **Every publish bumps the version and checks the lease** in the same transaction:
  `UPDATE state SET catalog_version = catalog_version + 1` and `… WHERE writer_lease.number = $mine`; zero rows → the
  writer has lost its lease and stops.
- **Multi-version runs on readers** come from files only (KD-4): the run-id filter over every file's `run_id` column
  and `needs_merge` rows, as the writer's start-up does (D-025).
- **Deletion**: a replaced file is deleted only when `now − replaced_at ≥ grace`; readers' queries are limited to
  less than `grace`.

## Reader state (memory only)

| State | Built from | Updated when |
|---|---|---|
| `Version` (files, field stats) | `segments`, `field_stats` | each new version: added files appended, replaced files removed |
| Multi-version set, run-id filter | files' `run_id` and `needs_merge` | each new file |
| Merged copies of multi-version runs (`VersionRows`, D-023/D-029) | files | new files incrementally; after removals, rebuilt in the background |
| Feedback index | `feedback` | rows with `updated_version` newer than the reader's |
