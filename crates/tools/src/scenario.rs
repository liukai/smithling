//! The overnight-eval scenario for `smithling demo` (research M2-R14, M3-R11): exact planted counts, deterministic per
//! seed, sent through `/runs/batch` like an agent.
//!
//! Batch `2026-11-05-a` (ends now − 30 h): 200 root tasks; 14 roots fail; 23 roots finish ok with a failed
//! `fetch_url` step; `metadata.prompt_version` on 1,840 runs (v6 928, v7 912). 300 `fetch_url` runs, 42 failed
//! (31 × 429, 11 × `ReadTimeout`), p95 31 s; llm runs cost $4.12 with 12 that have no price. Batch `2026-11-06-b`
//! (ends now − 6 h, after the retry fix): 300 `fetch_url`, 6 timeouts, p95 9 s, $4.07. Some steps finish 30–120 s
//! late, a few roots 40+ min late. Background runs (another tag, no `prompt_version`) fill the memtable several
//! times, so the scenario's starts are in files when its late updates arrive.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::seed::{Rng, SeedError, batch, dotted_ts, filler, iso, merge, outputs};

pub const PROJECT: &str = "overnight-eval";
pub const BATCH_TAG: &str = "batch=2026-11-05-a";
pub const BATCH_B_TAG: &str = "batch=2026-11-06-b";
pub const ROOTS: usize = 200;
pub const FAILING_ROOTS: usize = 14;
pub const QUIET_FAILURES: usize = 23;
pub const PROMPT_V6: usize = 928;
pub const PROMPT_V7: usize = 912;
/// Runs with `prompt_version = v7` and `outputs.status_code = 429`.
pub const V7_429: usize = 19;
/// In `outputs.body` and `error` of the 429 steps only.
pub const PHRASE: &str = "429 Too Many Requests";
/// Runs holding the phrase: every 429 `fetch_url` step (all in batch a).
pub const PHRASE_RUNS: usize = FAILING_ROOTS + QUIET_429;
pub const FETCH_URL_RUNS: usize = 300;
pub const BATCH_A_FETCH_ERRORS: usize = 42;
pub const BATCH_A_FETCH_P95_MS: u64 = 31_000;
pub const BATCH_A_COST_PICO: i64 = 4_120_000_000_000;
pub const BATCH_A_NO_PRICE_RUNS: usize = 12;
pub const BATCH_B_FETCH_ERRORS: usize = 6;
pub const BATCH_B_FETCH_P95_MS: u64 = 9_000;
pub const BATCH_B_COST_PICO: i64 = 4_070_000_000_000;
pub const FEEDBACK_KEY: &str = "correctness";
const HOUR_US: i64 = 3_600_000_000;
const LONG_LATE_ROOTS: usize = 5;
const RETRIED_FAILING_ROOTS: usize = 5;
const QUIET_429: usize = 17;
const BACKGROUND_TAG: &str = "batch=nightly-smoke";
const BACKGROUND_PAYLOAD: usize = 16 * 1024;
const BACKGROUND_RUNS_PER_TRACE: usize = 10;
const TOOLS: [&str; 3] = ["web_search", "python_exec", "read_file"];
const BACKGROUND_TOOLS: [&str; 4] = ["web_search", "fetch_url", "python_exec", "read_file"];
const RATE_LIMITED: &str = "HTTPError: 429 Too Many Requests";
const TIMED_OUT: &str = "ReadTimeout: fetch_url timed out after 30 s";
const UNAVAILABLE: &str = "HTTPError: 503 Service Unavailable";

