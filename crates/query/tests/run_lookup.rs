//! One run (M3-FR-12, research M3-R12): its payload is read only from the files that hold a version of it, its
//! children are counted without reading its trace, and the answer is the one M2 gave (the merged header with
//! the trace's child count, each payload field from the newest version that set it).

mod common;

use common::*;
use serde_json::json;
use tracedb_ingest::Include;
use tracedb_model::{Op, RunFields, RunHeader};
use tracedb_query::Query;

const TRACES: u64 = 30;
const STEPS: u64 = 10;

fn step_id(t: u64, k: u64) -> u64 {
    t * 100 + k
}

/// `TRACES` traces of a root and `STEPS` steps over many small files; then, after they're flushed, step 1 of
/// every trace and every root ends by an update (another version, in another file); the last trace's updates
/// stay in the memtable.
async fn loaded() -> (tempfile::TempDir, tracedb_ingest::Ingest) {
    const MEMTABLE: u64 = 16 * 1024;
    let (dir, ingest) = open(MEMTABLE);
    // A trace per upload, so the small memtable freezes between them.
    for t in 1..=TRACES {
        let mut r = root(t * 100, "p", t * 60);
        r.inputs = Some(json!({"q": format!("question {t}")}));
        let mut starts = vec![(Op::Post, r.clone())];
        for k in 1..=STEPS {
            let mut c = child(&r, step_id(t, k), t * 60 + k);
            c.inputs = Some(json!({"step": k}));
            starts.push((Op::Post, c));
        }
        send(&ingest, starts).await;
        settle(&ingest, MEMTABLE).await;
    }
    let ends = |t: u64| -> Vec<(Op, RunFields)> {
        let r = root(t * 100, "p", t * 60);
        let c = child(&r, step_id(t, 1), t * 60 + 1);
        [r, c]
            .into_iter()
            .map(|f| {
                let mut u = end_update(&f, None);
                u.outputs = Some(json!({"answer": format!("done {}", f.id.unwrap())}));
                (Op::Patch, u)
            })
            .collect()
    };
    for t in 1..TRACES {
        send(&ingest, ends(t)).await;
    }
    settle(&ingest, MEMTABLE).await;
    send(&ingest, ends(TRACES)).await;
    (dir, ingest)
}

fn all() -> Include {
    Include {
        inputs: true,
        outputs: true,
        events: true,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_reads_only_its_own_files_and_rows() {
    let (_dir, ingest) = loaded().await;
    let files = ingest.health().segments;
    assert!(files >= 20, "{files} files");
    let q = Query::new(ingest.clone());
    for t in [1, TRACES / 2, TRACES] {
        let trace = q.trace(uid(t * 100)).await.unwrap().unwrap();
        // (run, versions in files, has outputs)
        let cases = [
            (t * 100, if t == TRACES { 1 } else { 2 }, true),
            (step_id(t, 1), if t == TRACES { 1 } else { 2 }, true),
            (step_id(t, 5), 1, false),
        ];
        for (n, in_files, ended) in cases {
            let id = uid(n);
            let (view, stats) = q.run_with_stats(id, all()).await.unwrap();
            let view = view.unwrap();
            // As M2 answered: the trace's header for the run (child count included) and its payload.
            let want: &RunHeader = trace.runs.iter().find(|r| r.run_id == id).unwrap();
            assert_eq!(&view.header, want, "run {n}");
            assert_eq!(view.header.child_count, if n == t * 100 { STEPS as u32 } else { 0 });
            assert!(view.inputs.is_some(), "run {n}: inputs from its start");
            assert_eq!(view.outputs.is_some(), ended, "run {n}: outputs from its update");
            assert_eq!(stats.payload_files, in_files, "run {n}: payload files read");
            // Its own versions, plus a root's steps' versions (step 1 has two) to count them: a step reads
            // none of its trace's other rows.
            let rows = if n == t * 100 {
                in_files + STEPS as usize + in_files - 1
            } else {
                in_files
            };
            assert!(
                stats.rows_read <= rows,
                "run {n}: read {} rows, want ≤ {rows}",
                stats.rows_read
            );
        }
    }
    // Without payload fields nothing but core rows is read.
    let (_, stats) = q.run_with_stats(uid(step_id(2, 1)), Include::default()).await.unwrap();
    assert_eq!(stats.payload_files, 0);
    assert!(q.run(uid(999_999), all()).await.unwrap().is_none());
}
