# Data Model: tracedb v0

Logical model. Physical layout: design-notes §3.2–3.3. Merge rule: research R1.

## RunEvent (what arrives)

| Field | Type | Notes |
|---|---|---|
| run_id | uuid | required |
| trace_id | uuid | required; equals run_id for the root |
| parent_run_id | uuid? | empty for the root |
| dotted_order | string | required; `<start_ts><run_id>` per ancestor, joined by `.` |
| project | string | from `session_name`; default `default` |
| op | `post` \| `patch` | `post` = start, `patch` = update/end |
| event_seq | u64 | assigned at ingest, monotonic; orders the merge |
| received_at | timestamp | assigned at ingest |
| fields | partial Run | any subset of the Run fields below |

Validation: run_id, trace_id and dotted_order are present and well formed; dotted_order ends with this run_id; the
parent's prefix matches when parent_run_id is set; payload ≤ 20 MB per field (bigger fields are rejected with a reason).

## Run (what queries see = merge of its events)

| Group | Fields |
|---|---|
| Identity | run_id, trace_id, parent_run_id, dotted_order, depth (derived), is_root (derived) |
| Grouping | project, tags[], metadata (JSON), thread_id (from metadata `thread_id` / `session_id` / `conversation_id`) |
| Kind | name, run_type ∈ {llm, tool, chain, retriever, agent, prompt, parser, embedding, other} |
| Timing / outcome | start_time, end_time?, error?, status (derived), latency_ms (derived) |
| Cost | model?, prompt_tokens?, completion_tokens?, total_tokens (derived), cost_usd (derived) |
| Payload (lazy) | inputs (JSON), outputs (JSON), events (JSON array; latest full list wins) |
| Feedback | feedback[] (see below), denormalized as feedback scores per key for filtering |
| Storage pointers | payload_ref (segment + row), first_seq, last_seq |

**State transitions** (status is derived, never sent):

```
(no end_time, no error) ── running
running ── end_time set, no error ──> ok
running ── error set ──────────────> error
ok/error ── a later patch changes error ─> re-derived (last write wins)
```

## Trace
A view, not stored separately: all runs with the same trace_id, ordered by dotted_order. Root = the run where
run_id = trace_id. Trace-level fields (status, latency, tokens, cost) are the root's, and totals are summed over its runs.

## Thread
A view: root runs that share a thread_id, ordered by start_time.

## Feedback
| Field | Type |
|---|---|
| id | uuid |
| run_id | uuid |
| key | string, e.g. `correctness` |
| score | number? |
| comment | string? |
| created_at | timestamp |

It arrives with SDK traffic (`feedback.{id}` multipart parts) or through FR-020. It's stored as its own event stream
and joined to runs at query time.

## Project
name (unique), created_at. Retention is in the backlog (B1).

## Segment (metastore record)
id (ulid), project, kind {core, payload, bloom}, path, min/max start_time, min/max trace_id, row_count, tier {T0, T1, T2},
state {active, tombstoned}, created_at. A swap is atomic: new segments become `active` and old ones become `tombstoned`
in one transaction.