pub struct Scenario {
    /// Both batches' starts; steps that finished on time carry their end.
    pub posts: Vec<Value>,
    /// Late ends, in arrival order (how late, the update).
    pub late: Vec<(Duration, Value)>,
    /// Volume under another tag that forces flushes (up to three memtables; `load` sends what it needs); complete when sent.
    pub background: Vec<Value>,
    /// Batch a's failing and quiet-failure roots.
    pub failing: Vec<Uuid>,
    pub quiet: Vec<Uuid>,
    /// `POST /feedback` bodies: `correctness = 0` on two failing roots.
    pub feedback: Vec<Feedback>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Feedback {
    pub body: Value,
    /// Sent before its run is uploaded.
    pub before_run: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Normal,
    /// Fails after a 429; `retried` roots also have a `ReadTimeout` retry step.
    Failing {
        retried: bool,
    },
    Quiet429,
    QuietTimeout,
    LongLate,
}

#[derive(Clone, Copy, PartialEq)]
enum Planted {
    Limited,
    TimedOut,
}

#[derive(Clone, Copy)]
enum Reported {
    No,
    Total(f64),
    Parts(f64, f64),
}

/// `runs` llm calls of `model`, each with these token counts and reported cost.
struct ModelPlan {
    model: &'static str,
    runs: usize,
    input: u64,
    output: u64,
    reported: Reported,
}

const fn plan(model: &'static str, runs: usize, input: u64, output: u64, reported: Reported) -> ModelPlan {
    ModelPlan {
        model,
        runs,
        input,
        output,
        reported,
    }
}

/// gpt-4o-mini 3.2M in / 0.6M out → $0.84; gpt-4o 0.8M / 0.1M → $3.00; claude-haiku reports $0.28 (its tokens would
/// give $0.52); local-llama has no price.
const BATCH_A_MODELS: [ModelPlan; 4] = [
    plan("gpt-4o-mini", 800, 4000, 750, Reported::No),
    plan("gpt-4o", 200, 4000, 500, Reported::No),
    plan("claude-haiku", 100, 4000, 500, Reported::Total(0.0028)),
    plan("local-llama", 12, 4000, 500, Reported::No),
];
/// $0.84 + $3.00 + claude-haiku reporting `input_cost + output_cost` $0.23.
const BATCH_B_MODELS: [ModelPlan; 3] = [
    plan("gpt-4o-mini", 800, 4000, 750, Reported::No),
    plan("gpt-4o", 200, 4000, 500, Reported::No),
    plan("claude-haiku", 100, 4000, 500, Reported::Parts(0.0015, 0.0008)),
];

struct Batch {
    tag: &'static str,
    ends_before_now: i64,
    /// (role, roots), the rest `Normal`.
    roles: &'static [(Role, usize)],
    models: &'static [ModelPlan],
    /// Of the ok `fetch_url` runs, how many take exactly 9 s (batch b's p95).
    slow_fetch: usize,
    prompt_versions: bool,
}

const BATCH_A: Batch = Batch {
    tag: BATCH_TAG,
    ends_before_now: 30 * HOUR_US,
    roles: &[
        (Role::Failing { retried: true }, RETRIED_FAILING_ROOTS),
        (Role::Failing { retried: false }, FAILING_ROOTS - RETRIED_FAILING_ROOTS),
        (Role::Quiet429, QUIET_429),
        (Role::QuietTimeout, QUIET_FAILURES - QUIET_429),
        (Role::LongLate, LONG_LATE_ROOTS),
    ],
    models: &BATCH_A_MODELS,
    slow_fetch: 0,
    prompt_versions: true,
};

const BATCH_B: Batch = Batch {
    tag: BATCH_B_TAG,
    ends_before_now: 6 * HOUR_US,
    roles: &[(Role::QuietTimeout, BATCH_B_FETCH_ERRORS)],
    models: &BATCH_B_MODELS,
    slow_fetch: 14,
    prompt_versions: false,
};

struct Node {
    id: Uuid,
    parent: Option<usize>,
    name: &'static str,
    run_type: &'static str,
    start_us: i64,
    end_us: i64,
    late: Option<Duration>,
    error: Option<&'static str>,
    planted: Option<Planted>,
    model: Option<&'static ModelPlan>,
}

/// Generates the scenario relative to `now_us` (batch a ends 30 h before it, batch b 6 h before), with background
/// volume of three memtables of `memtable_bytes`.
pub fn generate(seed: u64, now_us: i64, memtable_bytes: u64) -> Scenario {
    let mut rng = Rng::new(seed);
    let (mut posts, mut late) = (Vec::new(), Vec::new());
    let a = gen_batch(&mut rng, &BATCH_A, now_us, &mut posts, &mut late);
    gen_batch(&mut rng, &BATCH_B, now_us, &mut posts, &mut late);
    late.sort_by_key(|(d, _)| *d);
    let background = background(&mut rng, a.start_us, 3 * memtable_bytes);
    let feedback = a
        .failing
        .iter()
        .take(2)
        .enumerate()
        .map(|(k, &(id, end_us))| Feedback {
            body: json!({
                "id": rng.uuid(), "run_id": id, "trace_id": id, "key": FEEDBACK_KEY, "score": 0.0,
                "comment": "gave up on 429", "feedback_source": {"type": "api", "metadata": {}},
                "created_at": iso(end_us),
            }),
            before_run: k == 0,
        })
        .collect();
    Scenario {
        posts,
        late,
        background,
        failing: a.failing.iter().map(|(id, _)| *id).collect(),
        quiet: a.quiet,
        feedback,
    }
}

struct Generated {
    start_us: i64,
    /// Failing roots with their end.
    failing: Vec<(Uuid, i64)>,
    quiet: Vec<Uuid>,
}

fn gen_batch(
    rng: &mut Rng,
    b: &Batch,
    now_us: i64,
    posts: &mut Vec<Value>,
    late: &mut Vec<(Duration, Value)>,
) -> Generated {
    let mut order: Vec<usize> = (0..ROOTS).collect();
    shuffle(rng, &mut order);
    let mut roles = [Role::Normal; ROOTS];
    let mut assigned = order.iter();
    for &(role, n) in b.roles {
        for &i in assigned.by_ref().take(n) {
            roles[i] = role;
        }
    }
    let mut traces: Vec<Vec<Node>> = roles
        .iter()
        .enumerate()
        .map(|(i, &role)| batch_trace(rng, role, i as i64 * 60_000_000))
        .collect();
    assign_leaves(rng, b, &mut traces);
    for (nodes, &role) in traces.iter_mut().zip(&roles) {
        finish_trace(rng, role, nodes);
    }
    let last_end = traces
        .iter()
        .flatten()
        .map(|n| n.end_us)
        .max()
        .expect("the batch has runs");
    let shift = now_us - b.ends_before_now - last_end;
    for n in traces.iter_mut().flatten() {
        n.start_us += shift;
        n.end_us += shift;
    }
    let versions = if b.prompt_versions {
        prompt_versions(rng, &traces)
    } else {
        HashMap::new()
    };

    for (t, nodes) in traces.iter().enumerate() {
        let trace_id = nodes[0].id;
        let dotted = dotted_orders(nodes);
        for (n, node) in nodes.iter().enumerate() {
            let mut metadata = json!({"task_id": format!("task-{t:03}"), "eval": "overnight"});
            if let Some(v) = versions.get(&(t, n)) {
                metadata["prompt_version"] = json!(v);
            }
            if let Some(m) = node.model {
                metadata["ls_model_name"] = json!(m.model);
            }
            let len = rng.range(200, 1200) as usize;
            let context = filler(rng, len);
            let mut post = json!({
                "id": node.id, "trace_id": trace_id, "dotted_order": dotted[n],
                "session_name": PROJECT, "name": node.name, "run_type": node.run_type,
                "start_time": iso(node.start_us),
                "inputs": {"task": format!("task {t} step {n}"), "context": context},
                "tags": [b.tag, "eval"],
                "extra": {"metadata": metadata},
            });
            if let Some(p) = node.parent {
                post["parent_run_id"] = json!(nodes[p].id);
            }
            let mut end = json!({"end_time": iso(node.end_us)});
            match (node.planted, node.model) {
                (Some(Planted::Limited), _) => {
                    end["outputs"] = json!({"status_code": 429, "body": format!("{PHRASE}: rate limit exceeded")});
                }
                (Some(Planted::TimedOut), _) => {}
                (None, Some(m)) => end["outputs"] = llm_outputs(m),
                (None, None) if node.run_type == "agent" || node.run_type == "chain" => {
                    end["outputs"] = json!({"result": "done"});
                }
                (None, None) => end["outputs"] = outputs(rng, node.run_type),
            }
            if let Some(e) = node.error {
                end["error"] = json!(e);
            }
            match node.late {
                Some(by) => {
                    let mut patch = json!({"id": node.id, "trace_id": trace_id, "dotted_order": dotted[n]});
                    merge(&mut patch, end);
                    late.push((by, patch));
                }
                None => merge(&mut post, end),
            }
            posts.push(post);
        }
    }

    let pick = |want: fn(Role) -> bool| -> Vec<&Vec<Node>> {
        roles
            .iter()
            .zip(&traces)
            .filter(|(r, _)| want(**r))
            .map(|(_, t)| t)
            .collect()
    };
    Generated {
        start_us: traces[0][0].start_us,
        failing: pick(|r| matches!(r, Role::Failing { .. }))
            .iter()
            .map(|t| (t[0].id, t[0].end_us))
            .collect(),
        quiet: pick(|r| matches!(r, Role::Quiet429 | Role::QuietTimeout))
            .iter()
            .map(|t| t[0].id)
            .collect(),
    }
}

fn llm_outputs(m: &ModelPlan) -> Value {
    let mut usage = json!({"input_tokens": m.input, "output_tokens": m.output, "total_tokens": m.input + m.output});
    match m.reported {
        Reported::No => {}
        Reported::Total(usd) => usage["total_cost"] = json!(usd),
        Reported::Parts(i, o) => {
            usage["input_cost"] = json!(i);
            usage["output_cost"] = json!(o);
        }
    }
    json!({"content": "generated text", "usage_metadata": usage})
}

/// prompt_version: the planted 429 steps first (exactly V7_429 of them v7), then a shuffled pool of steps.
fn prompt_versions(rng: &mut Rng, traces: &[Vec<Node>]) -> HashMap<(usize, usize), &'static str> {
    let mut limited: Vec<(usize, usize)> = Vec::new();
    let mut pool: Vec<(usize, usize)> = Vec::new();
    for (t, nodes) in traces.iter().enumerate() {
        for (n, node) in nodes.iter().enumerate().skip(1) {
            if node.planted == Some(Planted::Limited) {
                &mut limited
            } else {
                &mut pool
            }
            .push((t, n));
        }
    }
    shuffle(rng, &mut limited);
    shuffle(rng, &mut pool);
    let v6_limited = limited.len() - V7_429;
    let mut versions = HashMap::new();
    for (k, at) in limited.iter().enumerate() {
        versions.insert(*at, if k < V7_429 { "v7" } else { "v6" });
    }
    for (k, at) in pool
        .iter()
        .take(PROMPT_V7 - V7_429 + PROMPT_V6 - v6_limited)
        .enumerate()
    {
        versions.insert(*at, if k < PROMPT_V7 - V7_429 { "v7" } else { "v6" });
    }
    versions
}

