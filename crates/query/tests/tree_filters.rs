//! Conditions on a run's root (`trace_filter`) and on any run in its trace (`tree_filter`), across memtables
//! and data files: the overnight-eval questions "which roots failed" and "which roots look ok but hid an error".

mod common;

use common::*;
use tracedb_model::Op;
use tracedb_model::filter::{TimeAt, parse_filter};
use tracedb_query::{Query, QuerySpec, TimeRange};

fn spec(root: Option<bool>, filter: &str, trace: Option<&str>, tree: Option<&str>) -> QuerySpec {
    QuerySpec {
        project: Some("eval".into()),
        is_root: root,
        filter: Some(parse_filter(filter).unwrap()),
        trace_filter: trace.map(|f| parse_filter(f).unwrap()),
        tree_filter: tree.map(|f| parse_filter(f).unwrap()),
        limit: Some(200),
        ..QuerySpec::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finds_failing_roots_and_quiet_failures_across_files_and_memtable() {
    let (_dir, ingest) = open(16 * 1024);
    let (mut events, mut late) = (Vec::new(), Vec::new());
    let mut n = 1;
    for (project, roots, quiet_from, quiet_to) in [("eval", 200, 14, 37), ("other", 20, 0, 5)] {
        for i in 0..roots {
            let r = root(n, project, i * 10);
            let kids = [child(&r, n + 1, i * 10 + 1), child(&r, n + 2, i * 10 + 2)];
            n += 3;
            let root_error = (project == "eval" && i < 14).then_some("task failed");
            events.push((Op::Post, ended(&r, root_error)));
            for (k, c) in kids.iter().enumerate() {
                let hidden = k == 1 && (quiet_from..quiet_to).contains(&i);
                let error = (hidden || (root_error.is_some() && i % 2 == 0)).then_some("HTTPError: 429");
                if hidden && i % 2 == 1 {
                    // The step's error arrives after its root finished and was flushed.
                    events.push((Op::Post, c.clone()));
                    late.push((Op::Patch, end_update(c, error)));
                } else {
                    events.push((Op::Post, ended(c, error)));
                }
            }
        }
    }
    send(&ingest, events).await;
    settle(&ingest, 16 * 1024).await;
    send(&ingest, late).await;
    assert!(ingest.health().segments > 1, "most runs must be in data files");
    assert!(ingest.health().runs > 0, "late updates stay in the memtable");

    let q = Query::new(ingest.clone());
    let failing = q
        .query(&spec(Some(true), r#"eq(status, "error")"#, None, None))
        .await
        .unwrap();
    assert_eq!(failing.runs.len(), 14, "failing roots");
    let quiet = q
        .query(&spec(
            Some(true),
            r#"eq(status, "ok")"#,
            None,
            Some(r#"eq(status, "error")"#),
        ))
        .await
        .unwrap();
    assert_eq!(quiet.runs.len(), 23, "quiet failures");
    assert!(
        quiet
            .runs
            .iter()
            .all(|r| r.header.is_root && r.header.project == "eval")
    );
    let hidden_steps = q
        .query(&spec(
            Some(false),
            r#"eq(status, "error")"#,
            Some(r#"eq(status, "ok")"#),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(hidden_steps.runs.len(), 23, "errored steps under ok roots");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trace_conditions_over_many_candidates_read_only_those_traces() {
    // 6,300 older traces mixed with 2,100 inside the window (more candidates than one `trace_id IN (…)` list),
    // so every file holds both.
    let (_dir, ingest) = open(512 * 1024);
    let mut events = Vec::new();
    for i in 0..8_400u64 {
        let sec = if i % 4 == 0 { 7_200 + i / 40 } else { i / 10 };
        let r = root(i * 2 + 1, "p", sec);
        events.push((Op::Post, ended(&r, None)));
        events.push((Op::Post, ended(&child(&r, i * 2 + 2, sec + 1), Some("boom"))));
    }
    send(&ingest, events).await;
    settle(&ingest, 512 * 1024).await;
    assert!(ingest.health().segments > 1);

    let q = Query::new(ingest.clone());
    let page = q
        .query(&QuerySpec {
            project: Some("p".into()),
            time: TimeRange {
                from: TimeAt::parse("2026-11-05T02:00:00Z"),
                to: None,
            },
            is_root: Some(true),
            tree_filter: Some(parse_filter(r#"eq(status, "error")"#).unwrap()),
            limit: Some(10),
            count: true,
            ..QuerySpec::default()
        })
        .await
        .unwrap();
    assert_eq!(page.stats.matched, 2_100);
    // Roots in the window, plus their errored steps; nothing from the 6,000 older traces.
    assert!(
        page.stats.rows_read <= 2 * 2_100 + 500,
        "read {} file rows",
        page.stats.rows_read
    );
}
