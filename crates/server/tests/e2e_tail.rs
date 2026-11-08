//! Live tail: confirmed runs reach an open tab within 1 s (SC-02); bursts are collapsed (design §4.3).

use tracedb_server::testkit as common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmed_runs_reach_the_tab_within_a_second() {
    let server = start().await;
    let mut rx = sse(&server, "/api/tail?project=p").await;

    let mut confirmed: HashMap<String, Instant> = HashMap::new();
    for batch in 0..50u64 {
        let items: Vec<_> = (0..20)
            .map(|i| ls_root(batch * 20 + i + 1, "p", (i % 60) as u32))
            .collect();
        post_batch(&server, items, vec![]).await;
        let now = Instant::now();
        for i in 0..20 {
            confirmed.insert(uid(batch * 20 + i + 1), now);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut seen: HashMap<String, Instant> = HashMap::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while seen.len() < 1000 && Instant::now() < deadline {
        if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await
            && m.event == "runs"
        {
            for r in m.data.as_array().unwrap() {
                seen.entry(r["run_id"].as_str().unwrap().to_string()).or_insert(m.at);
            }
        }
    }
    assert_eq!(seen.len(), 1000, "every confirmed run reached the tab");
    let mut lat: Vec<Duration> = confirmed
        .iter()
        .map(|(id, c)| seen[id].saturating_duration_since(*c))
        .collect();
    lat.sort();
    let p99 = lat[lat.len() * 99 / 100];
    assert!(p99 < Duration::from_secs(1), "p99 confirm→visible = {p99:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_of_updates_to_one_run_is_collapsed() {
    let server = start().await;
    post_batch(&server, vec![ls_root(1, "p", 1)], vec![]).await;
    let mut rx = sse(&server, "/api/tail?project=p").await;

    let started = Instant::now();
    for i in 0..1000u32 {
        let mut upd = ls_root(1, "p", 1);
        upd["tags"] = serde_json::json!([format!("v{i}")]);
        post_batch(&server, vec![], vec![upd]).await;
    }
    let burst = started.elapsed();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let (mut messages, mut last_tag) = (0, String::new());
    while let Ok(m) = rx.try_recv() {
        if m.event == "runs" {
            messages += 1;
            last_tag = m.data[0]["tags"][0].as_str().unwrap().to_string();
        }
    }
    let secs = burst.as_secs_f64().max(1.0) + 0.3;
    assert!(
        messages as f64 <= 10.0 * secs + 2.0,
        "{messages} messages for a {burst:?} burst"
    );
    assert_eq!(last_tag, "v999", "the newest version always arrives");
}

#[tokio::test]
async fn trace_subscription_sees_children_and_ignores_other_traces() {
    let server = start().await;
    post_batch(&server, vec![ls_root(1, "p", 1), ls_root(2, "p", 2)], vec![]).await;
    let mut rx = sse(&server, &format!("/api/tail?trace={}", uid(1))).await;
    post_batch(&server, vec![ls_child(1, 1, 11, "p"), ls_child(2, 2, 21, "p")], vec![]).await;
    let m = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.event, "runs");
    let ids: Vec<&str> = m
        .data
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["run_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![uid(11).as_str()]);
}