fn shuffle<T>(rng: &mut Rng, xs: &mut [T]) {
    for i in (1..xs.len()).rev() {
        xs.swap(i, rng.range(0, i as u64) as usize);
    }
}

fn node(rng: &mut Rng, parent: Option<usize>, name: &'static str, run_type: &'static str) -> Node {
    Node {
        id: rng.uuid(),
        parent,
        name,
        run_type,
        start_us: 0,
        end_us: 0,
        late: None,
        error: None,
        planted: None,
        model: None,
    }
}

/// A root with 3–5 research steps of 3–4 leaves each (kinds assigned later), plus the role's failed `fetch_url`
/// steps (timings set later).
fn batch_trace(rng: &mut Rng, role: Role, start_us: i64) -> Vec<Node> {
    let mut nodes = vec![node(rng, None, "research_task", "agent")];
    nodes[0].start_us = start_us;
    let chains = rng.range(3, 5) as usize;
    let mut cursor = start_us;
    for _ in 0..chains {
        let c = nodes.len();
        nodes.push(node(rng, Some(0), "research_step", "chain"));
        cursor += rng.range(1_000, 200_000) as i64;
        nodes[c].start_us = cursor;
        let mut leaf_end = cursor;
        for _ in 0..rng.range(3, 4) {
            let mut leaf = node(rng, Some(c), "", "tool");
            leaf.start_us = leaf_end + rng.range(1_000, 50_000) as i64;
            leaf.end_us = leaf.start_us + rng.range(200_000, 4_000_000) as i64;
            leaf_end = leaf.end_us;
            nodes.push(leaf);
        }
        cursor = leaf_end;
    }
    let planted: &[Planted] = match role {
        Role::Failing { retried: true } => &[Planted::Limited, Planted::TimedOut],
        Role::Failing { retried: false } | Role::Quiet429 => &[Planted::Limited],
        Role::QuietTimeout => &[Planted::TimedOut],
        Role::Normal | Role::LongLate => &[],
    };
    let chain_at: Vec<usize> = (1..nodes.len()).filter(|&i| nodes[i].run_type == "chain").collect();
    for &p in planted {
        let parent = chain_at[rng.range(0, chain_at.len() as u64 - 1) as usize];
        let mut leaf = node(rng, Some(parent), "fetch_url", "tool");
        leaf.start_us = nodes[parent].start_us + rng.range(1_000, 50_000) as i64;
        leaf.error = Some(match p {
            Planted::Limited => RATE_LIMITED,
            Planted::TimedOut => TIMED_OUT,
        });
        leaf.planted = Some(p);
        nodes.push(leaf);
    }
    nodes
}

