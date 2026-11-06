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
