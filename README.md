# Smithling

**A pocket-sized SmithDB. Trace your agents locally.**

SmithDB is the storage engine behind LangSmith, built for the cloud. Smithling is a little clone of it that lives on
your laptop: same design, one folder, and your agent's data never leaves the machine.

A local trace store and viewer for AI agents. Point an unmodified LangSmith-SDK agent (LangChain, LangGraph,
`@traceable`) at it, watch its traces live in your browser, and query them. Everything stays in one folder on your
machine (or, in cloud mode, in S3-compatible storage and Postgres).

An independent open-source project modeled on the design LangChain described for SmithDB; not affiliated with or
endorsed by LangChain. LangSmith is LangChain's trademark; smithling speaks its SDK's upload protocol.

Status: **M3, "search, aggregates and speed"**: built and passed locally; performance work (the full 2M-run bench,
idle memory) deferred until before M4. Plans and progress: [`specs/000-smithling-master-plan/roadmap.md`](specs/000-smithling-master-plan/roadmap.md).
New to the project? [STARTER_GUIDE.md](STARTER_GUIDE.md) explains the problem, the architecture and how each
milestone built on the last. Background: [SECRET_OF_COLUMNAR_DB.md](SECRET_OF_COLUMNAR_DB.md) (columnar storage, with
a runnable example) and [ROCKSDB_COMPARISON.md](ROCKSDB_COMPARISON.md) (the storage engine, for RocksDB people).

## Run it

```bash
(cd ui && npm ci && npm run build)   # the UI, once (Node 24); the server build embeds ui/dist if present
cargo build --release                # no Node needed; without ui/dist the UI is a placeholder page
./target/release/smithling serve       # http://localhost:7070, data in ./.smithling
```

Point any agent at it; only the endpoint changes:

```bash
export LANGSMITH_TRACING=true
export LANGSMITH_ENDPOINT=http://localhost:7070
export LANGSMITH_API_KEY=local        # any value; smithling doesn't check it
export LANGSMITH_PROJECT=my-agent
python your_agent.py
```

No agent at hand? Try the example (no API keys, no network) or generate data:

```bash
pip install -r examples/requirements.txt && python examples/research_agent.py
./target/release/smithling seed --traces 100 --no-delay
```

Open http://localhost:7070: a live runs list, click a run for its tree and timing waterfall, click a step for its
full inputs and outputs. Every view is a shareable URL (M1 links still work).

## Query