/// Fixed counts across the batch: llm calls per model, then ok `fetch_url` (never late), then other tools; the
/// planted failures' latencies (429s: 22 at 31 s, the rest 35–95 s; timeouts 30 s), delivered late.
fn assign_leaves(rng: &mut Rng, b: &Batch, traces: &mut [Vec<Node>]) {
    let mut leaves: Vec<(usize, usize)> = Vec::new();
    let mut limited: Vec<(usize, usize)> = Vec::new();
    for (t, nodes) in traces.iter().enumerate() {
        for (n, node) in nodes.iter().enumerate() {
            match node.planted {
                Some(Planted::Limited) => limited.push((t, n)),
                Some(Planted::TimedOut) => {}
                None if node.name.is_empty() => leaves.push((t, n)),
                None => {}
            }
        }
    }
    shuffle(rng, &mut leaves);
    let planted = traces.iter().flatten().filter(|n| n.planted.is_some()).count();
    let mut rest = leaves.into_iter();
    for m in b.models {
        for (t, n) in rest.by_ref().take(m.runs) {
            let leaf = &mut traces[t][n];
            (leaf.name, leaf.run_type, leaf.model) = ("llm", "llm", Some(m));
            maybe_late(rng, leaf);
        }
    }
    for (k, (t, n)) in rest.by_ref().take(FETCH_URL_RUNS - planted).enumerate() {
        let leaf = &mut traces[t][n];
        leaf.name = "fetch_url";
        if k < b.slow_fetch {
            leaf.end_us = leaf.start_us + BATCH_B_FETCH_P95_MS as i64 * 1000;
        }
    }
    for (t, n) in rest {
        let leaf = &mut traces[t][n];
        leaf.name = rng.pick(&TOOLS);
        maybe_late(rng, leaf);
    }

    shuffle(rng, &mut limited);
    for (k, &(t, n)) in limited.iter().enumerate() {
        let ms = if k < 22 { 31_000 } else { rng.range(35_000, 95_000) };
        planted_end(&mut traces[t][n], ms);
    }
    for leaf in traces.iter_mut().flatten() {
        if leaf.planted == Some(Planted::TimedOut) {
            planted_end(leaf, 30_000);
        }
    }
}

