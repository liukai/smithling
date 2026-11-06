# tracedb

A local trace store and viewer for AI agents. Point an unmodified LangSmith-SDK agent (LangChain, LangGraph,
`@traceable`) at it and watch its traces live in your browser. Everything stays in one folder on your machine.

Status: **M0, "see agent trace live"**. Plans and progress: [`specs/000-tracedb-master-plan/roadmap.md`](specs/000-tracedb-master-plan/roadmap.md).

## Run it

```bash
cargo build --release            # builds the embedded UI too (needs Node 24 at build time only)
./target/release/tracedb serve   # http://localhost:7070, data in ./.tracedb
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

Open http://localhost:7070: a live runs list (filter by project and status), click a run for its tree and timing
waterfall, click a step for its full inputs and outputs. Every view is a shareable URL.

## What M0 does and doesn't do

- **Does**: LangSmith SDK JSON uploads (Python SDK 0.14.x and 0.8.x, recorded in `tests/fixtures/`), runs that
  arrive in pieces (latest value wins), agent addresses, crash safety (nothing confirmed is lost on `kill -9`), live
  updates, zero outbound connections.
- **Doesn't yet**: everything is held in memory (default limit 2 GB / 500k runs, `--max-memory`, `--max-runs`;
  past it, new runs get `507`), so disk storage, the query builder, search, stats and charts come in M1–M2;
  multipart uploads and feedback return `415` / `501`.

## Develop

```bash
cargo test --workspace                       # Rust tests
(cd ui && npm test)                          # UI component tests
cargo test --release -p tracedb --test crash --test restart --test throughput -- --ignored   # slow checks
/path/to/python scripts/record-sdk-fixtures.py   # re-record SDK contract fixtures (see the script header)
```

Set `TRACEDB_SKIP_UI=1` to skip rebuilding the UI once `ui/dist` exists. Design docs live in `specs/`.
