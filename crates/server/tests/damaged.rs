//! A data file deleted while serving: queries, traces, runs and fields answer from the other files and name
//! the skipped one; `/healthz` reports it (spec Story 7 scenario 1, M3-FR-18).

use smithling_server::testkit as common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};

async fn get(server: &Server, path: &str) -> (u16, Value) {
    let r = reqwest::get(server.url(path)).await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

async fn query(server: &Server, body: Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(server.url("/api/query"))
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

/// `/api/query` names skipped files in `warnings`; trace, run and field responses in `skipped_files` (a run's
/// own `warnings` are its run warnings).
fn skipped_files(v: &Value) -> Vec<String> {
    v["skipped_files"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| i.as_str().unwrap().to_string())
        .collect()
}

fn skipped(v: &Value) -> Vec<String> {
    v["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| w["kind"] == "skipped_files")
        .flat_map(|w| {
            w["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i.as_str().unwrap().to_string())
        })
        .collect()
}

/// A server with `n` roots flushed into several files.
async fn flushed(n: u64) -> Server {
    let server = start_with(|c| {
        c.limits.memtable_bytes = 32 * 1024;
        c.limits.flush_age_us = 100_000;
    })
    .await;
    for chunk in (1..=n).collect::<Vec<_>>().chunks(20) {
        let posts: Vec<Value> = chunk
            .iter()
            .map(|&i| {
                let mut r = ls_root(i, "p", (i % 600) as u32);
                r["inputs"] = json!({"q": "x".repeat(400)});
                r
            })
            .collect();
        // A small memtable answers "busy" while it flushes; retry as the SDK does.
        loop {
            let r = reqwest::Client::new()
                .post(server.url("/runs/batch"))
                .json(&json!({"post": posts}))
                .send()
                .await
                .unwrap();
            match r.status().as_u16() {
                202 => break,
                429 => tokio::time::sleep(Duration::from_millis(20)).await,
                s => panic!("upload: {s}"),
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while server.state.ingest.health().runs > 0 {
        assert!(Instant::now() < deadline, "not flushed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_deleted_while_serving_is_skipped_and_reported() {
    let n = 200u64;
    let server = flushed(n).await;
    let (lost, kept_run) = {
        let mem = server.state.ingest.mem();
        let m = mem.read().unwrap();
        assert!(m.version.segments.len() >= 2);
        (m.version.segments[0].clone(), m.version.segments[1].min_trace_id)
    };
    let (status, page) = query(&server, json!({"count": true})).await;
    assert_eq!(status, 200);
    assert_eq!(page["stats"]["matched"], n);

    std::fs::remove_file(server.dir.path().join("data").join(&lost.core_path)).unwrap();

    let (status, page) = query(&server, json!({"count": true})).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(skipped(&page), vec![lost.id.clone()]);
    assert_eq!(
        page["stats"]["matched"],
        n - lost.row_count,
        "answered from the other files"
    );

    let (status, h) = get(&server, "/healthz").await;
    assert_eq!(status, 200);
    assert_eq!(h["ok"], false);
    assert_eq!(h["damaged_segments"][0]["id"], lost.id);
    assert_eq!(h["damaged_segments"][0]["reason"], "missing");
    assert!(h["damaged_segments"][0]["since"].is_string());
    assert_eq!(h["corrupt_segments"], json!([lost.id]));

    // Later queries skip it without failing first, and still say so.
    let (status, page) = query(&server, json!({"filter": "eq(name, \"root\")", "limit": 5})).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(skipped(&page), vec![lost.id.clone()]);

    // A trace in another file answers whole; one only in the lost file isn't found, and says why.
    let (status, t) = get(&server, &format!("/api/traces/{kept_run}")).await;
    assert_eq!(status, 200, "{t}");
    assert_eq!(t["run_count"], 1);
    assert!(skipped_files(&t).is_empty(), "{t}");
    let (status, r) = get(&server, &format!("/api/runs/{kept_run}?include=inputs")).await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["inputs"]["q"], "x".repeat(400));
    assert!(skipped_files(&r).is_empty(), "{r}");
    let (status, t) = get(&server, &format!("/api/traces/{}", lost.min_trace_id)).await;
    assert_eq!(status, 404, "{t}");
    assert_eq!(skipped_files(&t), vec![lost.id.clone()]);
    let (status, r) = get(&server, &format!("/api/runs/{}", lost.min_trace_id)).await;
    assert_eq!(status, 404, "{r}");
    assert_eq!(skipped_files(&r), vec![lost.id.clone()]);
    let (status, f) = get(&server, "/api/fields").await;
    assert_eq!(status, 200, "{f}");
    assert_eq!(skipped_files(&f), vec![lost.id.clone()]);
    let (status, v) = get(&server, "/api/fields/values?path=name").await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["values"][0]["count"], n - lost.row_count);
    assert_eq!(skipped_files(&v), vec![lost.id.clone()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_damaged_past_its_footer_is_found_by_reading_files_alone() {
    let n = 200u64;
    let server = flushed(n).await;
    let bad = server.state.ingest.mem().read().unwrap().version.segments[1].clone();
    let path = server.dir.path().join("data").join(&bad.core_path);
    let mut bytes = std::fs::read(&path).unwrap();
    let mid = bytes.len() / 3;
    bytes[mid..mid + 256].fill(0xA5);
    std::fs::write(&path, &bytes).unwrap();

    let (status, page) = query(&server, json!({"count": true, "columns": ["inputs.q"]})).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(skipped(&page), vec![bad.id.clone()]);
    assert_eq!(page["stats"]["matched"], n - bad.row_count);
    let (_, h) = get(&server, "/healthz").await;
    assert_eq!(h["damaged_segments"][0]["reason"], "unreadable", "{h}");
}