fn planted_end(leaf: &mut Node, ms: u64) {
    leaf.end_us = leaf.start_us + ms as i64 * 1000;
    leaf.late = Some(Duration::from_millis(ms));
}

fn maybe_late(rng: &mut Rng, leaf: &mut Node) {
    if rng.chance(0.10) {
        late_by(rng, leaf);
    }
}

/// Parents end after their last child; roots that finish late (failing, long) end that much later.
fn finish_trace(rng: &mut Rng, role: Role, nodes: &mut [Node]) {
    for i in (1..nodes.len()).rev() {
        let p = nodes[i].parent.unwrap();
        nodes[p].end_us = nodes[p].end_us.max(nodes[i].end_us + 1_000);
    }
    match role {
        Role::Failing { .. } => {
            late_by(rng, &mut nodes[0]);
            nodes[0].error = Some("AgentError: gave up after fetch_url failed 3 times");
        }
        Role::LongLate => {
            let by = Duration::from_secs(rng.range(40 * 60, 55 * 60));
            nodes[0].end_us += by.as_micros() as i64;
            nodes[0].late = Some(by);
        }
        Role::Normal | Role::Quiet429 | Role::QuietTimeout => {}
    }
}

fn late_by(rng: &mut Rng, n: &mut Node) {
    let by = Duration::from_secs(rng.range(30, 120));
    n.end_us += by.as_micros() as i64;
    n.late = Some(by);
}

