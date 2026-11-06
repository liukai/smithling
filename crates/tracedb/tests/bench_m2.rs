//! Field suggestions stay fast at scale (SC-03): p50 ≤ 500 ms for `GET /api/fields` and `/api/fields/values`
//! over 2M runs in data files plus a memtable. Slow, so `#[ignore]`; run with `--release`.

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};
use tracedb_ingest::{AcceptError, Incoming};
use tracedb_model::{Op, RunFields, Source, Ts};
use uuid::Uuid;

const RUNS: u64 = 2_000_000;
const STEPS: u64 = 9;

fn stamp(t: Ts) -> String {
    t.to_string().replace(['-', ':', '.'], "")
}

fn run(n: u64, now: Ts) -> RunFields {
    let trace = n / (STEPS + 1) * (STEPS + 1);
    let (id, tid) = (Uuid::from_u128(n as u128 + 1), Uuid::from_u128(trace as u128 + 1));
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
        outputs: Some(json!({"status_code": if n.is_multiple_of(50) { 429 } else { 200 }})),
        ..Default::default()
    }
}

fn p50(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn field_suggestions_p50_under_500ms_at_2m_runs() {
    let server = start().await;
    let ingest = server.state.ingest.clone();
    let now = Ts::now();
    let loaded = Instant::now();
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
        loop {
            match ingest.accept(items.clone()).await {
                Ok(r) => {
                    assert_eq!(r.accepted, items.len(), "{:?}", r.rejected.first());
                    break;
                }
                Err(AcceptError::Busy) => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(e) => panic!("{e}"),
            }
        }
    }
    while ingest.health().frozen {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let h = ingest.health();
    eprintln!(
        "loaded {RUNS} runs in {:.1?}: {} files ({} rows), {} runs in memory",
        loaded.elapsed(),
        h.segments,
        h.segment_rows,
        h.runs
    );
    assert!(h.segments >= 2, "most runs must be in data files");

    let http = reqwest::Client::new();
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
        let mut took = Vec::new();
        let mut last = Value::Null;
        for _ in 0..21 {
            let t = Instant::now();
            let r = http.get(server.url(path)).send().await.unwrap();
            assert_eq!(r.status(), 200);
            last = r.json().await.unwrap();
            took.push(t.elapsed());
        }
        let p = p50(took.clone());
        eprintln!("{label:>14}: p50 {p:.1?}, max {:.1?}", took.iter().max().unwrap());
        assert!(!last.is_null());
        assert!(p <= Duration::from_millis(500), "{label}: p50 {p:?}");
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
}