The **query panel** above the list holds the project, time range, where-conditions as chips (on the run, on its
trace's root, or on any run in its trace), order and limit. Click any status, tag, name, metadata value or table
cell to add a chip; negate, disable or remove chips without retyping. *Text* shows the same query in the LangSmith
filter grammar (`and(eq(status, "error"), has(tags, "prod"))`) and keeps both forms in step; a mistake is pointed
at inline. The **field browser** lists every field seen in metadata, inputs and outputs with its types, counts and
top values. Switch between *Samples* and *Table* (pick your columns) without rebuilding the query, and turn on
*live* to see matching new and updated runs as they arrive.

**Search.** The search box finds words and phrases in inputs, outputs and errors (case-insensitive; a phrase is
consecutive words within one text value; no fuzzy matching or ranking). It becomes a chip like any other condition,
`search("429 Too Many Requests")` in *Text*, or `search(error, "timeout")` for one side; each hit shows an excerpt
with the match highlighted. Each data file carries a small text index, so a search reads only the files and rows
that can match.

**Stats.** *Stats* groups the matching runs by up to two keys (name, `run_type`, model, status, tag,
`metadata.<key>`, `feedback_key`) with metrics: count, error rate, p50/p95/p99 latency, token sums, cost and
`avg(feedback.<key>)`; a stats line (`by name | count, error_rate, p95(latency), sum(cost)`) does the same in
one string. Click any cell to see exactly its runs in *Samples*. *Time series* draws a metric per group over time
and *Distribution* a latency histogram; click a bucket or bin for its runs. *Compare* puts a second query (another
batch, or the same window a day earlier) side by side in the table and overlaid in the charts. On large data the
first numbers are estimates (marked as such) that refine to the exact answer. Grouping, metrics, chart and
comparison are in the URL.

**Cost** is the cost the run reports; otherwise tokens × a built-in price list for common models (USD per 1M
tokens). Edit `<data>/prices.toml` (or pass `--prices FILE`) to replace entries by name; prices are never fetched
from the network. Runs with no price are counted, not guessed.

**Feedback** the SDK sends (`client.create_feedback(...)`, evaluators, multipart `feedback.*` parts) is stored, even
when it arrives before its run. Filter on it (`lt(feedback.correctness, 0.5)`), average it
(`avg(feedback.correctness)`), group by `feedback_key`, and find `feedback.<key>` in the field browser.
`GET /api/feedback?run_id=…` lists a run's feedback; `?orphaned=true` lists feedback whose run hasn't arrived.

From a terminal, against a running server:

```bash
./target/release/smithling query --project my-agent --time 12h --root 'eq(status, "error")'
./target/release/smithling query --tree-filter 'eq(status, "error")' --json 'eq(status, "ok")'   # JSON lines
./target/release/smithling query --search '429 Too Many Requests'
./target/release/smithling query --group-by name --metrics 'count,error_rate,p95(latency),sum(cost)' \
  --compare 'has(tags, "batch=2026-11-06-b")' 'has(tags, "batch=2026-11-05-a")'
```

To check it all on generated data, `smithling demo` loads an overnight-eval scenario (two batches, before and after
a retry fix) into its own temporary server and prints a pass/fail table with a timing per check: the M2 rows (14
failing roots, 23 quiet failures, top `prompt_version` values, a planted count, reads across memory and files)
and the M3 rows (31 search hits all on `fetch_url`, `fetch_url` 14.0% errors / p95 31 s, batch cost $4.12 with 12
unpriced runs, batch a vs b side by side and overlaid, and a feedback filter and average with one score sent before
its run). Exit code 0 when every check passes; 1 naming expected vs actual otherwise:

```bash
./target/release/smithling demo --seed 7                 # M2 and M3 checks; --checks m2 (or m3) for one set
./target/release/smithling demo --url http://localhost:7070   # into a running server (sized from its /healthz)
```

**Speed.** `smithling bench` loads a reference volume (2M runs by default, `--smoke` for 200k) into its own server
while querying it, then prints P50/P99 per query type (trace, run, filtered list, text search in three rows: rare
phrase, common word, search + filter; then grouped, first estimate, suggestions), load rate and disk use against
the targets. Idle memory is reported but not checked: the 200 MB target is waived until M4. With `--check` it exits 1
naming any row that misses its budget; on a shared machine the targets are scaled by a short calibration run. CI runs
`--smoke --check` on every PR; the full run is started by hand ("Run workflow") until it's re-evaluated
(roadmap D3-1).

```bash
./target/release/smithling bench --smoke --check
./target/release/smithling bench --runs 2000000 --json /tmp/bench.json --check
```

## Storage

Runs are kept in memory, logged to disk as they arrive, and moved to Parquet data files in the background (no
pause for uploads; bursts get `429` retries instead of refusals). A catalog lists the files. Defaults: all in
`--data` (`<data>/data` and `<data>/meta.sqlite`). M1 data folders open with no manual step.

| Flag | Default | |
|---|---|---|
| `--storage` (`SMITHLING_STORAGE`) | `<data>/data` | a folder or `s3://bucket/prefix` |
| `--metastore` (`SMITHLING_METASTORE`) | `sqlite://<data>/meta.sqlite` | `sqlite://path` or `postgres://user@host/db` |
| `--memtable` | `64MB` | flush the in-memory batch when its heap reaches this size… |
| `--flush-age` | `2m` | …or when its oldest event is this old |
| `--dedup-window` | `10m` | how long an identical retry is recognized as a duplicate |
| `--cache` | `32MB` | memory for file footers, text-index pages and payload pages read back |
| `--prices` | `<data>/prices.toml` if present | model prices replacing the built-in entries by name |

`--max-memory` and `--max-runs` (M1) are accepted with a warning; they no longer apply.

**Cloud mode** is the same binary with other URLs. S3 settings come from the standard `AWS_*` variables
(`AWS_ENDPOINT` for MinIO and other S3-compatible stores; plain `http://` endpoints are allowed); the Postgres password from `PGPASSWORD` or the URL, and
passwords are never logged. The write-ahead log stays in `--data`.

```bash
export AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… AWS_REGION=us-east-1 AWS_ENDPOINT=http://127.0.0.1:9000
export PGPASSWORD=…
./target/release/smithling serve --storage s3://smithling/prod --metastore postgres://smithling@127.0.0.1/smithling
```

`ci/cloud.yml` starts MinIO (bucket `smithling`) and Postgres 17 (database `smithling`) locally from the same
variables; CI's `cloud` job runs the storage tests, `smithling demo` and the crash tests against it, and checks the
demo table matches the local one.

## What M3 does and doesn't do

- **Does**: everything M2 does (SDK uploads, crash safety, live updates, data on disk or S3 + Postgres, the query
  panel, field browser, Table view, `smithling query`, zero outbound connections), plus multipart uploads on by
  default (the SDK's default upload format, plain or zstd; `/info` advertises it, so no SDK setting changes),
  feedback, text search, Stats / Time series / Distribution / Compare, costs from a local price list,
  `smithling bench` and the M3 demo checks. A damaged or missing data file is skipped and named (in the response and
  in `/healthz`) instead of failing every query.
- **Doesn't yet**:
  - no compaction: data files are never merged, so many small flushes slow reads down;
  - the text index stores no word positions: a phrase is confirmed by reading the candidate rows' text, so a
    phrase of very common words reads more;
  - attachments (multipart `attachment.*` parts) are rejected by name (`attachment_not_stored`); the run is kept;
  - no feedback writes from the UI (adding or editing scores, threads, export are M4).
- **Not verified yet**:
  - the UI (search, Stats, charts, Compare, "N new") is covered by component tests but hasn't been checked in a
    real browser;
  - idle memory: the ≤ 200 MB target is waived until M4 (compaction); `smithling bench` reports it as `WAIVED (M4)`
    with its own process's RSS, an upper bound, and never fails on it;
  - the full 2M-run bench hasn't had a clean pass on the reference laptop yet;
  - the cloud job (S3 + Postgres) hasn't had its first M3 run.

## Code layout

| Crate | Holds |
|---|---|
| `crates/model` (`smithling-model`) | runs, events, the merge rule, the filter language, cost |
| `crates/ingest` (`smithling-ingest`) | the writer, WAL, memtables, flusher, recovery, feedback store, text-index backfill |
| `crates/storage` (`smithling-storage`) | Parquet data files, text index, catalog (SQLite / Postgres), object store, cache |
| `crates/query` (`smithling-query`) | scans, merge-on-read, filters, search, aggregates, live views |
| `crates/server` (`smithling-server`) | the HTTP API, LangSmith upload mapping and the embedded UI: what `serve` runs |
| `crates/tools` (`smithling-tools`) | `seed`, `query`, `demo`, `bench` and the scenario data; the server never depends on it |
| `crates/smithling` (`smithling`) | the binary: parses the command and calls the server or the tools |

The tools are in the binary by default. For a serve-only binary:

```bash
cargo build --release -p smithling --no-default-features   # only `smithling serve`
```

Integration tests live with the crate they test. Tests that start the real binary (crash, restart, idle memory,
the CLI tools) are in `crates/smithling/tests`; the in-process test server is `smithling_server::testkit` (feature
`testkit`).

## Develop

```bash
cargo test --workspace                       # Rust tests (S3/Postgres storage tests skip unless SMITHLING_TEST_S3_URL / SMITHLING_TEST_PG_URL are set)
scripts/no-outbound.sh target/release/smithling   # Linux: no non-loopback connection through seed, search, aggregate, multipart, demo
(cd ui && npm test)                          # UI component tests
cargo test --release -p smithling --test crash --test restart --test throughput -- --ignored   # slow checks
/path/to/python scripts/record-sdk-fixtures.py   # re-record SDK contract fixtures (see the script header)
```

For UI work, `smithling serve --ui-dir ui/dist` serves the UI from disk, so only the UI needs rebuilding. Design docs
live in `specs/`.

Changes are recorded in [CHANGELOG.md](CHANGELOG.md) (milestones are tagged `m1`, `m2`, `m3`); notes for coding
agents are in [AGENTS.md](AGENTS.md).

## License and security

[MIT](LICENSE). smithling is a local, single-user tool with no login: read [SECURITY.md](SECURITY.md) before exposing
it beyond `127.0.0.1`.
