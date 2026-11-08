# tracedb primer: why this matters and what done looks like

> Background reading for [spec.md](spec.md). Start here if you're new to agent tracing. The spec holds the testable requirements; this file explains the problem and walks through the finished product.

## Part 1: why this matters

*If you have debugged an AI agent from its traces before, skip to Part 2.*

### 1. What a trace is

An AI agent doesn't answer in one step. It calls a model, the model asks for a tool, the tool's output goes back to
the model, a sub-agent gets started, and so on. Each step is a **run** (also called a span). The runs nest, and the
whole tree for one task is a **trace**.

Here is one task from the research agent used throughout this spec:

```
research_task  (agent, 41 min, ok)                         <- root run
├── plan            (llm,   2.1 s, 1,200 tokens)
├── web_search      (tool,  0.8 s)
├── fetch_url       (tool, 31.0 s, ERROR: 429 Too Many Requests)   <- the real problem
├── fetch_url       (tool,  1.2 s)
├── summarize       (sub-agent, 3 min)
│   ├── llm         (llm,   4.0 s, 6,800 tokens)
│   └── python_exec (tool,  0.3 s)
└── final_answer    (llm,   3.2 s)
```

Every run records what went **in** (the prompt or tool arguments), what came **out**, how long it took, how many
tokens it used, what it cost, and whether it failed. A real trace has 20 to 300 of these runs.

### 2. Why ordinary logs and tracing tools fall short

Agent traces have four properties that together break the usual tools:

| Property | What it means | What goes wrong today |
|---|---|---|
| **Deep and wide** | Hundreds of nested runs per task | A flat log of 250 lines shows *what* happened but not *who called whom*. You end up rebuilding the tree in your head. |
| **Arrives in pieces** | A run's start is sent when it begins; its end can come 40 minutes later, and long runs send updates in between | Most stores treat a record as written once. The run stays "running" forever, or the end shows up as a second, disconnected record. |
| **Heavy payloads, tiny filters** | Prompts and tool outputs are 1 KB to MB each; the fields you filter on (status, latency, tags) are a few bytes | Filtering means reading every payload, so "show me the errors" takes minutes instead of milliseconds. |
| **Asked odd questions** | "Which tasks had *any* failing step?", "search outputs for this phrase", "rebuild this conversation" | General tracing tools (built for microservices) don't know about prompts, tokens, threads or late updates. |

So people fall back to print statements, grepping JSON files, or sending everything to a cloud service. The last one
means prompts, and sometimes customer data or secrets, leave the laptop.

### 3. The seven questions a trace store must answer quickly

These are the seven workloads named in LangChain's [SmithDB post](https://www.langchain.com/blog/introducing-smithdb), in plain words:

| # | Question | Example | Why it's hard |
|---|---|---|---|
| 1 | "Show me this one" | Open trace X; open run Y's full input | Must be instant even with millions of runs stored |
| 2 | "Narrow it down" | Errors in the last 12 h, tagged `batch=a`, slower than 2 s | Many filters combined, results newest first |
| 3 | "Find the text" | Outputs containing `"429 Too Many Requests"` | Searches the big payloads, not the small fields |
| 4 | "Look inside the JSON" | `metadata.prompt_version = "v7"`, tool output `status_code = 429` | The structure is user-defined; nothing is known in advance |
| 5 | "Think in trees" | Root runs that succeeded but where *any* step underneath failed | One trace's answer depends on all of its runs |
| 6 | "Rebuild the conversation" | All six turns of one chat, in order | The turns are separate traces joined only by a thread id |
| 7 | "Add it up" | Error rate, p95 latency, tokens and cost per tool | Must aggregate millions of runs in seconds |

The post also names run rules, bulk export and experiments, and it mentions deletes, upgrades and TTL as storage
mechanics. It does **not** specify API shapes, wire formats, query syntax, UI layout, auth or multi-tenancy. Those are
ours to design.

### 4. Why local first

