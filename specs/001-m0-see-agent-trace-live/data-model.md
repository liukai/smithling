# Data Model: M0 — See agent trace live

Everything M0 stores, how each field arrives, how it merges, and how it's held. The master model
([000 data-model](../000-tracedb-master-plan/data-model.md)) covers all phases; this file is the M0 detail of record.
Field sources were checked against `langsmith` SDK 0.8.18 source on 2026-11-05 and are pinned by contract fixtures
(research M0-R5).

## 1. The entities and how they relate

```
Project 1 ──< Trace 1 ──< Run 1 ──< RunEvent        (RunEvent = one upload message about a run)
                 │           │
                 │           └── parent_run_id ──▶ Run (same trace)
                 └── root = the Run with no parent (run_id == trace_id)
```

| Entity | What it is | Stored? | Identity |
|---|---|---|---|
| **RunEvent** | one message about one run: a start (`post`) or an update (`patch`) | yes: WAL record | `(run_id, event_seq)` |
| **Run** | one step, the merge of all its events | yes: in the run store | `run_id` |
| **Trace** | all runs of one task, as a tree | no, a view over runs | `trace_id` |
| **Project** | the bucket a trace belongs to | no, a key in the indexes | `name` |

## 2. RunEvent

| Field | Type | Set by | Notes |
|---|---|---|---|
| `op` | `post` \| `patch` | HTTP layer | `post` from `post[]` items or `POST /runs`; `patch` from `patch[]` items or `PATCH /runs/{id}` |
| `event_seq` | u64 | writer | strictly increasing across the whole store; defines merge order |
| `received_at` | timestamp (µs, UTC) | writer | server clock |
| `source` | `ls_batch` \| `ls_single` \| `native` | HTTP layer | for debugging and contract tests |
| `body_hash` | 64-bit hash | HTTP layer | hash of the canonical item JSON; used for dedup |
| `fields` | partial Run (§3) | client | only the fields present in the message; `null` counts as absent |

## 3. Run fields

**Legend.** *Req*: required on `post`. *Merge*: **R** = the latest present value replaces the old one; **I** =
immutable, so a later event with a different value is rejected (`identity_conflict`); **D** = derived, recomputed after
every merge, never sent.

### 3.1 Identity and position in the tree
| Field | Type | LangSmith source | Req | Merge | Validation and meaning |
|---|---|---|---|---|---|
| `run_id` | UUID | `id` (or path id for `PATCH`) | yes | I | valid UUID |
| `trace_id` | UUID | `trace_id` | yes | I | valid UUID; equals `run_id` for the root |
| `parent_run_id` | UUID? | `parent_run_id` | no | I | absent for the root; must equal the UUID in the second-to-last `dotted_order` segment |
| `dotted_order` | string | `dotted_order` | yes | I | see §3.7; last segment's UUID = `run_id`, first = `trace_id` |
| `depth` | u16 | — | — | D | number of `dotted_order` segments − 1 (root = 0) |
| `is_root` | bool | — | — | D | `parent_run_id` absent |

### 3.2 Grouping
| Field | Type | LangSmith source | Req | Merge | Validation and meaning |
|---|---|---|---|---|---|
| `project` | string | `session_name` | no | I | default `default`; 1–128 chars |
| `tags` | [string] | `tags` | no | R | whole list replaced (the SDK re-sends the full list); ≤ 100 tags, each ≤ 256 chars |
| `metadata` | JSON object | `extra.metadata` | no | R | whole object replaced; ≤ 1 MB |
| `extra` | JSON object | `extra` minus `metadata` | no | R | kept as is (runtime, invocation params) |
| `thread_id` | string? | — | — | D | first present of `metadata.thread_id`, `metadata.session_id`, `metadata.conversation_id`; stored for M3 |

### 3.3 What kind of step
| Field | Type | LangSmith source | Req | Merge | Validation and meaning |
|---|---|---|---|---|---|
| `name` | string | `name` | yes | R | 1–512 chars |
| `run_type` | enum | `run_type` | yes | R | `llm`, `chain`, `tool`, `retriever`, `embedding`, `prompt`, `parser`, `agent`; anything else is stored as `other`, with the original in `run_type_raw` |

