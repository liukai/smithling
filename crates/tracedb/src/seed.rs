//! `tracedb seed`: realistic research-agent traces, sent through `/runs/batch` like an agent would.
//!
//! Shapes (spec Story 4): depth 3–8, 20–300 steps per trace, ~5% errors, ~10% of steps finishing
//! 30–120 s late, payloads 1–50 KB. Deterministic for a given `--seed`.

use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

pub const PROJECT: &str = "seed";

/// SplitMix64: tiny, deterministic, good enough for test data.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo + 1)
    }
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next_u64() as f64 / u64::MAX as f64) < p
    }
    pub(crate) fn uuid(&mut self) -> Uuid {
        uuid::Builder::from_random_bytes(((self.next_u64() as u128) << 64 | self.next_u64() as u128).to_le_bytes())
            .into_uuid()
    }
    pub(crate) fn pick<'a>(&mut self, xs: &'a [&'a str]) -> &'a str {
        xs[self.range(0, xs.len() as u64 - 1) as usize]
    }
}

/// One step of a generated trace, as LangSmith items.
#[derive(Clone, Debug)]
pub struct GenStep {
    pub depth: u16,
    pub run_type: String,
    pub error: bool,
    pub payload_bytes: usize,
    /// Sent in the first batch. Complete steps carry their end here.
    pub post: Value,
    /// For late-finishing steps: the update with the end, and how late it arrives.
    pub late_patch: Option<(Duration, Value)>,
}

#[derive(Clone, Debug)]
pub struct GenTrace {
    pub trace_id: Uuid,
    pub depth: u16,
    pub steps: Vec<GenStep>,
}

const TOOLS: [&str; 4] = ["web_search", "fetch_url", "python_exec", "read_file"];
const ERRORS: [&str; 3] = [
    "HTTPError: 429 Too Many Requests",
    "TimeoutError: tool took longer than 30 s",
    "ValueError: bad tool arguments",
];

struct Node {
    id: Uuid,
    parent: Option<usize>,
    depth: u16,
    name: String,
    run_type: &'static str,
    start_us: i64,
    dur_us: i64,
}

/// Generates `n` traces starting around `base_us` (µs since epoch).
pub fn generate(seed: u64, n: usize, base_us: i64) -> Vec<GenTrace> {
    let mut rng = Rng::new(seed);
    (0..n)
        .map(|i| gen_trace(&mut rng, base_us + i as i64 * 7_000_000))
        .collect()
}

fn gen_trace(rng: &mut Rng, start_us: i64) -> GenTrace {
    let max_depth = rng.range(3, 8) as u16;
    let total = rng.range(20, 300) as usize;
    let mut nodes = vec![Node {
        id: rng.uuid(),
        parent: None,
        depth: 0,
        name: "research_task".into(),
        run_type: "agent",
        start_us,
        dur_us: 0,
    }];
    // A spine that reaches the full depth, then random branches within it.
    for d in 1..max_depth {
        let parent = nodes.len() - 1;
        nodes.push(new_node(rng, parent, d, d + 1 == max_depth));
    }
    while nodes.len() < total {
        let parent = loop {
            let p = rng.range(0, nodes.len() as u64 - 1) as usize;
            if nodes[p].depth + 1 < max_depth && nodes[p].run_type != "llm" && nodes[p].run_type != "tool" {
                break p;
            }
        };
        let depth = nodes[parent].depth + 1;
        let leaf = depth + 1 == max_depth || rng.chance(0.6);
        nodes.push(new_node(rng, parent, depth, leaf));
    }
    // Children start after their parent, one after another.
    let mut next_child = vec![0i64; nodes.len()];
    for i in 1..nodes.len() {
        let p = nodes[i].parent.unwrap();
        let offset = next_child[p] + rng.range(1_000, 200_000) as i64;
        nodes[i].start_us = nodes[p].start_us + offset;
        nodes[i].dur_us = rng.range(20_000, 4_000_000) as i64;
        next_child[p] = offset + nodes[i].dur_us / 4;
    }
    for i in (1..nodes.len()).rev() {
        let p = nodes[i].parent.unwrap();
        let end = nodes[i].start_us + nodes[i].dur_us;
        nodes[p].dur_us = nodes[p].dur_us.max(end - nodes[p].start_us + 1_000);
    }

    let dotted: Vec<String> = {
        let mut v: Vec<String> = Vec::with_capacity(nodes.len());
        for n in &nodes {
            let seg = format!("{}{}", dotted_ts(n.start_us), n.id);
            v.push(match n.parent {
                Some(p) => format!("{}.{seg}", v[p]),
                None => seg,
            });
        }
        v
    };
    let trace_id = nodes[0].id;
    let steps = nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let error = n.parent.is_some() && rng.chance(0.05);
            let late = n.parent.is_some() && rng.chance(0.10);
            let late_by = Duration::from_secs(rng.range(30, 120));
            let payload_bytes = rng.range(1024, 50 * 1024) as usize;
            let end_us = n.start_us + n.dur_us + if late { late_by.as_micros() as i64 } else { 0 };
            let mut post = json!({
                "id": n.id, "trace_id": trace_id, "dotted_order": dotted[i],
                "session_name": PROJECT, "name": n.name, "run_type": n.run_type,
                "start_time": iso(n.start_us),
                "inputs": {"query": format!("step {i} of task {trace_id}"), "context": filler(rng, payload_bytes)},
                "tags": ["seed", format!("batch=seed-{}", trace_id.as_u128() % 3)],
                "extra": {"metadata": {"task_id": format!("t-{}", trace_id.as_u128() % 1000), "prompt_version": rng.pick(&["v6", "v7"])}},
            });
            if let Some(p) = n.parent {
                post["parent_run_id"] = json!(nodes[p].id);
            }
            let mut end = json!({"end_time": iso(end_us), "outputs": outputs(rng, n.run_type)});
            if error {
                end["error"] = json!(if n.name == "fetch_url" { ERRORS[0] } else { rng.pick(&ERRORS) });
            }
            let late_patch = if late {
                let mut patch = json!({"id": n.id, "trace_id": trace_id, "dotted_order": dotted[i]});
                merge(&mut patch, end);
                Some((late_by, patch))
            } else {
                merge(&mut post, end);
                None
            };
            GenStep { depth: n.depth, run_type: n.run_type.into(), error, payload_bytes, post, late_patch }
        })
        .collect();
    GenTrace {
        trace_id,
        depth: max_depth - 1,
        steps,
    }
}

