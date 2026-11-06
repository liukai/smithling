# Data Model: M0

The entities and fields are those in the [master data model](../000-tracedb-master-plan/data-model.md). This file only
records what M0 uses and how it holds them.

| Entity | In M0 | Held as |
|---|---|---|
| RunEvent | `post` / `patch`, assigned `event_seq` | a WAL record (design §5) |
| Run | all master fields; feedback is empty in M0 | `runs` map, merged on write |
| Trace | a view over `by_trace` | index |
| Project | from `session_name` | a key of the `roots` index |
| Feedback, Thread, Segment | not in M0 | — |

**Validation (M0)**: `run_id`, `trace_id` and `dotted_order` are present; `dotted_order` ends with this run's id; a
parent's `dotted_order` prefix matches when the parent is known; each field ≤ 20 MB; the body ≤ 50 MB.

**Derived** (recomputed on every merge): `status` (running / ok / error), `latency_ms`, `depth`, `is_root`,
`total_tokens`.
