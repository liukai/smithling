#![allow(dead_code)]

use std::time::{Duration, Instant};

use serde_json::Value;
use tracedb_ingest::{AcceptError, Incoming, Ingest, IngestConfig, Limits};
use tracedb_model::{Op, RunFields, Source, Ts};
use uuid::Uuid;

pub fn uid(n: u64) -> Uuid {
    Uuid::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0000 | n as u128)
}

/// `sec` seconds after 2026-11-05T00:00Z, as a dotted-order stamp and as a time.
fn stamp(sec: u64) -> (String, Option<Ts>) {
    let (h, m, s) = (sec / 3600, sec / 60 % 60, sec % 60);
    (
        format!("20261105T{h:02}{m:02}{s:02}000000Z"),
        Ts::parse(&format!("2026-11-05T{h:02}:{m:02}:{s:02}Z")),
    )
}

pub fn root(n: u64, project: &str, sec: u64) -> RunFields {
    let (st, at) = stamp(sec);
    RunFields {
        id: Some(uid(n)),
        trace_id: Some(uid(n)),
        dotted_order: Some(format!("{st}{}", uid(n))),
        project: Some(project.into()),
        name: Some("root".into()),
        run_type: Some("chain".into()),
        start_time: at,
        ..Default::default()
    }
}

pub fn child(parent: &RunFields, n: u64, sec: u64) -> RunFields {
    let (st, at) = stamp(sec);
    RunFields {
        id: Some(uid(n)),
        trace_id: parent.trace_id,
        parent_run_id: parent.id,
        dotted_order: Some(format!("{}.{st}{}", parent.dotted_order.as_ref().unwrap(), uid(n))),
        project: parent.project.clone(),
        name: Some("step".into()),
        run_type: Some("tool".into()),
        start_time: at,
        ..Default::default()
    }
}

pub fn with_metadata(mut f: RunFields, m: Value) -> RunFields {
    f.metadata = Some(m.as_object().unwrap().clone());
    f
}

/// The run as finished: in a start (`Op::Post`) it arrives in one piece; as an update it ends the run.
pub fn ended(f: &RunFields, error: Option<&str>) -> RunFields {
    RunFields {
        end_time: f.start_time.map(|t| Ts(t.0 + 30_000_000)),
        error: error.map(str::to_string),
        ..f.clone()
    }
}

pub fn end_update(f: &RunFields, error: Option<&str>) -> RunFields {
    RunFields {
        id: f.id,
        trace_id: f.trace_id,
        dotted_order: f.dotted_order.clone(),
        end_time: f.start_time.map(|t| Ts(t.0 + 30_000_000)),
        error: error.map(str::to_string),
        ..Default::default()
    }
}

pub fn open(memtable_bytes: u64) -> (tempfile::TempDir, Ingest) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = IngestConfig::new(dir.path());
    cfg.limits = Limits {
        memtable_bytes,
        dedup_window_us: 0,
        ..Limits::default()
    };
    let (ingest, _) = Ingest::open(cfg).unwrap();
    (dir, ingest)
}

pub async fn send(ingest: &Ingest, events: Vec<(Op, RunFields)>) {
    let items: Vec<Incoming> = events
        .into_iter()
        .enumerate()
        .map(|(i, (op, fields))| Incoming {
            op,
            source: Source::Native,
            body_hash: i as u64,
            fields,
        })
        .collect();
    for chunk in items.chunks(200) {
        loop {
            match ingest.accept(chunk.to_vec()).await {
                Ok(r) => {
                    assert_eq!(r.accepted, chunk.len(), "{:?}", r.rejected);
                    break;
                }
                Err(AcceptError::Busy) => tokio::time::sleep(Duration::from_millis(5)).await,
                Err(e) => panic!("{e}"),
            }
        }
    }
}

/// Waits until every full memtable has been flushed (the writer freezes just after it replies).
pub async fn settle(ingest: &Ingest, memtable_bytes: u64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while ingest.health().frozen || ingest.health().memtable_bytes >= memtable_bytes {
        assert!(Instant::now() < deadline, "flush did not finish");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
