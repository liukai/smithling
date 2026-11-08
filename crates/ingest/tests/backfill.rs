//! Backfill (q7, design §4.2): an M2 folder's files get a text index one at a time, each after a flush age
//! with no flush; the index is the one a flush would have written; a crash mid-backfill leaves only an
//! orphan, deleted on start.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use common::*;
use smithling_ingest::{Ingest, IngestConfig, Limits};

const AGE: Duration = Duration::from_millis(400);

fn config(dir: &Path, flush_age: Duration) -> IngestConfig {
    let mut cfg = IngestConfig::new(dir);
    cfg.limits = Limits {
        memtable_bytes: 16 * 1024,
        flush_age_us: flush_age.as_micros() as i64,
        dedup_window_us: 0,
        ..Limits::default()
    };
    cfg
}

fn sql<T, F>(dir: &Path, f: impl FnOnce(sqlx::SqlitePool) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T>,
{
    let url = format!("sqlite://{}", dir.join("meta.sqlite").display());
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
                let out = f(pool.clone()).await;
                pool.close().await;
                out
            })
    })
    .join()
    .unwrap()
}

/// Every sidecar's bytes by segment id, as written at flush.
fn sidecars(dir: &Path) -> BTreeMap<String, (Vec<u8>, Vec<u8>)> {
    let rows: Vec<(String, Option<String>)> = sql(dir, |p| async move {
        sqlx::query_as("SELECT id, fts_path FROM segments")
            .fetch_all(&p)
            .await
            .unwrap()
    });
    let data = dir.join("data");
    rows.into_iter()
        .filter_map(|(id, stem)| {
            let stem = stem?;
            let read = |ext: &str| std::fs::read(data.join(format!("{stem}{ext}"))).unwrap();
            Some((id, (read(".dict.parquet"), read(".post"))))
        })
        .collect()
}

/// Makes the folder look as M2 left it: no sidecars, no `fts_path`.
fn strip_indexes(dir: &Path) {
    let data = dir.join("data");
    for e in walk(&data) {
        if e.file_name().unwrap().to_string_lossy().contains(".fts") {
            std::fs::remove_file(e).unwrap();
        }
    }
    sql(dir, |p| async move {
        sqlx::query("UPDATE segments SET fts_path = NULL, fts_bytes = NULL")
            .execute(&p)
            .await
            .unwrap();
    });
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

async fn upload(ingest: &Ingest, n: u128) {
    for i in 1..=n {
        let mut f = root(uuid(i), "2026-11-05T02:00:00Z", "r");
        f.inputs = Some(serde_json::json!({"q": format!("run {i}: 429 Too Many Requests {}", "x".repeat(300))}));
        f.error = (i % 3 == 0).then(|| "HTTPError: rate limit".to_string());
        loop {
            match ingest.accept(vec![post(f.clone())]).await {
                Ok(_) => break,
                Err(smithling_ingest::AcceptError::Busy) => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(e) => panic!("{e}"),
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_m2_folder_is_indexed_one_file_per_idle_period() {
    let dir = tempfile::tempdir().unwrap();
    {
        // Everything flushed (by size, the rest by age), so a reopen has nothing to flush.
        let (ingest, _) = Ingest::open(config(dir.path(), AGE)).unwrap();
        upload(&ingest, 160).await;
        let deadline = Instant::now() + Duration::from_secs(20);
        while ingest.health().segments < 3 || ingest.health().frozen || ingest.health().runs > 0 {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let flushed = sidecars(dir.path());
    assert!(flushed.len() >= 3, "{}", flushed.len());
    strip_indexes(dir.path());

    let (ingest, _) = Ingest::open(config(dir.path(), AGE)).unwrap();
    let total = ingest.health().segments;
    assert_eq!(
        ingest.health().unindexed_segments,
        total,
        "an M2 folder: no file has an index"
    );
    let started = Instant::now();
    let mut seen = vec![(Duration::ZERO, total)];
    while seen.last().unwrap().1 > 0 {
        assert!(started.elapsed() < Duration::from_secs(30), "{seen:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
        let left = ingest.health().unindexed_segments;
        if left != seen.last().unwrap().1 {
            seen.push((started.elapsed(), left));
        }
    }
    for w in seen.windows(2) {
        assert_eq!(w[0].1 - w[1].1, 1, "one file at a time: {seen:?}");
        assert!(w[1].0 - w[0].0 >= AGE * 9 / 10, "one per idle period: {seen:?}");
    }
    // The index is the one a flush writes, so searches give the same rows before and after.
    let backfilled = sidecars(dir.path());
    assert_eq!(
        backfilled.keys().collect::<Vec<_>>(),
        flushed.keys().collect::<Vec<_>>()
    );
    assert!(
        backfilled == flushed,
        "backfilled sidecars differ from the flushed ones"
    );
    let mem = ingest.mem();
    let in_memory: Vec<_> = mem
        .read()
        .unwrap()
        .version
        .segments
        .iter()
        .map(|s| s.fts_path.clone())
        .collect();
    assert!(
        in_memory
            .iter()
            .all(|p| p.as_deref().is_some_and(|p| p.contains(".fts-")))
    );
    drop(mem);
    drop(ingest);
    let (reopened, _) = Ingest::open(config(dir.path(), Duration::from_secs(3600))).unwrap();
    assert_eq!(reopened.health().unindexed_segments, 0, "kept in the catalog");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphans_from_a_crash_mid_backfill_are_removed_on_start() {
    let dir = tempfile::tempdir().unwrap();
    {
        // A short flush age flushes everything, so the log is trimmed and no longer covers stray files.
        let (ingest, _) = Ingest::open(config(dir.path(), Duration::from_millis(200))).unwrap();
        upload(&ingest, 80).await;
        let deadline = Instant::now() + Duration::from_secs(20);
        while ingest.health().segments < 2 || ingest.health().frozen || ingest.health().runs > 0 {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let flushed = sidecars(dir.path());
    strip_indexes(dir.path());
    // Everything is in files: the log may be trimmed to nothing, so it covers no stray file.
    for f in walk(&dir.path().join("wal")) {
        std::fs::remove_file(f).unwrap();
    }
    // What a crash after the sidecar's put and before its catalog update leaves: a sidecar no one names.
    let (id, (dict, post)) = flushed.iter().next().unwrap();
    let core = walk(&dir.path().join("data"))
        .into_iter()
        .find(|p| p.to_string_lossy().ends_with(&format!("seg-{id}.core.parquet")))
        .unwrap();
    let stem = core.to_string_lossy().trim_end_matches(".core.parquet").to_string() + ".fts-1";
    std::fs::write(format!("{stem}.dict.parquet"), dict).unwrap();
    std::fs::write(format!("{stem}.post"), post).unwrap();

    // A long flush age: nothing is backfilled yet.
    let (ingest, _) = Ingest::open(config(dir.path(), Duration::from_secs(3600))).unwrap();
    assert!(
        !Path::new(&format!("{stem}.post")).exists(),
        "the orphan is deleted on start"
    );
    assert!(!Path::new(&format!("{stem}.dict.parquet")).exists());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let h = ingest.health();
    assert_eq!(h.unindexed_segments, h.segments);
}
