//! The overnight-eval scenario (research M2-R14): deterministic, exact planted counts, and loaded through
//! `/runs/batch` so its checks cross memtable and files.

mod common;

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use tracedb::scenario::{self, Scenario, generate, load};
use tracedb_model::filter::parse_filter;
use tracedb_query::QuerySpec;

const BASE: i64 = 1_762_308_000_000_000;
const MB: u64 = 1024 * 1024;

/// Final state per run id: the post with its patch applied.
fn merged(s: &Scenario) -> HashMap<String, Value> {
    let mut runs: HashMap<String, Value> = HashMap::new();
    for item in s.posts.iter().chain(&s.background) {
        runs.insert(item["id"].as_str().unwrap().into(), item.clone());
    }
    for (_, p) in &s.late {
        let r = runs
            .get_mut(p["id"].as_str().unwrap())
            .expect("a patch follows its post");
        for (k, v) in p.as_object().unwrap() {
            r[k] = v.clone();
        }
    }
    runs
}

fn tagged(r: &Value) -> bool {
    r["tags"]
        .as_array()
        .is_some_and(|t| t.iter().any(|t| t == scenario::BATCH_TAG))
}

#[test]
fn same_seed_same_data() {
    let (a, b, c) = (generate(7, BASE, MB), generate(7, BASE, MB), generate(8, BASE, MB));
    assert_eq!(a.posts, b.posts);
    assert_eq!(a.late, b.late);
    assert_eq!(a.background, b.background);
    assert_ne!(a.posts, c.posts);
}

#[test]
fn planted_counts_are_exact() {
    for seed in [7, 8, 42] {
        let s = generate(seed, BASE, MB);
        let runs = merged(&s);
        let roots: Vec<&Value> = runs
            .values()
            .filter(|r| r["parent_run_id"].is_null() && tagged(r))
            .collect();
        assert_eq!(roots.len(), 200, "seed {seed}");
        let errored = |r: &Value| !r["error"].is_null();
        let mut trace_errored: HashSet<&str> = HashSet::new();
        for r in runs.values().filter(|r| errored(r)) {
            trace_errored.insert(r["trace_id"].as_str().unwrap());
        }
        let failing: HashSet<String> = roots
            .iter()
            .filter(|r| errored(r))
            .map(|r| r["id"].as_str().unwrap().into())
            .collect();
        let quiet: HashSet<String> = roots
            .iter()
            .filter(|r| !errored(r) && !r["end_time"].is_null())
            .filter(|r| trace_errored.contains(r["trace_id"].as_str().unwrap()))
            .map(|r| r["id"].as_str().unwrap().into())
            .collect();
        assert_eq!(failing.len(), scenario::FAILING_ROOTS, "seed {seed}");
        assert_eq!(quiet.len(), scenario::QUIET_FAILURES, "seed {seed}");
        assert_eq!(failing, s.failing.iter().map(|u| u.to_string()).collect());
        assert_eq!(quiet, s.quiet.iter().map(|u| u.to_string()).collect());
        assert!(roots.iter().all(|r| !r["end_time"].is_null()), "every root finishes");

        let pv = |r: &Value| r["extra"]["metadata"]["prompt_version"].as_str().map(str::to_string);
        let count = |v: &str| runs.values().filter(|r| pv(r).as_deref() == Some(v)).count();
        assert_eq!(runs.values().filter(|r| pv(r).is_some()).count(), 1840);
        assert_eq!((count("v6"), count("v7")), (928, 912), "seed {seed}");
        let v7_429 = runs
            .values()
            .filter(|r| pv(r).as_deref() == Some("v7") && r["outputs"]["status_code"] == 429)
            .count();
        assert_eq!(v7_429, scenario::V7_429, "seed {seed}");

        for (late, p) in &s.late {
            let secs = late.as_secs();
            assert!((30..=120).contains(&secs) || secs >= 40 * 60, "late by {secs} s");
            assert!(!p["end_time"].is_null());
        }
        assert!(s.late.iter().filter(|(d, _)| d.as_secs() >= 40 * 60).count() >= 3);
        assert!(
            s.late.windows(2).all(|w| w[0].0 <= w[1].0),
            "patches go out in arrival order"
        );
    }
}

#[test]
fn background_volume_covers_three_memtables() {
    let s = generate(7, BASE, 2 * MB);
    let bytes: u64 = s
        .background
        .iter()
        .map(|r| 512 + serde_json::to_string(&r["inputs"]).unwrap().len() as u64)
        .sum();
    assert!(bytes >= 3 * 2 * MB, "background {bytes} bytes");
    assert!(
        s.background
            .iter()
            .all(|r| r["extra"]["metadata"]["prompt_version"].is_null())
    );
    assert!(s.background.iter().all(|r| !tagged(r)));
}

#[tokio::test]
async fn loads_through_the_batch_api_across_memtable_and_files() {
    let server = common::start_with(|c| c.limits.memtable_bytes = MB).await;
    let s = generate(7, tracedb_model::Ts::now().0 - 6 * 3_600_000_000, MB);
    let loaded = load(&format!("http://{}", server.addr), &s).await.unwrap();
    let health = server.state.ingest.health();
    assert!(health.flushes >= 2, "flushes {}", health.flushes);
    assert_eq!(loaded.flushes, health.flushes);
    assert!(loaded.in_files);
    assert!(loaded.posts > s.posts.len() && loaded.posts <= s.posts.len() + s.background.len());
    assert_eq!(loaded.patches, s.late.len());

    let spec = QuerySpec {
        is_root: Some(true),
        filter: Some(parse_filter(r#"and(has(tags, "batch=2026-11-05-a"), eq(status, "error"))"#).unwrap()),
        limit: Some(200),
        ..QuerySpec::default()
    };
    let page = server.state.query.query(&spec).await.unwrap();
    assert_eq!(page.stats.matched, scenario::FAILING_ROOTS);
    let flushed = health.flushed_seq;
    assert!(
        page.runs
            .iter()
            .all(|r| r.header.first_seq <= flushed && r.header.last_seq > flushed),
        "each failing root started in a file and ended in the memtable"
    );
}

#[tokio::test]
async fn refuses_a_server_that_already_holds_the_batch() {
    let server = common::start_with(|c| c.limits.memtable_bytes = MB).await;
    let url = format!("http://{}", server.addr);
    let s = generate(7, tracedb_model::Ts::now().0 - 6 * 3_600_000_000, MB);
    load(&url, &s).await.unwrap();
    let err = load(&url, &s).await.unwrap_err();
    assert!(matches!(err, tracedb::seed::SeedError::Refused(..)), "{err}");
}

#[tokio::test]
async fn unreachable_server_is_reported() {
    let err = load("http://127.0.0.1:9", &generate(1, BASE, MB)).await.unwrap_err();
    assert!(matches!(err, tracedb::seed::SeedError::Unreachable(..)));
}
