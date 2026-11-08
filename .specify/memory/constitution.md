# smithling Constitution

## Core Principles

### I. Rust, One Binary

- smithling MUST be written in Rust and MUST ship as a single executable.
- Running it MUST NOT require installing anything else: no container runtime, database server, JVM, or
  separate language runtime. The browser UI MUST be embedded in the binary.
- Optional external services (cloud storage, a hosted metadata database) are allowed only as
  configuration choices under Principle V, never as requirements for local use.

**Rationale**: "install one thing, run it" is what makes a local tool get tried at all.

### II. SmithDB Is the Reference

- Every major component MUST map to its counterpart in LangChain's SmithDB (ingestion, memtable and
  write-ahead log, immutable columnar segments, metastore, compaction, query engine, inverted index,
  cache, progressive querying, late materialization, delete/upgrade vectors).
- SmithDB is the target architecture, reached in stages. It does not all have to be copied at once.
- Any difference from SmithDB MUST be recorded with a written reason that is one of: (a) required by
  local-first operation (Principle I or III); (b) the SmithDB choice is not yet mature enough in the
  Rust ecosystem; or (c) deliberately staged: a simpler version ships first by explicit decision, and
  the SmithDB version is listed in the backlog with a trigger for revisiting it.
- Every difference MUST sit behind an interface, so the SmithDB-equivalent can replace it without
  changes to callers.

**Rationale**: the project exists to rebuild a proven architecture. Unrecorded drift loses both the
learning and the path to cloud scale.

### III. Local and Private

- In local mode smithling MUST make zero outbound network connections: no telemetry, update checks, or
  remote assets.
- All persisted state MUST live under one data directory chosen at startup.
- The server MUST bind to localhost by default.

**Rationale**: users trace prompts and customer data they will not send to a third party.

### IV. UI First

- Every user-facing capability MUST ship with its browser view in the same release.
- The command line and API are equivalents for scripting and agents. No capability may be available
  only through them.
- The first release of any feature MUST be demonstrable in the browser.

**Rationale**: the product is seeing and understanding traces. A store without a viewer has nothing
to show.

### V. Cloud-Portable Storage

- Data and metadata access MUST go through storage abstractions that work unchanged against local disk
  and cloud object storage, and against the local and hosted metadata databases.
- Switching between local and cloud MUST be a configuration change only, and the same test suite MUST
  pass on both.
- Storage layouts MUST assume object-storage semantics: immutable files, range reads, no in-place
  rewrites.

**Rationale**: the local layout is the cloud layout, so scaling up never means a rewrite.

### VI. Done Is Measured

- A feature is done only when its acceptance checks pass in an automated demo against the scenario
  data, with the expected counts asserted.
- Response-time and throughput targets in the spec MUST be measured by a benchmark command and
  reported as P50/P99.
- A crash test (abrupt kill during ingest, then restart) MUST show zero loss of acknowledged events
  and MUST run in CI.

**Rationale**: "works on my machine" doesn't prove a database is fast or durable. Numbers do.

## Architecture Constraints

- Query execution builds on Apache DataFusion and Arrow, as SmithDB does.
- Ingestion, query and compaction are separate roles behind internal interfaces. Locally they may run
  in one process; each MUST be splittable into its own service without changing its contract.
- Specific component picks (segment file format, full-text index implementation, cache tiers) and
  their recorded differences from SmithDB live in each feature's plan or design notes, not here.

## Development Workflow & Quality Gates

- Every `/speckit-plan` MUST pass a Constitution Check against Principles I–VI before research starts,
  and again after design. Violations go in the plan's Complexity Tracking table with a justification.
- Every pull request MUST state which principles it touches, and MUST include the recorded reason for
  any new difference from SmithDB.
- The demo, benchmark and crash test (Principle VI) MUST pass before a milestone is declared done.

## Governance

- This constitution overrides other project practices. Where a spec, plan or design note conflicts
  with it, this document wins until amended.
- Amendments require: a written proposal stating the change and its reason, an update to this file
  with a new Sync Impact Report, and a review of affected specs and plans.
- Versioning follows semantic versioning. MAJOR: a principle is removed or redefined. MINOR: a
  principle or section is added or materially expanded. PATCH: wording and clarifications.
- Compliance is reviewed at each plan (Constitution Check) and each pull request.

**Version**: 1.1.0 | **Ratified**: 2026-11-05 | **Last Amended**: 2026-11-05
