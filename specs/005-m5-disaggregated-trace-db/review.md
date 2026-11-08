# Review: the 2026-11-08 disaggregation note against the code

Input to the M5 [spec](spec.md) and plan. Each claim from the note is checked against the code on `feat/demo-scenarios`
(`master` + demo commits) on 2026-11-08. Paths are relative to `crates/`.

## Claims about today's code

| Note says | Code says | Verdict |
|---|---|---|
| Sort by trace ID and "apparently" dotted order | Rows sorted by `(trace_id, dotted_order)` before writing a segment (`storage/src/segment.rs:196`). One version per run per flush, so no tie-breaker is needed | ✅ confirmed |
| Immutable memtables flush on size or ~10 min | 64 MB measured as heap or **2 min** (D-028); 10 min was M2 | ⚠️ outdated |
| Local write log exists; ack guarantees unverified | Writer order is dedup → identity → number → WAL → **fsync (group commit)** → apply → publish → confirm (`ingest/src/writer.rs:1`). Ack means "survives a process kill on this disk"; the kill -9 crash test (10 rounds) runs in CI | ✅ durable to process crash; ❌ not to disk or machine loss |
| Pub/sub tailing exists | `Hub` is an in-process ring of 4096 changes; a lagging subscriber gets `Resync` (`ingest/src/hub.rs`, `query/src/tail.rs`) | ✅ exists; as the note says, not usable for recovery |
| Queries reconcile across files | Merge-on-read over memtables + files, one evaluator (D-017); multi-version runs kept merged across requests (D-023, D-029) | ✅ |
| "Move immutable data to object storage" is future work | Already done: files go through `Store`, local disk or S3-compatible by URL, write-once `PutMode::Create` (`storage/src/object.rs`); catalog is SQLite or Postgres with the same SQL (`storage/src/metastore.rs:1`). Never run against real S3/Postgres (D3-3) | ⚠️ mostly built; verification is the gap |

## What already exists that the design needs

- **Contiguous coverage, for one writer.** A flush registers its files, saves dedup keys and moves `flushed_seq` in one
  catalog transaction (`ingest/src/flush.rs:5`); replay skips `seq ≤ flushed_seq`. Files carry `min_seq`/`max_seq`. One
  writer flushing in order makes this a gap-free prefix. M5 makes it per shard.
- **Snapshot pinning, in one process.** A query takes active + frozen memtables + file set in one step under a lock
  (D-015); files referenced by a pinned `Version` are not deleted (R-7, `ingest/src/memtable.rs:20`). Across processes
  this needs leases.
- **Deterministic merge.** Latest present value per field wins, by sequence (D-001). A reader applying the same log in
  order gets the same state, so FR-12 is a property to test, not new logic.

## Gaps the note doesn't mention

1. **WAL is trimmed right after each flush** (D-022). Readers replaying the log need a retention window, and the dedup
   window (10 min, in writer memory, D-008) must survive handoff. Spec FR-07, FR-08.
2. **Orphans are deleted on start** (`ingest/src/flush.rs:208`, D-026): any file not in the catalog. With a shared store,
   that deletes another writer's or compactor's in-flight upload. Needs a grace period (FR-18).
3. **State loaded once at start:** the feedback index (D-027, written straight to the catalog, not the WAL), per-file
   field stats (D-020), the flushed-run filter (D-025), the project map (D-024). Readers need these refreshed from the
   catalog (FR-11).
4. **Text-index backfill runs in ingest** (`ingest/src/backfill.rs`). In split mode it belongs with compaction.
5. **SmithDB difference (constitution II).** Design notes §2 record SmithDB as querying fresh data from the ingestion
   node; the note's preferred direction (readers consume the log themselves) is a difference, and none of Principle
   II's three allowed reasons obviously fits. Either take SmithDB's path first (allowed as "staged"), or amend the
   constitution / record a new reason. Spec Q2.
6. **Shard key and ingest state.** The project map, flushed-run filter and multi-version set assume one writer sees
   every version of a run. Sharding by project keeps that true; sharding by trace or hash of run would not, because a
   project-less late update (D-024) couldn't be routed. Spec Q3.
7. **Scope line in the master roadmap** says the cloud split is out of scope; adding M5 changes that (D-036).

## Points in the note that hold up

- Readers of one shard each need the full stream; a shared consumer group would give each only part. ✅
- Max position is not coverage when gaps are possible. ✅ (today there are no gaps, since there's one writer and positions are assigned in order; with a remote log and retries, there can be.)
- Atomic swap alone doesn't make deletion safe; leases do. ✅
- Using Postgres delegates catalog durability; it doesn't remove the requirement. ✅
- Neon analogy: separate compute, durable log and storage serving; keep columnar files and don't import page
  reconstruction. ✅ Not verified against Neon's current architecture (the note says so too).

## Suggested milestone order

The note's six steps, mapped to this repo: step 1 (code review) is this file; step 2 is M4; steps 3–5 are M5; step 6 is
M6. Inside M5, the plan should go: per-shard log and coverage in one process → readers as a second process on the same
machine → compaction out of process with leases → handoff and fencing → cloud-style CI.
