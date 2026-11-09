//! Compaction never changes an answer (M4 SC-02): two stores get the same events; one is compacted. Lists, trace
//! trees, single runs with payloads, field counts and aggregates match after the swap, after a late update to
//! a compacted run, and after a restart. Memory follows: merged runs leave the multi-version set.

mod common;

use common::*;
use serde_json::Value;
use smithling_ingest::Include;
use smithling_ingest::Ingest;
use smithling_model::Op;
use smithling_model::agg::{AggSpec, GroupKey, Metric};
use smithling_model::cost::Prices;
use smithling_model::filter::parse_filter;
use smithling_query::{Query, QuerySpec, TimeRange};

const MEMTABLE: u64 = 16 * 1024;

/// Drops what depends on how the answer was computed (stats, ids of the request), not on what it is.
fn strip(mut v: Value) -> Value {
    match &mut v {
        Value::Object(m) => {
            for k in ["stats", "query_id", "took_ms", "ms"] {
                m.remove(k);
            }
            for x in m.values_mut() {
                *x = strip(x.take());
            }
        }
        Value::Array(a) => {
            for x in a.iter_mut() {
                *x = strip(x.take());
            }
        }
        _ => {}
    }
    v
}

fn spec(filter: Option<&str>, is_root: Option<bool>) -> QuerySpec {
    QuerySpec {
        project: Some("p".into()),
        filter: filter.map(|f| parse_filter(f).unwrap()),
        is_root,
        limit: Some(1_000),
        count: true,
        ..QuerySpec::default()
    }
}

