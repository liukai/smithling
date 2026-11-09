# Smithling

**A pocket-sized SmithDB. Trace your agents locally.**

SmithDB is the storage engine behind LangSmith, built for the cloud. Smithling is a little clone of it that lives on
your laptop: same design, one folder, and your agent's data never leaves the machine.

| I want to… | Start here |
|---|---|
| see what my agent is doing | [Use it today](#use-it-today) |
| learn how a trace database works | [Learn from it](#learn-from-it) |
| know where this is going | [What's next](#whats-next) |

## Credit: SmithDB

Smithling is named after **SmithDB** and exists because of it. The LangChain team described SmithDB, the storage
engine they built for LangSmith, in [Introducing SmithDB](https://www.langchain.com/blog/introducing-smithdb). That
post laid out the problem (seven kinds of questions people ask about agent traces, listed
[below](#the-seven-questions-from-the-smithdb-post)) and a design for answering them fast. Smithling is my attempt to
learn that design by rebuilding it, small enough to run on a laptop: hence the name, a *little* Smith.

**From the post**: the seven workloads; the architecture (Rust and Apache DataFusion, a log-structured write path
with memtables, immutable columnar segments and compaction, object storage plus a small metadata catalog, ingestion,
query and compaction as separate services); late materialization of large payloads; an inverted text index built for
object storage; progressive answers; time-tiered compaction.

**Smithling's own**: all of the code (no SmithDB code is used, and none is public as far as I know), the local-first
packaging as one program and one folder, the APIs, query syntax and UI, and the places where it differs from SmithDB,
each written down with a reason in the [design notes](specs/000-smithling-master-plan/design-notes.md) and
[decision log](specs/000-smithling-master-plan/decisions.md).

Thank you to the LangChain team for writing the design up in the open. Smithling is an independent project, not
affiliated with or endorsed by LangChain; LangSmith is LangChain's trademark, and Smithling speaks its SDK's upload
protocol so existing agents work unchanged.

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
| Think in trees | tasks that finished ok although a step inside them failed (23 of them) |
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
| **M4** Compaction | merges small files into larger ones, one row per run, so reads touch fewer files | in progress |
| **M5** Disaggregated trace database | ingest, readers and compaction as separate services that scale out | spec and plan drafted (step 1: read workers) |
| **M6** Agents debugging agents | import Claude Code sessions, agent tools over MCP | planned |

Also deferred: conversation threads, export, feedback from the UI and "what stands out" (moved out of M4,
[D-038](specs/000-smithling-master-plan/decisions.md#d-038)); a full 2M-run benchmark and the idle-memory target
(re-evaluated before M5's plan); delete and retention. Status and plans:
[roadmap](specs/000-smithling-master-plan/roadmap.md) · what changed: [CHANGELOG.md](CHANGELOG.md).

---

## The seven questions from the SmithDB post

The SmithDB post names seven questions a trace store has to answer quickly. Smithling's demo data is built so you can
ask each of them, and see the answer, on your laptop.

**Run the demo**:

```bash
./target/release/smithling serve                               # terminal 1
./target/release/smithling demo --url http://localhost:7070    # terminal 2: loads the data, checks 10 numbers
```

Then open **http://localhost:7070/scenarios**: one card per question, one click each.

The screenshots below come from that demo data: an overnight evaluation of a research agent in two batches, `a`
before a bug fix and `b` after. Orange boxes mark what to look at.

### 1. Show me this one ✅

Open the slowest failed task of batch a, then any step's full input. Smithling shows the trace tree, a timing
waterfall, and each step's inputs and outputs.

![A failed research_task: five research_step chains with their tool calls; in the last one, fetch_url fails after 31 s](docs/images/q1-trace-waterfall.png)

*A failed task, 24 steps in 3m 56s. The red `fetch_url` in the last step is where it broke; the long `llm` bar
beside it is where the time went.*

![A successful trace with one llm step open: latency, tokens, model, tags, inputs, outputs and metadata](docs/images/q1-step-detail.png)

*Click any step to read it whole: 2.44 s, 4,750 tokens on `gpt-4o-mini`, its inputs, outputs and metadata.*

### 2. Narrow it down ✅

Failed steps in batch a that took longer than 2 s. Filter on any field, by clicking or as text.

![Three filter chips (batch a, status = error, latency > 2) and the matching runs: fetch_url steps and the failed tasks above them](docs/images/q2-narrow-down.png)

*Three clicks, three conditions: the `fetch_url` steps that hit a 429 or a 30 s timeout, and the failed tasks above
them (56 runs; the list stops at the page limit of 50).*

### 3. Find the text ✅

Runs mentioning "429 Too Many Requests". Smithling searches inputs, outputs and errors, and highlights each match.

![Text search for "429 Too Many Requests": 31 runs, all fetch_url, the match highlighted in each error](docs/images/q3-text-search.png)

*31 hits, every one a `fetch_url` failing with `HTTPError: 429`. The agent is being rate limited.*

### 4. Look inside the JSON ✅

Runs on `metadata.prompt_version = v7` whose output had `status_code = 429` (19 in batch a). Filter on fields inside
the metadata, input and output JSON. The step opened under question 5 is one of them.

### 5. Think in trees ✅

Tasks that finished ok although a step inside them failed (23 of them). Put conditions on a trace's root or on any run
in it.

![Root-only research_task runs with status ok, filtered on any run in the trace having status error: 23 runs, all ok](docs/images/q5-quiet-failures-list.png)

*Two rows of conditions: the root says `status = ok`, and some run in its trace says `status = error`. 23 tasks, every
one marked ✓ ok. Open one:*

![A research_task marked ok, with one failed fetch_url inside it open: HTTPError 429, outputs.status_code 429, metadata.prompt_version v7](docs/images/q5-quiet-failure.png)

*The task says ✓ ok, but one `fetch_url` inside it failed with a 429. Its `outputs.status_code` and
`metadata.prompt_version` are the fields question 4 filters on.*

### 6. Rebuild the conversation ⏳

All turns of one chat, in order. Not yet: conversation threads are in the backlog.

### 7. Add it up ✅

Per step name: how often it fails, how slow it is, what it costs. Smithling shows stats, time series and distributions.

![Batch a grouped by step name: error rate over time per step, and a table with runs, error rate, p95 latency and cost](docs/images/q7-stats-by-step.png)

*`fetch_url` fails 14% of the time with a p95 of 31 s; the model calls cost $4.12 for the batch.*

### Bonus: did the fix work?

Compare batch a with batch b, after the fix, side by side.

![Batch a vs batch b: p95 latency of llm over time, and a table with both batches side by side](docs/images/compare-batches.png)

*`fetch_url` errors drop from 14% to 2% and its p95 from 31 s to 9 s; failed tasks go from 7% to none.*

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

---

## Personal notes: why I'm doing this experiment

I've worked on low-level storage engines like RocksDB, and when I read the SmithDB post I recognized the skeleton:
an LSM with a log, memtables and immutable files. But a columnar database bends each of those pieces toward a
different job. The two worlds share a lot of interesting ideas, and each has problems of its own. I wanted to see
exactly where they meet and where they part.

I've also always been amazed by how fast columnar systems are. With tools like Facebook's Scuba, you ask a question
over a huge pile of events and the aggregation comes back almost before you've finished typing. I wanted to know how
that works from the inside. AI agents gave me the perfect excuse: they produce deep, tree-shaped traces of spans that
arrive in pieces, which is a fresh and slightly awkward shape for a database. That felt like the right problem to
learn on.

I started with something small: one program, one folder, on my laptop. Each milestone fixed one thing that was
broken:

- ✅ **M1, see an agent trace live**: you can't fix what you can't see. Point an unchanged agent at Smithling, watch
  every step appear, and lose nothing on a crash.
- ✅ **M2, durable storage and a query builder**: memory forgets; disk remembers. Keep millions of runs on disk and find
  any of them by clicking.
- ✅ **M3, search, aggregates and speed**: find the needle, then count the haystack. Search the text, add it up, compare
  batches.
- 🔜 **M4, compaction**: every flush leaves a crumb. Merge small files into big ones and store each run whole. Planned
  and scoped.
- 🔜 **M5, a disaggregated trace database**: reads and writes stop fighting over one box. Ingest, read workers and
  compaction as separate services over shared storage. Planned and scoped.
- 💭 **M6, agents debugging agents**: let an agent answer "why did batch a fail?" and cite the runs. Not started.

Then, with a slightly inflated ego, I started asking: what if this had to scale? What would have to change? That
question became M5. It's specced, not built, and it has already made me think harder about the problem than anything
else in this project: storage and compute on separate machines, each scaling on its own.

My favorite rule in system design has always been **think big, but start incrementally**. Smithling started light,
and building it raised the interesting questions on its own, one step at a time: why the memtable shouldn't be
sorted, why files should be grouped by trace instead of by time, why bloom filters can't find words. I think that's
the best way to learn anything.

If you know RocksDB, [ROCKSDB_COMPARISON.md](ROCKSDB_COMPARISON.md) is the long version of this note.
