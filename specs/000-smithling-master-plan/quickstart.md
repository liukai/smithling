# Quickstart: validate smithling v0

How to prove each milestone works. Contracts: [http-api](contracts/http-api.md), [filter DSL](contracts/filter-dsl.md),
[CLI](contracts/cli.md).

## Prerequisites
- Rust 1.99+, and Node 24+ (needed only to build the embedded UI)
- For step 3: Python 3.12 with `langgraph` and `langsmith` installed
- For the cloud check: Docker with MinIO and Postgres (only in CI)

## 1. Build and start
```bash
cargo build --release
./target/release/smithling serve --data /tmp/smithling-demo
```
Expected: it prints `smithling listening on http://127.0.0.1:7070   data: /tmp/smithling-demo`, and the browser shows
the empty runs list with "point an agent here" instructions.

## 2. M1: see a trace in the browser
```bash
./target/release/smithling seed --traces 1     # one research-agent trace, sent to localhost:7070
```
Expected: the trace appears in the runs list within 1 s. Opening it shows the tree and waterfall, and clicking a run
shows its inputs and outputs.

## 3. Unmodified LangGraph agent (S1)
```bash
LANGSMITH_TRACING=true LANGSMITH_ENDPOINT=http://localhost:7070 LANGSMITH_API_KEY=local \
  python examples/research_agent.py
```
Expected: the trace streams in live. The root shows `running` until the agent finishes.

## 4. The scenario demo (S1–S5; the main done-check)
```bash
./target/release/smithling demo --data /tmp/smithling-scenario
```
Expected: every check reports PASS, and the command exits with 0.

| Check | Expected |
|---|---|
| failing roots, batch a | 14 |
| quiet failures (any tool errored) | 23 |
| search `"429 Too Many Requests"` | 31, all `fetch_url` |
| `fetch_url` error rate / p95 | 14% / 31 s |
| batch a cost | $4.12 |
| thread size (M4) | 6 |
| exported bad roots (M4) | 37 |

Then open the browser and repeat the checks by clicking (SC-008).

## 5. Performance (QR-004/005)
```bash
./target/release/smithling bench --smoke --check                 # 200k runs: the PR gate
./target/release/smithling bench --runs 2000000 --check          # full 2M runs; deferred (roadmap D3-1)
```
`bench` loads its own data (`--data <dir>` keeps it for reuse).
Expected: a P50/P99 table within the targets: tree and single run ≤ 50 ms, filter ≤ 100 ms, search ≤ 500 ms,
thread ≤ 150 ms (from M4), P99 ≤ 5× P50, and ingest ≥ 20k events/s. Idle memory is reported, not checked, until M4 (D-028).

## 6. Durability (QR-003)
```bash
cargo test -p smithling --test crash -- --ignored    # 10 kill -9 rounds during ingest
```
Expected: 0 acknowledged events missing in every round.

## 7. Privacy (QR-001)
Run step 4 under a network monitor (`nettop` / `ss`), or in CI with outbound traffic blocked.
Expected: no connections except to localhost.