### 3.4 Timing and outcome
| Field | Type | LangSmith source | Req | Merge | Validation and meaning |
|---|---|---|---|---|---|
| `start_time` | timestamp (µs, UTC) | `start_time` | yes | R | ISO-8601 with or without `Z`; naive times are taken as UTC |
| `end_time` | timestamp? | `end_time` | no | R | if before `start_time`, it's kept and `latency_ms` is left empty with warning `negative_latency` |
| `error` | string? | `error` | no | R | ≤ 1 MB; any non-empty value means the step failed |
| `status` | `running` \| `ok` \| `error` | — | — | D | §4 |
| `latency_ms` | u64? | — | — | D | `end_time − start_time` when both are present and non-negative |

### 3.5 Model and tokens
| Field | Type | LangSmith source | Req | Merge | Validation and meaning |
|---|---|---|---|---|---|
| `model` | string? | — | — | D | `metadata.ls_model_name` (set by SDK wrappers and LangChain chat models); provider in `metadata.ls_provider` |
| `input_tokens` | u64? | — | — | D | `outputs.usage_metadata.input_tokens`, else `metadata.usage_metadata.input_tokens` |
| `output_tokens` | u64? | — | — | D | same, `output_tokens` |
| `total_tokens` | u64? | — | — | D | `usage_metadata.total_tokens`, else `input + output`, else empty |
| `cost_usd` | — | — | — | — | not in M0 (needs the price table, M2); shown as `—` |

### 3.6 Payload (loaded only when a step is opened)
| Field | Type | LangSmith source | Req | Merge | Validation and meaning |
|---|---|---|---|---|---|
| `inputs` | JSON? | `inputs` | no | R | ≤ 20 MB |
| `outputs` | JSON? | `outputs` | no | R | ≤ 20 MB |
| `events` | JSON array? | `events` | no | R | ≤ 20 MB; the SDK re-sends the full list, so it's replaced, not appended (master R1) |
| `dropped` | {field → bytes} | `serialized`, attachments | — | R | M0 doesn't store these; it records their sizes so the user can see something was dropped |

### 3.7 `dotted_order` format
```
<seg>(.<seg>)*        seg = <YYYYMMDDTHHMMSSffffffZ><UUID>     e.g. 20261105T021400000000Z0b9f…11
                                                                 .20261105T021407123456Z7c1e…a4
```
- Segments are root → this run. Sorting runs of one trace by this string gives depth-first order with siblings by
  start time.
- Checks: every segment has a 22-char timestamp and a 36-char UUID; the first UUID = `trace_id`; the last = `run_id`;
  the second-to-last = `parent_run_id`.

### 3.8 Bookkeeping (held by the store, shown in debug views)
| Field | Meaning |
|---|---|
| `first_seq`, `last_seq` | `event_seq` of the first and latest event merged |
| `created_at`, `updated_at` | `received_at` of those events |
| `event_count` | events merged into this run |
| `payload_bytes` | approximate bytes of inputs + outputs + events (for the memory budget) |
| `has_start` | false while only `patch` events have arrived (§5) |

## 4. Derived status

```
            error non-empty
   any ────────────────────────▶ error
    │
    │  no error, end_time set
    ├──────────────────────────▶ ok
    │
    └─ no error, no end_time ──▶ running
```
Recomputed after every merge. Because absent and `null` fields never overwrite (§5), an error can't be cleared once
set. A later non-empty `error` replaces the message, and the status stays `error`.

## 5. Merge rules in detail

Events for a run are applied in `event_seq` order (= arrival order, since one writer assigns it).

| Case | Behaviour |
|---|---|
| `post` then `patch` (normal) | each present field in the patch replaces the old value; absent fields stay |
| `patch` before `post` | a run is created with `has_start = false`. It shows in its trace (if the trace is open) but **not** in the root list until `start_time` and `name` arrive |
| second `post` for the same run | treated like a patch (SDK retries can resend posts) |
| identity field differs (`trace_id`, `parent_run_id`, `dotted_order`, `project`) | the item is rejected with `identity_conflict`; the run is unchanged |
| exact duplicate (`run_id`, `op`, `body_hash` seen in the last 24 h) | dropped silently and counted as accepted (idempotent) |
| field sent as `null` | ignored (absent), matching the SDK, which skips `None` |

## 6. Validation and rejection reasons

