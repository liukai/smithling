//! Lists and single runs built from D-023's kept merged rows (D-029) answer as reading every version did: list
//! rows keep `metadata`, an update flushed before its run's start still merges with the start in memory, and a
//! single run's payload lookup never takes another project's file of the same flush for damaged.

mod common;

use common::*;
use serde_json::json;
use tracedb_ingest::Include;
use tracedb_model::filter::parse_filter;
use tracedb_model::{Op, RunFields};
use tracedb_query::{Query, QuerySpec};

const MEMTABLE: u64 = 16 * 1024;

async fn filler(ingest: &tracedb_ingest::Ingest, from: u64) {
    let runs = (from..from + 40)
        .map(|n| (Op::Post, ended(&root(n, "filler", n), None)))
        .collect();
    send(ingest, runs).await;
    settle(ingest, MEMTABLE).await;
}

fn list(project: &str, filter: &str) -> QuerySpec {
    QuerySpec {
        project: Some(project.into()),
        filter: Some(parse_filter(filter).unwrap()),
        limit: Some(50),
        ..QuerySpec::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_rows_of_flushed_multi_version_runs_keep_their_metadata() {
    let (_dir, ingest) = open(MEMTABLE);
    let r = with_metadata(root(1, "p", 60), json!({"prompt": "v7", "n": 3}));
    send(&ingest, vec![(Op::Post, r.clone())]).await;
    filler(&ingest, 1_000).await;
    send(&ingest, vec![(Op::Patch, end_update(&r, Some("boom")))]).await;
    filler(&ingest, 2_000).await;
    assert_eq!(ingest.health().runs, 0, "both versions in files");
    let q = Query::new(ingest.clone());
    let want = q.trace(uid(1)).await.unwrap().unwrap().runs.remove(0);
    for _ in 0..2 {
        let page = q.query(&list("p", r#"eq(status, "error")"#)).await.unwrap();
        assert_eq!(page.runs.len(), 1);
        let got = &page.runs[0].header;
        assert_eq!(got.metadata, want.metadata);
        assert_eq!(got.metadata["prompt"], "v7");
        assert_eq!(got.extra, want.extra);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_update_flushed_before_its_start_merges_with_the_start_in_memory() {
    let (_dir, ingest) = open(MEMTABLE);
    let r = root(1, "p", 60);
    // The SDK's end, without a project, before the start: flushed under no project.
    send(&ingest, vec![(Op::Patch, end_update(&r, Some("boom")))]).await;
    filler(&ingest, 1_000).await;
    send(&ingest, vec![(Op::Post, r.clone())]).await;
    assert_eq!(ingest.health().runs, 1, "the start is still in memory");
    let q = Query::new(ingest.clone());
    for _ in 0..2 {
        let page = q.query(&list("p", r#"eq(status, "error")"#)).await.unwrap();
        let ids: Vec<_> = page.runs.iter().map(|r| r.header.run_id).collect();
        assert_eq!(ids, vec![uid(1)], "{:?}", page.stats);
        assert_eq!(page.runs[0].header.error.as_deref(), Some("boom"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_lookup_leaves_other_projects_files_of_its_flush_alone() {
    let (_dir, ingest) = open(MEMTABLE);
    // One upload, one flush, two files: "b" (first in the file list) holds 4 runs whose traces sit among "z"'s.
    let b_runs = [2, 50, 100, 149];
    let runs: Vec<(Op, RunFields)> = (1..=150u64)
        .map(|n| {
            let project = if b_runs.contains(&n) { "b" } else { "z" };
            let mut f = ended(&root(n, project, n), None);
            f.inputs = Some(json!({"q": format!("question {n}")}));
            (Op::Post, f)
        })
        .collect();
    send(&ingest, runs).await;
    settle(&ingest, MEMTABLE).await;
    filler(&ingest, 1_000).await;
    let q = Query::new(ingest.clone());
    let all = Include {
        inputs: true,
        outputs: true,
        events: true,
    };
    for n in [120, 140, 3, 100] {
        let view = q.run(uid(n), all).await.unwrap().unwrap();
        assert_eq!(view.inputs.unwrap()["q"], format!("question {n}"));
    }
    assert!(ingest.damaged().is_empty(), "{:?}", ingest.damaged());
    let b = q
        .query(&QuerySpec {
            project: Some("b".into()),
            ..QuerySpec::default()
        })
        .await
        .unwrap();
    assert_eq!(b.runs.len(), 4);
}
