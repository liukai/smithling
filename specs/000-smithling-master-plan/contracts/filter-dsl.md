# Filter DSL contract (v0)

A subset of LangSmith's filter grammar, so existing users already know it. The UI's click-to-filter builds these
strings, and the UI, CLI and agent tools all use the same grammar.

```
expr     := call
call     := op "(" args ")"
op       := and | or | not | eq | neq | gt | gte | lt | lte | has | in | exists | search
args     := arg ("," arg)*
arg      := call | field | literal
field    := name ("." name)*          -- e.g. status, metadata.prompt_version, outputs.status_code
literal  := string | number | bool | null | duration ("2s", "500ms") | time ("2026-11-05T02:00:00Z", "-12h")
```

| Op | Meaning | Example |
|---|---|---|
| `eq/neq/gt/gte/lt/lte` | compare | `gt(latency_ms, 2000)` |
| `has` | array contains | `has(tags, "batch=2026-11-05-a")` |
| `in` | one of | `in(run_type, "tool", "llm")` |
| `exists` | JSON path present | `exists(outputs.status_code)` |
| `search` | phrase in inputs/outputs | `search("429 Too Many Requests")` |
| `and/or/not` | combine | `and(eq(status,"error"), eq(run_type,"tool"))` |

Three positions in a query (FR-010):
- `filter`: the run itself
- `trace_filter`: the root run of the run's trace
- `tree_filter`: any run in the same trace

Example (scenario step 5): `trace_filter: eq(status,"ok")`, `tree_filter: and(eq(run_type,"tool"), eq(status,"error"))`,
`filter: eq(is_root,true)` → 23.

Errors return `{code: "filter_syntax", message, details: {start, end}}` (FR-013).
