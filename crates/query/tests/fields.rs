//! Field catalog (data-model §10): paths with types and counts, and a path's top values, summed over the data
//! files in scope plus the memtables.

mod common;

use common::*;
use serde_json::{Map, Value, json};
use smithling_model::Op;
use smithling_model::filter::TimeAt;
use smithling_query::{Query, TimeRange};

fn all() -> TimeRange {
    TimeRange::default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sums_files_and_memtable_within_scope() {
    let (_dir, ingest) = open(16 * 1024);
    let mut events: Vec<_> = (0..300)
        .map(|i| {
            let v = if i % 2 == 0 { "v7" } else { "v6" };
            (
                Op::Post,
                with_metadata(root(i + 1, "p", i), json!({"prompt_version": v})),
            )
        })
        .collect();
    events.push((
        Op::Post,
        with_metadata(root(1_000, "other", 5), json!({"other_key": 1})),
    ));
    send(&ingest, events).await;
    settle(&ingest, 16 * 1024).await;
    assert_eq!(ingest.health().runs, 0, "everything so far is in files");
    let files = ingest.health().segments;
    assert!(files > 1);
    // A key first seen now, still in the memtable.
    let today: Vec<_> = (0..10)
        .map(|i| {
            let m = json!({"prompt_version": "v7", "new_key": "x"});
            (Op::Post, with_metadata(root(2_000 + i, "p", 7_200 + i), m))
        })
        .collect();
    send(&ingest, today).await;
    assert_eq!((ingest.health().runs, ingest.health().segments), (10, files));

    let q = Query::new(ingest.clone());
    let f = q.fields(Some("p"), all(), "").await.unwrap();
    assert!(f.complete);
    let get = |path: &str| f.fields.iter().find(|x| x.path == path);
    let pv = get("metadata.prompt_version").expect("prompt_version listed");
    assert_eq!(
        (pv.types.len(), pv.types[0].ty.as_str(), pv.types[0].count),
        (1, "string", 310)
    );
    assert!(pv.complete);
    assert_eq!(get("metadata.new_key").map(|x| x.types[0].count), Some(10));
    assert!(get("metadata.other_key").is_none(), "other projects are out of scope");
    assert_eq!(get("status").map(|x| x.types[0].count), Some(310));
    let counts: Vec<u64> = f.fields.iter().map(|x| x.types.iter().map(|t| t.count).sum()).collect();
    assert!(counts.windows(2).all(|w| w[0] >= w[1]), "sorted by count");

    let only: Vec<_> = q.fields(Some("p"), all(), "metadata.n").await.unwrap().fields;
    assert_eq!(
        only.iter().map(|x| x.path.as_str()).collect::<Vec<_>>(),
        ["metadata.new_key"]
    );

    let v = q.values(Some("p"), all(), "metadata.prompt_version", "").await.unwrap();
    let pairs: Vec<(Value, u64)> = v.values.iter().map(|x| (x.value.clone(), x.count)).collect();
    assert_eq!(pairs, vec![(json!("v7"), 160), (json!("v6"), 150)]);
    assert_eq!((v.other_count, v.approximate), (0, false));
    let v6 = q
        .values(Some("p"), all(), "metadata.prompt_version", "v6")
        .await
        .unwrap();
    assert_eq!(v6.values.len(), 1);
    assert_eq!(
        (v6.values[0].count, v6.other_count),
        (150, 160),
        "the rest is counted as other"
    );

    // Only files and memtable runs in the time window count.
    let late = TimeRange {
        from: TimeAt::parse("2026-11-05T02:00:00Z"),
        to: None,
    };
    let w = q.values(Some("p"), late, "metadata.prompt_version", "").await.unwrap();
    assert_eq!(w.values.iter().map(|x| x.count).sum::<u64>(), 10);
    assert!(
        q.fields(Some("p"), late, "")
            .await
            .unwrap()
            .fields
            .iter()
            .any(|x| x.path == "metadata.new_key")
    );
    let none = TimeRange {
        from: TimeAt::parse("2026-11-06T00:00:00Z"),
        to: None,
    };
    assert!(q.fields(Some("p"), none, "").await.unwrap().fields.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn past_10000_paths_the_catalog_says_incomplete() {
    let (_dir, ingest) = open(1024);
    let mut m = Map::new();
    for i in 0..10_001 {
        m.insert(format!("k{i}"), json!(i));
    }
    let mut wide = with_metadata(root(1, "p", 0), Value::Object(m));
    wide.inputs = Some(json!({"q": "x".repeat(2_000)}));
    send(&ingest, vec![(Op::Post, wide)]).await;
    settle(&ingest, 1024).await;
    assert!(ingest.health().segments >= 1, "the run is in a data file");
    let q = Query::new(ingest.clone());
    assert!(!q.fields(Some("p"), all(), "").await.unwrap().complete);
    let small = q.fields(Some("p"), all(), "status").await.unwrap();
    assert_eq!(small.fields.len(), 1);
    assert!(!small.complete, "the catalog as a whole is still incomplete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn values_cut_off_by_a_top_50_are_approximate() {
    // One write group, so one file with 61 distinct values.
    let (_dir, ingest) = open(1024);
    let events: Vec<_> = (0..100)
        .map(|i| {
            let user = if i < 40 { "common".to_string() } else { format!("u{i}") };
            (Op::Post, with_metadata(root(i + 1, "p", i), json!({"user": user})))
        })
        .collect();
    send(&ingest, events).await;
    settle(&ingest, 1024).await;
    assert_eq!((ingest.health().segments, ingest.health().runs), (1, 0));
    let q = Query::new(ingest.clone());
    let v = q.values(Some("p"), all(), "metadata.user", "").await.unwrap();
    assert!(v.values.len() <= 50);
    assert_eq!(
        v.values[0],
        smithling_query::ValueCount {
            value: json!("common"),
            count: 40
        }
    );
    assert_eq!(v.values.iter().map(|x| x.count).sum::<u64>() + v.other_count, 100);
    assert!(v.approximate, "some files list only their top 50 values");
    let f = q.fields(Some("p"), all(), "metadata.user").await.unwrap();
    assert!(!f.fields[0].complete, "its values are not all known");
    assert!(f.complete, "but no path is missing");
}

/// Runs that `count: true` finds matching `filter` in project `p`.
async fn matched(q: &Query, project: Option<&str>, filter: &str) -> u64 {
    let spec = smithling_query::QuerySpec {
        project: project.map(str::to_string),
        filter: Some(smithling_model::filter::parse_filter(filter).unwrap()),
        limit: Some(1),
        count: true,
        ..Default::default()
    };
    let page = q.query(&spec).await.unwrap();
    assert!(page.stats.complete);
    page.stats.matched as u64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn counts_are_runs_not_versions() {
    let (_dir, ingest) = open(16 * 1024);
    let pv = |i: u64| json!({"prompt_version": if i.is_multiple_of(2) { "v6" } else { "v7" }});
    // Starts flushed to files: 1..=100 still running, 101..=200 done in one piece.
    let starts: Vec<_> = (1..=200)
        .map(|i| {
            let r = with_metadata(root(i, "p", i), pv(i));
            (Op::Post, if i > 100 { common::ended(&r, None) } else { r })
        })
        .collect();
    send(&ingest, starts).await;
    settle(&ingest, 16 * 1024).await;
    let files = ingest.health().segments;
    assert!(files >= 1);
    let patch_only = |n: u64, sec: u64| {
        let mut f = end_update(&root(n, "p", sec), None);
        f.project = Some("p".into());
        f
    };
    // Updates flushed to later files: new metadata for 1..=30, plain ends for 31..=60, and runs created by an
    // update; filler pushes them out of the memtable.
    let mut late: Vec<_> = (1..=60)
        .map(|i| {
            let mut u = end_update(&root(i, "p", i), None);
            if i <= 30 {
                u.metadata = json!({"prompt_version": "v8", "stage": "late"}).as_object().cloned();
            }
            (Op::Patch, u)
        })
        .collect();
    late.extend((1_000..1_020).map(|n| (Op::Patch, patch_only(n, n))));
    late.extend((300..600).map(|i| (Op::Post, common::ended(&with_metadata(root(i, "p", i), pv(i)), None))));
    send(&ingest, late).await;
    settle(&ingest, 16 * 1024).await;
    assert!(ingest.health().segments > files);
    // Updates still in the memtable: errors for 61..=80, a third version of 1..=10, more update-created runs.
    let mut fresh: Vec<_> = (61..=80)
        .map(|i| (Op::Patch, end_update(&root(i, "p", i), Some("boom"))))
        .collect();
    fresh.extend((1..=10).map(|i| {
        let mut u = end_update(&root(i, "p", i), None);
        u.tags = Some(vec!["third".into()]);
        (Op::Patch, u)
    }));
    fresh.extend((1_100..1_105).map(|n| (Op::Patch, patch_only(n, n))));
    send(&ingest, fresh).await;
    assert!(ingest.health().runs > 0, "some versions are still in memory");

    let q = Query::new(ingest.clone());
    agrees_with_queries(&q, 525).await;

    // A later flush with more versions of known runs (and of new ones), read through the same catalog.
    let mut more: Vec<_> = (81..=100)
        .map(|i| {
            let mut u = end_update(&root(i, "p", i), None);
            u.metadata = json!({"prompt_version": "v9"}).as_object().cloned();
            (Op::Patch, u)
        })
        .collect();
    more.extend((1..=5).map(|i| (Op::Patch, end_update(&root(i, "p", i), Some("late")))));
    more.extend((600..900).map(|i| (Op::Post, common::ended(&with_metadata(root(i, "p", i), pv(i)), None))));
    send(&ingest, more).await;
    settle(&ingest, 16 * 1024).await;
    agrees_with_queries(&q, 825).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_updated_while_their_memtable_is_flushed() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = smithling_ingest::IngestConfig::new(dir.path());
    cfg.limits = smithling_ingest::Limits {
        memtable_bytes: 16 * 1024,
        dedup_window_us: 0,
        ..Default::default()
    };
    cfg.test_flush_delay = Some(std::time::Duration::from_millis(800));
    let (ingest, _) = smithling_ingest::Ingest::open(cfg).unwrap();
    // One batch over the memtable limit: frozen whole, after it.
    let starts: Vec<_> = (1..=150)
        .map(|i| {
            let mut r = with_metadata(root(i, "p", i), json!({"prompt_version": "v6"}));
            r.inputs = Some(json!({"q": "x".repeat(200)}));
            (Op::Post, r)
        })
        .collect();
    send(&ingest, starts).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ingest.health().frozen {
        assert!(std::time::Instant::now() < deadline, "the memtable froze");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // Updates after the flusher read the multi-version set: its file doesn't mark these runs.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let ends: Vec<_> = (1..=150)
        .map(|i| (Op::Patch, end_update(&root(i, "p", i), None)))
        .collect();
    send(&ingest, ends).await;
    let q = Query::new(ingest.clone());
    agrees_with_queries(&q, 150).await;
    assert!(ingest.health().frozen, "read while the memtable was frozen");
    settle(&ingest, 16 * 1024).await;
    agrees_with_queries(&q, 150).await;
}

/// Every listed count equals the merged-run count a `count: true` query gives.
async fn agrees_with_queries(q: &Query, runs: u64) {
    for project in [Some("p"), None] {
        assert_eq!(matched(q, project, "neq(status, null)").await, runs, "{project:?}");
        let f = q.fields(project, all(), "").await.unwrap();
        for path in [
            "status",
            "run_type",
            "name",
            "metadata.prompt_version",
            "metadata.stage",
        ] {
            let info = f.fields.iter().find(|x| x.path == path);
            let total: u64 = info.into_iter().flat_map(|x| &x.types).map(|t| t.count).sum();
            let want = matched(q, project, &format!("neq({path}, null)")).await;
            assert_eq!(total, want, "{project:?} {path}: runs that have it");
            let v = q.values(project, all(), path, "").await.unwrap();
            assert_eq!(v.other_count, 0, "{project:?} {path}");
            assert!(!v.approximate);
            for x in &v.values {
                let want = matched(q, project, &format!("eq({path}, {})", x.value)).await;
                assert_eq!(x.count, want, "{project:?} {path} = {}", x.value);
            }
            assert_eq!(
                v.values.iter().map(|x| x.count).sum::<u64>(),
                total,
                "{project:?} {path}"
            );
        }
    }
}