The developer in our scenario does not want prompts or customer data to leave the machine. They also don't want to
run Docker, a database server, or a cloud account just to look at last night's runs. A single program that stores
everything in one folder covers both. Because the storage layout is designed so it can later sit on cloud storage,
moving to the cloud becomes a configuration change rather than a rewrite.

---

## Part 2: the end state, what it looks like when done

This section walks through the driving scenario, "the overnight eval run went sideways", as the finished product.
The developer builds a local research agent (LangGraph; tools `web_search`, `fetch_url`, `python_exec`, and a
`summarize` sub-agent) and runs a 200-task eval batch tagged `batch=2026-11-05-a`, with metadata
`{model, prompt_version, task_id}`. Some tasks are multi-turn and share a `thread_id`.
Each step names the user story that delivers it. The screens are illustrative mockups, not UI designs.

### Evening: capture, without changing code (Story 1)

```
$ tracedb serve
tracedb listening on http://127.0.0.1:7070   data: ./.tracedb

$ export LANGSMITH_ENDPOINT=http://localhost:7070
$ python run_eval.py --batch 2026-11-05-a        # unchanged agent code
```

The browser at `localhost:7070` shows runs appearing live within a second. One trace opens, its tree looks right, and
the developer goes home. Some root runs show `running` for 40+ minutes and flip to `ok` or `error` when they finish.

### Morning: triage (Stories 2–5)

**"What failed?"** Filter: last 12 h, `batch=2026-11-05-a`, root runs only, `status=error` → **14 runs**.
**"Did anything fail quietly?"** Root runs that succeeded but have *any* errored step → **23 more**.

```
Runs  [ last 12h ] [ tag: batch=2026-11-05-a ] [ root only ] [ any step errored ]     23 results
──────────────────────────────────────────────────────────────────────────────────────────────
 status  name           start     latency  tokens   cost    task_id
 ok      research_task  02:14:07  41m 3s   48,210   $0.11   t-117
 ok      research_task  02:09:51  12m 40s  22,980   $0.05   t-093
 ...
```

**"What happened in this one?"** Opening a 250-run trace shows the tree and a timing waterfall at once. Clicking the
failing `fetch_url` step loads its full 40 KB input and output in a side panel.

**"Is this a pattern?"** Full-text search `"429 Too Many Requests"` → **31 runs, all `fetch_url`**.
**"Which ones exactly?"** Add `outputs.status_code = 429` and `metadata.prompt_version = "v7"`.

**"How bad is it?"** Group by tool name:

```
 name         runs   error rate   p50      p95     tokens    cost
 fetch_url      300  14.0 %       1.1 s    31.0 s  —         —
 web_search   1,210   0.4 %       0.7 s     1.9 s  —         —
 summarize      402   1.2 %       2.8 min   6 min  2.1 M     $3.02
 ...                                                          batch total: $4.12
```

### Follow-through (Stories 6–9)

- **"What did the user see?"** One multi-turn task opens as a **thread**: six traces shown as a chat conversation.
- **"Never again."** The 37 bad root runs (14 + 23) are exported as a JSONL file and used as a regression eval set.
- **Cleanup** *(backlog, not v0)*. One trace captured a real API key. The developer deletes it and it disappears from every view at once.
  The project keeps 14 days of data; older data is removed automatically.

### Afternoon: rerun and compare (Story 5)

After a retry/backoff fix, `batch=2026-11-06-b` runs and the two batches' aggregates appear side by side.
`fetch_url` error rate goes from 14% to 2% (p95 31 s → 9 s).

### Variant: zero instrumentation (Story 10)

```
$ tracedb import claude-code ~/.claude/projects/<repo>
```

The same triage steps now work on the developer's Claude Code sessions: "which session ran the failing `cargo test`,
and what did the agent do before that?"

### The done-check, in one line

A single demo command replays this whole scenario on generated data with planted failures, asserts the expected counts
(14, 23, 31, 37 …); a separate bench command (`tracedb bench --check`) checks the response-time targets. If every
assertion passes on both, v0 is done.
