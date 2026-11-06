//! Flush: a full memtable becomes registered data files, the WAL is trimmed only after, and a restart
//! replays only what isn't in files yet (design §4.2, §4.6).

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use tracedb_ingest::{Ingest, IngestConfig, Limits};

fn small(dir: &Path) -> IngestConfig {
    let mut cfg = IngestConfig::new(dir);
    cfg.limits = Limits {
        memtable_bytes: 32 * 1024,
        dedup_window_us: 0,
        ..Limits::default()
    };
    cfg
}

fn wal_files(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("wal")).unwrap().count()
}

async fn upload(ingest: &Ingest, from: u128, n: u128) {
    for i in from..from + n {
        let mut f = root(uuid(i), "2026-11-05T02:00:00Z", "r");
        f.inputs = Some(serde_json::json!({"q": "x".repeat(400)}));
        loop {
            match ingest.accept(vec![post(f.clone())]).await {
                Ok(r) => {
                    assert_eq!(r.accepted, 1);
                    break;
                }
                Err(tracedb_ingest::AcceptError::Busy) => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(e) => panic!("{e}"),
            }
        }
    }
}

async fn wait_for(cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_memtables_become_files_and_the_wal_is_trimmed() {
    let dir = tempfile::tempdir().unwrap();
    let (ingest, _) = Ingest::open(small(dir.path())).unwrap();
    upload(&ingest, 1, 300).await;
    wait_for(|| ingest.health().segments >= 2 && !ingest.health().frozen).await;

    let h = ingest.health();
    assert!(h.flush_ok, "{h:?}");
    assert!(h.flushed_seq > 0);
    assert!(h.runs < 300, "flushed runs left memory: {} in memtables", h.runs);
    let files = std::fs::read_dir(dir.path().join("data")).unwrap().count();
    assert!(files >= 1, "project folder written");
    assert!(dir.path().join("meta.sqlite").exists());
    assert!(wal_files(dir.path()) < h.segments + 2, "flushed WAL files are deleted");

    // Every confirmed run is in a file or a memtable, exactly once.
    let mem = ingest.mem();
    let (seg_rows, mem_runs) = {
        let m = mem.read().unwrap();
        (
            m.version.segments.iter().map(|s| s.row_count).sum::<u64>(),
            m.active.len() as u64,
        )
    };
    assert_eq!(seg_rows + mem_runs, 300);
    drop(ingest);

    let (reopened, stats) = Ingest::open(small(dir.path())).unwrap();
    assert_eq!(stats.segments, h.segments);
    assert_eq!(stats.flushed_seq, h.flushed_seq);
    assert_eq!(stats.runs as u64, mem_runs, "only unflushed events are replayed");
    assert_eq!(reopened.health().segments, h.segments);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_updates_after_a_flush_are_marked_multi_version() {
    let dir = tempfile::tempdir().unwrap();
    let (ingest, _) = Ingest::open(small(dir.path())).unwrap();
    upload(&ingest, 1, 120).await;
    wait_for(|| ingest.health().segments >= 1).await;
    let mut end = root(uuid(1), "2026-11-05T02:00:00Z", "r");
    end.end_time = tracedb_model::Ts::parse("2026-11-05T02:05:00Z");
    ingest.accept(vec![patch(end)]).await.unwrap();
    assert!(ingest.multi_version().read().unwrap().contains(&uuid(1)));
    drop(ingest);
    let (reopened, _) = Ingest::open(small(dir.path())).unwrap();
    assert!(
        reopened.multi_version().read().unwrap().contains(&uuid(1)),
        "rebuilt from the WAL on restart"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_files_are_removed_on_start() {
    let dir = tempfile::tempdir().unwrap();
    drop(Ingest::open(small(dir.path())).unwrap());
    let orphan = dir.path().join("data/p-00000000/2026-11-06/seg-X.core.parquet");
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"half written").unwrap();
    let (_, stats) = Ingest::open(small(dir.path())).unwrap();
    assert_eq!(stats.orphans_removed, 1);
    assert!(!orphan.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_big_unflushed_log_after_restart_flushes_instead_of_staying_busy() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = |d: &Path| {
        let mut c = IngestConfig::new(d);
        c.limits = Limits {
            // Memory never fills; only the log limit can trigger a flush.
            memtable_bytes: u64::MAX / 4,
            max_unflushed_wal: 64 * 1024,
            dedup_window_us: 0,
            ..Limits::default()
        };
        c
    };
    {
        let (ingest, _) = Ingest::open(cfg(dir.path())).unwrap();
        upload(&ingest, 1, 100).await;
    }
    let (ingest, stats) = Ingest::open(cfg(dir.path())).unwrap();
    assert!(stats.runs > 0 || stats.segments > 0);
    // New uploads are accepted (after at most a short "busy" while the replayed log is flushed).
    let started = Instant::now();
    upload(&ingest, 1000, 20).await;
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "took {:?}",
        started.elapsed()
    );
    wait_for(|| ingest.health().unflushed_wal_bytes < 64 * 1024).await;
}
