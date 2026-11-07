//! M2 at laptop scale: 2M runs, about 5 GB of raw JSON (spec "Scale"), mostly in data files plus a memtable.
//! - SC-03: p50 ≤ 500 ms for a filtered list's first page and for field/value suggestions; p50 ≤ 200 ms to
//!   open a trace tree or a single run.
//! - SC-04: the memtable stays under its limit; data on disk takes no more than the raw JSON.
//! - R-9: files opened per trace lookup with and without bloom filters.
//!
//! Trace ids are scattered (as from SDKs that send random UUIDs), so file trace-id bounds prune nothing and
//! the bloom filters do all the work. Slow, so `#[ignore]`; run with `--release -- --ignored --nocapture`.

mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use common::*;
use datafusion::physical_plan::collect;
use datafusion::physical_plan::metrics::MetricValue;
use datafusion::prelude::{ParquetReadOptions, SessionConfig, SessionContext, col, lit};
use datafusion::scalar::ScalarValue;
use serde_json::{Value, json};
use tracedb_ingest::{AcceptError, DEFAULT_MEMTABLE_BYTES, Health, Incoming};
use tracedb_model::validate::json_size;
use tracedb_model::{Op, RunFields, Source, Ts};
use uuid::Uuid;

const RUNS: u64 = 2_000_000;
const STEPS: u64 = 9;
static WORDS: std::sync::LazyLock<Vec<&str>> = std::sync::LazyLock::new(|| {
    "the model returned a plan with three steps fetch page parse table retry after rate limit user asked for \
     summary of quarterly revenue and costs tool call failed succeeded context window tokens"
        .split_whitespace()
        .collect()
});

/// Both ingest-heavy tests load 2M runs; run them one at a time so neither measures the other's load.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn trace_of(n: u64) -> u64 {
    n / (STEPS + 1) * (STEPS + 1)
}

fn trace_id(n: u64) -> Uuid {
    let t = trace_of(n);
    Uuid::from_u64_pair(mix(t) | 1 << 63, t)
}

fn run_id(n: u64) -> Uuid {
    if n == trace_of(n) {
        trace_id(n)
    } else {
        Uuid::from_u128(n as u128 + 1)
    }
}

fn stamp(t: Ts) -> String {
    t.to_string().replace(['-', ':', '.'], "")
}

