# CLI contract (v0, FR-023)

Every command takes `--data <dir>` (default `./.tracedb`) and prints human output, or JSON with `--json`.

| Command | Purpose | Milestone |
|---|---|---|
| `tracedb serve [--port 7070] [--bind 127.0.0.1]` | run all three services + UI | M0 |
| `tracedb tail [--filter …]` | live stream of new runs | M0 |
| `tracedb query '<filter>' [--trace-filter …] [--tree-filter …] [--search …] [--limit n]` | filtered runs | M1 |
| `tracedb seed --scenario overnight-eval \| --traces N` | generate demo or bulk data through the ingest API | M1 |
| `tracedb stats --group-by name [--split-by tag] '<filter>'` | aggregates | M2 |
| `tracedb bench` | P50/P99 table: tree, run, filter, search, thread, ingest | M2 |
| `tracedb demo` | seed the scenario, run all checks, report pass/fail + timings; exit code ≠ 0 on any failure | M2 (grows each milestone) |
| `tracedb export '<filter>' -o file.jsonl` | export | M3 |
| `tracedb import claude-code <dir>` | import sessions (idempotent) | M4 |
| `tracedb mcp` | stdio MCP server for agent tools | M4 |

Exit codes: 0 = ok, 1 = check or usage failure, 2 = server not reachable.
