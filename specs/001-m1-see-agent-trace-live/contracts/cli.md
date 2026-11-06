# CLI contract (M1 subset)

| Command | Purpose |
|---|---|
| `tracedb serve [--data ./.tracedb] [--port 7070] [--bind 127.0.0.1] [--max-memory 2GB]` | run tracedb |
| `tracedb serve … [--max-runs 500000]` | run-count part of the memory budget |
| `tracedb seed [--traces 100] [--seed 42] [--url http://localhost:7070] [--no-delay]` | generate realistic traces via `/runs/batch` (Story 4 shapes); late updates follow 30–120 s later unless `--no-delay` |

Exit codes: 0 = ok, 1 = usage or failure, 2 = server not reachable.
