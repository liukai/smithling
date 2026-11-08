//! `tracedb bench` (contracts/cli.md, design §4.10): loads the reference volume through `/runs/batch` from 8
//! clients with a query loop alongside, then times each query type n ≥ 100 times with varied values and windows
//! against the SC-03 targets, scaled by a calibration run on this machine (`budget = target × r`).
//!
//! The generator is M2's (2.4 KB of model-ish text per run, scattered trace ids). One run in ten finishes late:
//! its update follows ~160k runs later without a project, after its start was flushed (the harder case for
//! reads: files overlap in time, and the update's file is under no project).

use std::collections::VecDeque;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use axum::body::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracedb_ingest::Limits;
use tracedb_model::Ts;
use uuid::Uuid;

use crate::{AppState, ServeConfig, app};

pub const PROJECT: &str = "bench";
pub const STEPS: u64 = 9;
const CLIENTS: u64 = 8;
const BATCH: u64 = 500;
/// Batches a client sends between a late run's start and its update (~160k runs in all: past a memtable).
const PATCH_LAG: usize = 40;
const MARKER: &str = "bench-load.json";
const WARMUP: usize = 10;
const REFERENCE: &[u8] = include_bytes!("../../../tests/fixtures/bench-reference.parquet");
/// [`reference_workload`] on the reference laptop (M5 Max, release build): 80.0–80.4 ms over 6 runs.
pub const REFERENCE_MS: f64 = 80.0;
const LOAD_TARGET: f64 = 20_000.0;
const DISK_TARGET_PCT: f64 = 50.0;
const IDLE_TARGET_MB: f64 = 200.0;
/// SC-05 is waived for M3 until M4 compaction (roadmap 2026-11-07, D-028): reported, never failed.
const IDLE_WAIVED: &str = "M4";

#[derive(clap::Parser, Clone, Debug)]
#[command(name = "tracedb bench")]
pub struct BenchArgs {
    /// Runs to load (default 2,000,000; 200,000 with `--smoke`).
    #[arg(long, conflicts_with = "smoke")]
    pub runs: Option<u64>,
    /// Load into this folder and keep it; a folder from an earlier bench is reused without loading.
    #[arg(long)]
    pub data: Option<PathBuf>,
    /// Also write the table as JSON here.
    #[arg(long)]
    pub json: Option<PathBuf>,
    /// Exit 1 when a row misses its budget.
    #[arg(long)]
    pub check: bool,
    /// 200,000 runs: the PR gate.
    #[arg(long)]
    pub smoke: bool,
    /// Timed requests per query type (trace and run lookups take one more).
    #[arg(long, default_value_t = 100, hide = true)]
    pub samples: usize,
    /// Seconds of idle footprint to take the median of.
    #[arg(long, default_value_t = 30, hide = true)]
    pub idle_secs: u64,
    #[arg(long, hide = true)]
    pub flush_age_secs: Option<u64>,
    #[arg(long, hide = true)]
    pub dedup_window_secs: Option<u64>,
    /// Print the calibration and exit.
    #[arg(long, hide = true)]
    pub calibrate_only: bool,
}

impl BenchArgs {
    fn runs(&self) -> u64 {
        self.runs.unwrap_or(if self.smoke { 200_000 } else { 2_000_000 })
    }
}

// ---- generator ----

static WORDS: std::sync::LazyLock<Vec<&str>> = std::sync::LazyLock::new(|| {
    "the model returned a plan with three steps fetch page parse table retry after rate limit user asked for \
     summary of quarterly revenue and costs tool call failed succeeded context window tokens"
        .split_whitespace()
        .collect()
});

pub fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub fn trace_of(n: u64) -> u64 {
    n / (STEPS + 1) * (STEPS + 1)
}

pub fn trace_id(n: u64) -> Uuid {
    let t = trace_of(n);
    Uuid::from_u64_pair(mix(t) | 1 << 63, t)
}

pub fn run_id(n: u64) -> Uuid {
    if n == trace_of(n) {
        trace_id(n)
    } else {
        Uuid::from_u128(n as u128 + 1)
    }
}

fn stamp(t: Ts) -> String {
    t.to_string().replace(['-', ':', '.'], "")
}

/// Model-ish text from a 32-word vocabulary, different per run.
pub fn text(n: u64, salt: u64, words: usize) -> String {
    let mut s = String::with_capacity(words * 7);
    let mut h = mix(n ^ (salt << 40));
    for i in 0..words {
        if i % 16 == 0 {
            h = mix(h);
        }
        s.push_str(WORDS[((h >> ((i % 16) * 4)) & 31) as usize]);
        s.push(' ');
    }
    s
}

/// About one run in 64k carries a rare phrase, for text search.
pub fn planted(n: u64) -> bool {
    n % 64_513 == 11
}

/// One step in ten finishes late.
pub fn late(n: u64) -> bool {
    n != trace_of(n) && n % 10 == 7
}

fn start_of(n: u64, base_us: i64) -> Ts {
    Ts(base_us + n as i64 * 10_000)
}

