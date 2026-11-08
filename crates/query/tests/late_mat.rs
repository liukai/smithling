//! Late materialization (M3-FR-12, Story 4 scenario 3): a list reads no payload; a search page reads payload
//! rows only for the runs it shows, plus the one that tells there's a next page; the same query again is at
//! least as fast (payload pages come from the cache) and returns the same page.

mod common;

use std::time::Instant;

use common::*;
use serde_json::json;
use tracedb_model::Op;
use tracedb_model::filter::{AttrRoot, Path, parse_filter};
use tracedb_query::{Query, QuerySpec};

const RUNS: u64 = 2_000;

/// `RUNS` roots over many files with ~1 KB of inputs each; every 10th mentions "quota exceeded".
async fn loaded() -> (tempfile::TempDir, tracedb_ingest::Ingest) {
    const MEMTABLE: u64 = 256 * 1024;
    let (dir, ingest) = open(MEMTABLE);
    for chunk in (0..RUNS).collect::<Vec<_>>().chunks(100) {
        let events = chunk
            .iter()
            .map(|&i| {
                let mut r = ended(&root(i + 1, "p", i), None);
                let note = if i % 10 == 0 {
                    "quota exceeded on retry"
                } else {
                    "all good"
                };
                r.inputs = Some(json!({"prompt": format!("{note} {}", "lorem ipsum ".repeat(80)), "n": i}));
                r.outputs = Some(json!({"answer": "fine"}));
                (Op::Post, r)
            })
            .collect();
        send(&ingest, events).await;
        settle(&ingest, MEMTABLE).await;
    }
    assert!(ingest.health().segments > 5, "{} files", ingest.health().segments);
    (dir, ingest)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pages_read_payload_only_for_the_runs_they_show() {
    let (_dir, ingest) = loaded().await;
    let q = Query::new(ingest.clone());

    let list = QuerySpec {
        project: Some("p".into()),
        filter: Some(parse_filter(r#"gt(inputs.n, 100)"#).unwrap()),
        columns: vec![Path::Attr {
            root: AttrRoot::Inputs,
            key: "n".into(),
        }],
        limit: Some(20),
        ..QuerySpec::default()
    };
    let page = q.query(&list).await.unwrap();
    assert_eq!(page.runs.len(), 20);
    assert_eq!(page.stats.payload_rows, 0, "a list reads no payload: {:?}", page.stats);

    for limit in [5, 20, 50] {
        let search = QuerySpec {
            project: Some("p".into()),
            filter: Some(parse_filter(r#"search(inputs, "quota exceeded")"#).unwrap()),
            limit: Some(limit),
            ..QuerySpec::default()
        };
        let started = Instant::now();
        let first = q.query(&search).await.unwrap();
        let first_ms = started.elapsed();
        assert_eq!(first.runs.len(), limit);
        assert!(
            first.runs.iter().all(|r| r.r#match.is_some()),
            "shown rows carry excerpts"
        );
        assert!(first.next_cursor.is_some());
        assert!(
            first.stats.payload_rows <= limit + 1,
            "limit {limit}: read {} payload rows",
            first.stats.payload_rows
        );

        let mut again = Vec::new();
        for _ in 0..3 {
            let started = Instant::now();
            let page = q.query(&search).await.unwrap();
            again.push(started.elapsed());
            assert_eq!(page.runs, first.runs, "the same page");
            assert_eq!(page.next_cursor, first.next_cursor);
            assert!(page.stats.payload_rows <= limit + 1);
        }
        // The first search read its pages from the files; later limits find them cached already.
        if limit == 5 {
            let best = again.iter().min().unwrap();
            assert!(*best <= first_ms, "limit {limit}: first {first_ms:?}, again {again:?}");
        }
    }
    assert!(ingest.storage().cache.hits() > 0, "payload pages served from the cache");
}
