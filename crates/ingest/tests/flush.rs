//! Flush: a full memtable becomes registered data files, the WAL is trimmed only after, and a restart
//! replays only what isn't in files yet (design §4.2, §4.6).

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use smithling_ingest::{Ingest, IngestConfig, Limits};

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
    upload_with(ingest, from, n, |_| serde_json::json!({"q": "x".repeat(400)})).await
}

async fn upload_with(ingest: &Ingest, from: u128, n: u128, inputs: impl Fn(u128) -> serde_json::Value) {
    for i in from..from + n {
        let mut f = root(uuid(i), "2026-11-05T02:00:00Z", "r");
        f.inputs = Some(inputs(i));
        loop {
            match ingest.accept(vec![post(f.clone())]).await {
                Ok(r) => {
                    assert_eq!(r.accepted, 1);
                    break;
                }
                Err(smithling_ingest::AcceptError::Busy) => tokio::time::sleep(Duration::from_millis(20)).await,
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
    end.end_time = smithling_model::Ts::parse("2026-11-05T02:05:00Z");
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

fn segment_rows_in_catalog(dir: &Path) -> (i64, i64) {
    let url = format!("sqlite://{}", dir.join("meta.sqlite").display());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
            let r: (i64, i64) = sqlx::query_as("SELECT count(*), coalesce(sum(row_count), 0) FROM segments")
                .fetch_one(&pool)
                .await
                .unwrap();
            pool.close().await;
            r
        })
    })
    .join()
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retried_flush_whose_commit_landed_registers_its_files_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = small(dir.path());
    cfg.test_lost_commit_replies = 2;
    let (ingest, _) = Ingest::open(cfg).unwrap();
    upload(&ingest, 1, 300).await;
    wait_for(|| {
        let h = ingest.health();
        h.flushes >= 3 && h.flush_ok && !h.frozen
    })
    .await;
    let mem = ingest.mem();
    let (ids, seg_rows, mem_runs) = {
        let m = mem.read().unwrap();
        let ids: Vec<String> = m.version.segments.iter().map(|s| s.id.clone()).collect();
        let rows: u64 = m.version.segments.iter().map(|s| s.row_count).sum();
        (ids, rows, m.active.len() as u64)
    };
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "a segment published twice: {ids:?}");
    assert_eq!(seg_rows + mem_runs, 300, "every run stored once");
    let h = ingest.health();
    drop(ingest);
    assert_eq!(
        segment_rows_in_catalog(dir.path()),
        (ids.len() as i64, seg_rows as i64),
        "catalog rows match the published files"
    );
    let (_, stats) = Ingest::open(small(dir.path())).unwrap();
    assert_eq!(stats.segments, ids.len());
    assert_eq!(stats.runs as u64, mem_runs);
    assert_eq!(stats.flushed_seq, h.flushed_seq);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_new_file_gets_a_text_index_registered_in_the_same_commit() {
    use smithling_storage::fts::{self, Fts, TextField, TextIndex};
    use smithling_storage::{Store, segment};
    let dir = tempfile::tempdir().unwrap();
    let (ingest, _) = Ingest::open(small(dir.path())).unwrap();
    upload_with(
        &ingest,
        1,
        300,
        |i| serde_json::json!({"q": format!("needle{i} {}", "x".repeat(400))}),
    )
    .await;
    wait_for(|| ingest.health().segments >= 2 && !ingest.health().frozen).await;
    let segs: Vec<_> = {
        let mem = ingest.mem();
        let m = mem.read().unwrap();
        m.version.segments.iter().map(|s| (**s).clone()).collect()
    };
    drop(ingest);

    let data = dir.path().join("data");
    let store = Store::local(&data).unwrap();
    let index = Fts::new(store.clone());
    let mut hits = Vec::new();
    for s in &segs {
        let stem = s.fts_path.clone().expect("every new file has a sidecar");
        assert_eq!(
            stem,
            s.core_path.replace(".core.parquet", ".fts"),
            "beside its core file"
        );
        let (dict, post) = (data.join(fts::dict_path(&stem)), data.join(fts::post_path(&stem)));
        assert!(stem.ends_with(&format!("seg-{}.fts", s.id)));
        let bytes = std::fs::metadata(&dict).unwrap().len() + std::fs::metadata(&post).unwrap().len();
        assert_eq!(s.fts_bytes, bytes);
        for row in index
            .lookup(s, &["needle77".to_string()], TextField::Inputs)
            .await
            .unwrap()
        {
            let ids = segment::run_ids(store.get(&s.core_path).await.unwrap()).unwrap();
            hits.push(ids[row as usize]);
        }
    }
    assert_eq!(hits, vec![uuid(77)], "the one run with that word");
    let url = format!("sqlite://{}", dir.path().join("meta.sqlite").display());
    let unindexed: i64 = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
                let n = sqlx::query_scalar("SELECT count(*) FROM segments WHERE fts_path IS NULL OR fts_bytes IS NULL")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                pool.close().await;
                n
            })
    })
    .join()
    .unwrap();
    assert_eq!(unindexed, 0);

    // Sidecars put by a flush that never committed are orphans like its data files.
    let day = data.join("p-00000000/2026-11-06");
    std::fs::create_dir_all(&day).unwrap();
    let orphans = [
        day.join("seg-X.fts.dict.parquet"),
        day.join("seg-X.fts.post"),
        day.join("seg-Z.fts-01JD0000000000000000000000.post"),
        day.join(".seg-Y.fts.post.tmp-01JD0000000000000000000000"),
    ];
    for o in &orphans {
        std::fs::write(o, b"half written").unwrap();
    }
    let (ingest, stats) = Ingest::open(small(dir.path())).unwrap();
    assert_eq!(stats.orphans_removed, orphans.len());
    assert!(orphans.iter().all(|o| !o.exists()));
    for s in &segs {
        let stem = s.fts_path.as_deref().unwrap();
        assert!(data.join(fts::dict_path(stem)).exists() && data.join(fts::post_path(stem)).exists());
    }
    assert!(ingest.health().ok);
}
