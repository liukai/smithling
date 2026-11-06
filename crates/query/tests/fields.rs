//! Field catalog (data-model §10): paths with types and counts, and a path's top values, summed over the data
//! files in scope plus the memtables.

mod common;

use common::*;
use serde_json::{Map, Value, json};
use tracedb_model::Op;
use tracedb_model::filter::TimeAt;
use tracedb_query::{Query, TimeRange};

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
    let f = q.fields(Some("p"), all(), "");
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

    let only: Vec<_> = q.fields(Some("p"), all(), "metadata.n").fields;
    assert_eq!(
        only.iter().map(|x| x.path.as_str()).collect::<Vec<_>>(),
        ["metadata.new_key"]
    );

    let v = q.values(Some("p"), all(), "metadata.prompt_version", "");
    let pairs: Vec<(Value, u64)> = v.values.iter().map(|x| (x.value.clone(), x.count)).collect();
    assert_eq!(pairs, vec![(json!("v7"), 160), (json!("v6"), 150)]);
    assert_eq!((v.other_count, v.approximate), (0, false));
    let v6 = q.values(Some("p"), all(), "metadata.prompt_version", "v6");
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
    let w = q.values(Some("p"), late, "metadata.prompt_version", "");
    assert_eq!(w.values.iter().map(|x| x.count).sum::<u64>(), 10);
    assert!(
        q.fields(Some("p"), late, "")
            .fields
            .iter()
            .any(|x| x.path == "metadata.new_key")
    );
    let none = TimeRange {
        from: TimeAt::parse("2026-11-06T00:00:00Z"),
        to: None,
    };
    assert!(q.fields(Some("p"), none, "").fields.is_empty());
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
    assert!(!q.fields(Some("p"), all(), "").complete);
    let small = q.fields(Some("p"), all(), "status");
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
    let v = q.values(Some("p"), all(), "metadata.user", "");
    assert!(v.values.len() <= 50);
    assert_eq!(
        v.values[0],
        tracedb_query::ValueCount {
            value: json!("common"),
            count: 40
        }
    );
    assert_eq!(v.values.iter().map(|x| x.count).sum::<u64>() + v.other_count, 100);
    assert!(v.approximate, "some files list only their top 50 values");
    let f = q.fields(Some("p"), all(), "metadata.user");
    assert!(!f.fields[0].complete, "its values are not all known");
    assert!(f.complete, "but no path is missing");
}