/// About 2.4 KB of model-ish text, different per run.
fn text(n: u64, salt: u64, words: usize) -> String {
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

fn run(n: u64, now: Ts) -> RunFields {
    let trace = trace_of(n);
    let (id, tid) = (run_id(n), trace_id(n));
    let start = Ts(now.0 - 6 * 3_600_000_000 + (n as i64) * 10_000);
    let root_start = Ts(now.0 - 6 * 3_600_000_000 + (trace as i64) * 10_000);
    let root_seg = format!("{}{tid}", stamp(root_start));
    let is_root = n == trace;
    let user = format!("user-{}", n % 5_000);
    RunFields {
        id: Some(id),
        trace_id: Some(tid),
        parent_run_id: (!is_root).then_some(tid),
        dotted_order: Some(if is_root {
            root_seg
        } else {
            format!("{root_seg}.{}{id}", stamp(start))
        }),
        project: Some("bench".into()),
        name: Some(
            if is_root {
                "eval_task"
            } else {
                ["fetch_url", "llm", "plan"][(n % 3) as usize]
            }
            .into(),
        ),
        run_type: Some(if is_root { "chain" } else { "tool" }.into()),
        start_time: Some(start),
        end_time: Some(Ts(start.0 + 2_000_000)),
        error: n.is_multiple_of(97).then(|| "HTTPError: 429".into()),
        tags: Some(vec![format!("batch={}", n % 4)]),
        metadata: Some(
            json!({"prompt_version": if n.is_multiple_of(2) { "v6" } else { "v7" }, "user_id": user, "attempt": n % 3})
                .as_object()
                .unwrap()
                .clone(),
        ),
        inputs: Some(json!({"messages": [{"role": "user", "content": text(n, 1, 150)}]})),
        outputs: Some(if planted(n) {
            json!({"status_code": 429, "content": text(n, 2, 180), "detail": "upstream said 429 Too Many Requests"})
        } else {
            json!({
                "status_code": if n.is_multiple_of(50) { 429 } else { 200 },
                "content": text(n, 2, 180),
            })
        }),
        ..Default::default()
    }
}

/// About 31 runs with a rare phrase, for text search.
fn planted(n: u64) -> bool {
    n % 64_513 == 11
}

struct Loaded {
    server: Server,
    health: Health,
    raw_json: u64,
    /// Largest active memtable seen during the load.
    max_memtable: u64,
    /// Largest batch sent: the writer freezes after the batch that crosses the limit.
    max_batch: u64,
}

async fn load() -> Loaded {
    let server = start().await;
    let ingest = server.state.ingest.clone();
    let now = Ts::now();
    let loaded = Instant::now();
    let (mut raw_json, mut max_memtable, mut max_batch) = (0, 0, 0);
    for chunk in (0..RUNS).collect::<Vec<_>>().chunks(2_000) {
        let items: Vec<Incoming> = chunk
            .iter()
            .map(|&n| Incoming {
                op: Op::Post,
                source: Source::Native,
                body_hash: n,
                fields: run(n, now),
            })
            .collect();
        let batch: u64 = items.iter().map(|i| json_size(&i.fields)).sum();
        raw_json += batch;
        max_batch = max_batch.max(batch);
        loop {
            match ingest.accept(items.clone()).await {
                Ok(r) => {
                    assert_eq!(r.accepted, items.len(), "{:?}", r.rejected.first());
                    max_memtable = max_memtable.max(ingest.health().memtable_bytes);
                    break;
                }
                Err(AcceptError::Busy) => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(e) => panic!("{e}"),
            }
        }
    }
    while ingest.health().frozen || ingest.health().memtable_bytes >= DEFAULT_MEMTABLE_BYTES {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let health = ingest.health();
    eprintln!(
        "loaded {RUNS} runs ({:.2} GB raw JSON) in {:.1?}: {} files ({} rows), {} runs in memory",
        raw_json as f64 / 1e9,
        loaded.elapsed(),
        health.segments,
        health.segment_rows,
        health.runs
    );
    assert!(health.segments >= 2, "most runs must be in data files");
    Loaded {
        server,
        health,
        raw_json,
        max_memtable,
        max_batch,
    }
}

fn pct(v: &mut [Duration], p: f64) -> Duration {
    v.sort();
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// Times each request; asserts it succeeds with a real answer (`ok`) and prints p50/p99. Returns p50.
async fn measure(label: &str, reqs: Vec<reqwest::RequestBuilder>, ok: impl Fn(&Value) -> bool) -> Duration {
    let mut took = Vec::new();
    for r in reqs {
        let t = Instant::now();
        let resp = r.send().await.unwrap();
        assert_eq!(resp.status(), 200, "{label}");
        let body: Value = resp.json().await.unwrap();
        took.push(t.elapsed());
        assert!(ok(&body), "{label}: {body}");
    }
    let (p50, p99) = (pct(&mut took, 0.5), pct(&mut took, 0.99));
    let tail = if took.len() < 100 { "max" } else { "p99" };
    eprintln!("{label:>22}: p50 {p50:>8.1?}  {tail} {p99:>8.1?}  (n={})", took.len());
    p50
}

fn non_empty(key: &'static str) -> impl Fn(&Value) -> bool {
    move |v| v[key].as_array().is_some_and(|a| !a.is_empty())
}

fn dir_bytes(dir: &std::path::Path) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let md = e.metadata().unwrap();
            if md.is_dir() {
                stack.push(e.path());
            } else {
                let path = e.path();
                let top = path.strip_prefix(dir).unwrap().iter().next().unwrap();
                *out.entry(top.to_string_lossy().into_owned()).or_default() += md.len();
            }
        }
    }
    out
}

/// Bytes of the text index sidecars (`*.fts.dict.parquet`, `*.fts.post`) under `dir`.
fn fts_bytes(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let md = e.metadata().unwrap();
            if md.is_dir() {
                stack.push(e.path());
            } else if e.file_name().to_string_lossy().contains(".fts.") {
                total += md.len();
            }
        }
    }
    total
}

