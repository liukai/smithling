//! Live views for any query (spec Story 4, design §4.5): the query re-run over the traces that changed.
//! M1's `?project=` / `?trace=` tails are covered by `e2e_tail.rs`.

use smithling_server::testkit as common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};

fn live(q: Value) -> String {
    let enc: String = q
        .to_string()
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("/api/tail?q={enc}")
}

/// Waits (generously) for the next message of `event`, skipping others.
async fn next(rx: &mut tokio::sync::mpsc::UnboundedReceiver<SseMsg>, event: &str) -> SseMsg {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let m = tokio::time::timeout(left, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("no {event} message"))
            .unwrap();
        if m.event == event {
            return m;
        }
    }
}

fn ids(m: &SseMsg) -> Vec<String> {
    m.data
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().or(r["run_id"].as_str()).unwrap().to_string())
        .collect()
}

fn ended(mut v: Value, error: Option<&str>) -> Value {
    v["end_time"] = json!("2026-11-05T03:00:00.000000");
    if let Some(e) = error {
        v["error"] = json!(e);
    }
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn matching_runs_arrive_within_a_second_and_others_are_ignored() {
    let server = start().await;
    let mut rx = sse(
        &server,
        &live(json!({"filter": "and(eq(status, \"error\"), eq(name, \"fetch_url\"))"})),
    )
    .await;

    let named = |n: u64, name: &str| {
        let mut v = ls_root(n, "p", n as u32);
        v["name"] = json!(name);
        v
    };
    let quiet = vec![
        ended(named(1, "fetch_url"), None),
        ended(named(2, "other"), Some("boom")),
        named(3, "fetch_url"),
    ];
    post_batch(&server, quiet, vec![]).await;
    post_batch(&server, vec![ended(named(4, "fetch_url"), Some("boom"))], vec![]).await;
    let confirmed = Instant::now();

    let m = next(&mut rx, "runs").await;
    assert!(
        m.at.duration_since(confirmed) < Duration::from_secs(1),
        "{:?}",
        m.at - confirmed
    );
    assert_eq!(ids(&m), vec![uid(4)], "only the matching run");
    assert_eq!(m.data[0]["status"], "error");
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(m) = rx.try_recv() {
        assert!(m.event != "runs" || ids(&m) == vec![uid(4)], "{} {}", m.event, m.data);
        assert_ne!(
            m.event, "left",
            "runs the tab never had aren't sent as left: {}",
            m.data
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_that_stops_matching_is_sent_as_left() {
    let server = start().await;
    post_batch(&server, vec![ls_root(3, "p", 3)], vec![]).await;
    let mut rx = sse(
        &server,
        &live(json!({"project": "p", "filter": "eq(status, \"running\")"})),
    )
    .await;
    post_batch(&server, vec![ls_root(1, "p", 1), ls_root(2, "p", 2)], vec![]).await;
    let mut seen = Vec::new();
    while seen.len() < 2 {
        seen.extend(ids(&next(&mut rx, "runs").await));
    }
    seen.sort();
    assert_eq!(seen, vec![uid(1), uid(2)]);

    post_batch(&server, vec![], vec![ended(ls_root(1, "p", 1), None)]).await;
    let m = next(&mut rx, "left").await;
    assert_eq!(ids(&m), vec![uid(1)], "only the run that stopped matching");

    post_batch(&server, vec![], vec![ended(ls_root(3, "p", 3), None)]).await;
    let m = next(&mut rx, "left").await;
    assert_eq!(ids(&m), vec![uid(3)], "a run the tab loaded itself");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quiet_failure_view_gets_the_root_when_a_child_errors_later() {
    let server = start().await;
    let q = json!({"is_root": true, "filter": "eq(status, \"ok\")", "tree_filter": "eq(status, \"error\")"});
    let tree = |n: u64| {
        vec![
            ended(ls_root(n, "p", n as u32), None),
            ls_child(n, n as u32, n * 10 + 1, "p"),
        ]
    };
    post_batch(&server, tree(1), vec![]).await;
    let mut rx = sse(&server, &live(q)).await;
    post_batch(&server, tree(2), vec![]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(m) = rx.try_recv() {
        assert_ne!(m.event, "runs", "no child errored yet: {}", m.data);
    }
    let failed = |n: u64| ended(ls_child(n, n as u32, n * 10 + 1, "p"), Some("boom"));
    post_batch(&server, vec![], vec![failed(1), failed(2)]).await;
    let mut seen = Vec::new();
    while seen.len() < 2 {
        seen.extend(ids(&next(&mut rx, "runs").await));
    }
    seen.sort();
    assert_eq!(
        seen,
        vec![uid(1), uid(2)],
        "the ok roots whose child errored, loaded before the view or not"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn too_many_changed_traces_send_resync() {
    // A hub bigger than the burst: the tab gets `resync` for the changed traces, not for lagging.
    let server = start_with(|c| c.hub_capacity = 40_000).await;
    let mut rx = sse(&server, &live(json!({"project": "p"}))).await;
    let items: Vec<_> = (1..=12_000).map(|n| ls_root(n, "p", (n % 600) as u32)).collect();
    post_batch(&server, items, vec![]).await;
    next(&mut rx, "resync").await;
}

#[tokio::test]
async fn a_bad_query_is_rejected() {
    let server = start().await;
    for path in [live(json!({"filter": "eq(staus, 1)"})), "/api/tail?q=nope".to_string()] {
        let r = reqwest::get(server.url(&path)).await.unwrap();
        assert_eq!(r.status(), 400, "{path}");
    }
}
