# Quickstart: prove M3 works

Validation run for M3. Expected values come from the [spec](spec.md#success-criteria-mandatory) and the planted
scenario ([research M3-R11](research.md#-m3-r11-scenario-plant-batch-a-re-planted-batch-b-added)). Query syntax is in
[contracts/filter-language.md](contracts/filter-language.md); commands are in [contracts/cli.md](contracts/cli.md);
the API is in [contracts/http-api.md](contracts/http-api.md).

**Delta key**: 🟢 New · 🟠 Improved · ⚪ Existing (as in [spec.md](spec.md)).

**Prerequisites**: a release build (`cd ui && npm ci && npm run build && cd .. && cargo build --release`). Step 3
needs Python with the pinned SDK and the example's deps (`pip install -r examples/requirements.txt`: langsmith 0.14.17,
langgraph). Step 7 needs Docker on Linux (CI runs it).

**Can't run in the planning sandbox**: steps 2 (browser) and 7 (Docker); step 4 at full 2M scale needs ~10 GB
free disk and ~15 min. Everything else is a terminal command. Each step names the unit that makes it pass
([plan](plan.md#delivery-order-pr-sized-each-ends-in-something-checkable)).

## 🟠 1. The scenario checks (one command) — unit 13

```bash
./target/release/tracedb demo --seed 7
```
Expect the M2 rows, plus:
- `31 text hits, all fetch_url ✓`;
- `fetch_url error rate / p95 (batch a) 14.0% / 31 s ✓`;
- `batch a cost $4.12 (12 no price) ✓`;
- `batch a vs b 14.0% → 2.0%, 31 s → 9 s ✓`;
- `feedback filter and average ✓`.

Exit code 0. Change a planted number in a scratch build: the check fails naming expected vs actual, exit 1.

## 🟢 2. Triage in the browser (SC-01, SC-02, SC-09) — units 10–11 · *needs a browser*

```bash
./target/release/tracedb serve --data /tmp/m3 --memtable 4MB &   # the demo's memtable: else the across-files row fails
./target/release/tracedb demo --url http://localhost:7070 --seed 7
```
Open http://localhost:7070 and time yourself (SC-09: under 5 minutes, by clicking):
1. Search box → `429 Too Many Requests` → **31** rows, all `fetch_url`, the phrase highlighted in each excerpt; the
   search shows as a chip; *Text* shows `search("429 Too Many Requests")`.
2. Clear the search; click the tag `batch=2026-11-05-a`; open *Stats*; *Group by* `name`; metrics error rate,
   p95 latency, cost → `fetch_url` **14.0%**, **31 s** (n = 300); total **$4.12**, "12 runs had no price". Click
   the `fetch_url` error-rate cell → Samples with the 42 errored `fetch_url` runs.
3. *Time series*, metric error rate → 5-minute buckets across the batch's ~3 h (batches are placed relative to now, ending
   30 h ago; the planted errors are spread out, no single spike); click a bucket → Samples with its runs.
   *Distribution* → a tall bin at 31 s (33 `fetch_url` runs in the 29.0–31.6 s bin).
4. *Compare* → add `batch=2026-11-06-b` → two columns per group (`fetch_url` 14.0% vs 2.0%; 31 s vs 9 s); *Time
   series* shows two lines on one offset axis.
5. Copy the URL, open it in a new tab → same grouping, metrics, view and comparison.
6. Field browser → `feedback.correctness` → click it → 2 runs.

## 🟢 3. SDK defaults work unchanged (SC-08) — units 8–9

```bash
curl -s localhost:7070/info | jq '{m: .batch_ingest_config.use_multipart_endpoint, z: .instance_flags.zstd_compression_enabled}'
# {"m": true, "z": true}
LANGSMITH_ENDPOINT=http://localhost:7070 LANGSMITH_API_KEY=local LANGSMITH_TRACING=true \
  python examples/research_agent.py              # no upload-format override (requirements.txt pins the SDK)
```
- The example agent's trace (9 runs, project `default`) appears within 1 s. The server doesn't log accepted requests;
  rejections log at debug (`RUST_LOG=tracedb=debug`). The contract test below proves multipart + zstd.
- Send feedback: `python -c 'from langsmith import Client; Client(api_url="http://localhost:7070", api_key="local").create_feedback(run_id="<root id from the UI>", key="helpful", score=1)'`
  → `200`. `tracedb query 'gt(feedback.helpful, 0.5)'` lists that root.
- Feedback for an unknown run id → `200`; `curl -s 'localhost:7070/api/feedback?orphaned=true' | jq .count` → 1.

```bash
cargo test -p tracedb --test contract          # 0.8.18 batch fixtures + 0.14.17 multipart (zstd on/off) + feedback fixtures
cargo test -p tracedb --test multipart         # round trip: same items as JSON batch → identical runs; attachment part rejected by name
```

The SDK's read API (`list_runs`) is not part of M3; find run ids in the UI or with `tracedb query --json`.

## 🟢 4. The speed table (SC-03 – SC-06, SC-10) — unit 12

*Moved to the backlog* ([roadmap](../000-tracedb-master-plan/roadmap.md) D3-1): the full run needs a quiet machine.

```bash
./target/release/tracedb bench --smoke              # 200k runs, ~2 min: the PR gate
./target/release/tracedb bench --runs 2000000 --json /tmp/bench.json --check   # full; every checked row PASS; exit 0
```
Expect:
- every row PASS: trace and run ≤ 50 ms; list ≤ 100 ms; text ≤ 500 ms (three rows: rare phrase, common word,
  search + filter); grouped ≤ 1 s; first estimate ≤ 1 s; suggestions ≤ 200 ms;
- every p99 ≤ 5× its p50;
- load ≥ 20,000 events/s while querying;
- disk (files + index) ≤ 50% of raw;
- idle memory is reported, not checked: the line ends `WAIVED (M4)` (≤ 200 MB waived until M4, D-028) and shows
  the bench process's RSS, an upper bound, since the server runs inside it.

To see the gate fail: `TRACEDB_BENCH_SLEEP_MS=200 tracedb bench --smoke --check` (test-only hook) → exit 1 naming the
query, target and measured value.

## 🟢 5. Search and aggregates from the terminal — units 4, 6, 7

```bash
curl -s localhost:7070/api/query -H 'content-type: application/json' \
  -d '{"filter": "search(\"429 Too Many Requests\")", "limit": 50, "count": true}' \
  | jq '{n: .stats.matched, names: ([.runs[].name] | unique), ex: .runs[0].match}'
# {"n": 31, "names": ["fetch_url"], "ex": {"path": "…", "excerpt": "…429 Too Many Requests…"}}

curl -s localhost:7070/api/aggregate -H 'content-type: application/json' \
  -d '{"filter": "has(tags, \"batch=2026-11-05-a\")", "stats": "by name | count, error_rate, p95(latency), sum(cost)"}' \
  | jq '(.groups[] | select(.key[0] == "fetch_url") | .cells | {runs, error_rate, p95}), .totals.cost_pico'
# {"runs": 300, "error_rate": 0.14, "p95": 31000}
# 4120000000000                                        ($4.12 in picodollars)

q=$(jq -rn '{filter: "has(tags, \"batch=2026-11-05-a\")", group_by: ["name"], metrics: ["count"]} | @uri')
curl -sN "localhost:7070/api/aggregate/stream?q=$q"     # small data: only `result`; at 2M runs: `estimate` first
tracedb query --group-by name --metrics 'error_rate,p95(latency)' --compare 'has(tags, "batch=2026-11-06-b")' \
  'has(tags, "batch=2026-11-05-a")'
```

## 🟠 6. Correctness against naive checks — units 1, 4, 6

```bash
cargo test -p tracedb-query --test search            # index lookup == naive phrase check over merged runs; M2 files (no sidecar) agree
cargo test -p tracedb-query --test aggregate         # aggregate == naive over merged runs, incl. (none), other, tags, compare, buckets
cargo test -p tracedb-model --test text              # Unicode tokenizer cases; phrase across leaves doesn't match
cargo test -p tracedb-ingest --test writer           # second start for a flushed run joins the multi-version set
cargo test --release -p tracedb --test crash -- --ignored   # 10/10 incl. the after_fts_put kill point
```

## 🟠 7. Same numbers on cloud-style storage (SC-07) — unit 13 · *needs Docker*

As M2's [quickstart step 7](../002-m2-durable-storage-query-builder/quickstart.md#7-same-numbers-on-cloud-style-storage-sc-08),
then `tracedb demo --seed 7 --storage s3://tracedb/test --metastore postgres://…`. Expect the same check rows as
step 1 (sidecars in the bucket, `feedback` table in Postgres).

## 🟠 8. M2 rough edges (Story 7) — unit 1 (and 10 for the UI)

```bash
tracedb serve --data /tmp/m3e --memtable 4MB & sleep 1; tracedb seed --traces 300 --no-delay
rm "$(ls /tmp/m3e/data/*/*/*.core.parquet | head -1)"                         # delete a data file while serving
curl -s localhost:7070/api/query -H 'content-type: application/json' -d '{"count": true}' | jq '.warnings, .stats.matched'
# a skipped_files warning naming it; a count from the other files (not a 500)
curl -s localhost:7070/healthz | jq '.ok, .damaged_segments'                   # false, [{id, reason: "missing", …}]
tracedb seed --traces 300 --no-delay; tracedb seed --traces 300 --no-delay     # no identity_conflict rejections
curl -s localhost:7070/api/query -H 'content-type: application/json' \
  -d '{"filter": "gt(start_time, \"-0s\")", "count": true}' | jq .stats.matched   # 0: nothing starts in the future
cargo test -p tracedb-ingest --test flush            # lost commit reply → retry registers once; row counts exact
```
*Needs a browser*: scroll the live Samples view down while `seed` runs → an "N new" pill; make a run stop matching
a live Table view → its row shows the new values.

## Last verified

2026-11-07 (T126), macOS, release build of `003-m3-search-aggregates-speed` at `80b7c73` plus this run's doc fixes;
`npm run build` + `cargo build --release -p tracedb`. Small data (the seed-7 scenario), not 2M runs.

| Step | Result | Numbers seen |
|---|---|---|
| 1. `demo --seed 7` | passed | all 10 checks ✓, exit 0; 408 traces, 10 flushes, ~3 s; 31 text hits (9 ms); 14.0% / 31 s; $4.12 (12 no price); 14.0% → 2.0%, 31 s → 9 s; feedback 2 runs. Fail path (exit 1, expected vs actual): `cargo test -p tracedb --test demo` 6/6 |
| 2. Triage (SC-01, SC-02, SC-09) | passed after a fix, through the API/CLI; UI not checked in a browser (D3-4) | Fix: `serve` needs `--memtable 4MB` (default 64MB → "checks across memtable and files" ✗). Then `demo --url` 10/10. API: search 31, all `fetch_url`, excerpt `HTTPError: 429 Too Many Requests` (11–32); `fetch_url` 300 runs, 42 errors, 0.14, p95 31000 ms, total 4.12e12 pico, 12 no price; 42 errored samples; series 41 × 5-min buckets, no single spike (doc fixed); dist bin 29.0–31.6 s = 33; compare 0.14/31000 vs 0.02/9000, series + `compare.side_b`; `feedback.correctness` in fields, 2 runs; `/` serves the built UI. SC-09: all six questions as API calls in 0.22 s — a script timing, not a real user timing |
| 3. SDK defaults (SC-08) | passed after a doc fix | `/info` → `{"m":true,"z":true}`; `research_agent.py` (langsmith 0.14.17, no override) → 9 runs in `default`; feedback via SDK → id returned, `query 'gt(feedback.helpful, 0.5)'` → 1 run (the root); unknown run id → `orphaned` count 1. Fixes: prerequisites install `examples/requirements.txt` (needs langgraph); the server logs no accepted requests, so the "log shows multipart + zstd" line is replaced. `--test contract` 1/1, `--test multipart` 12/12 |
| 4. Speed table | moved to backlog (D3-1) | not run (not even `--smoke`) |
| 5. Search and aggregates (terminal) | passed after a fix | search 31 / `["fetch_url"]`; aggregate 300 / 0.14 / 31000; jq precedence fixed so `.totals.cost_pico` prints 4120000000000 (was `null`); stream → one `result` event; `query --group-by name … --compare` → `fetch_url` 14.0% 31.0 s │ 2.0% 9.0 s, exit 0 |
| 6. Correctness tests | passed | search 2/2 (295 s debug), aggregate 1/1 (112 s), model text 11/11, ingest writer 9/9, crash (release, ignored) 10/10 rounds incl. `after_fts_put` |
| 7. Cloud storage (SC-07) | not runnable here (D3-3) | no Docker |
| 8. M2 rough edges | passed (terminal part); UI not checked in a browser (D3-4) | deleted file → `200`, `skipped_files` warning naming it, count 46674; `/healthz` ok false, `damaged_segments` [{reason "missing"}]; two more seeds, no rejections; `gt(start_time, "-0s")` → 0; `--test flush` 5/5. Added `sleep 1` before `seed` (it raced the server start) |
