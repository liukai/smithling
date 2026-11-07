# tracedb

A local trace store and viewer for AI agents. Point an unmodified LangSmith-SDK agent (LangChain, LangGraph,
`@traceable`) at it, watch its traces live in your browser, and query them. Everything stays in one folder on your
machine (or, in cloud mode, in S3-compatible storage and Postgres).

Status: **M2, "durable storage and query builder"**. Plans and progress: [`specs/000-tracedb-master-plan/roadmap.md`](specs/000-tracedb-master-plan/roadmap.md).

## Run it

```bash
(cd ui && npm ci && npm run build)   # the UI, once (Node 24); the server build embeds ui/dist if present
cargo build --release                # no Node needed; without ui/dist the UI is a placeholder page
./target/release/tracedb serve       # http://localhost:7070, data in ./.tracedb
```

Point any agent at it; only the endpoint changes:

```bash
export LANGSMITH_TRACING=true
export LANGSMITH_ENDPOINT=http://localhost:7070
export LANGSMITH_API_KEY=local        # any value; tracedb doesn't check it
export LANGSMITH_PROJECT=my-agent
python your_agent.py
```

No agent at hand? Try the example (no API keys, no network) or generate data:

```bash
pip install -r examples/requirements.txt && python examples/research_agent.py
./target/release/tracedb seed --traces 100 --no-delay
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

From a terminal, against a running server:

```bash
./target/release/tracedb query --project my-agent --time 12h --root 'eq(status, "error")'
./target/release/tracedb query --tree-filter 'eq(status, "error")' --json 'eq(status, "ok")'   # JSON lines
```

To check it all on generated data, `tracedb demo` loads an overnight-eval scenario into its own temporary server
and prints a pass/fail table (14 failing roots, 23 quiet failures, top `prompt_version` values, a planted count,
and reads across memory and files); exit code 0 when every check passes:

```bash
./target/release/tracedb demo --seed 7
./target/release/tracedb demo --url http://localhost:7070 --memtable 128MB   # into a running server (pass its --memtable)
```

## Storage

Runs are kept in memory, logged to disk as they arrive, and moved to Parquet data files in the background (no
pause for uploads; bursts get `429` retries instead of refusals). A catalog lists the files. Defaults: all in
`--data` (`<data>/data` and `<data>/meta.sqlite`). M1 data folders open with no manual step.

| Flag | Default | |
|---|---|---|
| `--storage` (`TRACEDB_STORAGE`) | `<data>/data` | a folder or `s3://bucket/prefix` |
| `--metastore` (`TRACEDB_METASTORE`) | `sqlite://<data>/meta.sqlite` | `sqlite://path` or `postgres://user@host/db` |
| `--memtable` | `128MB` | flush the in-memory batch at this size… |
| `--flush-age` | `10m` | …or when its oldest event is this old |
| `--dedup-window` | `10m` | how long an identical retry is recognized as a duplicate |

`--max-memory` and `--max-runs` (M1) are accepted with a warning; they no longer apply.

**Cloud mode** is the same binary with other URLs. S3 settings come from the standard `AWS_*` variables
(`AWS_ENDPOINT` for MinIO and other S3-compatible stores; plain `http://` endpoints are allowed); the Postgres password from `PGPASSWORD` or the URL, and
passwords are never logged. The write-ahead log stays in `--data`.

```bash
export AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… AWS_REGION=us-east-1 AWS_ENDPOINT=http://127.0.0.1:9000
export PGPASSWORD=…
./target/release/tracedb serve --storage s3://tracedb/prod --metastore postgres://tracedb@127.0.0.1/tracedb
```

`ci/cloud.yml` starts MinIO (bucket `tracedb`) and Postgres 17 (database `tracedb`) locally from the same
variables; CI's `cloud` job runs the storage tests, `tracedb demo` and the crash tests against it.

## What M2 does and doesn't do

- **Does**: everything M1 does (LangSmith SDK JSON uploads, runs that arrive in pieces, crash safety, live updates,
  zero outbound connections), plus data on disk or S3 + Postgres, the query panel, field browser, Table view, live
  queries, `tracedb query` and `tracedb demo`.
- **Doesn't yet**: no compaction (data files are never merged, so many small flushes slow reads down); no text
  search; multipart uploads and feedback return `415` / `501` (M3). Speed targets are M2-sized; M3 owns the speed
  work. At 2M runs (5.1 GB raw JSON, release build, laptop): filtered list p50 14–53 ms, trace 6 ms, single run
  30 ms; data on disk 19% of the raw JSON.

## Develop

```bash
cargo test --workspace                       # Rust tests (S3/Postgres storage tests skip unless TRACEDB_TEST_S3_URL / TRACEDB_TEST_PG_URL are set)
(cd ui && npm test)                          # UI component tests
cargo test --release -p tracedb --test crash --test restart --test throughput -- --ignored   # slow checks
/path/to/python scripts/record-sdk-fixtures.py   # re-record SDK contract fixtures (see the script header)
```

For UI work, `tracedb serve --ui-dir ui/dist` serves the UI from disk, so only the UI needs rebuilding. Design docs
live in `specs/`.
