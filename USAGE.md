# Using Smithling

The reference: every view, command and flag. For a five-minute tour, start with the [README](README.md#use-it-today).

- [Install and run](#install-and-run)
- [Trace your agent](#trace-your-agent)
- [The browser UI](#the-browser-ui)
- [The command line](#the-command-line)
- [Storage and flags](#storage-and-flags)
- [Cloud mode](#cloud-mode)
- [What works today, and what doesn't yet](#what-works-today-and-what-doesnt-yet)
- [Building and developing](#building-and-developing)

---

## Install and run

You need Rust 1.99 or newer, and Node 24 for building the UI (once).

```bash
(cd ui && npm ci && npm run build)    # the UI; the server embeds ui/dist if it's there
cargo build --release                 # no Node needed; without ui/dist the UI is a placeholder page
./target/release/smithling serve      # http://localhost:7070, data in ./.smithling
```

---

## Trace your agent

Any agent built on the LangSmith SDK (LangChain, LangGraph, `@traceable`) works unchanged; only the endpoint changes:

```bash
export LANGSMITH_TRACING=true
export LANGSMITH_ENDPOINT=http://localhost:7070
export LANGSMITH_API_KEY=local        # any value; Smithling doesn't check it
export LANGSMITH_PROJECT=my-agent
python your_agent.py
```

No agent at hand? Run the example (no API keys, no network), or generate data:

```bash
pip install -r examples/requirements.txt && python examples/research_agent.py
./target/release/smithling seed --traces 100 --no-delay
```

The SDK's default upload format (multipart, plain or zstd) works without any setting changes. Feedback the SDK sends
(`client.create_feedback(...)`, evaluators) is stored too, even when it arrives before its run.

---

## The browser UI

Open http://localhost:7070.

**Runs list and traces.** New and updated runs appear live. Click a run for its tree and timing waterfall, click a
step for its full inputs and outputs. Every view is a shareable URL.

**The query panel** above the list holds the project, time range, conditions, order and limit.

- Conditions are chips: on the run itself, on its trace's root, or on *any* run in its trace ("roots that succeeded
  but where some step failed").
- Click any status, tag, name, metadata value or table cell to add a chip; negate, disable or remove chips without
  retyping.
- *Text* shows the same query in LangSmith's filter syntax, e.g. `and(eq(status, "error"), has(tags, "prod"))`, and
  keeps both forms in step; mistakes are pointed out inline.
- The **field browser** lists every field seen in metadata, inputs and outputs, with types, counts and top values.
- Switch between *Samples* and *Table* (pick your columns); turn on *live* to see matching runs as they arrive.

**Search** finds words and phrases in inputs, outputs and errors.

- Case-insensitive; a phrase means consecutive words within one text value; no fuzzy matching or ranking.
- It becomes a chip like any other condition: `search("429 Too Many Requests")`, or `search(error, "timeout")` for
  one side. Each hit shows the matching excerpt.

**Stats** adds things up.

- Group by up to two keys (name, `run_type`, model, status, tag, `metadata.<key>`, `feedback_key`).
- Metrics: count, error rate, p50/p95/p99 latency, token sums, cost, `avg(feedback.<key>)`. A stats line such as
  `by name | count, error_rate, p95(latency), sum(cost)` does the same in one string.
- Click any cell to see exactly its runs. *Time series* draws a metric over time, *Distribution* a latency
  histogram; click a bucket for its runs.
- *Compare* puts a second query (another batch, or the same window a day earlier) side by side and overlaid.
- On large data the first numbers are estimates, marked as such, that refine to the exact answer.

**Cost** is the cost the run reports; otherwise tokens × a built-in price list (USD per 1M tokens). Edit
`<data>/prices.toml` or pass `--prices FILE` to replace entries by name. Prices are never fetched from the network;
runs with no price are counted, not guessed.

**Feedback**: filter on it (`lt(feedback.correctness, 0.5)`), average it, group by `feedback_key`.
`GET /api/feedback?run_id=…` lists a run's feedback; `?orphaned=true` lists feedback whose run hasn't arrived.

**Scenarios** (`/scenarios`) asks the SmithDB post's questions of the demo data, one click each (load it first with
`smithling demo --url http://localhost:7070`).

---

## The command line

All commands talk to a running server unless noted.

**`smithling query`**: the same queries as the UI, printed as a table or JSON lines.

```bash
smithling query --project my-agent --time 12h --root 'eq(status, "error")'
smithling query --tree-filter 'eq(status, "error")' --json 'eq(status, "ok")'     # JSON lines
smithling query --search '429 Too Many Requests'
smithling query --group-by name --metrics 'count,error_rate,p95(latency),sum(cost)' \
  --compare 'has(tags, "batch=2026-11-06-b")' 'has(tags, "batch=2026-11-05-a")'
```

**`smithling seed`**: realistic research-agent traces, sent the way an agent would (`--traces N`, `--no-delay` to
send late updates right away).

**`smithling demo`**: loads an overnight-eval scenario (two batches of an agent, before and after a retry fix) and
checks ten numbers: 14 failing roots, 23 quiet failures, top `prompt_version` values, a planted count, reads across
memory and files, 31 search hits all on `fetch_url`, `fetch_url` 14.0% errors / p95 31 s, batch cost $4.12, batch a
vs b, and a feedback filter and average. Exit code 0 when all pass; 1 names expected vs actual.

```bash
smithling demo --seed 7                        # into its own temporary server
smithling demo --url http://localhost:7070     # into a running server, so you can look around afterwards
```

**`smithling bench`**: loads a reference volume (2M runs, or `--smoke` for 200k) while querying it, then prints
P50/P99 per query type, load rate and disk use against the targets. `--check` exits 1 naming any row over budget.
Idle memory is reported but not checked (its target is waived until M4).

```bash
smithling bench --smoke --check
smithling bench --runs 2000000 --json /tmp/bench.json --check
```

---

## Storage and flags

Runs are kept in memory, logged to disk as they arrive, and moved to Parquet data files in the background. Uploads
never pause; bursts get `429` + `Retry-After` (the SDK retries). A catalog lists the files. By default everything is
in `--data`: `<data>/data` and `<data>/meta.sqlite`.

| Flag | Default | Meaning |
|---|---|---|
| `--data` | `./.smithling` | the data folder |
| `--port` / `--bind` | `7070` / `127.0.0.1` | where it listens |
| `--storage` (`SMITHLING_STORAGE`) | `<data>/data` | a folder or `s3://bucket/prefix` |
| `--metastore` (`SMITHLING_METASTORE`) | `sqlite://<data>/meta.sqlite` | `sqlite://path` or `postgres://user@host/db` |
| `--memtable` | `64MB` | flush the in-memory batch at this size… |
| `--flush-age` | `2m` | …or when its oldest event is this old |
| `--dedup-window` | `10m` | how long an identical retry is recognized |
| `--cache` | `32MB` | memory for file footers, text-index pages and payload pages |
| `--prices` | `<data>/prices.toml` if present | model prices replacing built-in entries |

`--max-memory` and `--max-runs` (from M1) are accepted with a warning; they no longer apply.

---

## Cloud mode

The same binary with different URLs. S3 settings come from the standard `AWS_*` variables (`AWS_ENDPOINT` for
S3-compatible stores; plain `http://` endpoints are allowed); the Postgres password from `PGPASSWORD` or the URL.
Passwords are never logged. The write-ahead log stays in `--data`.

```bash
export AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… AWS_REGION=us-east-1 AWS_ENDPOINT=http://127.0.0.1:9000
export PGPASSWORD=…
smithling serve --storage s3://smithling/prod --metastore postgres://smithling@127.0.0.1/smithling
```

`ci/cloud.yml` starts an S3-compatible store and Postgres 17 locally from the same variables; CI's `cloud` job runs
the storage tests, `smithling demo` and the crash tests against them.

---

## What works today, and what doesn't yet

**Works**: SDK uploads (JSON and multipart), crash safety, live updates, data on disk or S3 + Postgres, the query
panel and field browser, search, Stats / Time series / Distribution / Compare, cost, feedback, `query`, `demo`,
`bench`, no outbound connections. A damaged or missing data file is skipped and named instead of failing queries.

**Not yet**:

- No compaction: data files are never merged, so many small flushes slow reads down over time (M4).
- The text index stores no word positions, so a phrase of very common words reads more text to confirm.
- Attachments (multipart `attachment.*` parts) are rejected by name; the run is kept.
- No feedback writes from the UI; no threads or export yet (M4).
- No delete or retention yet (backlog).

**Not verified yet**: the UI in a real browser (covered by component tests), idle memory (≤ 200 MB, waived until M4),
a clean full 2M-run bench.

---

## Building and developing

| Crate | Holds |
|---|---|
| `crates/model` (`smithling-model`) | runs, events, the merge rule, the filter language, cost |
| `crates/ingest` (`smithling-ingest`) | the writer, WAL, memtables, flusher, recovery, feedback store, text-index backfill |
| `crates/storage` (`smithling-storage`) | Parquet data files, text index, catalog (SQLite / Postgres), object store, cache |
| `crates/query` (`smithling-query`) | scans, merge-on-read, filters, search, aggregates, live views |
| `crates/server` (`smithling-server`) | the HTTP API, LangSmith upload mapping and the embedded UI |
| `crates/tools` (`smithling-tools`) | `seed`, `query`, `demo`, `bench` and the scenario data |
| `crates/smithling` (`smithling`) | the binary |

```bash
cargo build --release -p smithling --no-default-features   # serve-only binary, without the tools
cargo test --workspace                                      # S3/Postgres tests skip unless SMITHLING_TEST_S3_URL / SMITHLING_TEST_PG_URL are set
(cd ui && npm test)                                         # UI component tests
cargo test --release -p smithling --test crash --test restart --test throughput -- --ignored   # slow checks
scripts/no-outbound.sh target/release/smithling            # Linux: no non-loopback connections
smithling serve --ui-dir ui/dist                            # UI work: serve the UI from disk
```

Integration tests live with the crate they test; tests that start the real binary are in `crates/smithling/tests`.
Notes for contributors and coding agents are in [AGENTS.md](AGENTS.md); every change gets a line in
[CHANGELOG.md](CHANGELOG.md).
