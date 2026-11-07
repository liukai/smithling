mod common;

use std::fs;
use std::path::Path;

use common::*;
use tracedb_ingest::store::Include;
use tracedb_ingest::wal::{DEFAULT_ROLL_BYTES, Wal};
use tracedb_ingest::{AcceptError, Ingest, IngestConfig};
use tracedb_model::Reason;

fn open(dir: &Path) -> Ingest {
    Ingest::open(IngestConfig::new(dir)).unwrap().0
}

fn wal_records(data_dir: &Path) -> u64 {
    let copy = tempfile::tempdir().unwrap();
    for e in fs::read_dir(data_dir.join("wal")).unwrap() {
        let p = e.unwrap().path();
        fs::copy(&p, copy.path().join(p.file_name().unwrap())).unwrap();
    }
    Wal::open(copy.path(), DEFAULT_ROLL_BYTES, |_| {}).unwrap().1.records
}

#[tokio::test]
async fn concurrent_uploads_are_durable_once_confirmed() {
    let dir = tempfile::tempdir().unwrap();
    let ingest = open(dir.path());
    let mut tasks = Vec::new();
    for c in 0..8u128 {
        let ingest = ingest.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..50u128 {
                let id = uuid(c * 1000 + i + 1);
                let r = ingest
                    .accept(vec![post(root(id, "2026-11-05T02:00:00Z", "r"))])
                    .await
                    .unwrap();
                assert_eq!(r.accepted, 1);
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(
        wal_records(dir.path()),
        400,
        "everything confirmed is already in the log on disk"
    );
    assert_eq!(ingest.health().runs, 400);
    drop(ingest);

    let (reopened, stats) = Ingest::open(IngestConfig::new(dir.path())).unwrap();
    assert_eq!(stats.replayed_events, 400);
    assert_eq!(reopened.health().runs, 400);
}

#[tokio::test]
async fn duplicates_are_stored_once_even_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let t = uuid(1);
    let batch = vec![
        post(root(t, "2026-11-05T02:00:00Z", "r")),
        post(child(t, "2026-11-05T02:00:00Z", uuid(2), "2026-11-05T02:00:01Z")),
    ];
    let ingest = open(dir.path());
    assert_eq!(ingest.accept(batch.clone()).await.unwrap().accepted, 2);
    assert_eq!(
        ingest.accept(batch.clone()).await.unwrap().accepted,
        2,
        "a retry is confirmed again"
    );
    drop(ingest);

    let ingest = open(dir.path());
    assert_eq!(ingest.accept(batch).await.unwrap().accepted, 2);
    let snap = ingest.snapshot();
    assert_eq!(snap.run(t, Include::default()).unwrap().header.event_count, 1);
    assert_eq!(wal_records(dir.path()), 2);
}

#[tokio::test]
async fn bad_items_are_rejected_one_by_one() {
    let dir = tempfile::tempdir().unwrap();
    let ingest = open(dir.path());
    let t = uuid(1);
    ingest
        .accept(vec![post(root(t, "2026-11-05T02:00:00Z", "r"))])
        .await
        .unwrap();

    let mut no_trace = root(uuid(5), "2026-11-05T02:00:00Z", "x");
    no_trace.trace_id = None;
    let mut moved = root(t, "2026-11-05T02:00:00Z", "r");
    moved.project = Some("elsewhere".into());
    let good = post(child(t, "2026-11-05T02:00:00Z", uuid(2), "2026-11-05T02:00:01Z"));
    let r = ingest.accept(vec![post(no_trace), patch(moved), good]).await.unwrap();
    assert_eq!(r.accepted, 1);
    let mut reasons: Vec<Reason> = r.rejected.iter().map(|x| x.reason).collect();
    reasons.sort_by_key(|r| format!("{r:?}"));
    assert_eq!(reasons, vec![Reason::IdentityConflict, Reason::MissingField]);
    assert_eq!(ingest.health().runs, 2);
}

#[tokio::test]
async fn changes_are_published_before_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let ingest = open(dir.path());
    let mut rx = ingest.subscribe();
    let t = uuid(1);
    ingest
        .accept(vec![post(root(t, "2026-11-05T02:00:00Z", "r"))])
        .await
        .unwrap();
    let change = rx.try_recv().expect("published by the time accept returns");
    assert_eq!(change.header.run_id, t);
    assert_eq!(change.kind, tracedb_model::ChangeKind::NewRoot);
    assert!(change.is_new());

    let u = uuid(2);
    ingest
        .accept(vec![patch(root(u, "2026-11-05T02:00:00Z", "r"))])
        .await
        .unwrap();
    let change = rx.try_recv().unwrap();
    assert!(
        !change.is_new(),
        "a run first seen through an update may exist elsewhere"
    );
}

#[tokio::test]
async fn too_many_in_flight_is_busy() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = IngestConfig::new(dir.path());
    cfg.queue_events = 4;
    let (ingest, _) = Ingest::open(cfg).unwrap();
    let mut handles = Vec::new();
    for i in 0..64u128 {
        let ingest = ingest.clone();
        handles.push(tokio::spawn(async move {
            let batch = (0..4u128)
                .map(|j| post(root(uuid(i * 10 + j + 1), "2026-11-05T02:00:00Z", "r")))
                .collect();
            ingest.accept(batch).await
        }));
    }
    let mut busy = 0;
    for h in handles {
        match h.await.unwrap() {
            Err(AcceptError::Busy) => busy += 1,
            Ok(r) => assert_eq!(r.accepted, 4),
            Err(e) => panic!("unexpected {e}"),
        }
    }
    assert!(
        busy > 0,
        "with room for one batch in flight, concurrent uploads see Busy"
    );
}

