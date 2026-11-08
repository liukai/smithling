# Filter language contract (M3)

Only what M3 adds or changes. Slots, grammar, fields, typing rules and errors are the
[M2 contract](../../002-m2-durable-storage-query-builder/contracts/filter-language.md); the
[master contract](../../000-smithling-master-plan/contracts/filter-dsl.md) already lists `exists` and `search`.

**Delta key**: 🟢 New · 🟠 Improved · ⚪ Existing (as in [spec.md](../spec.md)).

## 🟠 Grammar additions

```ebnf
name     = … (M2) | "exists" | "search" ;
call     = … (M2)
         | "exists" "(" path ")"
         | "search" "(" [ side "," ] string ")" ;
side     = "inputs" | "outputs" | "error" ;
path     = … (M2) | "feedback" "." ident ;
```

| | Function | Meaning | Example |
|---|---|---|---|
| 🟢 | `search(text)` | the words of `text`, in order, inside one text value of the run's inputs, outputs or error; case-insensitive; one word is a phrase of one | `search("429 Too Many Requests")` |
| 🟢 | `search(side, text)` | the same, in one side only | `search(outputs, "rate limit")` |
| 🟢 | `exists(path)` | the path has a value (same result as `neq(path, null)`) | `exists(metadata.prompt_version)` |
| 🟢 | `feedback.<key>` | number: the mean of the run's feedback scores for `key`; missing if none | `gt(feedback.correctness, 0.5)` |
| 🟠 | `feedback_key`, `feedback_score` (LangSmith) | accepted together in one `and(…)` and rewritten | `and(eq(feedback_key, "correctness"), gt(feedback_score, 0.5))` → `gt(feedback.correctness, 0.5)` |

Words are split by Unicode word boundaries and lower-cased; punctuation is ignored (`"429, Too"` = `"429 too"`).
No wildcards, stemming or fuzzy matching. `search` works in every slot (`filter`, `trace_filter`, `tree_filter`) and
combines with everything; under `not(…)` or `or(…)` it is exact but slower (no index).

Printing is canonical: `search("…")` keeps the text as written (escaped); `exists(p)` and `neq(p, null)` each print
as written.

## 🟢 The stats line

A separate text slot (`stats` in the API, the *Text* view of the Stats panel) that says how to add up the runs the
filter selects. smithling's own syntax; LangSmith has none.

```ebnf
stats    = [ "by" key { "," key } "|" ] metric { "," metric } [ "|" "top" number ] ;   (* no keys: totals only *)
key      = "name" | "run_type" | "model" | "status" | "tag" | "feedback_key" | path ;   (* path: metadata.<…> *)
metric   = "count" | "error_rate"
         | ( "p50" | "p95" | "p99" ) "(" "latency" ")"
         | "sum" "(" ( "input_tokens" | "output_tokens" | "total_tokens" | "cost" ) ")"
         | "avg" "(" "feedback" "." ident ")" ;
```

| Example | Meaning |
|---|---|
| `by name \| count, error_rate, p95(latency), sum(cost)` | the step 9 table |
| `by model \| sum(total_tokens), sum(cost)` | tokens and cost per model |
| `by tag \| count \| top 20` | runs per tag (groups overlap) |
| `by metadata.prompt_version, name \| error_rate` | two keys |
| `by name \| avg(feedback.correctness)` | average feedback per step |

Definitions:
- `error_rate` = errored / finished runs (running runs excluded and reported as `running`);
- percentiles are nearest rank over finished runs, in seconds in text output;
- `sum(cost)` follows the cost rules (reported → tokens × price → no price; data-model §5).

## 🟢 Errors (added to `tests/fixtures/filter-golden.json`)

| Input | Error |
|---|---|
| `search()` | `7..7 search needs a string` |
| `search("")` | `7..9 search needs at least one word` |
| `search(body, "x")` | `7..11 search side is inputs, outputs or error` |
| `exists("x")` | `7..10 exists() needs a field path` |
| `gt(feedback, 1)` | `3..11 feedback needs a key: feedback.<key>` |
| `by name \| p90(latency)` | `10..13 unknown metric "p90"; expected p50, p95 or p99` |
| `by a, b, c \| count` | `9..10 at most 2 group keys` |

The M2 golden case `search("rate limit")` → "supported from M3" becomes an `ok` case.

## Scenario queries (asserted by `smithling demo`)

| | Question | Query |
|---|---|---|
| ⚪ | What failed? → **14** | `is_root: true`, `filter: and(has(tags, "batch=2026-11-05-a"), eq(status, "error"))` (no time window; the batch tag selects) |
| ⚪ | What failed quietly? → **23** | as M2, by tag |
| 🟢 | Is this a pattern? → **31**, all `fetch_url` | `filter: search("429 Too Many Requests")` |
| 🟢 | How bad? → `fetch_url` **14.0%**, p95 **31 s**; batch **$4.12** | `filter: has(tags, "batch=2026-11-05-a")`, `stats: by name \| count, error_rate, p95(latency), sum(cost)` |
| 🟢 | Did the fix work? → **14.0% → 2.0%**, **31 s → 9 s** | the same, `compare: has(tags, "batch=2026-11-06-b")` |
| 🟢 | Feedback | `filter: lt(feedback.correctness, 0.5)`; `stats: by name \| avg(feedback.correctness)` |
