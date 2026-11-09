# M5 design: options and key decisions (open for discussion)

**Status**: Proposed, 2026-11-08; KD-13 to KD-17 added 2026-11-09. Nothing here is decided until its entry in the [decision register](#key-decision-register)
says **Decided**. Spec: [spec.md](spec.md) · Plan: [plan.md](plan.md) · Options from other systems: [research.md](research.md).

## How we use this document

This is the place to discuss the design before building it. It is deliberately not a locked architecture.

1. **Architecture options** ([below](#architecture-options)) describe the whole system at different levels of
   disaggregation. M5 picks one; the others stay written down so later steps start from them.
2. **Components and jobs** list what each part does, with its options.
3. **The decision register** holds every key decision: the question, options, how we judge them, a recommendation,
   and a status. Status moves **Open → Proposed → Decided** (or **Deferred** to the spec backlog). Only the owner moves
   an entry to Decided.
4. **Discussion log**: each entry keeps dated notes under it. A changed decision keeps its old text struck through
   with the reason, as [decisions.md](../000-smithling-master-plan/decisions.md) does for the master plan.
5. **Plan units name the decisions they need** ([plan.md](plan.md#delivery-order)). A unit doesn't start until its
   decisions are Decided.

## Architecture options

Four levels of disaggregation, from least to most. Each later step adds to the one before it.

| Option | What runs where | Fixes | Doesn't fix | Status |
|---|---|---|---|---|
| **A1. Read workers over files** | Writer: ingest, flush, compaction, live view, all reads. Readers: reads from published files and the catalog only | P1 (reads), P2, P7, P9; P10, P12 light | P5, writer is one machine, readers ≤ flush age | **Proposed for M5** (spec Decisions) |
| A2. A1 + fresher readers | A1, plus readers see unflushed data: snapshot through N plus a shared WAL tail (N, U], merged before filtering (research §7); or a shorter flush | + P3, P4, P6 | P5 | Backlog B2 (research) |
| A3. Full disaggregation | A2, plus a replicated WAL before confirming, compaction as its own service, writer handoff; storage and compute fully apart | + P5, P8, P10 full, P1 (compaction) | write rate of one writer | Backlog B1, B3, B4; researched as B11 |
| A4. Sharded | A3 with several writers, by trace id | + P11 | — | Backlog B5 |

```mermaid
flowchart TB
    subgraph A1["A1: M5"]
      W["Writer<br/>ingest, flush, compact,<br/>live view, all reads"]:::old
      R["Readers<br/>files + catalog only"]:::new
    end
    S["Shared files<br/>(folder or S3)"]:::old
    C["Catalog<br/>(Postgres)"]:::revised
    W --> S
    W --> C
    S --> R
    C --> R
    classDef new fill:#c8e6c9,stroke:#2e7d32,color:#1b1b1b
    classDef revised fill:#ffe0b2,stroke:#ef6c00,color:#1b1b1b
    classDef old fill:#eeeeee,stroke:#9e9e9e,color:#1b1b1b
```

Colours: 🟢 new in M5 · 🟠 changed · ⚪ as M4.

## Components and jobs

### Writer (⚪ as M4, 🟠 small changes)

| Job | Today | M5 change | Decisions |
|---|---|---|---|
| Ingest (validate, dedup, log, fsync, apply, confirm) | as M1–M3 | none | — |
| Confirm | after the group's WAL fsync, no fixed wait | also returns the batch's highest `seq` (optional) | KD-14, KD-15 |
| Flush and publish | one catalog commit per flush, moves `flushed_seq` | also bumps the catalog version; checked against the writer lease; `flushed_seq` is reported as `published_through_seq` | KD-2, KD-7, KD-13 |
| Compaction (M4) | background task in the writer | must meet M4-REQ-1–4; its swaps bump the catalog version | KD-2, KD-4 |
| Delete replaced files | M4 design | after the grace period | KD-6 |
| Lease | none | take at start, renew on a timer, stop if lost | KD-7 |
| Serve reads, live view | as today | none | — |

### Read worker (🟢 new)

| Job | What it does | Decisions |
|---|---|---|
| Start | Open storage and catalog read-only; load the newest catalog version and build its state; serve once ready | KD-1, KD-8 |
| Check for a new version | At the start of each query (and on a timer when idle) | KD-2, OQ-3 |
| Refresh state | New files' stats, multi-version runs, merged copies, feedback; files removed by compaction | KD-3, KD-4, KD-5 |
| Serve reads | Every read API from one pinned snapshot per query, with a time limit; says which `published_through_seq` it used | KD-6, KD-9, KD-13 |
| Wait for a write (optional) | `min_seq` + deadline: re-check the catalog until a snapshot reaches it, else "still publishing" | KD-14 |
| Report status | Catalog version, age of newest data, current or behind; check in to the catalog | KD-10 |

### Catalog (🟠)

| Part | Today | M5 change | Decisions |
|---|---|---|---|
| Files, field stats, feedback, dedup keys | as M3/M4 | none | — |
| Catalog version | none (the writer's in-memory `Version.id` only) | a stored counter, bumped by every publish | KD-2 |
| Writer lease | none | one row | KD-7 |
| Reader check-ins | none | one row per reader | KD-10 |
| Backend | SQLite or Postgres | split mode across machines needs Postgres | KD-8 |

### Shared storage (⚪)

No change. Write-once files with unique names, read by range, on a folder or S3-compatible storage. Readers want a
larger cache on S3 (KD-11).

## Key decision register

| ID | Question | Recommendation | Status | Blocks |
|---|---|---|---|---|
| [KD-1](#kd-1-how-does-a-reader-exist-without-a-writer) | How does a reader exist without a writer? | Read-only `Ingest` for M5 | Proposed | U2 |
| [KD-2](#kd-2-what-is-a-catalog-version) | What is a catalog version, and how do readers notice a new one? | Stored counter, looked up per query | Proposed | U1, U3 |
| [KD-3](#kd-3-how-do-readers-refresh-their-state) | How do readers refresh their state? | Incremental per new file; background rebuild after compaction | Proposed | U3 |
| [KD-4](#kd-4-how-do-readers-know-which-runs-have-several-versions) | How do readers know which runs have versions in several files? | Files are the truth: readers use the run-id filter, as the writer does at start | **Open (risk)** | U3, M4 |
| [KD-5](#kd-5-how-does-feedback-reach-readers) | How does feedback reach readers? | Feedback writes bump the catalog version | Proposed | U3 |
| [KD-6](#kd-6-grace-period-and-reader-query-time-limit) | Grace period and query time limit | 24 h and 1 h | Proposed (spec OQ-2) | U4 |
| [KD-7](#kd-7-writer-lease) | Writer lease: how it works | Lease row with a number, checked in every publish | Proposed | U1 |
| [KD-8](#kd-8-catalog-backend-in-split-mode) | Catalog backend in split mode | Postgres across machines; SQLite on one machine for tests | Proposed | U2 |
| [KD-9](#kd-9-the-live-view-on-a-reader) | What does a reader do with the live view? | Serve from files, labelled, with a link to the writer | Open | U2 |
| [KD-10](#kd-10-reader-status-and-check-ins) | How readers report status | `/healthz` + a check-in row in the catalog | Proposed | U5 |
| [KD-11](#kd-11-reader-cache-on-object-storage) | Reader cache on object storage | Same cache, larger default on S3 | Open | U6 |
| [KD-12](#kd-12-constitution-ii-record) | Record files-only reads as a staged SmithDB difference? | Yes, Principle II (c), B2 as the revisit | Proposed (spec OQ-1) | plan gate |
| [KD-13](#kd-13-what-a-snapshot-carries) | What does a snapshot carry, and where does `published_through_seq` live? | `catalog_version` + today's `flushed_seq`, read in one lookup | Proposed | U1 |
| [KD-14](#kd-14-the-min_seq-read-contract) | `min_seq` read contract | Query params, capped wait, 503 "still publishing"; optional in M5 | Open (spec OQ-6) | U8 |
| [KD-15](#kd-15-group-commit-interval) | Group commit: fixed 1 s interval or as today? | As today; 1 s is the ack P99 ceiling | Proposed (spec OQ-7) | — |
| [KD-16](#kd-16-shared-wal-tail) | Shared WAL tail for readers | Measure first; evaluate only if publication lag is too slow | Deferred → B2 | — |
| [KD-17](#kd-17-replicated-wal) | Replicated WAL durability | Separate decision, on durability needs | Deferred → B1 | — |

### KD-1: How does a reader exist without a writer?

`Query::new` takes a whole `Ingest` (`crates/query/src/lib.rs:218`); queries reach the writer's memtables, the
multi-version set, the project map, feedback and storage through it.

| Option | Change | For | Against |
|---|---|---|---|
| **a. Read-only `Ingest`** | `Ingest::open_reader(cfg)`: no log, no writer thread, empty memtables; `Version` and state from the catalog; `accept` refuses | Smallest change; queries unchanged | Writer-only paths stay reachable and must refuse; the name is misleading |
| b. Extract a read-state type | Move what queries need (`Version`, multi-version set, feedback index, storage) into one type both writer and reader provide | Clean seam; helps B2/B3 later | Touches every query module; bigger PR |
| c. Separate reader crate | A new crate with its own state | Clear ownership | Duplicates loading logic; most work |

**Criteria**: size of change (< ~400 lines per PR), risk to the writer path, how much later steps reuse.
**Recommendation**: (a) now, with (b) as a follow-up refactor once B2 or B3 needs it.

### KD-2: What is a catalog version?

Today the version id lives only in the writer's memory (`Version.id`, `crates/ingest/src/memtable.rs:22`), and the
catalog's `state` row holds `schema_version` and `flushed_seq`. Compaction swaps (M4) won't move `flushed_seq`.

| Option | For | Against |
|---|---|---|
| **a. A `catalog_version` counter in `state`, bumped in every publishing transaction** (flush, compaction swap, sidecar, feedback) | One cheap lookup tells a reader whether anything changed; exact | Every publisher must remember to bump it (test it) |
| b. Derive from data (newest segment id, newest feedback time) | No schema change | Misses removals; several lookups |
| c. Postgres LISTEN/NOTIFY | Readers learn at once | Postgres only; still needs (a) as fallback |

**Criteria**: exact (never miss a change), cheap per query, same on SQLite and Postgres (constitution V).
**Recommendation**: (a), looked up at the start of each query; (c) later if lookups show up in latency (spec OQ-3).

### KD-3: How do readers refresh their state?

State that follows the catalog: the file set and field stats (`Version`), merged copies of multi-version runs
(`VersionRows`, D-023/D-029), the multi-version set, feedback, the run-id filter (D-025). M3 measured a cold rebuild
of 203k multi-version runs at 9 s (M3-R4).

| Option | For | Against |
|---|---|---|
| **a. Incremental per new file**, as the writer does at each flush (`Version::with_flush`, new files' `needs_merge` rows); a full rebuild only when compaction removes files, done in the background while the reader keeps serving the previous version | Cheap per flush; reuses existing code | Two paths to test (incremental, rebuild) |
| b. Full rebuild on every new version | One path | Seconds per flush at 2M runs; a reader could fall permanently behind |
| c. Writer publishes a ready-made state snapshot file | Readers just load it | New file format; couples readers to writer internals |

**Criteria**: answers equal the writer's (SC-04), refresh time well under the flush age, code reuse.
**Recommendation**: (a). Answers are always from one whole version, so a slow rebuild only delays freshness.

### KD-4: How do readers know which runs have several versions?

**Risk found while planning.** The writer tracks multi-version runs in memory (`multi_version`, used by
`query/src/scan.rs:234`, `fields.rs:254`, `search.rs:276`) and marks file rows `needs_merge`. A run that joins the set
while its memtable is being flushed may not be marked in that file (`query/src/fields.rs:92`). The writer covers this
from memory; a reader can't. If a reader misses it, the run is counted twice in field counts and aggregates.

| Option | For | Against |
|---|---|---|
| **a. Readers rebuild the set from files**, as the writer does at start: the run-id filter over every file's `run_id` column (D-025), plus `needs_merge` rows; a new file's runs that hit the filter join the set | Same rule as the writer's restart path, already tested | Reader memory for the filter (~2.4 MB at 2M runs); a false positive only sends a run down the slower merging path |
| b. Make the flush mark every multi-version run in the file (close the race in the writer) | Files fully describe themselves; also needed for B3 (M4-REQ-1) | Touches the flush path; must not slow it |
| c. Writer publishes the set to the catalog | Exact | Grows with runs; another publish step |

**Criteria**: no double counting (SC-04), files are the source of truth (M4-REQ-1).
**Recommendation**: (a) and (b) together: (b) makes files correct, (a) lets readers rely on files alone. Needs a
property test: reader answers equal the writer's across random late updates and flush timing.
**Status**: Open, because it touches M4's flush and compaction rules.

### KD-5: How does feedback reach readers?

Feedback is written straight to the catalog (D-027), not the write log; an in-memory index is loaded at start.

| Option | For | Against |
|---|---|---|
| **a. Feedback writes bump the catalog version (KD-2)**; readers reload changed feedback rows | One change signal for everything | Needs a way to read only changed rows (an `updated_seq` column) |
| b. Readers reload all feedback on a timer | No schema change | Stale between reloads; full reload grows with feedback |
| c. Readers read feedback from the catalog per query | Always current | A catalog query per read; slower aggregates |

**Recommendation**: (a).

### KD-6: Grace period and reader query time limit

Spec OQ-2. Iceberg defaults to 3 days for orphan cleanup; Delta keeps 7 days and warns shorter can break readers
(research §3).

| Option | Grace | Query limit | For | Against |
|---|---|---|---|---|
| **a** | 24 h | 1 h | Ample margin; small storage cost | Replaced files linger a day |
| b | 1 h | 5 min | Less storage | Long agent queries and exports may fail |
| c | 7 days (Delta) | 1 h | Very safe | Storage for a week of replaced files |

**Recommendation**: (a), both configurable. The writer deletes; readers enforce the limit on their own queries.

### KD-7: Writer lease

| Option | For | Against |
|---|---|---|
| **a. A `writer_lease` row: owner, number, expiry; taking it raises the number; every publish checks the number in the same transaction** | Works on SQLite and Postgres; a paused writer can't publish after losing it | Renewal traffic (tiny for one writer) |
| b. Postgres advisory lock | Simple | No number to check; lost on disconnect; Postgres only |
| c. A lock file in shared storage | No catalog change | No fencing on S3 without compare-and-swap; weaker |

**Recommendation**: (a), also in one-process mode (one code path). Expiry 30 s, renewal every 10 s (configurable).

### KD-8: Catalog backend in split mode

| Option | For | Against |
|---|---|---|
| **a. Postgres for readers on other machines; SQLite allowed when every process shares one machine's folder** (tests, demos) | Both already supported (`storage/src/metastore.rs:1`) | SQLite readers must open read-only; SQLite on a network filesystem is unsafe, so refuse it |
| b. Postgres always in split mode | One rule | Local split demos need Postgres installed (constitution I: optional services only) |

**Recommendation**: (a).

### KD-9: The live view on a reader

| Option | For | Against |
|---|---|---|
| a. Refuse, pointing to the writer | Honest; simple | A UI pointed at a reader has no live page |
| **b. Serve from files, labelled "data up to <time>", with a link to the writer's live view** | UI works everywhere | Users may not notice the label |
| c. Proxy to the writer | Live everywhere | A reader-to-writer link, against the M5 rule |

**Recommendation**: (b). **Status**: Open; it's a UI choice for the owner.

### KD-10: Reader status and check-ins

| Option | For | Against |
|---|---|---|
| **a. Each reader writes a check-in row (id, address, catalog version, newest data time, at) every 10 s; `/healthz` on each reader shows its own; the writer's UI panel lists check-ins from the last minute** | The writer sees readers without contacting them | Readers write to the catalog (small, separate table) |
| b. Status only on each reader's `/healthz` | No writes from readers | No single place to see all readers (P12) |

**Recommendation**: (a).

### KD-11: Reader cache on object storage

The cache is 32 MB by default (D-028). On S3 every miss is a request of tens of ms.

| Option | For | Against |
|---|---|---|
| a. Same default everywhere | Simple | Slow reads on S3 |
| b. Larger default when storage is S3 (e.g. 512 MB) | Better S3 latency | Readers use more memory |
| c. Memory + local SSD tier (SmithDB, design-notes §2) | Best latency | New code; backlog |

**Recommendation**: (b), measured in U6; (c) stays backlog. **Status**: Open until measured.

### KD-12: Constitution II record

SmithDB queries the ingestion node for fresh data; M5 readers read files only. **Recommendation**: record as
Principle II (c), deliberately staged (simpler first), revisit with backlog B2. Listed in [plan.md](plan.md#constitution-check).

### KD-13: What a snapshot carries

A query pins one snapshot: `catalog_version`, `published_through_seq` and the active file set. `flushed_seq` already
means "every event with `seq ≤ N` is in published files" (`ingest/src/memtable.rs:26`), and it is contiguous because
one flusher publishes frozen memtables in `seq` order (`ingest/src/flush.rs`).

| Option | For | Against |
|---|---|---|
| **a. Keep the `flushed_seq` column; expose it as `published_through_seq`; read it with `catalog_version` in one lookup** | No migration of meaning; already tested by the crash harness | Two names for one number (document it) |
| b. Rename the column | One name | A migration for a name |
| c. A snapshots table, one row per version | History for debugging | Grows; nothing needs history yet |

**Criteria**: exact, contiguous, one lookup. **Recommendation**: (a). Compaction swaps and feedback writes bump
`catalog_version` but leave `published_through_seq` alone.

### KD-14: The `min_seq` read contract

Neon's pageserver waits for the requested LSN (research §7); `min_seq` is the same idea at flush granularity.

| Part | Options | Recommendation |
|---|---|---|
| Where `seq` comes from | a. confirmation body field; b. response header | (a), as an **opaque write token** (today the batch's highest `seq`; all-duplicate batch → current confirmed `seq`), so per-partition log offsets later (research §9, L7) don't break the API |
| How to ask | a. `?min_seq=&wait_ms=` on read APIs; b. a header | (a), `wait_ms` capped at 30 s |
| When behind at the deadline | a. 503 + `Retry-After`, body `still_publishing` with reached `seq`; b. 200 with a stale flag; c. 425 Too Early | (a): no rows, so nothing stale is shown |
| Waiting | a. re-check the catalog every ~250 ms; b. LISTEN/NOTIFY (OQ-3) | (a) first |
| On the writer | always satisfied at once | — |

**Status**: Open: the owner decides whether FR-16 ships in M5 (U8) or is deferred.

### KD-15: Group commit interval

Today a group is whatever queued during the previous fsync, up to 1 MB (`ingest/src/writer.rs:27`); there is no fixed
wait. A fixed 1 s interval would add up to 1 s to every confirmation for no gain on one disk.

**Recommendation**: keep today's group commit; record "acknowledgment P99 ≤ 1 s" as the ceiling in SC-02.
Revisit only with B1, where each group becomes a shared-storage write and batching by time can pay off.

### KD-16: Shared WAL tail

Deferred to backlog B2. Design sketch and open questions in research §7: snapshot through N + committed tail
(N, U], U fixed at query start, merge span updates before filtering. **Trigger**: SC-02's measured publication lag
is slower than readers need.

### KD-17: Replicated WAL

Deferred to backlog B1. Decided by one question: must an acknowledged write survive loss of the writer's disk
before publication? Not by reader freshness (research §7).

## Risks

| Risk | Where | Mitigation |
|---|---|---|
| Readers count a multi-version run twice | KD-4 | Files are the truth; property test against the writer |
| A publisher forgets to bump the catalog version | KD-2 | One helper used by every publish; a test that each publish path bumps it |
| Reader refresh can't keep up at 2M runs | KD-3 | Incremental refresh; measure refresh time per flush in the bench |
| M4 compaction relies on writer memory | M4-REQ-1 | Review M4's spec against M4-REQ-1–4 before it's planned |
| SQLite opened by several processes | KD-8 | Readers open read-only; refuse network filesystems |
| A reader reads a file that was uploaded but never published | KD-13 | Readers find files only through the snapshot (FR-14); spec Case A test |
| `min_seq` waits forever for a `seq` lost with the writer's disk | KD-14 | Every wait has a deadline; B1 removes the loss |

## Discussion log

- 2026-11-08: Register created. KD-4 raised as a correctness risk found in `query/src/fields.rs:92`.
- 2026-11-09: Owner's baseline added (one writer, readers pin a snapshot and read S3, three freshness points,
  optional `min_seq`, Neon boundary). KD-13 to KD-17 added; KD-16 and KD-17 deferred to B2 and B1.
- 2026-11-09: Owner prefers an external log as the source of truth as the B11 research direction (research §9).
  KD-14 now returns an opaque write token.
