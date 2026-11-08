//! `GET /api/aggregate/stream` (M3 T077): `estimate` (`partial: true`, `sampled_pct`) then `result`, each with
//! the request's `query_id`; a bad request or failure is one last `error {code, message}`; closing the
//! connection cancels the computation.

use smithling_server::testkit as common;

use std::time::{Duration, Instant};

use common::*;
use futures_util::StreamExt;
use serde_json::{Value, json};
use smithling_query::aggregate::{StreamOpts, progressive::stream_counts};

/// Serializes the tests here: they read the process-wide stream counts.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn path(body: &Value) -> String {
    let q: String = url::form_urlencoded::byte_serialize(body.to_string().as_bytes()).collect();
    format!("/api/aggregate/stream?q={q}")
}

/// (event, data) frames until the server ends the stream.
async fn frames(resp: reqwest::Response) -> Vec<(String, Value)> {
    let text = resp.text().await.unwrap();
    text.split("\n\n")
        .filter_map(|f| {
            let ev = f.lines().find_map(|l| l.strip_prefix("event:"))?.trim().to_string();
            let data: String = f
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(str::trim)
                .collect();
            Some((ev, serde_json::from_str(&data).unwrap()))
        })
        .collect()
}

async fn sampled() -> Server {
    start_state(
        |c| c.limits.memtable_bytes = 16 * 1024,
        |s| {
            s.agg_stream = StreamOpts {
                min_rows: 0,
                sample: 0.25,
                budget: Duration::from_secs(30),
                fill_wait: Duration::from_secs(30),
            }
        },
    )
    .await
}

async fn load(server: &Server, traces: u64) {
    for chunk in (0..traces).collect::<Vec<_>>().chunks(10) {
        let mut runs = vec![];
        for &t in chunk {
            let root = 1 + t * 10;
            runs.push(ls_root(root, "p", (t % 600) as u32));
            for k in 1..6 {
                let mut c = ls_child(root, (t % 600) as u32, root + k, "p");
                c["name"] = json!(["fetch_url", "plan", "llm_call"][(k % 3) as usize]);
                c["end_time"] = c["start_time"].clone();
                if (t + k) % 7 == 0 {
                    c["error"] = json!("boom");
                }
                runs.push(c);
            }
        }
        loop {
            let r = reqwest::Client::new()
                .post(server.url("/runs/batch"))
                .json(&json!({"post": runs, "patch": []}))
                .send()
                .await
                .unwrap();
            match r.status().as_u16() {
                202 => break,
                429 => tokio::time::sleep(Duration::from_millis(20)).await,
                s => panic!("{s}"),
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while server.state.ingest.health().frozen {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_estimate_then_the_result_with_one_query_id() {
    let _one = SERIAL.lock().await;
    let server = sampled().await;
    load(&server, 400).await;
    assert!(server.state.ingest.health().segments >= 8);
    let body = json!({"project": "p", "group_by": ["name"], "metrics": ["count", "error_rate", "p95(latency)"]});
    let resp = reqwest::get(server.url(&path(&body))).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let got = frames(resp).await;
    let names: Vec<&str> = got.iter().map(|(e, _)| e.as_str()).collect();
    assert_eq!(names, ["estimate", "result"], "{got:?}");
    let (est, res) = (&got[0].1, &got[1].1);
    assert_eq!(est["partial"], true);
    let pct = est["sampled_pct"].as_f64().unwrap();
    assert!(pct > 0.0 && pct < 100.0, "{pct}");
    assert!(
        est["groups"]
            .as_array()
            .unwrap()
            .iter()
            .all(|g| g["cells"]["estimated"] == true)
    );
    assert_eq!(est["totals"]["estimated"], true);
    assert_eq!(res["partial"], false);
    assert!(res.get("sampled_pct").is_none());
    assert_eq!(res["totals"]["runs"], 2400);
    assert!(est["query_id"].as_str().unwrap().starts_with("q-"));
    assert_eq!(est["query_id"], res["query_id"]);
    // The same as POST /api/aggregate, which is always exact.
    let posted: Value = reqwest::Client::new()
        .post(server.url("/api/aggregate"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(posted["groups"], res["groups"]);
}

#[tokio::test]
async fn a_bad_request_is_one_error_event() {
    let _one = SERIAL.lock().await;
    let server = start().await;
    for (body, code) in [
        (json!({"filter": "eq(name", "metrics": ["count"]}), "bad_filter"),
        (json!({"metrics": ["median(latency)"]}), "bad_stats"),
        (json!({"metrics": ["count"], "colour": 1}), "bad_request"),
    ] {
        let resp = reqwest::get(server.url(&path(&body))).await.unwrap();
        assert_eq!(resp.status(), 200);
        let got = frames(resp).await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(
            (got[0].0.as_str(), got[0].1["code"].as_str()),
            ("error", Some(code)),
            "{got:?}"
        );
        assert!(got[0].1["message"].as_str().is_some_and(|m| !m.is_empty()));
        assert!(got[0].1.get("query_id").is_some());
    }
    for p in ["/api/aggregate/stream?q=nope", "/api/aggregate/stream"] {
        let got = frames(reqwest::get(server.url(p)).await.unwrap()).await;
        assert_eq!(got[0].1["code"], "bad_request", "{p}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_connection_cancels_the_computation() {
    let _one = SERIAL.lock().await;
    let server = start_state(
        |c| c.limits.memtable_bytes = 64 * 1024,
        |s| s.agg_stream.min_rows = u64::MAX,
    )
    .await;
    load(&server, 3000).await;
    let (running, cancelled) = stream_counts();
    assert_eq!(running, 0);
    // An inexact filter: every row is decoded and evaluated.
    let body = json!({"filter": r#"or(neq(error, null), eq(metadata.k, "x"))"#, "group_by": ["name"],
                      "metrics": ["count", "p95(latency)"]});
    let mut resp = reqwest::get(server.url(&path(&body))).await.unwrap().bytes_stream();
    let deadline = Instant::now() + Duration::from_secs(10);
    while stream_counts().0 == 0 {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    drop(resp.next());
    drop(resp);
    while stream_counts().0 > 0 {
        assert!(Instant::now() < deadline, "the computation kept running");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        stream_counts().1,
        cancelled + 1,
        "it stopped because the stream was dropped"
    );
}
