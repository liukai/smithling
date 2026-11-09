# Implementation Plan: M5 — Disaggregated trace database, step 1: read workers over shared storage

**Branch**: `005-m5-disaggregated-trace-db` | **Date**: 2026-11-08, revised 2026-11-09 | **Status**: Proposed; units start only when the
decisions they name are Decided | **Spec**: [spec.md](spec.md) | **Design and decisions**: [design.md](design.md) |
**Research**: [research.md](research.md) | **Code check**: [review.md](review.md)

**Input**: Feature specification from `specs/005-m5-disaggregated-trace-db/spec.md`. Master:
[000 plan](../000-smithling-master-plan/plan.md). Builds on: M4 (compaction in the writer process; the M4 spec maps
M4-REQ-1–4 to M4-FR-03, -05, -07, -10; units 1–5 merged).

**Delta key**: 🟢 New · 🟠 Improved · ⚪ Existing (as in [spec.md](spec.md)).

## Summary

M5 adds **read workers**: `smithling serve --role reader` processes that serve every read API from the published
files and the catalog, never contacting the writer. The writer is unchanged except for four small additions: a
stored catalog version, a writer lease, a grace period before deleting replaced files, and reader check-ins on its
status panel.

- 🟢 **Readers** open storage and the catalog read-only, build their state from the newest catalog version, and check
  for a newer one at the start of each query (design [KD-1](design.md#kd-1-how-does-a-reader-exist-without-a-writer),
  [KD-2](design.md#kd-2-what-is-a-catalog-version)).
- 🟢 **State refresh** follows each new version incrementally, as the writer does at each flush; compaction's removals
  trigger a background rebuild while the previous version keeps serving ([KD-3](design.md#kd-3-how-do-readers-refresh-their-state),
  [KD-4](design.md#kd-4-how-do-readers-know-which-runs-have-several-versions), [KD-5](design.md#kd-5-how-does-feedback-reach-readers)).
- 🟠 **Deletion safety**: replaced files stay for a grace period; reader queries have a time limit ([KD-6](design.md#kd-6-grace-period-and-reader-query-time-limit)).
- 🟢 **One writer**: a lease row checked in every publish ([KD-7](design.md#kd-7-writer-lease)).
- 🟢 **Status**: each reader reports how current it is; the writer's panel lists readers ([KD-10](design.md#kd-10-reader-status-and-check-ins)).
- 🟠 **Snapshots and freshness**: each snapshot carries `catalog_version` and `published_through_seq` (today's
  `flushed_seq`); every reader answer names it; acknowledgment, publication and observation lags are measured
  separately before a freshness target is chosen ([KD-13](design.md#kd-13-what-a-snapshot-carries), spec
  [Freshness](spec.md#freshness-three-points-measured-separately)).
- 🟢 *(Optional)* **`min_seq` reads**: confirmations return `seq`; readers wait up to a deadline or answer "still
  publishing" ([KD-14](design.md#kd-14-the-min_seq-read-contract)).

The write path is unchanged: confirm after the WAL fsync (group commit as today, [KD-15](design.md#kd-15-group-commit-interval)),
upload files, then one catalog transaction. A shared WAL tail and a replicated WAL are separate later decisions
(backlog B2, B1; [research §7](research.md#7-the-neon-boundary-and-a-shared-wal-tail)).

### What exists and what M5 adds

| | Piece | Where |
|---|---|---|
| ⚪ | WAL + group commit, confirm after fsync | `ingest/src/wal.rs`, `writer.rs` |
| ⚪ | Upload files, then one catalog transaction moving `flushed_seq`; idempotent retry; orphan removal at start | `ingest/src/flush.rs`, `recover.rs` |
| ⚪ | Live view from writer memory | `ingest/src/hub.rs`, `server/src/routes/tail.rs` |
| ⚪ | Guarded compaction swap, 24 h grace, in-process holds | `ingest/src/compact.rs` |
| ⚪ | S3 + Postgres in CI | `.github/workflows/ci.yml`, `ci/cloud.yml` |
| 🟢 | `catalog_version`, snapshot lookup, writer lease | U1 |
| 🟢 | Readers: pin, read S3 directly, never list storage | U2 |
| 🟢 | Reader state from files alone (multi-version runs) | U3 |
| 🟢 | Cross-process pin: reader query time limit < grace | U4 |
| 🟢 | Three lags measured; boundary cases A–C in CI | U6 |
| 🟢 | `seq` in confirmations, `min_seq` reads (optional) | U8 |

Everything else from the full disaggregation design is in the spec's backlog (B1–B10).

## How decisions gate the work

The owner asked for open discussion before a full architecture. So:

- Every design choice is a **key decision** in [design.md](design.md#key-decision-register), with options, criteria,
  a recommendation and a status (Open → Proposed → Decided, or Deferred).
- Each [delivery unit](#delivery-order) lists the decisions it needs. A unit starts only when all of them are Decided.
- Changing a Decided entry later follows the master rule: a [decisions.md](../000-smithling-master-plan/decisions.md)
  entry and the docs fixed in the same commit.

## Technical Context

**Language/Version**: Rust (workspace edition and toolchain as M3/M4); UI TypeScript/React as M3.

**Primary Dependencies**: as M4. DataFusion, Arrow, Parquet, `object_store` (local and S3), `sqlx` (SQLite and
Postgres). No new dependencies expected.

**Storage**: files on a shared folder or S3-compatible storage; catalog in Postgres for readers on other machines,
SQLite only when every process shares one machine's folder ([KD-8](design.md#kd-8-catalog-backend-in-split-mode)).

**Testing**: `cargo test` (unit, property and multi-process integration tests via `smithling_server::testkit`);
`smithling demo` against each reader; crash harness for the writer; the cloud CI job (MinIO + Postgres) for the
split demo.

**Target Platform**: macOS and Linux, as M3.

**Project Type**: one binary with roles (server + CLI + embedded UI).

**Performance Goals** (spec SC-02, SC-03, SC-07; measured, may be deferred under D-033): the three freshness lags
reported as P50/P99, with acknowledgment P99 ≤ 1 s and the reader target chosen afterwards (spec OQ-5); writer confirmation P99 within 10% of the no-read baseline when readers carry the reads;
2 readers ≥ 1.7× one reader's read throughput; a new reader serves within 60 s at 2M runs.

**Constraints**: one binary, no required external service in one-process mode (constitution I); zero outbound in
local mode (III); same tests on local and cloud storage (V); readers never contact the writer (spec Decisions).

**Scale/Scope**: single tenant, one writer, 1–4 readers in tests; reference data 2M runs.

**Open items**: none blocks the plan's shape. The ones that block units are the design register's Open entries:
[KD-4](design.md#kd-4-how-do-readers-know-which-runs-have-several-versions) (blocks U3),
[KD-9](design.md#kd-9-the-live-view-on-a-reader) (blocks U2's live endpoint), [KD-11](design.md#kd-11-reader-cache-on-object-storage) (measured in U6),
[KD-14](design.md#kd-14-the-min_seq-read-contract) (blocks U8, or defers it).

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.* Constitution v1.1.0.

| Principle | M5 check | Pre | Post |
|---|---|---|---|
| I. Rust, one binary | Readers are the same binary with `--role reader`; no new runtime or required service. Split mode across machines uses Postgres, an optional service allowed under Principle I | PASS | PASS |
| II. SmithDB is the reference | Read serving as its own service matches SmithDB's split. Difference: readers read files only, no fresh data from the ingestion node; recorded below as (c) | PASS (with record) | PASS (with record) |
| III. Local and private | One-process mode unchanged, zero outbound. Readers connect only to the configured storage and catalog; default binding stays localhost | PASS | PASS |
| IV. UI first | Reader status panel and the writer's reader list ship with the API and CLI (`smithling status`); the live view on a reader is labelled (KD-9) | PASS | PASS (KD-9 Open) |
| V. Cloud-portable storage | No new storage paths; catalog changes (version counter, lease, check-ins) are migrations on SQLite and Postgres; the split demo runs on MinIO + Postgres in CI | PASS | PASS |
| VI. Done is measured | Split demo asserts the same counts as one-process mode; problem demos P2, P7, P9, P10; crash harness keeps running; SC-02/03/07 reported as P50/P99 | PASS | PASS |

Architecture constraint "each role MUST be splittable into its own service without changing its contract": M5
splits the query role out for reads. Ingest and compaction stay together on the writer (backlog B3).

**Differences from SmithDB recorded in M5** (constitution II; reasons (a) local-first, (b) Rust ecosystem, (c)
deliberately staged):

| | Difference | Reason | Revisit when |
|---|---|---|---|
| 🟢 | Readers read published files only; SmithDB also reads fresh data from the ingestion node | (c) | Backlog B2 trigger: readers need fresher than the flush age |
| ⚪ | Ingest and compaction share the writer machine; SmithDB runs compaction separately | (c) | Backlog B3 trigger |
| 🟠 | The confirmed-write guarantee stays at the writer's disk (SmithDB's post doesn't state its guarantee; research §1 options) | (c) | Backlog B1, the first follow-up |

## Project Structure

### Documentation (this feature)

```text
specs/005-m5-disaggregated-trace-db/
├── spec.md          # what breaks, what must be true, open questions, backlog
├── review.md        # the design note checked against the code
├── research.md      # options other systems use, with sources; Phase 0 summary
├── design.md        # architecture options, components and jobs, key decision register
├── plan.md          # this file
├── data-model.md    # catalog additions
├── quickstart.md    # how to see M5 working
├── contracts/
│   ├── cli.md       # serve --role reader and new options; smithling status
│   └── http-api.md  # reader refusals, /healthz fields, status endpoint
└── checklists/requirements.md
```

### Source Code (repository root)

Expected touch points; exact files settle with each unit.

```text
crates/
├── storage/src/metastore.rs   # 🟠 catalog_version, writer_lease, reader_checkins; read-only open
├── ingest/src/lib.rs          # 🟠 Ingest::open_reader (KD-1a); lease take/renew/check
├── ingest/src/recover.rs      # 🟠 reuse start-up loading for readers (run-id filter, needs_merge)
├── ingest/src/flush.rs        # 🟠 bump catalog_version, check lease in commit
├── ingest/src/feedback.rs     # 🟠 bump catalog_version; reload changed rows on readers
├── query/src/lib.rs           # 🟠 check catalog version per query; time limit
├── query/src/fields.rs        # 🟠 follow new versions; background rebuild after removals
├── server/src/routes/         # 🟠 refuse writes on readers; status endpoint; /healthz fields
├── tools/src/demo.rs          # 🟠 --url per reader; problem demos
└── smithling/src/main.rs      # 🟠 --role reader and options
ui/src/                        # 🟢 status panel; live view label on readers
crates/server/tests/           # 🟢 multi-process tests (1 writer + 2 readers), via testkit
```

**Structure Decision**: no new crates (KD-1a). If KD-1 moves to (b), a read-state type is extracted inside
`ingest` first.

## Delivery order

PR-sized, each ends in something checkable. "Needs" lists the decisions that must be Decided first.

| Unit | What | Needs | Done when |
|---|---|---|---|
| U0 | **M4 check**: M4's spec meets M4-REQ-1–4 | — | M4 spec reviewed against the four requirements; gaps fixed there |
| U1 | 🟢 Catalog version counter + writer lease; snapshot lookup returns `catalog_version`, `published_through_seq` and files together | KD-2, KD-7, KD-13 | Every publish path bumps the version (test per path); `published_through_seq` moves only on flush; a second writer refuses to start; a writer that lost its lease can't publish |
| U2 | 🟢 `serve --role reader`: read-only open, refuse writes, pin one snapshot per query, files only through the catalog | KD-1, KD-8, KD-9 | Reader passes `smithling demo --url <reader>` with the writer's counts; **Case A** (upload without publish; lost reply; lost lease) as a multi-process test |
| U3 | 🟢 Reader state refresh: per-query version check, incremental refresh, multi-version rule, feedback | KD-2, KD-3, KD-4, KD-5 | Property test: reader answers equal the writer's at every version under random late updates and flush timing; **Case C** (late update changes a match); P9 demo passes |
| U4 | 🟠 Reader query time limit under M4's grace period | KD-6 | **Case B** and the P7 demo: reader queries across 100 swaps return the same answers, zero missing-file errors |
| U5 | 🟢 Status: `/healthz` fields, check-ins, UI panel, `smithling status` | KD-10 | P12 demo: a blocked reader shows as behind within one check |
| U6 | 🟠 Split demo in CI (local folder + SQLite, MinIO + Postgres), problem demos, Cases A–C, bench rows for the three lags; reader cache measured | KD-11 | CI green on both; SC-02/03/07 and SC-11 reported; OQ-5 ready to decide |
| U7 | 🟠 Docs: starter guide for M5, roadmap STATE | — | Guide follows one query through a reader with line links |
| U8 | 🟢 *(Optional)* `seq` in confirmations; `min_seq` + deadline on read APIs; "still publishing" | KD-14 | US6 scenarios; SC-10: zero stale answers across a load with random deadlines |

U1 and U2 can start once their decisions are Decided; U3 waits on KD-4. U8 can follow U2 at any point, or be deferred
without changing the other units.

## Complexity Tracking

No constitution violations. Two choices worth naming:

| Choice | Why | Simpler option rejected because |
|---|---|---|
| A read-only `Ingest` instead of a separate read-state type (KD-1a) | Smallest change now | (b) is cleaner but touches every query module; kept as a follow-up |
| Readers write check-in rows to the catalog (KD-10a) | P12 needs one place that lists readers, without readers contacting the writer | Status only on each reader's `/healthz` leaves no single view |
