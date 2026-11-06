mod common;

use std::fs;
use std::path::Path;

use common::*;
use tracedb_ingest::store::Include;
use tracedb_ingest::wal::{DEFAULT_ROLL_BYTES, Wal};
use tracedb_ingest::{AcceptError, Budget, Ingest, IngestConfig};
use tracedb_model::{Reason, Status};

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
async fn full_budget_refuses_new_runs_but_accepts_updates() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = IngestConfig::new(dir.path());
    cfg.budget = Budget {
        max_bytes: u64::MAX,
        max_runs: 2,
    };
    let (ingest, _) = Ingest::open(cfg).unwrap();
    let (a, b) = (uuid(1), uuid(2));
    ingest
        .accept(vec![
            post(root(a, "2026-11-05T02:00:00Z", "a")),
            post(root(b, "2026-11-05T02:00:00Z", "b")),
        ])
        .await
        .unwrap();

    let mut end = root(a, "2026-11-05T02:00:00Z", "a");
    end.end_time = Some(tracedb_model::Ts::parse("2026-11-05T02:05:00Z").unwrap());
    let err = ingest
        .accept(vec![
            post(root(uuid(3), "2026-11-05T02:00:00Z", "c")),
            patch(end.clone()),
        ])
        .await
        .unwrap_err();
    assert_eq!(err, AcceptError::Capacity);
    let snap = ingest.snapshot();
    assert_eq!(
        snap.run(a, Include::default()).unwrap().header.status,
        Status::Running,
        "nothing in a refused batch is applied"
    );

    assert_eq!(ingest.accept(vec![patch(end)]).await.unwrap().accepted, 1);
    assert_eq!(snap.run(a, Include::default()).unwrap().header.status, Status::Ok);
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
