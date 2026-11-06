# Decision log: changes of direction

Every time an earlier decision is **changed** (not just made), it gets an entry here: what we decided before,
what we decided instead, why, and which documents were updated. Current rules live in the
[roadmap](roadmap.md)'s Decisions list; this file keeps the history of how they got there.

Newest first. IDs are stable; link to them as `decisions.md#d-012`.

| ID | Date | Was | Now | Why | Updated |
|---|---|---|---|---|---|
| <a id="d-016"></a>D-016 | 2026-11-06 | M2 design §4.3: the flusher also marks `needs_merge` when a run id hits an older file's bloom filter | `needs_merge` = the version was started by an update, or the run already had a frozen version, or it is in the multi-version set; no bloom lookups at flush | a late part of a run always arrives as an update (covered); the only miss is a second *start* for a run already on disk, which the SDK doesn't send | ingest `writer.rs`, `flush.rs`; design §4.3 to follow |
| <a id="d-015"></a>D-015 | 2026-11-06 | M2 plan: `Version` pinning in `storage/src/version.rs` | `Version` lives in ingest next to the memtables, behind the same lock, so a reader takes active + frozen + files in one step | a freeze or flush can't be seen half-done; storage stays free of ingest state | ingest `memtable.rs`; plan structure to follow |
| <a id="d-014"></a>D-014 | 2026-11-06 | M2 data model: core file keeps the first 1 KB of `error` | Core keeps the full `error` text (validation already caps it at 1 MB) | the stored version must merge exactly like the memtable state; truncating would change merged values | storage `segment.rs` |
| <a id="d-013"></a>D-013 | 2026-11-06 | M2 design: our own `Cache` trait (moka LRU) for footers, blooms, row groups | DataFusion's built-in Parquet metadata cache; no `Cache` trait in M2 | DataFusion 55 already caches file metadata; less code, same effect | storage `lib.rs`; design §3 and starter guide to follow |
| <a id="d-012"></a>D-012 | 2026-11-06 | M2 design: Parquet map columns `attrs_str/num/bool` for promoted JSON leaves | One JSON text column `attrs`, evaluated by our filter code after the scan; DataFusion pushdown only on built-in columns | maps get no row-group or page pruning anyway (primer §5), and one evaluator keeps results consistent | storage `segment.rs`; design §5, data model §2 to follow |
| <a id="d-011"></a>D-011 | 2026-11-06 | Unpromoted JSON values (long strings, arrays, past the leaf budget) were simply absent from attributes | They're kept as `unindexed` markers: present for `eq/neq null`, and comparisons on them report `not_indexed` | review of unit 1: `eq(metadata.long, null)` matched a run that has the value | `crates/model/src/attrs.rs`, filter contract |
| <a id="d-010"></a>D-010 | 2026-11-06 | M2 tasks: a separate `RunVersion` type | A version is a `RunState` with a `present` bitmask; `merge_versions` replays the M1 merge rule | one merge rule for live ingest, files and reads; less code | tasks T012, data-model §2 (bitmask) |
| <a id="d-009"></a>D-009 | 2026-11-06 | Identity conflicts always rejected (M1) | Runs only on disk: accepted, first identity kept, `identity_conflict` warning (M2 design §8 q1) | checking against disk would put file reads in the write path | M2 design §8, `merge_versions` |
| <a id="d-008"></a>D-008 | 2026-11-06 | Dedup window 24 h (M1) | 10 min, configurable; WAL kept at least that long (M2 design §8 q2) | a 24 h key set doesn't fit in memory at 2M+ runs; SDK retries come within seconds | M2 design, data-model §7 |
| <a id="d-007"></a>D-007 | 2026-11-06 | `cargo build` runs the UI build (M1 design §11.3) | `build.rs` embeds `ui/dist` if present, else a placeholder; CI builds the UI as its own job | M1 review: the server build shouldn't need Node; UI work shouldn't need a server rebuild | M2 spec FR-25, `build.rs`, CI |
| <a id="d-006"></a>D-006 | 2026-11-06 | M1: `507` when the memory budget is full | M2: `429` + `Retry-After` (write slowdown, backlog R-8); `507` removed | data moves to disk, so memory is no longer a data limit; the SDK retries `429` | M2 design §4.1, HTTP contract |
| <a id="d-005"></a>D-005 | 2026-11-06 | Milestones M0–M4 | M1–M5 everywhere (docs, code comments, messages, phase folder `001-m1-…`) | milestones numbered from 1 | all docs; commit `466ec73` |
| <a id="d-004"></a>D-004 | 2026-11-06 | Commits carried a `Co-Authored-By: Claude` trailer and a work email | No trailer; personal email; history rewritten (backup tags `backup/pre-rewrite/*`) | owner's request | git history, roadmap |
| <a id="d-003"></a>D-003 | 2026-11-05 | M1 spec: ready within 10 s after restart at the capacity limit | 30 s (M1 design §11.4); measured 0.83 s at 500k runs | full-WAL replay could take 5–20 s before measuring | M1 spec Story 3 |
| <a id="d-002"></a>D-002 | 2026-11-05 | Backlog R-2: replace `eq()/and()` with `field:value` | Keep `eq()/and()` canonical; `field:value` optional, M2 or later | SDK 0.14.17 still documents `eq()/and()/has()` | design-notes §5.2, roadmap R-2 |
| <a id="d-001"></a>D-001 | 2026-11-05 | Merge rule: `events` append | Every field, incl. `events`, latest present value wins | the SDK re-sends the full `events` list on every patch | research R1, data model |

## How to add an entry
1. Add a row at the top with the next ID.
2. Update the roadmap's Decisions list to the new rule (it shows what holds now).
3. Fix the affected docs in the same commit, and list them in "Updated".
