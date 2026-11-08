# Research: M1 — See agent trace live

Master decisions that apply unchanged: R1 merge rule, R2 LangSmith compatibility, R3 UI stack, R7 durability, R9
cursor, R11 dedup ([master research](../000-smithling-master-plan/research.md)). The decisions below are specific to M1.

## M1-R1. One writer task
- **Decision**: all accepted events go through one tokio task via a bounded channel. The task does dedup → seq → WAL →
  fsync (group commit) → apply → publish → confirm.
- **Rationale**: ordering and consistency without fine-grained locks. Group commit amortizes fsync, so one writer
  clears 5k events/s easily.
- **Alternatives**: a lock per run (more complex, and fsync ordering is harder); sharded writers (not needed at this scale).

## M1-R2. In-memory store, merged on write
- **Decision**: one merged `Run` per id with three indexes (design §5).
- **Rationale**: nothing on disk to merge across yet. It's the simplest correct design, and M2's flush writes these merged rows.
- **Alternatives**: keeping raw events and merging on read (SmithDB-like, but only pays off once segments exist).

## M1-R3. WAL record format
- **Decision**: `[len][crc32][seq][JSON]`, files rolled at 64 MB; corrupt tail records are truncated on replay.
- **Rationale**: debuggable, and the checksum catches torn writes. *Open for review (design §11.1).*

## M1-R4. Live updates via SSE
- **Decision**: Server-Sent Events, one stream per tab, coalesced every 100 ms; slow consumers are dropped.
- **Rationale**: one direction is all M1 needs; it's simpler than WebSockets and reconnects natively.

## M1-R5. SDK contract fixtures
- **Decision**: record real SDK traffic once per pinned version (Python `langsmith` 0.14.17 and 0.8.18, with a LangGraph example) through a recording
  endpoint, commit the requests and expected runs, and replay them in `cargo test`. Re-record per pinned version.
- **Rationale**: no Python needed in normal CI; drift is caught when the pin is bumped.

## M1-R6. No-outbound check
- **Decision**: in Linux CI, run the e2e demo under `strace -f -e trace=connect`, and fail on any non-loopback address.
- **Rationale**: it tests the real binary, not a mock. On macOS it's a manual check with `nettop`.

## M1-R7. Capacity limit
- **Decision**: default 2 GB of approximate run bytes or 500k runs, set with `--max-memory`. Past the limit, new runs
  are rejected with `capacity` and updates to existing runs are still accepted.
- **Rationale**: refusing up front is better than confirming and then failing. The limit goes away in M2.
