# Filter language contract (M2)

The text form of a query. It is LangSmith's `filter` grammar (documented in `langsmith` SDK 0.14.17,
`Client.list_runs`), plus a few tracedb additions, marked **(tracedb)**. Panel ⇄ text is lossless: the panel is a
view of the same AST (data-model §9), and printing is canonical.

## Slots

| Slot | Applies to | LangSmith |
|---|---|---|
| `filter` | the run itself | same |
| `trace_filter` | the root run of its trace | same |
| `tree_filter` | any run in its trace (M2 includes the run itself) | SDK docs say "OTHER runs in the trace tree"; see note |
| `is_root` | `true` = root runs only | same |

Note: for "root ok, some step errored", the run itself is the root and isn't an error, so both readings give the same
answer. tracedb includes the run itself so "any run in the trace" means exactly that (master FR-010).

## Grammar

```ebnf
expr     = call ;
call     = name "(" [ arg { "," arg } ] ")" ;
name     = "and" | "or" | "not" | "eq" | "neq" | "gt" | "gte" | "lt" | "lte" | "has" | "in" (tracedb) ;
arg      = expr | path | value | list ;
path     = ident { "." ident } ;                       (* dotted paths: tracedb *)
value    = string | number | "true" | "false" | "null" ;
list     = "[" [ value { "," value } ] "]" ;            (* tracedb, for in() *)
string   = '"' { char | escape } '"' ;
```

| Function | Meaning | Example |
|---|---|---|
| `and(a, b, …)`, `or(a, b, …)`, `not(a)` | logic | `and(eq(run_type, "tool"), not(eq(name, "plan")))` |
| `eq`, `neq` | equal / not equal; `null` tests existence | `neq(error, null)` = has an error |
| `gt`, `gte`, `lt`, `lte` | compare numbers, times, strings | `gt(latency, 30)` |
| `has(list, v)` | list contains | `has(tags, "batch=2026-11-05-a")` |
| `in(path, [..])` **(tracedb)** | one of | `in(name, ["fetch_url", "web_search"])` |

## Fields

| Path | Type | Notes |
|---|---|---|
| `status` | `"running"` \| `"ok"` \| `"error"` | derived |
| `name`, `run_type`, `model`, `thread_id`, `project` | string | |
| `start_time`, `end_time` | time | ISO 8601, or relative `"-12h"`, `"-30m"`, `"-7d"` **(tracedb)** |
| `latency` | number, **seconds** | as LangSmith (`gt(latency, 10)` = over 10 s) |
| `error` | string | `neq(error, null)` for "has an error" |
| `tags` | list of string | use `has` |
| `total_tokens`, `input_tokens`, `output_tokens` | number | |
| `trace_id`, `run_id`, `parent_run_id` | uuid string | |
| `metadata.<path>`, `inputs.<path>`, `outputs.<path>` **(tracedb)** | any scalar | promoted paths are fast; others read payloads (`not_indexed` warning) |

LangSmith's `feedback_key` / `feedback_score` parse but return `unsupported: feedback is supported from M4`.
`search(…)` returns `unsupported: search() is supported from M3`.

## Typing rules
- Comparisons only match values of the same type. `gte(outputs.status_code, 400)` matches numbers; string values
  are skipped and reported in a `mixed_types` warning.
- A path that a run doesn't have never matches `eq`/`gt`/…; it matches `eq(path, null)`.
- Strings compare by bytes; times by instant; numbers as f64.

## Errors

Every error names the slot, a byte range and what was expected:

| Input | Error |
|---|---|
| `eq(staus, "error")` | `3..8 unknown field "staus" (did you mean "status"?)` |
| `and(eq(status, "error")` | `24..24 expected ")"` |
| `gt(name, 5)` | `3..7 "name" is a string; gt() needs a number, time or string value of the same type` |
| `eq(status, "failed")` | `12..20 status is one of "running", "ok", "error"` |

## Scenario queries (asserted by `tracedb demo`)

| Question | Query |
|---|---|
| What failed? → **14** | `time: last 12h`, `is_root: true`, `filter: and(has(tags, "batch=2026-11-05-a"), eq(status, "error"))` |
| What failed quietly? → **23** | `time: last 12h`, `is_root: true`, `filter: and(has(tags, "batch=2026-11-05-a"), eq(status, "ok"))`, `tree_filter: eq(status, "error")` |
| Which ones exactly? | `filter: and(eq(metadata.prompt_version, "v7"), eq(outputs.status_code, 429))` |
