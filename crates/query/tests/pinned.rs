//! Pinned versions (R-7): a scan reads the file set and memtable copies it pinned when it started, whatever is
//! published while it runs, and a swapped-out file may be deleted only once no live `Arc<Version>` holds it.
//!
//! M2 has no compaction, so nothing deletes files yet: `swap` and `delete_unpinned` below are the test-only
//! stand-ins for M4's compaction swap and file deleter.

mod common;

use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Weak};
use std::task::Poll;

use common::*;
use tracedb_ingest::{Ingest, Version};
use tracedb_model::{Op, Status};
use tracedb_query::{Page, Query, QuerySpec};
use tracedb_storage::SegmentMeta;

const MEMTABLE: u64 = 8 * 1024;

fn spec() -> QuerySpec {
    QuerySpec {
        project: Some("a".into()),
        limit: Some(200),
        ..QuerySpec::default()
    }
}

/// Polls `fut` once: far enough to pin (the scan pins before its first await), not far enough to read files.
async fn start<F: Future>(fut: &mut std::pin::Pin<&mut F>) {
    let first = std::future::poll_fn(|cx| Poll::Ready(fut.as_mut().poll(cx).is_pending())).await;
    assert!(first, "the scan should wait on file reads after pinning");
}

/// Rewrites `seg`'s files under new paths (what compaction produces) and publishes a version that has the
/// rewrite instead of `seg`. Returns the replaced version, so the test can tell when it's no longer pinned.
async fn swap(ingest: &Ingest, seg: &SegmentMeta) -> Weak<Version> {
    let store = &ingest.storage().store;
    let mut add = seg.clone();
    add.id = format!("{}-rewritten", seg.id);
    add.core_path = seg.core_path.replace(".parquet", "-rewritten.parquet");
    add.payload_path = seg.payload_path.replace(".parquet", "-rewritten.parquet");
    assert_ne!(add.core_path, seg.core_path);
    store
        .put_new(&add.core_path, store.get(&seg.core_path).await.unwrap())
        .await
        .unwrap();
    store
        .put_new(&add.payload_path, store.get(&seg.payload_path).await.unwrap())
        .await
        .unwrap();

    let mem = ingest.mem();
    let mut mem = mem.write().unwrap();
    let old = mem.version.clone();
    let mut next = (*old).clone();
    next.id += 1;
    next.segments = old
        .segments
        .iter()
        .map(|s| {
            if s.id == seg.id {
                Arc::new(add.clone())
            } else {
                s.clone()
            }
        })
        .collect();
    mem.version = Arc::new(next);
    Arc::downgrade(&old)
}

/// The deleter rule: a swapped-out segment's files go only when no reader holds the version that listed them.
async fn delete_unpinned(ingest: &Ingest, old: &Weak<Version>, seg: &SegmentMeta) -> bool {
    if old.strong_count() > 0 {
        return false;
    }
    let store = &ingest.storage().store;
    store.delete(&seg.core_path).await.unwrap();
    store.delete(&seg.payload_path).await.unwrap();
    true
}

async fn files_exist(ingest: &Ingest, seg: &SegmentMeta) -> bool {
    let store = &ingest.storage().store;
    store.get(&seg.core_path).await.is_ok() && store.get(&seg.payload_path).await.is_ok()
}

#[tokio::test(flavor = "current_thread")]
async fn a_scan_across_a_swap_returns_the_same_rows_and_holds_the_old_files() {
    let (_dir, ingest) = open(MEMTABLE);
    for batch in 0..6 {
        let runs = batch * 25 + 1..=batch * 25 + 25;
        send(
            &ingest,
            runs.map(|n| (Op::Post, ended(&root(n, "a", n * 10), None))).collect(),
        )
        .await;
        settle(&ingest, MEMTABLE).await;
    }
    assert!(ingest.health().segments >= 2, "the test needs several files");
    let swapped = ingest.mem().read().unwrap().version.segments[0].clone();

    let query = Query::new(ingest.clone());
    let before = query.query(&spec()).await.unwrap();

    let q = spec();
    let mut scan = pin!(query.query(&q));
    start(&mut scan).await;
    let old = swap(&ingest, &swapped).await;
    assert!(
        !delete_unpinned(&ingest, &old, &swapped).await,
        "the running scan still pins the old version"
    );
    assert!(files_exist(&ingest, &swapped).await);

    let during: Page = scan.as_mut().await.unwrap();
    assert_eq!(during.runs, before.runs);
    assert_eq!(during.stats.files_total, before.stats.files_total);

    assert!(
        delete_unpinned(&ingest, &old, &swapped).await,
        "the scan dropped its Arc<Version>"
    );
    assert!(!files_exist(&ingest, &swapped).await);
    let after = query.query(&spec()).await.unwrap();
    assert_eq!(after.runs, before.runs, "the rewritten files serve the same rows");
}

