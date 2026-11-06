//! A real storage fault (the WAL directory becomes read-only, so the next file roll fails):
//! confirmations stop, health reports it, and everything confirmed before survives.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use common::*;
use tracedb_ingest::{AcceptError, Ingest, IngestConfig};

#[tokio::test]
async fn a_failed_write_stops_confirmations_and_keeps_confirmed_data() {
    let dir = tempfile::tempdir().unwrap();
    let wal_dir = dir.path().join("wal");
    let mut cfg = IngestConfig::new(dir.path());
    cfg.wal_roll_bytes = 2048;
    let (ingest, _) = Ingest::open(cfg.clone()).unwrap();

    let ok = ingest
        .accept(vec![post(root(uuid(1), "2026-11-05T02:00:00Z", "kept"))])
        .await
        .unwrap();
    assert_eq!(ok.accepted, 1);

    fs::set_permissions(&wal_dir, fs::Permissions::from_mode(0o555)).unwrap();
    let mut big = root(uuid(2), "2026-11-05T02:00:00Z", "lost");
    big.inputs = Some(serde_json::json!("x".repeat(4096)));
    let err = ingest.accept(vec![post(big)]).await.unwrap_err();
    assert!(matches!(err, AcceptError::Storage(_)), "{err:?}");
    let health = ingest.health();
    assert!(!health.wal_ok && health.error.is_some());
    let again = ingest
        .accept(vec![post(root(uuid(3), "2026-11-05T02:00:00Z", "x"))])
        .await
        .unwrap_err();
    assert!(matches!(again, AcceptError::Storage(_)), "stays refused until restart");
    drop(ingest);

    fs::set_permissions(&wal_dir, fs::Permissions::from_mode(0o755)).unwrap();
    let (reopened, _) = Ingest::open(cfg).unwrap();
    assert_eq!(
        reopened.health().runs,
        1,
        "the confirmed run is there; the refused one is not"
    );
}
