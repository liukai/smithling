# Smithling

**A pocket-sized SmithDB. Trace your agents locally.**

SmithDB is the storage engine behind LangSmith, built for the cloud. Smithling is a little clone of it that lives on
your laptop: same design, one folder, and your agent's data never leaves the machine.

| I want to… | Start here |
|---|---|
| see what my agent is doing | [Use it today](#use-it-today) |
| learn how a trace database works | [Learn from it](#learn-from-it) |
| know where this is going | [What's next](#whats-next) |

> An independent open-source project modeled on the design LangChain described for SmithDB. Not affiliated with or
> endorsed by LangChain; LangSmith is LangChain's trademark. Smithling speaks its SDK's upload protocol.

---

## Use it today

### What you get

- **No code changes.** Point an agent built on the LangSmith SDK (LangChain, LangGraph, `@traceable`) at Smithling
  with one environment variable.
- **Watch it live.** Each step shows up in the browser as it happens. Open a run to see its tree, a timing
  waterfall, and every step's inputs and outputs.
- **Find things.** Build filters by clicking, search the text of inputs, outputs and errors, and ask about whole
  traces ("tasks that looked fine but had a failed step").
- **Add it up.** Error rate, p95 latency and cost per tool, over time, or batch against batch.
- **Stays local.** One folder, no account, no cloud, no outbound connections.

### A five-minute tour

You need Rust 1.99+ and Node 24.

```bash
git clone https://github.com/liukai/smithling && cd smithling
(cd ui && npm ci && npm run build)          # the browser UI, once
cargo build --release
./target/release/smithling serve            # http://localhost:7070
```

In a second terminal, load an example:

```bash
./target/release/smithling demo --url http://localhost:7070
```

That sends an overnight evaluation of a research agent (two batches, before and after a bug fix) into your server,
then checks ten numbers it planted. You should see `all 10 checks passed`.

Now open **http://localhost:7070/scenarios** and click any question. Things to try:

| Question | What you'll see |
|---|---|
| Find the text | 31 runs mentioning "429 Too Many Requests", each with the match highlighted |
| Think in trees | tasks that succeeded although a step inside them failed |
| Add it up | `fetch_url` fails 14% of the time in batch a, with p95 latency of 31 s |
| Did the fix work? | batch b after the fix, side by side: 2% errors and 9 s |

Click any run to open its trace.

### Trace your own agent

```bash
export LANGSMITH_TRACING=true
export LANGSMITH_ENDPOINT=http://localhost:7070
export LANGSMITH_API_KEY=local          # any value; Smithling doesn't check it
export LANGSMITH_PROJECT=my-agent
python your_agent.py
```

No agent handy? `python examples/research_agent.py` runs one with no API keys and no network
(`pip install -r examples/requirements.txt` first).

**More**: every view, command and flag, cloud mode and current limits are in [USAGE.md](USAGE.md).

---

## Learn from it

I built Smithling to learn how a modern trace database works, by rebuilding SmithDB's published design step by step.
The docs are written so you can learn along: they explain the problem first, then each technique, with diagrams and
real numbers.

| Read | You'll learn |
|---|---|
| [STARTER_GUIDE.md](STARTER_GUIDE.md) | the problem with agent traces, the architecture, and how each milestone built on the last |
| [SECRET_OF_COLUMNAR_DB.md](SECRET_OF_COLUMNAR_DB.md) | why columnar storage is fast, with examples you can run |
| [ROCKSDB_COMPARISON.md](ROCKSDB_COMPARISON.md) | the storage engine part by part, if you know RocksDB |
| milestone guides: [M1](specs/001-m1-see-agent-trace-live/starter-guide.md), [M2](specs/002-m2-durable-storage-query-builder/starter-guide.md), [M3](specs/003-m3-search-aggregates-speed/starter-guide.md) | the code, one request traced through it, with links to the lines |
| the [decision log](specs/000-smithling-master-plan/decisions.md) | why things are the way they are, and what changed |

Two ways to see the code evolve:

```bash
git checkout m1      # the first version: everything in memory, a seventh of today's code
git checkout main    # back to today
cargo run --release -p smithling-tools --example columnar_tour   # rows vs columns, on your laptop
```

---

## What's next

| Milestone | What it adds | Status |
|---|---|---|
| **M1** See an agent trace live | uploads, crash-safe log, live browser view | ✅ built |
| **M2** Durable storage + query builder | data on disk (Parquet), filters by clicking, trace conditions | ✅ built |
| **M3** Search, aggregates, speed | text search, stats and charts, batch comparison | ✅ built |
| **M4** Follow-through | compaction, conversation threads, export, feedback from the UI | planned |
| **M5** Disaggregated trace database | ingest, readers and compaction as separate services that scale out | spec drafted |
| **M6** Agents debugging agents | import Claude Code sessions, agent tools over MCP | planned |

Also deferred: a full 2M-run benchmark and the idle-memory target (both before M4), delete and retention. Status and
plans: [roadmap](specs/000-smithling-master-plan/roadmap.md) · what changed: [CHANGELOG.md](CHANGELOG.md).

---

## Contributing

```bash
cargo test --workspace && (cd ui && npm test)
```

Read [AGENTS.md](AGENTS.md) before a change (checks to run, and how the changelog and decision log are kept). Build
and test details are in [USAGE.md](USAGE.md#building-and-developing).

## License and security

[MIT](LICENSE). Smithling is a local, single-user tool with no login: read [SECURITY.md](SECURITY.md) before exposing
it beyond `127.0.0.1`.