fn new_node(rng: &mut Rng, parent: usize, depth: u16, leaf: bool) -> Node {
    let (name, run_type) = if leaf {
        if rng.chance(0.4) {
            ("llm".to_string(), "llm")
        } else {
            (rng.pick(&TOOLS).to_string(), "tool")
        }
    } else {
        (
            rng.pick(&["plan", "summarize", "research_step", "critique"])
                .to_string(),
            "chain",
        )
    };
    Node {
        id: rng.uuid(),
        parent: Some(parent),
        depth,
        name,
        run_type,
        start_us: 0,
        dur_us: 0,
    }
}

pub(crate) fn outputs(rng: &mut Rng, run_type: &str) -> Value {
    if run_type == "llm" {
        let (i, o) = (rng.range(200, 8000), rng.range(50, 1500));
        json!({"content": "generated text", "usage_metadata": {"input_tokens": i, "output_tokens": o, "total_tokens": i + o}})
    } else {
        json!({"status_code": 200, "result": "ok"})
    }
}

pub(crate) fn filler(rng: &mut Rng, bytes: usize) -> String {
    const WORDS: [&str; 8] = [
        "agent", "trace", "tool", "search", "model", "token", "result", "context",
    ];
    let mut s = String::with_capacity(bytes + 8);
    while s.len() < bytes {
        s.push_str(rng.pick(&WORDS));
        s.push(' ');
    }
    s.truncate(bytes);
    s
}

pub(crate) fn merge(into: &mut Value, from: Value) {
    if let (Some(a), Value::Object(b)) = (into.as_object_mut(), from) {
        a.extend(b);
    }
}

pub(crate) fn iso(us: i64) -> String {
    tracedb_model::Ts(us).to_string()
}

pub(crate) fn dotted_ts(us: i64) -> String {
    let d: String = iso(us).chars().filter(char::is_ascii_digit).collect();
    format!("{}T{}Z", &d[..8], &d[8..20])
}

#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    #[error("tracedb is not reachable at {0}: {1}")]
    Unreachable(String, String),
    #[error("upload failed: {0}")]
    Upload(String),
    #[error("{0}")]
    Refused(String),
}

/// Sends the traces: all starts (and complete steps) first, then late updates after their delay
/// (or right away with `no_delay`). Batches stay within the 100 items `/info` advertises.
pub async fn send(url: &str, traces: &[GenTrace], no_delay: bool) -> Result<(usize, usize), SeedError> {
    let http = reqwest::Client::new();
    http.get(format!("{url}/info"))
        .send()
        .await
        .map_err(|e| SeedError::Unreachable(url.into(), e.to_string()))?;
    let posts: Vec<&Value> = traces.iter().flat_map(|t| t.steps.iter().map(|s| &s.post)).collect();
    let mut late: Vec<(Duration, &Value)> = traces
        .iter()
        .flat_map(|t| {
            t.steps
                .iter()
                .filter_map(|s| s.late_patch.as_ref().map(|(d, p)| (*d, p)))
        })
        .collect();
    late.sort_by_key(|(d, _)| *d);
    for chunk in posts.chunks(100) {
        batch(&http, url, json!({"post": chunk})).await?;
    }
    let started = tokio::time::Instant::now();
    for group in late.chunk_by(|a, b| no_delay || a.0 == b.0) {
        if !no_delay {
            tokio::time::sleep_until(started + group[0].0).await;
        }
        for chunk in group.chunks(100) {
            let patch: Vec<&Value> = chunk.iter().map(|(_, p)| *p).collect();
            batch(&http, url, json!({"patch": patch})).await?;
        }
    }
    Ok((posts.len(), late.len()))
}

/// One `/runs/batch` upload; "busy" (429) is retried shortly, as the SDK does.
pub(crate) async fn batch(http: &reqwest::Client, url: &str, body: Value) -> Result<(), SeedError> {
    for _ in 0..600 {
        let r = http
            .post(format!("{url}/runs/batch"))
            .json(&body)
            .send()
            .await
            .map_err(|e| SeedError::Upload(e.to_string()))?;
        let status = r.status();
        if status == 429 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        let v: Value = r.json().await.unwrap_or(Value::Null);
        if status != 202 || v["rejected"].as_array().is_some_and(|a| !a.is_empty()) {
            return Err(SeedError::Upload(format!("{status}: {v}")));
        }
        return Ok(());
    }
    Err(SeedError::Upload("tracedb stayed busy for 60 s".into()))
}