/// Run `n` in the LangSmith batch shape: its start, and for a late finisher the update that follows it (no project).
pub fn item(n: u64, base_us: i64) -> (Value, Option<Value>) {
    let trace = trace_of(n);
    let (id, tid) = (run_id(n), trace_id(n));
    let start = start_of(n, base_us);
    let root_seg = format!("{}{tid}", stamp(start_of(trace, base_us)));
    let is_root = n == trace;
    let dotted = if is_root {
        root_seg
    } else {
        format!("{root_seg}.{}{id}", stamp(start))
    };
    let name = if is_root {
        "eval_task"
    } else {
        ["fetch_url", "llm", "plan"][(n % 3) as usize]
    };
    let mut post = json!({
        "id": id, "trace_id": tid, "dotted_order": dotted, "session_name": PROJECT,
        "name": name, "run_type": if is_root { "chain" } else { "tool" },
        "start_time": start.to_string(),
        "tags": [format!("batch={}", n % 4)],
        "extra": {"metadata": {
            "prompt_version": if n.is_multiple_of(2) { "v6" } else { "v7" },
            "user_id": format!("user-{}", n % 5_000),
            "attempt": n % 3,
        }},
        "inputs": {"messages": [{"role": "user", "content": text(n, 1, 150)}]},
    });
    if !is_root {
        post["parent_run_id"] = json!(tid);
    }
    let outputs = if planted(n) {
        json!({"status_code": 429, "content": text(n, 2, 180), "detail": "upstream said 429 Too Many Requests"})
    } else {
        json!({"status_code": if n.is_multiple_of(50) { 429 } else { 200 }, "content": text(n, 2, 180)})
    };
    let mut end = json!({"end_time": Ts(start.0 + 2_000_000).to_string(), "outputs": outputs});
    if n.is_multiple_of(97) {
        end["error"] = json!("HTTPError: 429");
    }
    if late(n) {
        let mut patch = json!({"id": id, "trace_id": tid, "dotted_order": post["dotted_order"]});
        merge(&mut patch, end);
        return (post, Some(patch));
    }
    merge(&mut post, end);
    (post, None)
}

fn merge(into: &mut Value, from: Value) {
    if let (Some(a), Value::Object(b)) = (into.as_object_mut(), from) {
        a.extend(b);
    }
}

/// SplitMix over a counter: the bench's varied values and windows.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(1);
        mix(self.0)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

// ---- load ----

/// Written into `--data` after a load, so a later bench can reuse the folder.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct LoadInfo {
    runs: u64,
    base_us: i64,
    raw_bytes: u64,
    events: u64,
    late_updates: u64,
    secs: f64,
    queries_alongside: u64,
    /// The calibration of the machine that loaded, which the load rate is judged by.
    #[serde(default)]
    r: f64,
}

#[derive(Default)]
struct Sent {
    raw_bytes: u64,
    events: u64,
    late_updates: u64,
}

fn batch_body(posts: &[Vec<u8>], patches: &[Vec<u8>]) -> Bytes {
    let size: usize = posts.iter().chain(patches).map(|p| p.len() + 1).sum();
    let mut b = Vec::with_capacity(size + 32);
    for (key, items) in [(&br#"{"post":["#[..], posts), (&br#"],"patch":["#[..], patches)] {
        b.extend_from_slice(key);
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                b.push(b',');
            }
            b.extend_from_slice(item);
        }
    }
    b.extend_from_slice(b"]}");
    Bytes::from(b)
}

async fn send(http: &reqwest::Client, url: &str, body: Bytes) -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let r = http
            .post(format!("{url}/runs/batch"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.clone())
            .send()
            .await
            .context("POST /runs/batch")?;
        match r.status().as_u16() {
            202 => {
                let v: Value = r.json().await.context("POST /runs/batch")?;
                if let Some(first) = v["rejected"].as_array().and_then(|a| a.first()) {
                    bail!("POST /runs/batch rejected an item: {first}");
                }
                return Ok(());
            }
            429 if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(20)).await,
            status => bail!("POST /runs/batch: {status}: {}", r.text().await.unwrap_or_default()),
        }
    }
}

/// Client `k` sends every `CLIENTS`-th batch from `k`, so the clients move through the hours together; each
/// late finisher's update rides with the client's batch `PATCH_LAG` later. Raw bytes count each run once, as
/// its merged JSON.
async fn client(http: reqwest::Client, url: String, k: u64, runs: u64, base_us: i64) -> anyhow::Result<Sent> {
    let mut s = Sent::default();
    let mut pending: VecDeque<Vec<Vec<u8>>> = VecDeque::new();
    let mut from = k * BATCH;
    while from < runs {
        let (mut posts, mut patches) = (Vec::with_capacity(BATCH as usize), Vec::new());
        for n in from..(from + BATCH).min(runs) {
            let (post, patch) = item(n, base_us);
            let bytes = serde_json::to_vec(&post)?;
            match patch {
                Some(p) => {
                    let mut merged = post;
                    merge(&mut merged, p.clone());
                    s.raw_bytes += serde_json::to_vec(&merged)?.len() as u64;
                    patches.push(serde_json::to_vec(&p)?);
                }
                None => s.raw_bytes += bytes.len() as u64,
            }
            posts.push(bytes);
        }
        pending.push_back(patches);
        let due = if pending.len() > PATCH_LAG {
            pending.pop_front().unwrap_or_default()
        } else {
            Vec::new()
        };
        s.events += (posts.len() + due.len()) as u64;
        s.late_updates += due.len() as u64;
        send(&http, &url, batch_body(&posts, &due)).await?;
        from += CLIENTS * BATCH;
    }
    let rest: Vec<Vec<u8>> = pending.into_iter().flatten().collect();
    for chunk in rest.chunks(BATCH as usize) {
        s.events += chunk.len() as u64;
        s.late_updates += chunk.len() as u64;
        send(&http, &url, batch_body(&[], chunk)).await?;
    }
    Ok(s)
}