/// Current process RSS, if `ps` may run here (report only: M2 sets no RSS limit, only the memtable size).
fn rss() -> String {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok())
        .map_or("unavailable".into(), |kb| format!("{:.2} GB", kb as f64 * 1024.0 / 1e9))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn m2_latencies_and_footprint_at_2m_runs() {
    let _serial = SERIAL.lock().await;
    let Loaded {
        server,
        health,
        raw_json,
        max_memtable,
        max_batch,
    } = load().await;
    let http = reqwest::Client::new();

    let mut missed = Vec::new();
    let mut check = |label: &str, p50: Duration, target_ms: u64| {
        if p50 > Duration::from_millis(target_ms) {
            missed.push(format!("SC-03 {label}: p50 {p50:.1?} > {target_ms} ms"));
        }
    };

    let list = |body: Value| -> Vec<reqwest::RequestBuilder> {
        (0..21)
            .map(|_| http.post(server.url("/api/query")).json(&body))
            .collect()
    };
    for (label, body) in [
        (
            "list: errors",
            json!({"project": "bench", "time": {"last": "12h"}, "filter": "eq(status, \"error\")"}),
        ),
        (
            "list: metadata + name",
            json!({"project": "bench", "time": {"last": "12h"},
                   "filter": "and(eq(metadata.prompt_version, \"v7\"), eq(name, \"llm\"))"}),
        ),
        (
            "list: quiet failures",
            json!({"project": "bench", "time": {"last": "12h"}, "is_root": true,
                   "filter": "eq(status, \"ok\")", "tree_filter": "eq(status, \"error\")"}),
        ),
    ] {
        let p = measure(label, list(body), non_empty("runs")).await;
        check(label, p, 500);
    }

    // Spread over the whole load: most in files, the last ones in the memtable.
    let sample: Vec<u64> = (0..101).map(|i| i * (RUNS / 101) + 3).collect();
    let traces = sample
        .iter()
        .map(|&n| http.get(server.url(&format!("/api/traces/{}", trace_id(n)))))
        .collect();
    let p = measure("trace tree", traces, |v| v["run_count"] == STEPS + 1).await;
    check("trace tree", p, 200);
    let runs = sample
        .iter()
        .map(|&n| http.get(server.url(&format!("/api/runs/{}?include=inputs,outputs", run_id(n)))))
        .collect();
    let p = measure("single run + payload", runs, |v| {
        v["inputs"]["messages"].is_array() && v["outputs"]["content"].is_string()
    })
    .await;
    check("single run", p, 200);

    for (label, path) in [
        ("fields", "/api/fields?project=bench&time=12h"),
        ("fields prefix", "/api/fields?project=bench&time=12h&prefix=metadata.u"),
        (
            "values",
            "/api/fields/values?project=bench&time=12h&path=metadata.prompt_version",
        ),
        (
            "values prefix",
            "/api/fields/values?project=bench&time=12h&path=metadata.user_id&prefix=user-12",
        ),
    ] {
        let key = if path.starts_with("/api/fields/values") {
            "values"
        } else {
            "fields"
        };
        let p = measure(
            label,
            (0..101).map(|_| http.get(server.url(path))).collect(),
            non_empty(key),
        )
        .await;
        check(label, p, 500);
    }
    let v: Value = http
        .get(server.url("/api/fields/values?project=bench&path=metadata.prompt_version"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let total: u64 = v["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["count"].as_u64().unwrap())
        .sum();
    assert_eq!(total, RUNS, "every run counted once");

    let disk = dir_bytes(server.dir.path());
    let on_disk: u64 = disk.values().sum();
    let mut parts: Vec<_> = disk.iter().collect();
    parts.sort();
    let index = fts_bytes(&server.dir.path().join("data"));
    let pct = |b: u64| b as f64 * 100.0 / raw_json as f64;
    eprintln!(
        "disk {:.2} GB = {:.0}% of raw JSON {:.2} GB (files {:.1}%, text index {:.1}%); by dir: {}",
        on_disk as f64 / 1e9,
        pct(on_disk),
        raw_json as f64 / 1e9,
        pct(on_disk - index),
        pct(index),
        parts
            .iter()
            .map(|(k, v)| format!("{k} {:.2} GB", **v as f64 / 1e9))
            .collect::<Vec<_>>()
            .join(", ")
    );
    eprintln!(
        "memory: active memtable peak {:.0} MB, now {:.0} MB (limit {:.0} MB, largest batch {:.1} MB); process RSS {}",
        max_memtable as f64 / 1e6,
        health.memtable_bytes as f64 / 1e6,
        DEFAULT_MEMTABLE_BYTES as f64 / 1e6,
        max_batch as f64 / 1e6,
        rss()
    );
    if max_memtable > DEFAULT_MEMTABLE_BYTES + max_batch {
        missed.push(format!("SC-04 memtable peak {max_memtable} B over its limit"));
    }
    if on_disk > raw_json {
        missed.push(format!("SC-04 disk {on_disk} B > raw JSON {raw_json} B"));
    }
    assert!(missed.is_empty(), "missed: {missed:#?}");
}

/// One trace lookup straight through DataFusion, as `scan::read_core` runs it: the files whose rows were decoded,
/// bytes fetched, and the rows found.
async fn engine_lookup(ctx: &SessionContext, urls: &[String], t: Uuid) -> (usize, usize, usize) {
    let id = lit(ScalarValue::FixedSizeBinary(16, Some(t.as_bytes().to_vec())));
    let df = ctx
        .read_parquet(urls.to_vec(), ParquetReadOptions::default())
        .await
        .unwrap()
        .filter(col("trace_id").eq(id))
        .unwrap();
    let plan = df.create_physical_plan().await.unwrap();
    let rows: usize = collect(plan.clone(), ctx.task_ctx())
        .await
        .unwrap()
        .iter()
        .map(|b| b.num_rows())
        .sum();
    let mut sets = Vec::new();
    let mut stack = vec![plan];
    while let Some(p) = stack.pop() {
        sets.extend(p.metrics());
        stack.extend(p.children().into_iter().cloned());
    }
    let (mut decoded, mut bytes) = (HashMap::<String, usize>::new(), 0);
    for m in sets.iter().flat_map(|s| s.iter()) {
        let Some(file) = m.labels().iter().find(|l| l.name() == "filename") else {
            continue;
        };
        match m.value() {
            MetricValue::Count { name, count } if name == "pushdown_rows_matched" || name == "pushdown_rows_pruned" => {
                *decoded.entry(file.value().to_string()).or_default() += count.value();
            }
            MetricValue::Count { name, count } if name == "bytes_scanned" => bytes += count.value(),
            _ => {}
        }
    }
    (decoded.values().filter(|&&n| n > 0).count(), bytes, rows)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn bloom_filters_cut_files_opened_per_trace_lookup() {
    let _serial = SERIAL.lock().await;
    let Loaded { server, health, .. } = load().await;
    let ingest = server.state.ingest.clone();
    let version = ingest.mem().read().unwrap().version.clone();
    let store = &ingest.storage().store;
    let urls: Vec<String> = version.segments.iter().map(|s| store.url(&s.core_path)).collect();

    // Traces that are certainly flushed (the memtable holds the newest runs).
    let flushed = health.segment_rows.min(RUNS) * 8 / 10;
    let sample: Vec<u64> = (0..200).map(|i| i * (flushed / 200)).collect();
    let in_bounds: usize = sample
        .iter()
        .map(|&n| {
            let t = trace_id(n);
            version
                .segments
                .iter()
                .filter(|s| s.min_trace_id <= t && t <= s.max_trace_id)
                .count()
        })
        .sum();
    let k = sample.len() as f64;
    eprintln!(
        "trace lookups over {} files: {:.2} files per lookup in trace-id bounds",
        urls.len(),
        in_bounds as f64 / k
    );

    let mut decoded_by_mode = Vec::new();
    for blooms in [false, true] {
        // The query service's settings (`Query::new`), with blooms on or off.
        let cfg = SessionConfig::new()
            .set_bool("datafusion.execution.parquet.pushdown_filters", true)
            .set_bool("datafusion.execution.parquet.reorder_filters", true)
            .set_bool("datafusion.execution.parquet.bloom_filter_on_read", blooms);
        let ctx = SessionContext::new_with_config(cfg);
        store.register(&ctx);
        let (mut decoded, mut max, mut bytes, mut took) = (0, 0, 0, Vec::new());
        for &n in &sample {
            let t = Instant::now();
            let (files, b, rows) = engine_lookup(&ctx, &urls, trace_id(n)).await;
            took.push(t.elapsed());
            assert_eq!(rows as u64, STEPS + 1, "the whole trace is found");
            decoded += files;
            max = max.max(files);
            bytes += b;
        }
        eprintln!(
            "blooms {:>3}: files read per lookup {:.2} (max {max}), {:.0} KB fetched, p50 {:.1?}",
            if blooms { "on" } else { "off" },
            decoded as f64 / k,
            bytes as f64 / k / 1024.0,
            pct(&mut took, 0.5)
        );
        decoded_by_mode.push(decoded);
    }
    assert!(
        decoded_by_mode[1] < decoded_by_mode[0],
        "blooms must skip files that trace bounds can't"
    );
}

/// The quickstart's server: the overnight-eval scenario, then ~2M seeded runs at the default memtable (300+ files
/// over the same hours), whose late updates come without a project after their starts were flushed (as the SDK
/// sends them). The scenario's lists must not pay for the seed's updates.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn sc03_lists_with_late_updates_in_another_project() {
    use tracedb::{scenario, seed};
    let _serial = SERIAL.lock().await;
    let server = start().await;
    let url = server.url("");
    let ingest = server.state.ingest.clone();
    let base = Ts::now().0 - 6 * 3_600_000_000;
    let s = scenario::generate(7, Ts::now().0, DEFAULT_MEMTABLE_BYTES);
    let loaded = Instant::now();
    let l = scenario::load(&url, &s).await.unwrap();
    assert!(l.in_files);
    let (mut posts, mut patches) = (0, 0);
    // 40 uploads of 305 traces (~49k runs, ~10 memtables each): each upload's late updates follow its starts.
    for batch in 0..40u64 {
        let traces = seed::generate(1_000 + batch, 305, base);
        let (p, u) = seed::send(&url, &traces, true).await.unwrap();
        posts += p;
        patches += u;
    }
    while ingest.health().frozen || ingest.health().memtable_bytes >= DEFAULT_MEMTABLE_BYTES {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let health = ingest.health();
    let unknown: u64 = {
        let mem = ingest.mem();
        let mem = mem.read().unwrap();
        mem.version
            .segments
            .iter()
            .filter(|s| s.project.is_empty())
            .map(|s| s.row_count)
            .sum()
    };
    eprintln!(
        "loaded {} seed runs ({patches} late updates) + scenario in {:.1?}: {} files ({} rows, {unknown} under no project), {} runs in memory",
        posts,
        loaded.elapsed(),
        health.segments,
        health.segment_rows,
        health.runs
    );
    assert!(health.segments >= 300, "{} files", health.segments);

    let http = reqwest::Client::new();
    let tag = scenario::BATCH_TAG;
    let roots = |filter: String, tree: Option<&str>, count: bool, project: &str| {
        json!({"project": project, "time": {"last": "48h"}, "is_root": true, "filter": filter,
               "tree_filter": tree, "limit": 200, "count": count})
    };
    let failing = |count| {
        roots(
            format!(r#"and(has(tags, "{tag}"), eq(status, "error"))"#),
            None,
            count,
            scenario::PROJECT,
        )
    };
    let quiet = |count| {
        roots(
            format!(r#"and(has(tags, "{tag}"), eq(status, "ok"))"#),
            Some(r#"eq(status, "error")"#),
            count,
            scenario::PROJECT,
        )
    };
    let n_runs = |n: usize| move |v: &Value| v["runs"].as_array().is_some_and(|a| a.len() == n);
    let mut missed = Vec::new();
    for (label, body, n, assert) in [
        ("failing roots", failing(false), scenario::FAILING_ROOTS, true),
        ("quiet failures", quiet(false), scenario::QUIET_FAILURES, true),
        ("failing roots, count", failing(true), scenario::FAILING_ROOTS, false),
        ("quiet failures, count", quiet(true), scenario::QUIET_FAILURES, false),
    ] {
        let reqs = (0..21)
            .map(|_| http.post(server.url("/api/query")).json(&body))
            .collect();
        let p50 = measure(label, reqs, n_runs(n)).await;
        let v: Value = http
            .post(server.url("/api/query"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        eprintln!("{label:>22}: stats {}", v["stats"]);
        let rows = v["stats"]["rows_read"].as_u64().unwrap();
        if assert && p50 > Duration::from_millis(500) {
            missed.push(format!("SC-03 {label}: p50 {p50:.1?} > 500 ms"));
        }
        if assert && rows > 10 * 200 {
            missed.push(format!("{label}: read {rows} rows for a 200-run page"));
        }
    }
    // Reported only: the seeded project's own lists merge its late updates.
    let seed_failing = roots(r#"eq(status, "error")"#.into(), None, false, seed::PROJECT);
    let reqs = (0..5)
        .map(|_| http.post(server.url("/api/query")).json(&seed_failing))
        .collect();
    measure("seed: failing roots", reqs, |v| v["runs"].is_array()).await;
    assert!(missed.is_empty(), "missed: {missed:#?}");
}

/// SC-03 text search at 2M runs: median ≤ 500 ms, p99 ≤ 5× the median, for a rare phrase (the index narrows
/// to a few rows), a word in every run (the index is skipped; the first page stops early), and a 1% word.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn m3_text_search_at_2m_runs() {
    let _serial = SERIAL.lock().await;
    let Loaded { server, .. } = load().await;
    let http = reqwest::Client::new();
    let planted = (0..RUNS).filter(|n| planted(*n)).count();
    let mut missed = Vec::new();
    // The newest runs are in the memtable; `to` keeps a query to the files.
    let body = |filter: &str, to: Option<&str>| json!({"project": "bench", "time": {"from": "-12h", "to": to}, "filter": filter});
    // (label, filter, end of the time range, exact number of runs on the page if known)
    let cases = [
        (
            "search: rare phrase",
            r#"search("429 Too Many Requests")"#,
            None,
            Some(planted),
        ),
        ("search: word in all", r#"search("revenue")"#, None, None),
        ("word in all, files", r#"search("revenue")"#, Some("-1h"), None),
        ("1% in error, files", r#"search(error, "httperror")"#, Some("-1h"), None),
        (
            "phrase + name, files",
            r#"and(search(outputs, "rate limit"), eq(name, "llm"))"#,
            Some("-1h"),
            None,
        ),
    ];
    for (label, filter, to, want) in cases {
        let body = body(filter, to);
        let ok = |v: &Value| {
            let n = v["runs"].as_array().map_or(0, Vec::len);
            want.map_or(n > 0, |w| n == w)
        };
        let one: Value = http
            .post(server.url("/api/query"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        eprintln!("{label:>22}: stats {}", one["stats"]);
        let mut took = Vec::new();
        for _ in 0..101 {
            let t = Instant::now();
            let resp = http.post(server.url("/api/query")).json(&body).send().await.unwrap();
            assert_eq!(resp.status(), 200, "{label}");
            let v: Value = resp.json().await.unwrap();
            took.push(t.elapsed());
            assert!(ok(&v), "{label}: {}", v["stats"]);
        }
        let (p50, p99) = (pct(&mut took, 0.5), pct(&mut took, 0.99));
        eprintln!("{label:>22}: p50 {p50:>8.1?}  p99 {p99:>8.1?}  (n={})", took.len());
        if p50 > Duration::from_millis(500) || p99 > p50 * 5 {
            missed.push(format!("SC-03 {label}: p50 {p50:.1?}, p99 {p99:.1?}"));
        }
    }
    assert!(missed.is_empty(), "{missed:#?}");
}
