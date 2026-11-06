# CLI contract (M0 subset)

| Command | Purpose |
|---|---|
| `tracedb serve [--data ./.tracedb] [--port 7070] [--bind 127.0.0.1] [--max-memory 2GB]` | run tracedb |
| `tracedb seed [--traces 100] [--seed 42] [--url http://localhost:7070]` | generate realistic traces via `/runs/batch` (master spec, Story 4 shapes) |

Exit codes: 0 = ok, 1 = usage or failure, 2 = server not reachable.