/// Queries while the clients upload, so the load rate is measured under reads (SC-04).
async fn query_loop(http: reqwest::Client, url: String, d: Data, done: Arc<AtomicBool>) -> u64 {
    let mut rng = Rng(0xA1);
    let mut n = 0;
    while !done.load(Ordering::Relaxed) {
        let req = match n % 3 {
            0 => list(&mut rng, &d).req,
            1 => Req::Get(format!("/api/traces/{}", trace_id(rng.below(d.runs)))),
            _ => text_search(&mut rng, &d).req,
        };
        let r = match &req {
            Req::Get(p) => http.get(format!("{url}{p}")),
            Req::Post(p, b) => http.post(format!("{url}{p}")).json(b),
            Req::Stream(_) => unreachable!("not in the load loop"),
        };
        if let Ok(r) = r.send().await {
            let _ = r.bytes().await;
        }
        n += 1;
    }
    n
}

async fn load(url: &str, runs: u64, base_us: i64, err: &mut impl Write) -> anyhow::Result<LoadInfo> {
    let http = reqwest::Client::new();
    let done = Arc::new(AtomicBool::new(false));
    let d = Data { runs, base_us };
    let alongside = tokio::spawn(query_loop(http.clone(), url.to_string(), d, done.clone()));
    let _ = writeln!(
        err,
        "tracedb bench: loading {} runs from {CLIENTS} clients…",
        thousands(runs)
    );
    let started = Instant::now();
    let clients: Vec<_> = (0..CLIENTS)
        .map(|k| tokio::spawn(client(http.clone(), url.to_string(), k, runs, base_us)))
        .collect();
    let mut info = LoadInfo {
        runs,
        base_us,
        ..LoadInfo::default()
    };
    let mut failed = None;
    for c in clients {
        match c.await.map_err(anyhow::Error::from).and_then(|r| r) {
            Ok(s) => {
                info.raw_bytes += s.raw_bytes;
                info.events += s.events;
                info.late_updates += s.late_updates;
            }
            Err(e) => failed = failed.or(Some(e)),
        }
    }
    info.secs = started.elapsed().as_secs_f64();
    done.store(true, Ordering::Relaxed);
    info.queries_alongside = alongside.await.unwrap_or(0);
    match failed {
        Some(e) => Err(e),
        None => Ok(info),
    }
}

// ---- queries ----

#[derive(Clone, Copy)]
struct Data {
    runs: u64,
    base_us: i64,
}

/// Lists, searches and grouped queries cover this share of the load, somewhere inside it: positions and
/// values vary, the amount of data doesn't (so the p99 rule measures the tail, not the mix).
const WINDOW: f64 = 0.25;

impl Data {
    fn end_us(&self) -> i64 {
        start_of(self.runs, self.base_us).0 + 2_000_000
    }

    fn window(&self, rng: &mut Rng) -> Value {
        let span = self.end_us() - self.base_us;
        let len = (span as f64 * WINDOW) as i64;
        let from = self.base_us + rng.below((span - len).max(1) as u64) as i64;
        json!({"from": Ts(from).to_string(), "to": Ts(from + len).to_string()})
    }

    /// `time=<n>m` for the fields API (only "the last span"): from somewhere in the load to now.
    fn last(&self, rng: &mut Rng) -> String {
        let now = Ts::now().0;
        let (min, max) = (
            (now - self.end_us()) / 60_000_000 + 1,
            (now - self.base_us) / 60_000_000 + 2,
        );
        format!("{}m", min + rng.below((max - min).max(1) as u64) as i64)
    }

    /// A random whole trace (the last one may be short when `runs` isn't a multiple of 10).
    fn traced(&self, rng: &mut Rng) -> u64 {
        rng.below(trace_of(self.runs).max(1))
    }
}

enum Req {
    Get(String),
    Post(&'static str, Value),
    /// `GET /api/aggregate/stream?q=<body>`, timed to its first event.
    Stream(Value),
}

impl std::fmt::Display for Req {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Req::Get(p) => write!(f, "GET {p}"),
            Req::Post(p, b) => write!(f, "POST {p} {b}"),
            Req::Stream(b) => write!(f, "GET /api/aggregate/stream {b}"),
        }
    }
}

/// A request, and the answer's array that must not be empty (where the data guarantees matches).
struct Probe {
    req: Req,
    nonempty: Option<&'static str>,
}

fn probe(req: Req, nonempty: Option<&'static str>) -> Probe {
    Probe { req, nonempty }
}

const NAMES: [&str; 4] = ["eval_task", "fetch_url", "llm", "plan"];