#[tokio::test]
async fn a_rejected_retry_is_rejected_again_and_leaves_no_trace() {
    let dir = tempfile::tempdir().unwrap();
    let ingest = open(dir.path());
    let t = uuid(1);
    let mut first = root(t, "2026-11-05T02:00:00Z", "r");
    first.project = Some("p".into());
    ingest.accept(vec![post(first)]).await.unwrap();
    let records = wal_records(dir.path());

    let mut moved = root(t, "2026-11-05T02:00:00Z", "r");
    moved.project = Some("q".into());
    for attempt in 0..2 {
        let r = ingest.accept(vec![patch(moved.clone())]).await.unwrap();
        assert_eq!(r.accepted, 0, "attempt {attempt}");
        assert_eq!(r.rejected.len(), 1, "attempt {attempt}");
        assert_eq!(r.rejected[0].reason, Reason::IdentityConflict, "attempt {attempt}");
    }
    assert_eq!(
        wal_records(dir.path()),
        records,
        "rejected items are never written to the log"
    );
}

#[tokio::test]
async fn identity_conflicts_inside_one_batch_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ingest = open(dir.path());
    let t = uuid(7);
    let a = root(t, "2026-11-05T02:00:00Z", "r");
    let mut b = root(t, "2026-11-05T02:00:00Z", "r");
    b.project = Some("other".into());
    let mut a2 = a.clone();
    a2.project = Some("p".into());
    let r = ingest.accept(vec![post(a2), patch(b)]).await.unwrap();
    assert_eq!(r.accepted, 1);
    assert_eq!(r.rejected[0].reason, Reason::IdentityConflict);
}

fn flushing(dir: &Path) -> IngestConfig {
    let mut cfg = IngestConfig::new(dir);
    cfg.limits = tracedb_ingest::Limits {
        flush_age_us: 100_000,
        dedup_window_us: 0,
        ..tracedb_ingest::Limits::default()
    };
    cfg
}

async fn wait_all_flushed(ingest: &Ingest) {
    let snap = ingest.snapshot();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let h = ingest.health();
        if h.runs == 0 && h.flushed_seq == snap.last_seq() {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "timed out: {h:?}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Deletes the saved dedup keys, so only the data files say which runs were flushed.
fn forget_dedup_keys(dir: &Path) {
    let url = format!("sqlite://{}", dir.join("meta.sqlite").display());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
            sqlx::query("DELETE FROM dedup_keys").execute(&pool).await.unwrap();
            pool.close().await;
        })
    })
    .join()
    .unwrap();
}

fn again(i: u128) -> tracedb_ingest::Incoming {
    let mut f = root(uuid(i), "2026-11-05T02:14:07Z", "r");
    f.inputs = Some(serde_json::json!({"q": "second start"}));
    post(f)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_start_for_a_flushed_run_joins_the_multi_version_set() {
    let dir = tempfile::tempdir().unwrap();
    {
        let (ingest, _) = Ingest::open(flushing(dir.path())).unwrap();
        let first: Vec<_> = (1..=4)
            .map(|i| post(root(uuid(i), "2026-11-05T02:14:07Z", "r")))
            .collect();
        assert_eq!(ingest.accept(first).await.unwrap().accepted, 4);
        wait_all_flushed(&ingest).await;

        assert_eq!(ingest.accept(vec![again(1)]).await.unwrap().accepted, 1);
        // Only a new memtable state is added to the set.
        assert!(ingest.multi_version().read().unwrap().contains(&uuid(1)));
        assert!(!ingest.multi_version().read().unwrap().contains(&uuid(2)));
        wait_all_flushed(&ingest).await;
        // Left in the WAL for the restart: replay must mark it too.
        let mut cfg = flushing(dir.path());
        cfg.limits.flush_age_us = i64::MAX / 2;
        drop(ingest);
        let (ingest, _) = Ingest::open(cfg).unwrap();
        assert_eq!(ingest.accept(vec![again(3)]).await.unwrap().accepted, 1);
    }
    forget_dedup_keys(dir.path());
    let (ingest, stats) = Ingest::open(flushing(dir.path())).unwrap();
    assert_eq!(stats.replayed_events, 1, "{stats:?}");
    let mv = ingest.multi_version();
    assert!(mv.read().unwrap().contains(&uuid(1)), "rebuilt from needs_merge rows");
    assert!(
        mv.read().unwrap().contains(&uuid(3)),
        "a replayed second start hits the rebuilt filter"
    );
    assert_eq!(ingest.accept(vec![again(2)]).await.unwrap().accepted, 1);
    assert!(
        mv.read().unwrap().contains(&uuid(2)),
        "a live second start hits the rebuilt filter"
    );
    assert!(!mv.read().unwrap().contains(&uuid(4)));
}