#[tokio::test(flavor = "current_thread")]
async fn a_scan_reads_its_pinned_file_set_not_the_one_published_under_it() {
    let (_dir, ingest) = open(MEMTABLE);
    for batch in 0..4 {
        let runs = batch * 25 + 1..=batch * 25 + 25;
        send(
            &ingest,
            runs.map(|n| (Op::Post, ended(&root(n, "a", n * 10), None))).collect(),
        )
        .await;
        settle(&ingest, MEMTABLE).await;
    }
    let query = Query::new(ingest.clone());
    let before = query.query(&spec()).await.unwrap();

    let q = spec();
    let mut scan = pin!(query.query(&q));
    start(&mut scan).await;
    // Publish a version without the first file (its files stay): a scan that looked at the live version
    // instead of its pinned one would miss those rows.
    let dropped = {
        let mem = ingest.mem();
        let mut mem = mem.write().unwrap();
        let mut next = (*mem.version).clone();
        next.id += 1;
        let dropped = next.segments.remove(0);
        mem.version = Arc::new(next);
        dropped
    };
    let during = scan.as_mut().await.unwrap();
    assert_eq!(during.runs, before.runs);

    let after = query.query(&spec()).await.unwrap();
    assert_eq!(after.runs.len(), before.runs.len() - dropped.row_count as usize);
}

#[tokio::test(flavor = "current_thread")]
async fn a_scan_across_a_flush_sees_one_consistent_state() {
    let (_dir, ingest) = open(MEMTABLE);
    send(
        &ingest,
        (1..=60)
            .map(|n| (Op::Post, ended(&root(n, "a", n * 10), None)))
            .collect(),
    )
    .await;
    settle(&ingest, MEMTABLE).await;
    let running: Vec<_> = (61..=65).map(|n| root(n, "a", n * 10)).collect();
    send(&ingest, running.iter().map(|r| (Op::Post, r.clone())).collect()).await;
    let h = ingest.health();
    assert!(
        h.segments >= 1 && h.runs >= 5,
        "rows in files and in the memtable: {h:?}"
    );

    let query = Query::new(ingest.clone());
    let before = query.query(&spec()).await.unwrap();
    assert_eq!(before.runs.len(), 65);
    assert_eq!(
        before
            .runs
            .iter()
            .filter(|r| r.header.status == Status::Running)
            .count(),
        5
    );

    let q = spec();
    let mut scan = pin!(query.query(&q));
    start(&mut scan).await;
    // While the scan runs: the running roots end, new roots arrive, and the memtable holding the rows the scan
    // copied is frozen, flushed to files and dropped.
    let mut events: Vec<_> = running.iter().map(|r| (Op::Patch, end_update(r, None))).collect();
    events.extend((66..=185).map(|n| (Op::Post, ended(&root(n, "a", n * 10), None))));
    send(&ingest, events).await;
    settle(&ingest, MEMTABLE).await;
    assert!(ingest.health().segments > h.segments, "new files were published");

    let during = scan.as_mut().await.unwrap();
    assert_eq!(during.runs, before.runs, "no new runs, no updates, no run twice");

    let after = query.query(&spec()).await.unwrap();
    assert_eq!(after.runs.len(), 185);
    let ids: std::collections::HashSet<_> = after.runs.iter().map(|r| r.header.run_id).collect();
    assert_eq!(ids.len(), 185);
    assert!(after.runs.iter().all(|r| r.header.status != Status::Running));
}
