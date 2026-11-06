//! The overnight-eval scenario for `tracedb demo` (research M2-R14): exact planted counts, deterministic per seed,
//! sent through `/runs/batch` like an agent.
//!
//! Batch `2026-11-05-a`: 200 root tasks; 14 roots fail; 23 roots finish ok with a `fetch_url` step that failed
//! with `status_code = 429`; `metadata.prompt_version` on 1,840 runs (v6 928, v7 912). Some steps finish
//! 30–120 s late, a few roots 40+ min late. Background runs (another tag, no `prompt_version`) fill the memtable
//! several times, so the scenario's starts are in files when its late updates arrive.

use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::seed::{Rng, SeedError, batch, dotted_ts, filler, iso, merge, outputs};

pub const PROJECT: &str = "overnight-eval";
pub const BATCH_TAG: &str = "batch=2026-11-05-a";
pub const ROOTS: usize = 200;
pub const FAILING_ROOTS: usize = 14;
pub const QUIET_FAILURES: usize = 23;
pub const PROMPT_V6: usize = 928;
pub const PROMPT_V7: usize = 912;
/// Runs with `prompt_version = v7` and `outputs.status_code = 429`.
pub const V7_429: usize = 19;
const LONG_LATE_ROOTS: usize = 5;
const BACKGROUND_TAG: &str = "batch=nightly-smoke";
const BACKGROUND_PAYLOAD: usize = 16 * 1024;
const BACKGROUND_RUNS_PER_TRACE: usize = 10;
const TOOLS: [&str; 4] = ["web_search", "fetch_url", "python_exec", "read_file"];
const RATE_LIMITED: &str = "HTTPError: 429 Too Many Requests";

pub struct Scenario {
    /// Batch runs' starts; steps that finished on time carry their end.
    pub posts: Vec<Value>,
    /// Late ends, in arrival order (how late, the update).
    pub late: Vec<(Duration, Value)>,
    /// Volume under another tag that forces flushes (up to three memtables; `load` sends what it needs); complete when sent.
    pub background: Vec<Value>,
    pub failing: Vec<Uuid>,
    pub quiet: Vec<Uuid>,
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Normal,
    Failing,
    Quiet,
    LongLate,
}

struct Node {
    id: Uuid,
    parent: Option<usize>,
    name: &'static str,
    run_type: &'static str,
    start_us: i64,
    end_us: i64,
    late: Option<Duration>,
    error: Option<&'static str>,
    rate_limited: bool,
}

