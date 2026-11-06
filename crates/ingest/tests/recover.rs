//! Recovery on start (design §4.6): orphans go, replay skips what's in files, the multi-version set and the
//! dedup window are rebuilt (from the catalog's dedup keys, D-022), and an M1 folder opens as is (M2-FR-20).

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use tracedb_ingest::{Ingest, IngestConfig, Limits};

fn cfg(dir: &Path, dedup_window_us: i64) -> IngestConfig {
    let mut cfg = IngestConfig::new(dir);
    cfg.limits = Limits {
        memtable_bytes: 32 * 1024,
        flush_age_us: 100_000,
        dedup_window_us,
        ..Limits::default()
    };
    cfg
}

fn item(i: u128) -> tracedb_ingest::Incoming {
    let mut f = root(uuid(i), "2026-11-05T02:00:00Z", "r");
    f.inputs = Some(serde_json::json!({"q": "x".repeat(400)}));
    post(f)
}

async fn upload(ingest: &Ingest, from: u128, n: u128) {
    for i in from..from + n {
        loop {
            match ingest.accept(vec![item(i)]).await {
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

/// Waits until every confirmed event is in a registered file.
async fn wait_all_flushed(ingest: &Ingest) {
    let snap = ingest.snapshot();
    wait_for(|| {
        let h = ingest.health();
        h.runs == 0 && h.flushed_seq == snap.last_seq()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_files_are_removed_on_start() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (ingest, _) = Ingest::open(cfg(dir.path(), 0)).unwrap();
        upload(&ingest, 1, 100).await;
        wait_all_flushed(&ingest).await;
    }
    let orphan = dir.path().join("data/p-00000000/2026-11-06/seg-X.core.parquet");
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"half written").unwrap();
    let (ingest, stats) = Ingest::open(cfg(dir.path(), 0)).unwrap();
    assert_eq!(stats.orphans_removed, 1);
    assert!(!orphan.exists());
    let h = ingest.health();
    assert_eq!(h.segment_rows, 100, "registered files are kept");
    assert!(h.corrupt_segments.is_empty() && h.ok, "{h:?}");
}

/// Seqs of the WAL records left in `dir/wal` (read from a copy, so the folder is untouched).
fn wal_seqs(dir: &Path) -> Vec<u64> {
    let copy = tempfile::tempdir().unwrap();
    for e in std::fs::read_dir(dir.join("wal")).unwrap().filter_map(Result::ok) {
        std::fs::copy(e.path(), copy.path().join(e.file_name())).unwrap();
    }
    let mut seqs = Vec::new();
    tracedb_ingest::wal::Wal::open(copy.path(), 1 << 20, |ev| seqs.push(ev.event_seq)).unwrap();
    seqs
}

async fn assert_retries_write_nothing(ingest: &Ingest, ids: std::ops::RangeInclusive<u128>) {
    let before = ingest.snapshot().last_seq();
    // The SDK retries batches it never got an answer for: same items, same bodies.
    for i in ids {
        assert_eq!(ingest.accept(vec![item(i)]).await.unwrap().accepted, 1);
    }
    assert_eq!(
        ingest.snapshot().last_seq(),
        before,
        "retries inside the window write nothing"
    );
    assert_eq!(ingest.health().runs, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flushed_events_leave_the_wal_and_retries_are_still_deduped_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let window = 10 * 60 * 1_000_000;
    let flushed_seq = {
        let (ingest, _) = Ingest::open(cfg(dir.path(), window)).unwrap();
        upload(&ingest, 1, 50).await;
        wait_all_flushed(&ingest).await;
        ingest.health().flushed_seq
    };
    assert_eq!(flushed_seq, 50);
    assert_eq!(
        wal_seqs(dir.path()),
        Vec::<u64>::new(),
        "flushed records are trimmed at once"
    );
    let (ingest, stats) = Ingest::open(cfg(dir.path(), window)).unwrap();
    assert_eq!((stats.skipped_flushed, stats.replayed_events), (0, 0), "{stats:?}");
    assert_retries_write_nothing(&ingest, 1..=50).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keys_of_replayed_events_survive_their_flush() {
    let dir = tempfile::tempdir().unwrap();
    let window = 10 * 60 * 1_000_000;
    let no_flush = |d: &Path| {
        let mut c = cfg(d, window);
        c.limits.memtable_bytes = u64::MAX / 4;
        c.limits.flush_age_us = i64::MAX / 4;
        c
    };
    {
        let (ingest, _) = Ingest::open(no_flush(dir.path())).unwrap();
        upload(&ingest, 1, 50).await;
    }
    {
        // Replayed into the active memtable, then flushed by this process.
        let (ingest, stats) = Ingest::open(cfg(dir.path(), window)).unwrap();
        assert_eq!(stats.replayed_events, 50);
        wait_all_flushed(&ingest).await;
    }
    assert_eq!(wal_seqs(dir.path()), Vec::<u64>::new());
    let (ingest, _) = Ingest::open(cfg(dir.path(), window)).unwrap();
    assert_retries_write_nothing(&ingest, 1..=50).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flushed_records_left_in_the_wal_are_skipped_then_trimmed_on_start() {
    // A crash after the catalog commit but before the trim (or an M2 folder from before D-022, which kept
    // flushed WAL files for the dedup window) leaves records that are already in data files.
    let dir = tempfile::tempdir().unwrap();
    let window = 10 * 60 * 1_000_000;
    let no_flush = |d: &Path| {
        let mut c = cfg(d, window);
        c.limits.memtable_bytes = u64::MAX / 4;
        c.limits.flush_age_us = i64::MAX / 4;
        c
    };
    {
        let (ingest, _) = Ingest::open(no_flush(dir.path())).unwrap();
        upload(&ingest, 1, 200).await;
    }
    let saved = tempfile::tempdir().unwrap();
    for e in std::fs::read_dir(dir.path().join("wal"))
        .unwrap()
        .filter_map(Result::ok)
    {
        std::fs::copy(e.path(), saved.path().join(e.file_name())).unwrap();
    }
    let flushed_seq = {
        let (ingest, _) = Ingest::open(cfg(dir.path(), window)).unwrap();
        wait_all_flushed(&ingest).await;
        ingest.health().flushed_seq
    };
    // Put the flushed records back, and forget their dedup keys in the catalog, as an old folder would.
    for e in std::fs::read_dir(saved.path()).unwrap().filter_map(Result::ok) {
        std::fs::copy(e.path(), dir.path().join("wal").join(e.file_name())).unwrap();
    }
    forget_dedup_keys(dir.path());
    assert_eq!(wal_seqs(dir.path()).len(), 200);

    let (ingest, stats) = Ingest::open(cfg(dir.path(), window)).unwrap();
    assert_eq!(stats.flushed_seq, flushed_seq);
    assert_eq!(stats.skipped_flushed, 200, "{stats:?}");
    assert_eq!((stats.replayed_events, stats.runs), (0, 0), "{stats:?}");
    assert_eq!(ingest.health().segment_rows, 200, "each run stored once");
    drop(ingest);
    assert_eq!(wal_seqs(dir.path()), Vec::<u64>::new(), "trimmed on start");
    // Their keys moved to the catalog before the trim, so a later restart still knows them.
    let (ingest, _) = Ingest::open(cfg(dir.path(), window)).unwrap();
    assert_retries_write_nothing(&ingest, 1..=200).await;
}

fn forget_dedup_keys(dir: &Path) {
    let url = format!("sqlite://{}", dir.join("meta.sqlite").display());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    std::thread::spawn(move || {
        rt.block_on(async {
            let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
            sqlx::query("DELETE FROM dedup_keys").execute(&pool).await.unwrap();
            pool.close().await;
        })
    })
    .join()
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_multi_version_set_is_rebuilt_from_needs_merge_rows() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (ingest, _) = Ingest::open(cfg(dir.path(), 0)).unwrap();
        upload(&ingest, 1, 20).await;
        wait_all_flushed(&ingest).await;
        // Updates for runs that are only in files: their new versions are flushed with `needs_merge`.
        for i in 1..=3 {
            let mut end = root(uuid(i), "2026-11-05T02:00:00Z", "r");
            end.end_time = tracedb_model::Ts::parse("2026-11-05T02:05:00Z");
            ingest.accept(vec![patch(end)]).await.unwrap();
        }
        wait_all_flushed(&ingest).await;
    }
    let (ingest, stats) = Ingest::open(cfg(dir.path(), 0)).unwrap();
    assert_eq!(stats.replayed_events, 0, "nothing comes from the WAL: {stats:?}");
    assert_eq!(stats.multi_version_runs, 3, "{stats:?}");
    let mv = ingest.multi_version();
    let mv = mv.read().unwrap();
    assert!((1..=3).all(|i| mv.contains(&uuid(i))));
    assert!(!mv.contains(&uuid(4)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_data_file_is_reported_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let lost = {
        let (ingest, _) = Ingest::open(cfg(dir.path(), 0)).unwrap();
        upload(&ingest, 1, 200).await;
        wait_all_flushed(&ingest).await;
        let mem = ingest.mem();
        let m = mem.read().unwrap();
        assert!(m.version.segments.len() >= 2);
        let s = m.version.segments[0].clone();
        std::fs::remove_file(dir.path().join("data").join(&s.payload_path)).unwrap();
        s
    };
    let (ingest, stats) = Ingest::open(cfg(dir.path(), 0)).unwrap();
    let h = ingest.health();
    assert_eq!(h.corrupt_segments, vec![lost.id.clone()]);
    assert!(!h.ok && h.wal_ok && h.flush_ok, "{h:?}");
    assert_eq!(h.segment_rows, 200 - lost.row_count, "the other files are still served");
    assert_eq!(stats.segments, h.segments);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_m1_data_folder_opens_with_no_manual_step() {
    // M1 left only `wal/`: no catalog, no data files. Generated here with the M1 log format rather than
    // checked in as a fixture.
    let dir = tempfile::tempdir().unwrap();
    {
        let mut c = IngestConfig::new(dir.path());
        c.limits.dedup_window_us = 0;
        let (ingest, _) = Ingest::open(c).unwrap();
        upload(&ingest, 1, 50).await;
    }
    for f in ["meta.sqlite", "meta.sqlite-wal", "meta.sqlite-shm", "LOCK"] {
        let _ = std::fs::remove_file(dir.path().join(f));
    }
    std::fs::remove_dir_all(dir.path().join("data")).unwrap();
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "only wal/ is left");

    let (ingest, stats) = Ingest::open(cfg(dir.path(), 0)).unwrap();
    assert_eq!((stats.flushed_seq, stats.segments), (0, 0));
    assert_eq!(stats.runs, 50, "the whole M1 log is replayed");
    assert!(dir.path().join("meta.sqlite").exists(), "a fresh catalog is created");
    // …and moves to data files on the next flush.
    wait_all_flushed(&ingest).await;
    assert_eq!(ingest.health().segment_rows, 50);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_the_wal_no_longer_covers_are_never_deleted_as_orphans() {
    let dir = tempfile::tempdir().unwrap();
    let small_wal = |d: &Path| {
        let mut c = cfg(d, 0);
        c.wal_roll_bytes = 4096;
        c
    };
    {
        let (ingest, _) = Ingest::open(small_wal(dir.path())).unwrap();
        upload(&ingest, 1, 200).await;
        wait_all_flushed(&ingest).await;
    }
    let files = |d: &Path| walk(&d.join("data")).len();
    let before = files(dir.path());
    assert!(before > 0);
    // The catalog is lost (or `--metastore` points elsewhere), and the trimmed WAL can't rebuild the files.
    for f in ["meta.sqlite", "meta.sqlite-wal", "meta.sqlite-shm", "LOCK"] {
        let _ = std::fs::remove_file(dir.path().join(f));
    }
    let err = Ingest::open(small_wal(dir.path())).err().expect("refuses to start");
    assert!(err.to_string().contains("not in the catalog"), "{err}");
    assert_eq!(files(dir.path()), before, "nothing deleted");
}

fn walk(p: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(p).unwrap().filter_map(Result::ok) {
        if e.file_type().unwrap().is_dir() {
            out.extend(walk(&e.path()));
        } else {
            out.push(e.path());
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_process_on_the_same_folder_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (first, _) = Ingest::open(cfg(dir.path(), 0)).unwrap();
    let err = Ingest::open(cfg(dir.path(), 0)).err().expect("second open must fail");
    assert!(matches!(err, tracedb_ingest::OpenError::Locked(_)), "{err}");
    drop(first);
    Ingest::open(cfg(dir.path(), 0)).expect("free again once the first is gone");
}