fn dotted_orders(nodes: &[Node]) -> Vec<String> {
    let mut v: Vec<String> = Vec::with_capacity(nodes.len());
    for n in nodes {
        let seg = format!("{}{}", dotted_ts(n.start_us), n.id);
        v.push(match n.parent {
            Some(p) => format!("{}.{seg}", v[p]),
            None => seg,
        });
    }
    v
}

/// Smoke-test traces (a root and 9 steps, 16 KB inputs each) until `bytes` of memtable estimate are reached.
fn background(rng: &mut Rng, base_us: i64, bytes: u64) -> Vec<Value> {
    let per_run = smithling_ingest::store::HEADER_BYTES + BACKGROUND_PAYLOAD as u64 + 64;
    let runs = bytes.div_ceil(per_run) as usize;
    let mut out = Vec::with_capacity(runs + 10);
    let mut traces = 0;
    while out.len() < runs {
        let start_us = base_us + traces as i64 * 20_000_000;
        let root = rng.uuid();
        let root_dotted = format!("{}{root}", dotted_ts(start_us));
        let mut failed = false;
        let mut end_us = start_us;
        let mut steps = Vec::new();
        for _ in 1..BACKGROUND_RUNS_PER_TRACE {
            let id = rng.uuid();
            let s = end_us + rng.range(1_000, 50_000) as i64;
            end_us = s + rng.range(20_000, 2_000_000) as i64;
            let tool = rng.pick(&BACKGROUND_TOOLS);
            let mut step = json!({
                "id": id, "trace_id": root, "parent_run_id": root,
                "dotted_order": format!("{root_dotted}.{}{id}", dotted_ts(s)),
                "session_name": PROJECT, "name": tool, "run_type": "tool",
                "start_time": iso(s), "end_time": iso(end_us),
                "inputs": {"context": filler(rng, BACKGROUND_PAYLOAD)},
                "outputs": {"status_code": 200, "result": "ok"},
                "tags": [BACKGROUND_TAG], "extra": {"metadata": {"suite": "smoke"}},
            });
            if rng.chance(0.05) {
                failed = true;
                step["error"] = json!(UNAVAILABLE);
                step["outputs"] = json!({"status_code": 503});
            }
            steps.push(step);
        }
        let mut r = json!({
            "id": root, "trace_id": root, "dotted_order": root_dotted,
            "session_name": PROJECT, "name": "smoke_test", "run_type": "chain",
            "start_time": iso(start_us), "end_time": iso(end_us + 1_000),
            "inputs": {"context": filler(rng, BACKGROUND_PAYLOAD)}, "outputs": {"result": "done"},
            "tags": [BACKGROUND_TAG], "extra": {"metadata": {"suite": "smoke"}},
        });
        if failed && rng.chance(0.5) {
            r["error"] = json!("AssertionError: smoke test failed");
        }
        out.push(r);
        out.extend(steps);
        traces += 1;
    }
    out
}

#[derive(Clone, Copy, Debug)]
pub struct Loaded {
    pub traces: usize,
    pub posts: usize,
    pub patches: usize,
    /// Flushes during the load.
    pub flushes: u64,
    pub flushed_seq: u64,
    /// The batch's starts reached files (after ≥ 2 flushes) before the late updates were sent.
    pub in_files: bool,
}