/// Generates the scenario with batch roots starting at `base_us`, one a minute, and background volume of
/// three memtables of `memtable_bytes`.
pub fn generate(seed: u64, base_us: i64, memtable_bytes: u64) -> Scenario {
    let mut rng = Rng::new(seed);
    let mut order: Vec<usize> = (0..ROOTS).collect();
    shuffle(&mut rng, &mut order);
    let mut roles = [Role::Normal; ROOTS];
    for (k, &i) in order.iter().enumerate() {
        roles[i] = match k {
            k if k < FAILING_ROOTS => Role::Failing,
            k if k < FAILING_ROOTS + QUIET_FAILURES => Role::Quiet,
            k if k < FAILING_ROOTS + QUIET_FAILURES + LONG_LATE_ROOTS => Role::LongLate,
            _ => Role::Normal,
        };
    }
    let traces: Vec<Vec<Node>> = roles
        .iter()
        .enumerate()
        .map(|(i, &role)| batch_trace(&mut rng, role, base_us + i as i64 * 60_000_000))
        .collect();

    // prompt_version: the planted 429 steps first (exactly V7_429 of them v7), then a shuffled pool of steps.
    let mut limited: Vec<(usize, usize)> = Vec::new();
    let mut pool: Vec<(usize, usize)> = Vec::new();
    for (t, nodes) in traces.iter().enumerate() {
        for (n, node) in nodes.iter().enumerate().skip(1) {
            if node.rate_limited { &mut limited } else { &mut pool }.push((t, n));
        }
    }
    shuffle(&mut rng, &mut limited);
    shuffle(&mut rng, &mut pool);
    let v6_limited = limited.len() - V7_429;
    let mut versions = std::collections::HashMap::new();
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

    let (mut posts, mut late) = (Vec::new(), Vec::new());
    for (t, nodes) in traces.iter().enumerate() {
        let trace_id = nodes[0].id;
        let dotted = dotted_orders(nodes);
        for (n, node) in nodes.iter().enumerate() {
            let mut metadata = json!({"task_id": format!("task-{t:03}"), "eval": "overnight"});
            if let Some(v) = versions.get(&(t, n)) {
                metadata["prompt_version"] = json!(v);
            }
            let len = rng.range(200, 1200) as usize;
            let context = filler(&mut rng, len);
            let mut post = json!({
                "id": node.id, "trace_id": trace_id, "dotted_order": dotted[n],
                "session_name": PROJECT, "name": node.name, "run_type": node.run_type,
                "start_time": iso(node.start_us),
                "inputs": {"task": format!("task {t} step {n}"), "context": context},
                "tags": [BATCH_TAG, "eval"],
                "extra": {"metadata": metadata},
            });
            if let Some(p) = node.parent {
                post["parent_run_id"] = json!(nodes[p].id);
            }
            let out = if node.rate_limited {
                json!({"status_code": 429, "result": "rate limited"})
            } else if node.run_type == "agent" || node.run_type == "chain" {
                json!({"result": "done"})
            } else {
                outputs(&mut rng, node.run_type)
            };
            let mut end = json!({"end_time": iso(node.end_us), "outputs": out});
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
    late.sort_by_key(|(d, _)| *d);

    let pick = |role: Role| -> Vec<Uuid> {
        roles
            .iter()
            .zip(&traces)
            .filter(|(r, _)| **r == role)
            .map(|(_, t)| t[0].id)
            .collect()
    };
    let (failing, quiet) = (pick(Role::Failing), pick(Role::Quiet));
    let background = background(&mut rng, base_us, 3 * memtable_bytes);
    Scenario {
        posts,
        late,
        background,
        failing,
        quiet,
    }
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
        rate_limited: false,
    }
}

/// A root with 3–5 research steps of 3–4 leaves each; failing and quiet traces get a rate-limited `fetch_url`.
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
            let (name, run_type) = if rng.chance(0.4) {
                ("llm", "llm")
            } else {
                (rng.pick(&TOOLS), "tool")
            };
            let mut leaf = node(rng, Some(c), name, run_type);
            leaf.start_us = leaf_end + rng.range(1_000, 50_000) as i64;
            leaf.end_us = leaf.start_us + rng.range(20_000, 4_000_000) as i64;
            if rng.chance(0.10) {
                late_by(rng, &mut leaf);
            }
            leaf_end = leaf.end_us;
            nodes.push(leaf);
        }
        cursor = leaf_end;
    }
    if matches!(role, Role::Failing | Role::Quiet) {
        let chain_at: Vec<usize> = (1..nodes.len()).filter(|&i| nodes[i].run_type == "chain").collect();
        let parent = chain_at[rng.range(0, chain_at.len() as u64 - 1) as usize];
        let mut leaf = node(rng, Some(parent), "fetch_url", "tool");
        leaf.start_us = nodes[parent].start_us + rng.range(1_000, 50_000) as i64;
        leaf.end_us = leaf.start_us + rng.range(20_000, 400_000) as i64;
        late_by(rng, &mut leaf);
        leaf.error = Some(RATE_LIMITED);
        leaf.rate_limited = true;
        nodes.push(leaf);
    }
    // Parents end after their last child; roots that finish late (failing, long) end that much later.
    for i in (1..nodes.len()).rev() {
        let p = nodes[i].parent.unwrap();
        nodes[p].end_us = nodes[p].end_us.max(nodes[i].end_us + 1_000);
    }
    match role {
        Role::Failing => {
            late_by(rng, &mut nodes[0]);
            nodes[0].error = Some("AgentError: gave up after fetch_url failed 3 times");
        }
        Role::LongLate => {
            let by = Duration::from_secs(rng.range(40 * 60, 55 * 60));
            nodes[0].end_us += by.as_micros() as i64;
            nodes[0].late = Some(by);
        }
        Role::Normal | Role::Quiet => {}
    }
    nodes
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
    let per_run = tracedb_ingest::store::HEADER_BYTES + BACKGROUND_PAYLOAD as u64 + 64;
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
            let tool = rng.pick(&TOOLS);
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
                step["error"] = json!(RATE_LIMITED);
                step["outputs"] = json!({"status_code": 429});
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

/// Sends the batch's starts, then background (one trace per upload, pausing while a memtable is being written)
/// until those starts are in files after at least two flushes, then the late updates, which land in the
/// memtable. Stopping early keeps the memtable from filling (and flushing the updates) before the checks.
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
            "this tracedb already holds {existing} runs tagged {BATCH_TAG}; use an empty data folder"
        )));
    }
    for chunk in s.posts.chunks(100) {
        batch(&http, url, json!({"post": chunk})).await?;
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
        traces: ROOTS + (sent - s.posts.len()) / BACKGROUND_RUNS_PER_TRACE,
        posts: sent,
        patches: patches.len(),
        flushes: h["flushes"].as_u64().unwrap_or(0) - start_flushes,
        flushed_seq: h["flushed_seq"].as_u64().unwrap_or(0),
        in_files,
    })
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