| Reason code | When |
|---|---|
| `missing_field` | `post` without `id`, `trace_id`, `dotted_order`, `name`, `run_type` or `start_time`; `patch` without `id`, `trace_id` or `dotted_order` |
| `invalid_uuid` | an id isn't a UUID |
| `invalid_timestamp` | a time can't be parsed |
| `invalid_dotted_order` | the format or segment checks in §3.7 fail |
| `identity_conflict` | §5 |
| `too_large` | a field over its limit; the body over 50 MB rejects the whole request (`413`) |
| `capacity` | the memory budget is reached and this is a **new** run (updates to known runs are still accepted) |

Each rejected item is returned as `{id, reason, detail}`. All other items in the batch are accepted.

## 7. What the API returns

| Shape | Contains | Used by |
|---|---|---|
| `RunHeader` | every Run field **except** `inputs`, `outputs`, `events`, plus `payload_sizes {inputs, outputs, events}` and `child_count` | runs list, trace tree, live tail |
| `Run` | `RunHeader` + the payload fields asked for in `include` | step panel |
| `TraceView` | `{root: RunHeader, runs: [RunHeader] sorted by dotted_order, run_count, has_running}` | trace view |
| `RootsPage` | `{runs: [RunHeader], next_cursor?}` | runs list |

## 8. How M0 holds it

**WAL** (`./.tracedb/wal/`)
- **Files**: `<first_seq>.wal`, rolled at 64 MB. Each file starts with an 8-byte header: magic `TDBW` + version `u32 = 1`.
- **Record**: `[u32 length][u32 crc32 of the rest][u64 event_seq][u64 received_at_us][u8 op][u8 source][JSON fields]`.
- **Replay**: read files in order; stop at the first record with a bad length or checksum, and truncate from there
  (that record was never confirmed).

**Run store** (in memory; written only by the writer)
| Structure | Key → value | Approx. size per run |
|---|---|---|
| `runs` | `run_id → RunState {run, bookkeeping}` | header ~0.5 KB + payload bytes |
| `by_trace` | `trace_id → Vec<run_id>` | 16 B |
| `roots` | per project: `BTreeSet<(Reverse(start_time), run_id)>` | 32 B per root |
| `seen` | `(run_id, op, body_hash)` with arrival time, pruned after 24 h | 40 B per event |

Budget: the sum of `payload_bytes` + headers, compared with `--max-memory` (default 2 GB) and a 500k-run limit.

## 9. Worked example

Two uploads for the `fetch_url` step:
```json
POST /runs/batch  {"post": [{"id": "7c1e…a4", "trace_id": "0b9f…11", "parent_run_id": "0b9f…11",
  "dotted_order": "20261105T021400000000Z0b9f…11.20261105T021407123456Z7c1e…a4",
  "session_name": "research-agent", "name": "fetch_url", "run_type": "tool",
  "start_time": "2026-11-05T02:14:07.123456", "inputs": {"url": "https://…"},
  "tags": ["batch=a"], "extra": {"metadata": {"prompt_version": "v7"}}}]}

POST /runs/batch  {"patch": [{"id": "7c1e…a4", "trace_id": "0b9f…11", "parent_run_id": "0b9f…11",
  "dotted_order": "20261105T021400000000Z0b9f…11.20261105T021407123456Z7c1e…a4",
  "end_time": "2026-11-05T02:14:38.200000", "error": "HTTPError: 429 Too Many Requests",
  "outputs": {"status_code": 429}, "tags": ["batch=a"], "extra": {"metadata": {"prompt_version": "v7"}}}]}
```
Resulting `RunHeader`, as the runs list and trace tree see it:
```json
{"run_id": "7c1e…a4", "trace_id": "0b9f…11", "parent_run_id": "0b9f…11", "depth": 1, "is_root": false,
 "project": "research-agent", "name": "fetch_url", "run_type": "tool",
 "start_time": "2026-11-05T02:14:07.123456Z", "end_time": "2026-11-05T02:14:38.200000Z",
 "status": "error", "error": "HTTPError: 429 Too Many Requests", "latency_ms": 31076,
 "tags": ["batch=a"], "metadata": {"prompt_version": "v7"}, "model": null, "total_tokens": null,
 "payload_sizes": {"inputs": 31, "outputs": 19, "events": 0}, "child_count": 0,
 "first_seq": 1042, "last_seq": 1987, "event_count": 2}
```
