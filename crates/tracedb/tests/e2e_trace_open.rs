//! A 250-step trace with 40 KB payloads opens fast (SC-03): the tree API returns headers only.

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_250_step_trace_tree_returns_in_under_200ms() {
    let server = start().await;
    post_batch(&server, vec![ls_root(1, "p", 5)], vec![]).await;
    for chunk in (0..249u64).collect::<Vec<_>>().chunks(50) {
        let items: Vec<Value> = chunk
            .iter()
            .map(|i| {
                let mut c = ls_child(1, 5, 100 + i, "p");
                c["inputs"] = json!({"page": "x".repeat(40 * 1024)});
                c
            })
            .collect();
        post_batch(&server, items, vec![]).await;
    }

    let http = reqwest::Client::new();
    let url = server.url(&format!("/api/traces/{}", uid(1)));
    http.get(&url).send().await.unwrap();
    let mut times = Vec::new();
    for _ in 0..10 {
        let t = Instant::now();
        let r = http.get(&url).send().await.unwrap();
        let body: Value = r.json().await.unwrap();
        times.push(t.elapsed());
        assert_eq!(body["run_count"], 250);
        let size = serde_json::to_vec(&body).unwrap().len();
        assert!(size < 1024 * 1024, "headers only: {size} bytes for 250 steps");
    }
    times.sort();
    let median = times[times.len() / 2];
    assert!(median < Duration::from_millis(200), "median {median:?}");
}
