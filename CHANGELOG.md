# Changelog

Milestones, key decisions and changes someone would look up later, newest first. Each milestone is tagged (`m1`,
`m2`, …), so `git checkout m2` shows the code as it was then. The full reasoning behind a decision is in the
[decision log](specs/000-smithling-master-plan/decisions.md); plans and status are in the
[roadmap](specs/000-smithling-master-plan/roadmap.md).

**Keeping it current** (CI checks the first rule on every pull request, see `scripts/check-changelog.sh`):

- A change to code, UI or specs adds a line under **Unreleased**: what changed for a user or a reader of the code,
  in one line, with its decision ID if it has one. Label the pull request `skip-changelog` only for changes nobody
  would look up (typos, formatting).
- A changed decision also gets a decision-log entry; link it here.
- When a milestone closes: rename **Unreleased** to `## Mn: <name> (<date>, tag mn)`, tag the closing commit
  (`git tag -a mn <commit> -m "Mn: <name>"`), and start a new empty **Unreleased**.

---

## Unreleased

### Changed

- **Renamed to Smithling** (was tracedb): crates `smithling-*`, binary `smithling`, `SMITHLING_*` environment
  variables, default data folder `./.smithling`. Existing `./.tracedb` folders can be renamed or passed with `--data`
  ([D-037](specs/000-smithling-master-plan/decisions.md#d-037)). Entries below use the old name.
- The `tracedb` crate is split into `tracedb-server` (the HTTP API and UI), `tracedb-tools` (seed, query, demo,
  bench) and the `tracedb` binary; `--no-default-features` builds a serve-only binary
  ([D-034](specs/000-smithling-master-plan/decisions.md#d-034)).
- Storage parts stay concrete types until a second implementation is needed; only the text index is a trait
  ([D-035](specs/000-smithling-master-plan/decisions.md#d-035)).
- CI: the no-Node build hides the Node that GitHub's runner image ships instead of assuming it's absent.
- CI: runs on pushes to `main` or `master`, with a read-only token; the full 2M-run bench is started by hand until
  it's re-evaluated (roadmap D3-1); pull requests must update this file.
- Specs brought in line with the built code (about 100 fixes); four earlier changes that had no decision-log entry
  were logged retroactively ([D-030](specs/000-smithling-master-plan/decisions.md#d-030)–D-033).
- M3 tasks T121 and T124 reopened: written but never run outside macOS (roadmap D3-3, D3-5).

### Added

- README rewritten as a front door: what you get and a five-minute tour (serve, `smithling demo --url`, the
  Scenarios page), a learning path through the guides, and the plan. The full reference moved to [USAGE.md](USAGE.md).
- Guides for newcomers: [STARTER_GUIDE.md](STARTER_GUIDE.md) (problem, architecture, milestones),
  [SECRET_OF_COLUMNAR_DB.md](SECRET_OF_COLUMNAR_DB.md) (columnar storage, with runnable examples
  `columnar_tour` and `columnar_json`), [ROCKSDB_COMPARISON.md](ROCKSDB_COMPARISON.md) (the storage engine,
  part by part).
- UI: a Scenarios page with the SmithDB post's questions, one click each.
- UI: a new look (navy header, sky-blue accents) across the views, charts and JSON viewer.
- A draft spec for **M5: disaggregated trace database** (ingest, read workers and compaction as separate services
  over shared storage); the agents milestone becomes M6
  ([D-036](specs/000-smithling-master-plan/decisions.md#d-036)).
- Roadmap backlog: ideas not yet in the system (typed columns for hot JSON paths, cost-based condition order,
  result cache, a Vortex benchmark, bounded attribute promotion) and a scale / failure-modes checklist.
- `LICENSE` (MIT), `SECURITY.md`, this changelog.

---

## M3: Search, aggregates, speed (2026-11-07, tag `m3`)

### Added

- Text search over inputs, outputs and errors, with a small inverted index written next to every data file
  ([D-026](specs/000-smithling-master-plan/decisions.md#d-026)).
- Stats: group by any field with count, error rate, p50/p95/p99 latency, tokens, cost and feedback average; Time
  series, Distribution and Compare views. Slow grouped queries send an estimate first, then the exact answer.
- The LangSmith SDK works on its default settings: multipart and zstd uploads
  ([D-030](specs/000-smithling-master-plan/decisions.md#d-030)); attachment parts are rejected by name, the run is kept
  ([D-031](specs/000-smithling-master-plan/decisions.md#d-031)).
- Feedback is stored, filtered on and averaged, even when it arrives before its run
  ([D-027](specs/000-smithling-master-plan/decisions.md#d-027)).
- Cost from a local price list; `tracedb bench`; a 32 MB cache for file footers, index and payload pages; a damaged
  data file is skipped and named instead of failing every query.

### Changed

- Defaults: 64 MB memtable measured as heap, 2-minute flush age, 16-byte dedup fingerprints with a cap, jemalloc
  ([D-028](specs/000-smithling-master-plan/decisions.md#d-028)).
- Lists and single-run lookups reuse merged rows and read payloads by row number
  ([D-029](specs/000-smithling-master-plan/decisions.md#d-029)); a second start for an already-flushed run is
  merged correctly ([D-025](specs/000-smithling-master-plan/decisions.md#d-025)).
- End-to-end first, performance later: the full 2M-run bench and the idle-memory target are deferred, not dropped
  ([D-032](specs/000-smithling-master-plan/decisions.md#d-032), [D-033](specs/000-smithling-master-plan/decisions.md#d-033)).

`tracedb demo` passes all 10 checks. Not verified here: the cloud CI job, the UI in a real browser, the
no-outbound check under strace.

---

## M2: Durable storage + query builder (2026-11-06, tag `m2`)

### Added

- Data on disk: the memtable is frozen and flushed to Parquet files (a small-fields file and a payload file per
  project), registered in a catalog (SQLite locally, Postgres in cloud mode) in one transaction, then the WAL is
  trimmed. Crash-safe at every step.
- Merge-on-read: a run whose start and end are in different places is merged before filtering; only runs that may
  be split pay for it.
- The query builder: chips and the LangSmith filter text kept in step, conditions on the run, its root or any run in
  its trace, the field browser, the Table view, live views; `tracedb query`; `tracedb demo`.
- Cloud mode: the same binary on S3-compatible storage + Postgres.
- From RocksDB: pinned file sets for readers, write slowdown (`429`), key bloom filters, two memtables.

### Changed

- Promoted JSON values live in one `attrs` column ([D-012](specs/000-smithling-master-plan/decisions.md#d-012)); one
  evaluator decides every filter after the merge ([D-017](specs/000-smithling-master-plan/decisions.md#d-017)).
- Lists read the newest slice of time first and stop at a full page
  ([D-021](specs/000-smithling-master-plan/decisions.md#d-021)); dedup keys are saved in the catalog instead of
  keeping old WAL files ([D-022](specs/000-smithling-master-plan/decisions.md#d-022)); the dedup window is 10 minutes
  ([D-008](specs/000-smithling-master-plan/decisions.md#d-008)).

At 2M runs: filtered lists p50 29–53 ms, opening a trace 6 ms, disk 19% of the raw JSON.

---

## M1: See an agent trace live (2026-11-06, tag `m1`)

### Added

- LangSmith-compatible uploads (`/info`, `/runs/batch`, `POST/PATCH /runs`) and a native `/v1/events`.
- One writer with a write-ahead log: nothing is acknowledged before it's fsynced; requests that queue during one
  fsync share the next (group commit).
- An in-memory run store that merges a run's start and its later updates; a live tail over server-sent events.
- The browser UI: runs list, trace tree, timing waterfall, step details; `tracedb seed`; a `kill -9` crash harness.

Measured: 74,247 events/s, restart in 0.83 s at 500k runs, 10 of 10 crash rounds with nothing lost.