/// Sends the feedback marked `before_run`, both batches' starts, the other feedback, then background (one trace
/// per upload, pausing while a memtable is being written) until those starts are in files after at least two
/// flushes, then the late updates, which land in the memtable. Stopping early keeps the memtable from filling
/// (and flushing the updates) before the checks.
pub async fn load(url: &str, s: &Scenario) -> Result<Loaded, SeedError> {
    let http = reqwest::Client::new();
    let start = health(&http, url)
        .await
        .map_err(|e| SeedError::Unreachable(url.into(), e))?;
    let start_flushes = start["flushes"].as_u64().unwrap_or(0);
    let probe =
        json!({"project": PROJECT, "filter": format!(r#"has(tags, "{BATCH_TAG}")"#), "limit": 1, "count": true});
    let existing = http
        .post(format!("{url}/api/query"))
        .json(&probe)
        .send()
        .await
        .map_err(|e| SeedError::Upload(e.to_string()))?
        .json::<Value>()
        .await
        .map_err(|e| SeedError::Upload(e.to_string()))?["stats"]["matched"]
        .as_u64()
        .unwrap_or(0);
    if existing > 0 {
        return Err(SeedError::Refused(format!(
            "this smithling already holds {existing} runs tagged {BATCH_TAG}; use an empty data folder"
        )));
    }
    for f in s.feedback.iter().filter(|f| f.before_run) {
        feedback(&http, url, &f.body).await?;
    }
    for chunk in s.posts.chunks(100) {
        batch(&http, url, json!({"post": chunk})).await?;
    }
    for f in s.feedback.iter().filter(|f| !f.before_run) {
        feedback(&http, url, &f.body).await?;
    }
    let last = s.posts.last().expect("the batch has runs")["id"]
        .as_str()
        .unwrap_or_default();
    let target = get(&http, &format!("{url}/api/runs/{last}"))
        .await
        .map_err(SeedError::Upload)?["first_seq"]
        .as_u64()
        .ok_or_else(|| SeedError::Upload(format!("run {last} has no first_seq")))?;
    let mut background = s.background.chunks(BACKGROUND_RUNS_PER_TRACE);
    let mut sent = s.posts.len();
    let mut exhausted: Option<tokio::time::Instant> = None;
    let in_files = loop {
        let h = health(&http, url).await.map_err(SeedError::Upload)?;
        if h["flushed_seq"].as_u64().unwrap_or(0) >= target && h["flushes"].as_u64().unwrap_or(0) >= start_flushes + 2 {
            break true;
        }
        if exhausted.is_some_and(|t| t.elapsed() > Duration::from_secs(30)) {
            break false;
        }
        if h["frozen"].as_bool() == Some(true) || exhausted.is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        }
        match background.next() {
            Some(chunk) => {
                batch(&http, url, json!({"post": chunk})).await?;
                sent += chunk.len();
            }
            None => exhausted = Some(tokio::time::Instant::now()),
        }
    };
    let patches: Vec<&Value> = s.late.iter().map(|(_, p)| p).collect();
    for chunk in patches.chunks(100) {
        batch(&http, url, json!({"patch": chunk})).await?;
    }
    let h = health(&http, url).await.map_err(SeedError::Upload)?;
    Ok(Loaded {
        traces: 2 * ROOTS + (sent - s.posts.len()) / BACKGROUND_RUNS_PER_TRACE,
        posts: sent,
        patches: patches.len(),
        flushes: h["flushes"].as_u64().unwrap_or(0) - start_flushes,
        flushed_seq: h["flushed_seq"].as_u64().unwrap_or(0),
        in_files,
    })
}

async fn feedback(http: &reqwest::Client, url: &str, body: &Value) -> Result<(), SeedError> {
    let r = http
        .post(format!("{url}/feedback"))
        .json(body)
        .send()
        .await
        .map_err(|e| SeedError::Upload(format!("POST /feedback: {e}")))?;
    let status = r.status();
    if !status.is_success() {
        let text = r.text().await.unwrap_or_default();
        return Err(SeedError::Upload(format!("POST /feedback: {status}: {text}")));
    }
    Ok(())
}

/// The server's memtable limit from `/healthz` (`None` from a server that doesn't report one).
pub async fn memtable_limit(url: &str) -> Result<Option<u64>, SeedError> {
    let h = health(&reqwest::Client::new(), url)
        .await
        .map_err(|e| SeedError::Unreachable(url.into(), e))?;
    Ok(h["memtable_limit_bytes"].as_u64())
}

async fn health(http: &reqwest::Client, url: &str) -> Result<Value, String> {
    get(http, &format!("{url}/healthz")).await
}

async fn get(http: &reqwest::Client, url: &str) -> Result<Value, String> {
    let r = http.get(url).send().await.map_err(|e| e.to_string())?;
    let status = r.status();
    let v: Value = r.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("{url}: {status}: {v}"));
    }
    Ok(v)
}
