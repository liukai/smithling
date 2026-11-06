//! Lists by start time read only the newest (or oldest) windows they need; attribute conditions are pushed
//! into the file scan without changing results, or warnings for the rows read.

mod common;

use common::*;
use serde_json::json;
use tracedb_model::Op;
use tracedb_model::filter::parse_filter;
use tracedb_query::{Query, QuerySpec, QueryWarning};

const RUNS: u64 = 3_000;

/// `RUNS` roots one second apart, across many files; every 10th has `metadata.v = "a"`, every 10th (offset 5) a
/// number, the rest `"b"`.
async fn loaded() -> (tempfile::TempDir, tracedb_ingest::Ingest) {
    let (dir, ingest) = open(64 * 1024);
    let events = (0..RUNS)
        .map(|i| {
            let v = match i % 10 {
                0 => json!("a%_\\"),
                5 => json!(1),
                _ => json!("b"),
            };
            (
                Op::Post,
                ended(&with_metadata(root(i + 1, "p", i), json!({"v": v})), None),
            )
        })
        .collect();
    send(&ingest, events).await;
    settle(&ingest, 64 * 1024).await;
    assert!(ingest.health().segments > 5, "{} files", ingest.health().segments);
    (dir, ingest)
}

fn spec(filter: &str) -> QuerySpec {
    QuerySpec {
        project: Some("p".into()),
        filter: Some(parse_filter(filter).unwrap()),
        limit: Some(10),
        ..QuerySpec::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_page_reads_only_the_newest_windows() {
    let (_dir, ingest) = loaded().await;
    let q = Query::new(ingest.clone()).with_first_window(100);
    for ascending in [false, true] {
        let s = QuerySpec {
            ascending,
            ..spec(r#"eq(status, "ok")"#)
        };
        let page = q.query(&s).await.unwrap();
        let want: Vec<u64> = if ascending {
            (1..=10).collect()
        } else {
            (RUNS - 9..=RUNS).rev().collect()
        };
        let got: Vec<_> = page.runs.iter().map(|r| r.header.run_id).collect();
        assert_eq!(
            got,
            want.iter().map(|&n| uid(n)).collect::<Vec<_>>(),
            "ascending {ascending}"
        );
        assert!(page.next_cursor.is_some());
        assert!(!page.stats.complete);
        assert!(page.stats.rows_read < 300, "read {} rows", page.stats.rows_read);

        let all = q.query(&QuerySpec { count: true, ..s }).await.unwrap();
        assert_eq!(all.stats.matched, RUNS as usize);
        assert!(all.stats.complete);
        assert_eq!(all.runs, page.runs);
    }
    // One window holds everything, so the count is the total.
    let one = Query::new(ingest.clone())
        .query(&spec(r#"eq(status, "ok")"#))
        .await
        .unwrap();
    assert!(one.stats.complete);
    assert_eq!(one.stats.matched, RUNS as usize);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attribute_conditions_are_pushed_down_and_keep_warnings() {
    let (_dir, ingest) = loaded().await;
    let q = Query::new(ingest.clone());
    let page = q
        .query(&QuerySpec {
            count: true,
            limit: Some(200),
            ..spec(r#"eq(metadata.v, "a%_\\")"#)
        })
        .await
        .unwrap();
    assert_eq!(page.stats.matched, RUNS as usize / 10);
    assert!(
        page.runs
            .iter()
            .all(|r| (r.header.run_id.as_u128() & 0xffff_ffff) % 10 == 1)
    );
    // The matching strings plus the numbers (another type, reported); not the other strings.
    assert!(
        page.stats.rows_read <= 2 * RUNS as usize / 10 + 200,
        "read {} rows",
        page.stats.rows_read
    );
    assert!(
        page.warnings
            .iter()
            .any(|w| matches!(w, QueryWarning::MixedTypes { path, counts }
            if path == "metadata.v" && counts.get("number") == Some(&(RUNS / 10)))),
        "{:?}",
        page.warnings
    );

    // An attribute condition inside `not` isn't pushed (only a superset is known).
    let page = q
        .query(&QuerySpec {
            count: true,
            ..spec(r#"not(eq(metadata.v, "b"))"#)
        })
        .await
        .unwrap();
    assert_eq!(page.stats.matched, 2 * RUNS as usize / 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_version_with_another_dotted_order_stays_with_its_run() {
    let (_dir, ingest) = open(8 * 1024);
    let events = (0..200).map(|i| (Op::Post, root(i + 1, "p", i * 10))).collect();
    send(&ingest, events).await;
    settle(&ingest, 8 * 1024).await;
    assert!(ingest.health().segments > 0);
    // Run 1's end arrives with a dotted_order an hour later: an identity conflict kept as a warning.
    let moved = root(1, "p", 3_600);
    send(&ingest, vec![(Op::Patch, end_update(&moved, Some("boom")))]).await;

    for first in [1, 20_000] {
        let q = Query::new(ingest.clone()).with_first_window(first);
        let mut s = QuerySpec {
            project: Some("p".into()),
            limit: Some(7),
            ..QuerySpec::default()
        };
        let mut got = Vec::new();
        loop {
            let page = q.query(&s).await.unwrap();
            got.extend(page.runs.iter().filter(|r| r.header.run_id == uid(1)).cloned());
            match page.next_cursor {
                Some(c) => s.cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(got.len(), 1, "first window {first}: {got:?}");
        assert_eq!(
            got[0].header.status,
            tracedb_model::Status::Error,
            "first window {first}: {got:?}"
        );
        assert_eq!(got[0].header.dotted_order, root(1, "p", 0).dotted_order.unwrap());
    }
}
