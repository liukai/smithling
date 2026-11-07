# CLI contract (M3)

Only what M3 adds or changes. Everything else is the [M2 CLI](../../002-m2-durable-storage-query-builder/contracts/cli.md).

**Delta key**: 🟢 New · 🟠 Improved · ⚪ Existing (as in [spec.md](../spec.md)).

| | Command | Purpose |
|---|---|---|
| 🟠 | `tracedb serve … [--cache 32MB] [--memtable 64MB] [--flush-age 2m] [--prices PATH]` | `--cache` sizes the bounded data cache; memtable and flush-age defaults change if approved (design §11 q1); `--prices` overrides `<data>/prices.toml` |
| 🟢 | `tracedb bench [--runs 2000000] [--data DIR] [--json FILE] [--check] [--smoke]` | load the reference volume (or reuse `--data`), run each query type n ≥ 100 times, print P50/P99 against the targets; `--check` exits 1 on a slipped target; `--smoke` = 200k runs |
| 🟠 | `tracedb query … [--search TEXT] [--group-by KEY[,KEY]] [--metrics LIST] [--top 10] [--compare EXPR \| --shift 1d] [--json] ['EXPR']` | M2's query plus search and aggregates; with `--group-by` or `--metrics` it prints the aggregate table (exact, `POST /api/aggregate`) |
| 🟠 | `tracedb demo [--seed 7] [--url URL \| --data DIR] [--checks m2,m3]` | generates batch a (re-planted) and batch b; runs the M2 and M3 checks (default both) |
| 🟠 | `tracedb seed …` | as M2; no run starts in the future; seeding the same seed again doesn't reuse run ids |

## `tracedb query` examples

```bash
tracedb query --search '429 Too Many Requests' 'has(tags, "batch=2026-11-05-a")'
tracedb query 'search(error, "timeout")'
tracedb query --group-by name --metrics 'count,error_rate,p95(latency),sum(cost)' 'has(tags, "batch=2026-11-05-a")'
tracedb query --group-by name --metrics 'error_rate,p95(latency)' --compare 'has(tags, "batch=2026-11-06-b")' \
  'has(tags, "batch=2026-11-05-a")'
```

Aggregate output (illustrative):

```
name          runs   error_rate   p95       cost     │ b: runs  error_rate  p95
fetch_url      300   14.0%        31.0 s    —        │    300   2.0%        9.0 s
llm_call     1,112    0.0%        4.1 s     $4.12    │  1,050   0.0%        3.9 s
…
(other)        …
total        4,210    1.3%        …         $4.12    12 runs had no price
```

## `tracedb bench` output (illustrative)

```
tracedb bench — 2,000,000 runs, 4.9 GB raw JSON, calibration r = 1.00 (reference)
  load            23,410 events/s while querying                    target ≥ 20,000      PASS
  query               p50       p99      target   budget   result
  trace tree         6.1 ms    7.1 ms    50 ms    50 ms    PASS
  run + payload     18.0 ms   31.0 ms    50 ms    50 ms    PASS
  filtered list     61.0 ms  128.0 ms   100 ms   100 ms    PASS
  text search        …         …        500 ms   500 ms    …
  grouped            …         …          1 s      1 s     …
  first estimate     …         …          1 s      1 s     …
  suggestions        3.0 ms    4.2 ms   200 ms   200 ms    PASS
  disk               files 19.3% + index 12.1% = 31.4% of raw     target ≤ 50%      PASS
  idle memory        182 MB (median over 30 s)                    target ≤ 200 MB   PASS
```

- A p99 over 5× its p50 fails, whatever the budget.
- `--json` writes `{runs, raw_bytes, r, rows: [{query, p50_ms, p99_ms, target_ms, budget_ms, pass}], disk, idle_mb}`.
- Exit codes: 0 = all pass; 1 = a row failed (the failing rows are printed last, naming the query, the target and
  the measured value); 2 = setup error.

## `tracedb demo` output (illustrative; counts are fixed by the generator)

```
tracedb demo — M2 + M3 checks  (seed 7, 2,614 traces, 4 flushes)
  ✓ 14 failing roots                          14                  31 ms
  ✓ 23 quiet failures                         23                  58 ms
  ✓ prompt_version top values                 v6 928 · v7 912     12 ms
  ✓ v7 ∧ status_code = 429                    19                  22 ms
  ✓ checks across memtable and files          yes
  ✓ 31 text hits, all fetch_url               31                  84 ms
  ✓ fetch_url error rate / p95 (batch a)      14.0% / 31 s       210 ms
  ✓ batch a cost                              $4.12 (12 no price)
  ✓ batch a vs b                              14.0% → 2.0%, 31 s → 9 s, side by side + overlaid
  ✓ feedback filter and average               2 runs · avg 0.00 (1 sent before its run)
all 10 checks passed
```

Exit codes are M2's: 0 = ok, 1 = usage or failed check (naming expected vs actual), 2 = server not reachable.