async fn answers(ingest: &Ingest) -> Vec<(String, Value)> {
    let q = Query::new(ingest.clone());
    let mut out = Vec::new();
    for (label, s) in [
        ("all runs", spec(None, None)),
        ("failing roots", spec(Some(r#"eq(status, "error")"#), Some(true))),
        ("running", spec(Some(r#"eq(status, "running")"#), None)),
        ("tagged a", spec(Some(r#"has(tags, "batch=a")"#), None)),
        ("search boom", spec(Some(r#"search(error, "boom")"#), None)),
        ("search very late", spec(Some(r#"search("very late")"#), None)),
    ] {
        let page = q.query(&s).await.unwrap();
        out.push((label.to_string(), strip(serde_json::to_value(&page).unwrap())));
    }
    for n in [1u64, 2, 7, 30, 61] {
        let t = q.trace(uid(n)).await.unwrap();
        out.push((format!("trace {n}"), strip(serde_json::to_value(&t).unwrap())));
        let all = Include {
            inputs: true,
            outputs: true,
            events: true,
        };
        let r = q.run(uid(n + 1), all).await.unwrap();
        out.push((format!("run {}", n + 1), strip(serde_json::to_value(&r).unwrap())));
    }
    let f = q.fields(Some("p"), TimeRange::default(), "").await.unwrap();
    out.push(("fields".into(), strip(serde_json::to_value(&f).unwrap())));
    let agg = AggSpec {
        group_by: vec![GroupKey::Name],
        metrics: vec![Metric::Count, Metric::ErrorRate],
        ..AggSpec::default()
    };
    let a = q.aggregate(&spec(None, None), &agg, &Prices::embedded()).await.unwrap();
    out.push(("aggregate by name".into(), strip(serde_json::to_value(&a).unwrap())));
    out
}

fn same(got: &[(String, Value)], want: &[(String, Value)], pass: &str) {
    assert_eq!(got.len(), want.len());
    for ((label, g), (_, w)) in got.iter().zip(want) {
        assert_eq!(g, w, "{pass}: {label}");
    }
}

async fn compact(ingest: &Ingest) -> smithling_ingest::compact::CompactReport {
    let i = ingest.clone();
    tokio::task::spawn_blocking(move || i.compact(true))
        .await
        .unwrap()
        .unwrap()
        .expect("something to compact")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_answer_is_the_same_before_and_after_compaction() {
    let (_dx, x) = loaded(7, 120, MEMTABLE).await;
    let (_dy, y) = loaded(7, 120, MEMTABLE).await;
    let before = x.health();
    assert!(
        before.files.l0 > 4,
        "the load wrote many small files: {:?}",
        before.files
    );
    assert!(
        x.multi_version().read().unwrap().len() > 10,
        "late updates left multi-version runs"
    );
    same(&answers(&x).await, &answers(&y).await, "loaded");

    // A query holding the pre-swap file set keeps the merged runs in the multi-version set until it lets go;
    // aggregates run meanwhile rebuild their kept rows with them, and must not count them twice afterwards.
    let held = x.mem().read().unwrap().version.clone();
    let rep = compact(&x).await;
    assert!(
        !x.multi_version().read().unwrap().is_empty(),
        "the shrink waits for the held file set"
    );
    same(&answers(&x).await, &answers(&y).await, "compacted, old file set held");
    drop(held);
    let i = x.clone();
    assert!(
        tokio::task::spawn_blocking(move || i.compact(true))
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    let after = x.health();
    assert_eq!(rep.inputs, before.files.l0);
    assert_eq!(after.files.l0, 0);
    // One file per project per hour of run start: the scenario's two batches span 12 h and 10 h (plus spill).
    assert!((22..=24).contains(&after.files.l1), "{:?}", after.files);
    assert_eq!(after.files.replaced, before.files.l0);
    assert_eq!(after.segment_rows, rep.rows_out);
    assert!(rep.rows_out < rep.rows_in, "versions were merged: {rep:?}");
    same(&answers(&x).await, &answers(&y).await, "compacted");
    assert!(
        x.multi_version().read().unwrap().is_empty(),
        "every merged run left the multi-version set"
    );

    // Very late updates to compacted runs: shown at once, then a second job files them in small extra files of
    // their old windows, marked, so they keep merging at query time (option a).
    let qy = Query::new(y.clone());
    let mut late = Vec::new();
    for n in [3u64, 4, 5, 40, 41] {
        let h = qy
            .run(uid(n), Include::default())
            .await
            .unwrap()
            .expect("run exists")
            .header;
        late.push((
            Op::Patch,
            smithling_model::RunFields {
                id: Some(h.run_id),
                trace_id: Some(h.trace_id),
                dotted_order: Some(h.dotted_order),
                error: Some("very late".into()),
                tags: Some(vec!["late-fix".into()]),
                ..Default::default()
            },
        ));
    }
    send(&x, late.clone()).await;
    send(&y, late).await;
    same(&answers(&x).await, &answers(&y).await, "late updates in memory");
    for ingest in [&x, &y] {
        filler(ingest).await;
    }
    same(&answers(&x).await, &answers(&y).await, "late updates flushed");
    compact(&x).await;
    let h = x.health();
    assert_eq!(h.files.l0, 0);
    assert!(
        x.multi_version().read().unwrap().len() >= 5,
        "runs with a row in two compacted files stay multi-version"
    );
    same(&answers(&x).await, &answers(&y).await, "late updates compacted");

    // A restart rebuilds the same state from the files.
    let (dx, dy) = (_dx, _dy);
    drop(qy);
    drop(x);
    drop(y);
    let (x, y) = (reopen(dx.path()), reopen(dy.path()));
    same(&answers(&x).await, &answers(&y).await, "after restart");
}

/// Fills the memtable with runs of another project, so everything before them is flushed.
async fn filler(ingest: &Ingest) {
    let runs = (0..400)
        .map(|i| (Op::Post, ended(&root(900_000 + i, "filler", i), None)))
        .collect();
    send(ingest, runs).await;
    settle(ingest, MEMTABLE).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while ingest.health().runs > 450 {
        assert!(std::time::Instant::now() < deadline, "flush did not finish");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn reopen(dir: &std::path::Path) -> Ingest {
    let mut cfg = smithling_ingest::IngestConfig::new(dir);
    cfg.limits = smithling_ingest::Limits {
        memtable_bytes: MEMTABLE,
        dedup_window_us: 0,
        ..smithling_ingest::Limits::default()
    };
    Ingest::open(cfg).unwrap().0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_background_job_compacts_on_its_own_and_outlives_a_panic() {
    let (_dx, x) = open_with(MEMTABLE, |c| {
        c.compact.enabled = true;
        c.compact.interval = std::time::Duration::from_millis(300);
        c.compact.test_panics = 1;
    });
    let (_dy, y) = open(MEMTABLE);
    for i in [&x, &y] {
        send(i, batches(7, 120)).await;
        settle(i, MEMTABLE).await;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let h = x.health();
        assert!(!h.compaction.stopped, "the thread outlives the panic");
        if h.compaction.jobs > 0 && x.compact(false).unwrap().is_none() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background compaction never caught up: {h:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(
        x.health().compaction.panics,
        1,
        "the panicking job is counted in /healthz"
    );
    assert!(x.health().files.l1 > 0);
    same(&answers(&x).await, &answers(&y).await, "compacted in the background");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_does_not_wait_for_the_next_tick() {
    let (_d, x) = open_with(MEMTABLE, |c| c.compact.enabled = true);
    let t = std::time::Instant::now();
    tokio::task::spawn_blocking(move || drop(x)).await.unwrap();
    assert!(
        t.elapsed() < std::time::Duration::from_secs(2),
        "took {:?}",
        t.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replaced_files_stay_for_the_grace_period_then_go_even_after_a_restart() {
    let (dx, x) = loaded(7, 40, MEMTABLE).await;
    let before = answers(&x).await;
    compact(&x).await;
    let replaced = x.health().files.replaced;
    assert!(replaced > 0);
    assert_eq!(x.delete_replaced().unwrap(), 0, "24 h grace period by default");
    drop(x);

    // The replaced rows outlive a restart; with no grace period they go on the next pass, files then rows.
    let mut cfg = smithling_ingest::IngestConfig::new(dx.path());
    cfg.limits = smithling_ingest::Limits {
        memtable_bytes: MEMTABLE,
        dedup_window_us: 0,
        ..smithling_ingest::Limits::default()
    };
    cfg.compact.grace_us = 0;
    let x = Ingest::open(cfg).unwrap().0;
    assert_eq!(x.health().files.replaced, replaced);
    assert_eq!(x.delete_replaced().unwrap(), replaced);
    assert_eq!(x.health().files.replaced, 0);
    same(&answers(&x).await, &before, "after deleting the replaced files");
    drop(x);
    let x = reopen(dx.path());
    assert_eq!(x.health().files.replaced, 0, "the catalog rows went too");
    same(&answers(&x).await, &before, "after a restart");
}
