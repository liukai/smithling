//! Write slowdown (R-8): when the flusher can't keep up, uploads are told "busy" instead of memory growing
//! or data being refused; once the client retries, nothing is lost.

mod common;

use std::time::{Duration, Instant};

use common::*;
use tracedb_ingest::{AcceptError, Ingest, IngestConfig, Limits};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_gets_busy_then_everything_lands() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = IngestConfig::new(dir.path());
    cfg.limits = Limits {
        memtable_bytes: 16 * 1024,
        dedup_window_us: 0,
        ..Limits::default()
    };
    cfg.test_flush_delay = Some(Duration::from_millis(300));
    let (ingest, _) = Ingest::open(cfg).unwrap();
    let (mut busy, mut max_mem) = (0, 0u64);
    let started = Instant::now();
    for i in 1..=200u128 {
        let mut f = root(uuid(i), "2026-11-05T02:00:00Z", "r");
        f.inputs = Some(serde_json::json!({"q": "y".repeat(300)}));
        loop {
            match ingest.accept(vec![post(f.clone())]).await {
                Ok(_) => break,
                Err(AcceptError::Busy) => {
                    busy += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(e) => panic!("{e}"),
            }
        }
        max_mem = max_mem.max(ingest.health().memtable_bytes);
        assert!(started.elapsed() < Duration::from_secs(60));
    }
    assert!(busy > 0, "the slow flusher made the writer push back");
    assert!(max_mem < 2 * 16 * 1024 + 4096, "memory stayed bounded: {max_mem}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while ingest.health().frozen || ingest.health().runs > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if ingest.health().runs > 0 && !ingest.health().frozen {
            break;
        }
    }
    let m = ingest.mem();
    let m = m.read().unwrap();
    let total = m.version.segments.iter().map(|s| s.row_count).sum::<u64>()
        + m.frozen.as_ref().map_or(0, |f| f.len() as u64)
        + m.active.len() as u64;
    assert_eq!(total, 200, "every confirmed run is somewhere, once");
}
