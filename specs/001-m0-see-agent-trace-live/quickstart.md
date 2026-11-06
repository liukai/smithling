# Quickstart: prove M0 works

Contracts: [http-api](contracts/http-api.md) · [cli](contracts/cli.md). Design: [design.md](design.md).

## 1. Build and start
```bash
cargo build --release
./target/release/tracedb serve --data /tmp/tracedb-m0
```
Expected: `listening on http://localhost:7070`. The browser shows an empty runs list with setup instructions.

## 2. Unmodified agent (Story 1)
```bash
pip install -r examples/requirements.txt   # langsmith 0.14.17 + langgraph; the example needs no API keys
LANGSMITH_TRACING=true LANGSMITH_ENDPOINT=http://localhost:7070 LANGSMITH_API_KEY=local \
  python examples/research_agent.py
```
Expected: runs appear in the list within 1 s. The root shows `running` and flips when the agent finishes. Opening the
trace shows the tree and waterfall (Story 2).

## 3. Without an agent (Story 4)
```bash
./target/release/tracedb seed --traces 100
```
Expected: 100 traces appear, including deep trees, errors, late finishes and large payloads.

## 4. Durability (Story 3)
```bash
cargo test --test crash -- --ignored      # 10 kill -9 rounds
```
Expected: `0 confirmed events missing` in every round.

## 5. Gates
```bash
cargo test                                 # unit, property, contract, e2e
scripts/no-outbound.sh                     # Linux: strace check; fails on any non-loopback connect
```
Expected: all green.

## Last verified (2026-11-05, macOS laptop, release build)

| Step | Result |
|---|---|
| 1. Build and start | `tracedb listening on http://127.0.0.1:…`; `/` serves the UI |
| 2. Unmodified agent | `examples/research_agent.py` (langsmith 0.14.17, LangGraph): root `running` mid-run, then `ok` (1.57 s); trace of 9 steps in tree order, both `fetch_url` steps `error` with the 429 |
| 3. Seed | `tracedb seed --traces 100 --no-delay`: 16,621 steps, 1,662 late updates, all accepted in 2.9 s |
| 4. Durability | crash harness 10/10 rounds; up to 11,720 starts + 5,845 ends confirmed per round, all present after `kill -9` |
| 5. Gates | 59 Rust tests + 12 UI tests pass; restart at 500k runs ready in 0.83 s; 74,247 events/s sustained for 60 s |
| Not run here | `scripts/no-outbound.sh` (needs Linux strace) and the clean-container check (needs Docker); both run in CI. UI not checked by eye: headless Chrome can't run in this sandbox, so screens are covered by component tests |