fn list(rng: &mut Rng, d: &Data) -> Probe {
    let (name, version) = (rng.pick(&NAMES), rng.pick(&["v6", "v7"]));
    let (mut body, kind) = (json!({}), rng.below(7));
    body["filter"] = json!(match kind {
        0 => r#"eq(status, "error")"#.to_string(),
        1 => format!(r#"eq(name, "{name}")"#),
        2 => format!(r#"and(eq(metadata.prompt_version, "{version}"), eq(name, "{name}"))"#),
        3 => format!(r#"eq(metadata.user_id, "user-{}")"#, rng.below(5_000)),
        4 => format!(r#"has(tags, "batch={}")"#, rng.below(4)),
        5 => r#"eq(status, "ok")"#.to_string(),
        _ => format!(r#"and(eq(status, "error"), has(tags, "batch={}"))"#, rng.below(4)),
    });
    if kind == 5 {
        body["is_root"] = json!(true);
        body["tree_filter"] = json!(r#"eq(status, "error")"#);
    }
    body["project"] = json!(PROJECT);
    body["time"] = d.window(rng);
    body["limit"] = json!(rng.pick(&[50, 100, 200]));
    // Roots are even-numbered, so always v6.
    let matches = matches!(kind, 0 | 1 | 4) || kind == 2 && (name, version) != ("eval_task", "v7");
    probe(Req::Post("/api/query", body), matches.then_some("runs"))
}

fn search(d: &Data, rng: &mut Rng, filter: String, nonempty: bool) -> Probe {
    probe(
        Req::Post(
            "/api/query",
            json!({"project": PROJECT, "time": d.window(rng), "filter": filter}),
        ),
        nonempty.then_some("runs"),
    )
}

/// The planted phrase: a handful of runs in a window.
fn rare_phrase(rng: &mut Rng, d: &Data) -> Probe {
    search(d, rng, r#"search("429 Too Many Requests")"#.into(), false)
}

/// One shape per row, so its p99 rule measures the tail, not a mix: only the word and the window vary.
fn common_word(rng: &mut Rng, d: &Data) -> Probe {
    let word = rng.pick(&WORDS);
    search(d, rng, format!(r#"search("{word}")"#), true)
}

fn search_filter(rng: &mut Rng, d: &Data) -> Probe {
    let f = format!(
        r#"and(search("{}"), eq(metadata.prompt_version, "{}"))"#,
        rng.pick(&WORDS),
        rng.pick(&["v6", "v7"])
    );
    search(d, rng, f, true)
}

fn text_search(rng: &mut Rng, d: &Data) -> Probe {
    [rare_phrase, common_word, search_filter][rng.below(3) as usize](rng, d)
}

fn grouped(rng: &mut Rng, d: &Data) -> Probe {
    let key = rng.pick(&["name", "metadata.prompt_version", "status"]);
    let mut body = match rng.below(4) {
        0 => json!({"stats": format!("by {key} | count, error_rate, p95(latency)")}),
        1 => json!({"group_by": [key], "metrics": ["count", "error_rate", "p50(latency)"], "view": "series",
                    "bucket": "auto"}),
        2 => json!({"stats": format!("by {key} | count, error_rate, p95(latency)"),
                    "compare": {"filter": format!(r#"has(tags, "batch={}")"#, rng.below(4))}}),
        _ => json!({"group_by": ["name", "metadata.prompt_version"], "metrics": ["count", "error_rate"]}),
    };
    body["project"] = json!(PROJECT);
    body["time"] = d.window(rng);
    if rng.below(3) == 0 {
        body["filter"] = json!(format!(r#"has(tags, "batch={}")"#, rng.below(4)));
    }
    probe(Req::Post("/api/aggregate", body), Some("groups"))
}

/// Over every run (SC-06: "a grouped query over all 2M runs").
fn first_estimate(rng: &mut Rng) -> Probe {
    let key = rng.pick(&["name", "metadata.prompt_version", "status"]);
    let metrics = rng.pick(&["count, error_rate", "count, p95(latency)", "error_rate, p50(latency)"]);
    probe(
        Req::Stream(json!({"project": PROJECT, "stats": format!("by {key} | {metrics}")})),
        None,
    )
}

fn suggestion(rng: &mut Rng, d: &Data) -> Probe {
    let base = format!("project={PROJECT}&time={}", d.last(rng));
    let (path, nonempty) = match rng.below(5) {
        0 => (format!("/api/fields?{base}"), Some("fields")),
        1 => (
            format!(
                "/api/fields?{base}&prefix={}",
                rng.pick(&["metadata.u", "metadata.p", "na", "out", "inp"])
            ),
            None,
        ),
        2 => (
            format!("/api/fields/values?{base}&path=metadata.prompt_version"),
            Some("values"),
        ),
        3 => (
            format!(
                "/api/fields/values?{base}&path=metadata.user_id&prefix=user-{}",
                rng.below(500)
            ),
            None,
        ),
        _ => (format!("/api/fields/values?{base}&path=name"), Some("values")),
    };
    probe(Req::Get(path), nonempty)
}

/// The answer must be real, not just a 200.
type Check = fn(&Value) -> bool;

struct Spec {
    query: &'static str,
    target_ms: f64,
    make: Box<dyn Fn(&mut Rng) -> Probe>,
    check: Check,
    n: usize,
}

fn specs(d: Data, samples: usize) -> Vec<Spec> {
    let spec = |query, target_ms, n, check: Check, make: Box<dyn Fn(&mut Rng) -> Probe>| Spec {
        query,
        target_ms,
        make,
        check,
        n,
    };
    vec![
        spec(
            "trace tree",
            50.0,
            samples + 1,
            |v| v["run_count"] == STEPS + 1,
            Box::new(move |r| probe(Req::Get(format!("/api/traces/{}", trace_id(d.traced(r)))), None)),
        ),
        spec(
            "run + payload",
            50.0,
            samples + 1,
            |v| v["inputs"]["messages"].is_array() && v["outputs"]["content"].is_string(),
            Box::new(move |r| {
                probe(
                    Req::Get(format!("/api/runs/{}?include=inputs,outputs", run_id(r.below(d.runs)))),
                    None,
                )
            }),
        ),
        spec(
            "filtered list",
            100.0,
            samples,
            |v| v["runs"].is_array(),
            Box::new(move |r| list(r, &d)),
        ),
        spec(
            "text search: rare phrase",
            500.0,
            samples,
            |v| v["runs"].is_array(),
            Box::new(move |r| rare_phrase(r, &d)),
        ),
        spec(
            "text search: common word",
            500.0,
            samples,
            |v| v["runs"].is_array(),
            Box::new(move |r| common_word(r, &d)),
        ),
        spec(
            "text search: search + filter",
            500.0,
            samples,
            |v| v["runs"].is_array(),
            Box::new(move |r| search_filter(r, &d)),
        ),
        spec(
            "grouped",
            1000.0,
            samples,
            |v| v["groups"].is_array(),
            Box::new(move |r| grouped(r, &d)),
        ),
        spec(
            "first estimate",
            1000.0,
            samples,
            |v| v["event"] == "estimate" || v["event"] == "result",
            Box::new(first_estimate),
        ),
        spec(
            "suggestions",
            200.0,
            samples,
            |v| v["fields"].is_array() || v["values"].is_array(),
            Box::new(move |r| suggestion(r, &d)),
        ),
    ]
}

/// One request's time, through reading the whole answer (or a stream's first event); JSON parsing is not timed.
async fn time_one(http: &reqwest::Client, url: &str, req: &Req, sleep: Duration) -> anyhow::Result<(f64, Value)> {
    let (what, r) = match req {
        Req::Get(p) => (format!("GET {p}"), http.get(format!("{url}{p}"))),
        Req::Post(p, b) => (format!("POST {p} {b}"), http.post(format!("{url}{p}")).json(b)),
        Req::Stream(b) => (
            format!("GET /api/aggregate/stream {b}"),
            http.get(format!("{url}/api/aggregate/stream"))
                .query(&[("q", b.to_string())]),
        ),
    };
    let t = Instant::now();
    let mut resp = r.send().await.with_context(|| what.clone())?;
    let status = resp.status();
    let mut buf = Vec::new();
    let stream = matches!(req, Req::Stream(_));
    if stream {
        while !buf.windows(2).any(|w| w == b"\n\n") {
            match resp.chunk().await.with_context(|| what.clone())? {
                Some(c) => buf.extend_from_slice(&c),
                None => break,
            }
        }
        drop(resp);
    } else {
        buf = resp.bytes().await.with_context(|| what.clone())?.to_vec();
    }
    if !sleep.is_zero() {
        tokio::time::sleep(sleep).await;
    }
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let v = if stream {
        // The dropped stream cancels the exact pass; let it stop before the next request.
        let until = Instant::now() + Duration::from_secs(30);
        while tracedb_query::aggregate::progressive::stream_counts().0 > 0 && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let frame = String::from_utf8_lossy(&buf).into_owned();
        let event = frame.lines().find_map(|l| l.strip_prefix("event:")).map(str::trim);
        json!({"event": event, "frame": frame})
    } else {
        serde_json::from_slice(&buf).unwrap_or_else(|_| json!(String::from_utf8_lossy(&buf)))
    };
    if !status.is_success() {
        bail!("{what}: {status}: {v}");
    }
    Ok((ms, v))
}

#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub query: String,
    pub n: usize,
    /// `null` in the JSON when the row couldn't be measured (`why` says so).
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub target_ms: f64,
    pub budget_ms: f64,
    pub pass: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// Nearest rank, as the product's percentiles.
fn pct(sorted: &[f64], p: f64) -> f64 {
    let i = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[i]
}

fn num(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{x:.0}")
    } else {
        format!("{x:.1}")
    }
}

/// A row passes when its median is within budget and its p99 within 5× the median, whatever the budget.
pub fn verdict(p50_ms: f64, p99_ms: f64, budget_ms: f64) -> Result<(), String> {
    if p50_ms > budget_ms {
        return Err(format!("p50 {p50_ms:.1} ms > budget {} ms", num(budget_ms)));
    }
    if p99_ms > 5.0 * p50_ms {
        return Err(format!("p99 {p99_ms:.1} ms > 5× p50 {p50_ms:.1} ms"));
    }
    Ok(())
}

/// Times one query type; a failed request or a wrong answer fails the row. The budget is set by [`judge`].
async fn measure(http: &reqwest::Client, url: &str, s: &Spec, seed: u64, sleep: Duration) -> Row {
    let timed = async {
        let mut warm = Rng(mix(seed ^ 0xFFFF));
        for _ in 0..WARMUP.min(s.n) {
            time_one(http, url, &(s.make)(&mut warm).req, Duration::ZERO).await?;
        }
        let mut rng = Rng(mix(seed));
        let mut took = Vec::with_capacity(s.n);
        for _ in 0..s.n {
            let p = (s.make)(&mut rng);
            let (ms, v) = time_one(http, url, &p.req, sleep).await?;
            let filled = p
                .nonempty
                .is_none_or(|k| v[k].as_array().is_some_and(|a| !a.is_empty()));
            if !(s.check)(&v) || !filled {
                let shown: String = v.to_string().chars().take(300).collect();
                bail!("wrong answer to {}: {shown}", p.req);
            }
            took.push(ms);
        }
        took.sort_by(f64::total_cmp);
        Ok(took)
    };
    let (p50, p99, why) = match timed.await {
        Ok(took) => (pct(&took, 0.5), pct(&took, 0.99), None),
        Err(e) => (f64::NAN, f64::NAN, Some(format!("{e:#}"))),
    };
    Row {
        query: s.query.into(),
        n: s.n,
        p50_ms: p50,
        p99_ms: p99,
        target_ms: s.target_ms,
        budget_ms: f64::NAN,
        pass: false,
        why,
    }
}

fn judge(row: &mut Row, r: f64) {
    row.budget_ms = row.target_ms * r;
    if row.why.is_none() {
        let v = verdict(row.p50_ms, row.p99_ms, row.budget_ms);
        row.pass = v.is_ok();
        row.why = v.err();
    }
}

// ---- calibration ----

/// `runner / reference`, clamped to [1, 3]: a faster machine keeps the targets, a much slower one gets 3× at most.
pub fn ratio(runner_ms: f64, reference_ms: f64) -> f64 {
    (runner_ms / reference_ms).clamp(1.0, 3.0)
}

/// The fixed workload: decode the shipped Parquet file 20 times, then sort its text column. Returns rows decoded.
pub fn reference_workload() -> anyhow::Result<usize> {
    use datafusion::arrow::array::AsArray;
    use datafusion::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let bytes = Bytes::from_static(REFERENCE);
    let (mut rows, mut last) = (0, Vec::new());
    for _ in 0..20 {
        last = ParquetRecordBatchReaderBuilder::try_new(bytes.clone())?
            .build()?
            .collect::<Result<Vec<_>, _>>()?;
        rows += last.iter().map(|b| b.num_rows()).sum::<usize>();
    }
    let mut texts: Vec<&str> = Vec::new();
    for b in &last {
        let col = b.column_by_name("text").context("reference file has no text column")?;
        texts.extend(col.as_string::<i32>().iter().flatten());
    }
    texts.sort_unstable();
    std::hint::black_box(&texts);
    Ok(rows)
}

/// The workload's best time of three (one when it's already past the clamp), in ms.
fn calibrate() -> anyhow::Result<f64> {
    let mut best = f64::INFINITY;
    for _ in 0..3 {
        let t = Instant::now();
        reference_workload()?;
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
        if best > 3.0 * REFERENCE_MS {
            break;
        }
    }
    Ok(best)
}

/// Writes the calibration file from the generator (`tests/bench_cli.rs` `write_reference_fixture`).
pub fn write_reference(path: &Path) -> anyhow::Result<()> {
    use datafusion::arrow::array::{ArrayRef, FixedSizeBinaryArray, Int64Array, StringArray};
    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::parquet::arrow::ArrowWriter;
    use datafusion::parquet::basic::{Compression, ZstdLevel};
    use datafusion::parquet::file::properties::WriterProperties;
    let n: Vec<u64> = (0..20_000).collect();
    let ids = |f: fn(u64) -> Uuid| -> anyhow::Result<ArrayRef> {
        Ok(std::sync::Arc::new(FixedSizeBinaryArray::try_from_iter(
            n.iter().map(|&i| f(i).into_bytes()),
        )?))
    };
    let names: Vec<&str> = n.iter().map(|&i| NAMES[(i % 4) as usize]).collect();
    let starts: Vec<i64> = n.iter().map(|&i| start_of(i, 0).0).collect();
    let texts: Vec<String> = n.iter().map(|&i| text(i, 1, 60)).collect();
    let batch = RecordBatch::try_from_iter([
        ("run_id", ids(run_id)?),
        ("trace_id", ids(trace_id)?),
        ("name", std::sync::Arc::new(StringArray::from(names)) as ArrayRef),
        ("start_time", std::sync::Arc::new(Int64Array::from(starts)) as ArrayRef),
        ("text", std::sync::Arc::new(StringArray::from(texts)) as ArrayRef),
    ])?;
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
        .set_max_row_group_row_count(Some(4_096))
        .build();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path)?, batch.schema(), Some(props))?;
    w.write(&batch)?;
    w.close()?;
    Ok(())
}

// ---- footprint ----

fn dir_bytes(dir: &Path) -> (u64, u64) {
    let (mut total, mut index) = (0, 0);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                stack.push(e.path());
            } else {
                total += md.len();
                if e.file_name().to_string_lossy().contains(".fts.") {
                    index += md.len();
                }
            }
        }
    }
    (total, index)
}

/// This process's resident memory in MB: `/proc` on Linux, else `ps`.
fn rss_mb() -> Option<f64> {
    let kb: f64 = match std::fs::read_to_string("/proc/self/status") {
        Ok(s) => s
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))?
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .ok()?,
        Err(_) => {
            let out = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &std::process::id().to_string()])
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout).trim().parse().ok()?
        }
    };
    Some(kb * 1024.0 / 1e6)
}

// ---- the run ----

struct Own {
    addr: SocketAddr,
    state: AppState,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

/// The same server as `serve` (open, warm the aggregate cache, routes) on a free local port.
async fn start(dir: &Path, limits: Limits) -> anyhow::Result<Own> {
    let cfg = ServeConfig {
        data_dir: dir.to_path_buf(),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        storage: None,
        metastore: None,
        limits,
        ui_dir: None,
        prices: None,
    };
    let listener = tokio::net::TcpListener::bind(cfg.addr).await?;
    let (state, _) = tokio::task::spawn_blocking(move || crate::open(&cfg)).await??;
    state.query.warm_aggregates();
    let addr = listener.local_addr()?;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let router = app(state.clone());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
            })
            .await;
    });
    Ok(Own {
        addr,
        state,
        stop,
        task,
    })
}

#[derive(Serialize)]
struct Report {
    runs: u64,
    raw_bytes: u64,
    r: f64,
    runner_ms: f64,
    reference_ms: f64,
    files: usize,
    file_rows: u64,
    late_updates: u64,
    load: Value,
    rows: Vec<Row>,
    disk: Value,
    /// The server's own idle footprint: `None` while it shares the bench process (`idle.process_mb` beside it).
    idle_mb: Option<f64>,
    idle: Value,
    pass: bool,
}

/// Runs the bench; returns the exit code (0 ok or no `--check`, 1 a row missed its budget, 2 setup error).
pub async fn run(a: &BenchArgs, out: &mut impl Write, err: &mut impl Write) -> u8 {
    match bench(a, out, err).await {
        Ok(true) => 0,
        Ok(false) if a.check => 1,
        Ok(false) => 0,
        Err(e) => {
            let _ = writeln!(err, "tracedb bench: {}", tracedb_storage::redact(&format!("{e:#}")));
            2
        }
    }
}

async fn bench(a: &BenchArgs, out: &mut impl Write, err: &mut impl Write) -> anyhow::Result<bool> {
    let runs = a.runs();
    if runs < 1_000 {
        bail!("--runs must be at least 1000");
    }
    let runner_ms = tokio::task::spawn_blocking(calibrate).await??;
    if a.calibrate_only {
        let r = ratio(runner_ms, REFERENCE_MS);
        writeln!(
            out,
            "calibration r = {r:.2} (runner {runner_ms:.1} ms, reference {REFERENCE_MS:.1} ms)"
        )?;
        return Ok(true);
    }

    let tmp;
    let dir = match &a.data {
        Some(d) => d.clone(),
        None => {
            tmp = tempfile::Builder::new().prefix("tracedb-bench-").tempdir()?;
            tmp.path().to_path_buf()
        }
    };
    let marker = dir.join(MARKER);
    let reused: Option<LoadInfo> = if marker.exists() {
        let info: LoadInfo = serde_json::from_slice(&std::fs::read(&marker)?)
            .with_context(|| format!("reading {}", marker.display()))?;
        if info.runs != runs {
            bail!(
                "{} holds a {}-run bench; pass --runs {} or another --data",
                dir.display(),
                thousands(info.runs),
                info.runs
            );
        }
        Some(info)
    } else if std::fs::read_dir(&dir).is_ok_and(|mut d| d.next().is_some()) {
        bail!("{} is not empty and holds no earlier bench ({MARKER})", dir.display());
    } else {
        None
    };

    let mut limits = Limits::default();
    if let Some(s) = a.flush_age_secs {
        limits.flush_age_us = s as i64 * 1_000_000;
    }
    if let Some(s) = a.dedup_window_secs {
        limits.dedup_window_us = s as i64 * 1_000_000;
    }
    let own = start(&dir, limits).await?;
    let url = format!("http://{}", own.addr);
    let ingest = own.state.ingest.clone();

    let info = match &reused {
        Some(i) => i.clone(),
        None => {
            let info = load(&url, runs, Ts::now().0 - 6 * 3_600_000_000, err).await?;
            std::fs::write(&marker, serde_json::to_vec_pretty(&info)?)?;
            info
        }
    };
    let quiet_since = Instant::now();
    let deadline = quiet_since + Duration::from_secs(600);
    while ingest.health().frozen || ingest.health().memtable_bytes >= limits.memtable_bytes {
        if Instant::now() > deadline {
            bail!(
                "the memtable didn't flush within 10 minutes of the load: {:?}",
                ingest.health()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let sleep = Duration::from_millis(
        std::env::var("TRACEDB_BENCH_SLEEP_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    );
    if !sleep.is_zero() {
        let _ = writeln!(
            err,
            "tracedb bench: TRACEDB_BENCH_SLEEP_MS adds {sleep:?} to every timed query (test hook)"
        );
    }
    let d = Data {
        runs,
        base_us: info.base_us,
    };
    let http = reqwest::Client::new();
    let mut rows = Vec::new();
    for (i, s) in specs(d, a.samples).iter().enumerate() {
        let _ = writeln!(err, "tracedb bench: {} ×{}…", s.query, s.n);
        rows.push(measure(&http, &url, s, 0xB0 + i as u64, sleep).await);
    }

    // Idle (research M3-R12): no uploads for one flush age, the memtable flushed, the dedup window drained; the
    // median footprint over `idle_secs`.
    let quiet = Duration::from_micros(limits.flush_age_us.max(limits.dedup_window_us).max(0) as u64);
    let deadline = quiet_since + quiet + Duration::from_secs(120);
    let _ = writeln!(err, "tracedb bench: waiting {quiet:?} after the last upload for idle…");
    while (quiet_since.elapsed() < quiet || ingest.health().runs > 0 || ingest.health().frozen)
        && Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let flushed = ingest.health().runs == 0;
    let mut samples = Vec::new();
    for i in 0..a.idle_secs.max(1) {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        samples.extend(rss_mb());
    }
    samples.sort_by(f64::total_cmp);
    let process_mb = (!samples.is_empty()).then(|| pct(&samples, 0.5));
    let (disk_bytes, index_bytes) = dir_bytes(&dir);
    let health = ingest.health();
    let _ = own.stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(30), own.task).await;

    // Again at the end: other work on the machine only slows the workload, so the faster run is the machine.
    let runner_ms = runner_ms.min(tokio::task::spawn_blocking(calibrate).await??);
    let r = ratio(runner_ms, REFERENCE_MS);
    for row in &mut rows {
        judge(row, r);
    }
    let mut info = info;
    if reused.is_none() {
        info.r = r;
        std::fs::write(&marker, serde_json::to_vec_pretty(&info)?)?;
    }
    let eps = info.events as f64 / info.secs.max(1e-9);
    let load_r = if info.r > 0.0 { info.r } else { r };
    let load_pass = eps >= LOAD_TARGET / load_r;
    let pct_raw = |b: u64| b as f64 * 100.0 / info.raw_bytes.max(1) as f64;
    let disk_pct = pct_raw(disk_bytes);
    let disk_pass = disk_pct <= DISK_TARGET_PCT;
    let pass = load_pass && disk_pass && rows.iter().all(|r| r.pass);
    let report = Report {
        runs,
        raw_bytes: info.raw_bytes,
        r,
        runner_ms,
        reference_ms: REFERENCE_MS,
        files: health.segments,
        file_rows: health.segment_rows,
        late_updates: info.late_updates,
        load: json!({"events_per_s": eps, "events": info.events, "secs": info.secs,
                     "queries_alongside": info.queries_alongside, "target": LOAD_TARGET,
                     "budget": LOAD_TARGET / load_r, "r": load_r, "pass": load_pass, "reused": reused.is_some()}),
        rows,
        disk: json!({"bytes": disk_bytes, "index_bytes": index_bytes, "files_bytes": disk_bytes - index_bytes,
                     "pct_of_raw": disk_pct, "target_pct": DISK_TARGET_PCT, "pass": disk_pass}),
        idle_mb: None,
        idle: json!({"mb": null, "process_mb": process_mb, "target_mb": IDLE_TARGET_MB, "flushed": flushed,
                     "secs": a.idle_secs, "waived": IDLE_WAIVED, "pass": null}),
        pass,
    };
    if let Some(path) = &a.json {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    print(&report, &info, reused.is_some(), &dir, a, out)?;
    Ok(pass)
}

fn ms(x: f64) -> String {
    if x.is_finite() {
        format!("{x:.1} ms")
    } else {
        "-".into()
    }
}

fn mark(pass: bool) -> &'static str {
    if pass { "PASS" } else { "FAIL" }
}

fn print(
    rep: &Report,
    info: &LoadInfo,
    reused: bool,
    dir: &Path,
    a: &BenchArgs,
    out: &mut impl Write,
) -> std::io::Result<()> {
    writeln!(
        out,
        "tracedb bench — {} runs, {:.2} GB raw JSON, calibration r = {:.2} (runner {:.0} ms, reference {:.0} ms)",
        thousands(rep.runs),
        rep.raw_bytes as f64 / 1e9,
        rep.r,
        rep.runner_ms,
        rep.reference_ms
    )?;
    writeln!(
        out,
        "  data            {} files ({} rows), {} late updates without a project, {CLIENTS} clients; n = {} per query",
        thousands(rep.files as u64),
        thousands(rep.file_rows),
        thousands(rep.late_updates),
        a.samples
    )?;
    let load_budget = rep.load["budget"].as_f64().unwrap_or(LOAD_TARGET);
    let eps = rep.load["events_per_s"].as_f64().unwrap_or(0.0);
    let load = if reused {
        format!(
            "{} events/s (reused from the load into {})",
            thousands(eps as u64),
            dir.display()
        )
    } else {
        format!(
            "{} events/s while querying ({} queries alongside)",
            thousands(eps as u64),
            thousands(info.queries_alongside)
        )
    };
    let load_pass = rep.load["pass"] == true;
    writeln!(
        out,
        "  load            {load:<56} target ≥ {}  budget ≥ {}  {}",
        thousands(LOAD_TARGET as u64),
        thousands(load_budget as u64),
        mark(load_pass)
    )?;
    writeln!(
        out,
        "  {:<30}{:>10}{:>10}{:>10}{:>10}   result",
        "query", "p50", "p99", "target", "budget"
    )?;
    for row in &rep.rows {
        writeln!(
            out,
            "  {:<30}{:>10}{:>10}{:>10}{:>10}   {}",
            row.query,
            ms(row.p50_ms),
            ms(row.p99_ms),
            format!("{} ms", num(row.target_ms)),
            format!("{} ms", num(row.budget_ms)),
            mark(row.pass)
        )?;
    }
    let (idx, total) = (
        rep.disk["index_bytes"].as_u64().unwrap_or(0),
        rep.disk["bytes"].as_u64().unwrap_or(0),
    );
    let raw = rep.raw_bytes.max(1) as f64;
    let disk_pass = rep.disk["pass"] == true;
    let disk = format!(
        "files {:.1}% + index {:.1}% = {:.1}% of raw",
        (total - idx) as f64 * 100.0 / raw,
        idx as f64 * 100.0 / raw,
        total as f64 * 100.0 / raw,
    );
    writeln!(
        out,
        "  disk            {disk:<56} target ≤ {}%  {}",
        num(DISK_TARGET_PCT),
        mark(disk_pass)
    )?;
    let flushed = rep.idle["flushed"] == true;
    // Server in-process: only the whole process's RSS, an upper bound. Kept within the 56-column text field.
    let idle = match rep.idle["process_mb"].as_f64() {
        Some(mb) if flushed => format!("process {mb:.0} MB RSS over {} s (upper bound)", a.idle_secs.max(1)),
        Some(mb) => format!("process {mb:.0} MB RSS (upper bound), not flushed by age"),
        None => "unavailable (server in-process, RSS not readable)".into(),
    };
    writeln!(
        out,
        "  idle memory     {idle:<56} target ≤ {} MB  WAIVED ({IDLE_WAIVED})",
        num(IDLE_TARGET_MB),
    )?;

    let mut failed = Vec::new();
    if !load_pass {
        failed.push(format!(
            "FAIL load: {} events/s < budget {} (target {}, r = {:.2})",
            thousands(eps as u64),
            thousands(load_budget as u64),
            thousands(LOAD_TARGET as u64),
            rep.load["r"].as_f64().unwrap_or(rep.r)
        ));
    }
    for row in rep.rows.iter().filter(|r| !r.pass) {
        failed.push(format!(
            "FAIL {}: {} (target {} ms, budget {} ms, p50 {}, p99 {}, r = {:.2})",
            row.query,
            row.why.as_deref().unwrap_or(""),
            num(row.target_ms),
            num(row.budget_ms),
            ms(row.p50_ms),
            ms(row.p99_ms),
            rep.r
        ));
    }
    if !disk_pass {
        failed.push(format!(
            "FAIL disk: {:.1}% of raw JSON > target {}%",
            rep.disk["pct_of_raw"].as_f64().unwrap_or(0.0),
            num(DISK_TARGET_PCT)
        ));
    }
    let total_rows = rep.rows.len() + 2;
    if failed.is_empty() {
        writeln!(
            out,
            "all {total_rows} rows pass (idle memory waived until {IDLE_WAIVED})"
        )?;
    } else {
        writeln!(
            out,
            "{} of {total_rows} rows failed (idle memory waived until {IDLE_WAIVED}):",
            failed.len()
        )?;
        for f in failed {
            writeln!(out, "{f}")?;
        }
    }
    Ok(())
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
