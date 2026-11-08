#![allow(dead_code)]

use std::time::{Duration, Instant};

use serde_json::Value;
use smithling_ingest::{AcceptError, Incoming, Ingest, IngestConfig, Limits};
use smithling_model::{Op, RunFields, Source, Ts};
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

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

pub fn ts(s: &str) -> Ts {
    Ts::parse(s).unwrap()
}

/// `2026-11-05T02:30:00.000000Z` → `20261105T023000000000Z`.
fn dotted_stamp(t: Ts) -> String {
    t.to_string().replace(['-', ':', '.'], "")
}

pub const MIN_US: i64 = 60_000_000;
pub const BATCH_A: &str = "batch=a";
pub const BATCH_B: &str = "batch=b";
/// Batch a starts here and runs 12 h, with no run between 05:00 and 06:00 and an error spike at 02:30–02:45.
pub const A_START: &str = "2026-11-05T00:00:00Z";
/// Batch b starts a day later and runs 10 h.
pub const B_START: &str = "2026-11-06T00:00:00Z";
pub const NAMES: [&str; 4] = ["fetch_url", "llm_call", "plan", "parse"];

/// Two batches of `traces` traces each: a root and 2–5 steps (names above, plus `only_a` / `only_b`), latencies
/// 5 ms … 90 s, some errors, some never ending, 40% of ends sent after every start (so a flush in between
/// leaves multi-version runs); `metadata.v` on every run of batch a, on 90% of batch b's.
pub fn batches(seed: u64, traces: u64) -> Vec<(Op, RunFields)> {
    let mut rng = Rng(seed.wrapping_mul(7919));
    let (mut posts, mut late) = (Vec::new(), Vec::new());
    let mut n = 1;
    for (tag, start, hours, only) in [(BATCH_A, A_START, 12, "only_a"), (BATCH_B, B_START, 10, "only_b")] {
        let t0 = ts(start).0;
        for _ in 0..traces {
            let mut minute = rng.below(hours * 60) as i64;
            if tag == BATCH_A && (280..360).contains(&minute) {
                minute -= 80;
            }
            let root_at = Ts(t0 + minute * MIN_US + rng.below(60) as i64 * 1_000_000);
            let root = uid(n);
            n += 1;
            let root_dotted = format!("{}{root}", dotted_stamp(root_at));
            let mut runs = vec![(root, root_dotted.clone(), None, "root", root_at)];
            for k in 0..2 + rng.below(4) {
                let id = uid(n);
                n += 1;
                let at = Ts(root_at.0 + (k as i64 + 1) * 1_000_000 * (1 + rng.below(90) as i64));
                let name = if rng.chance(3) {
                    only
                } else {
                    NAMES[rng.below(NAMES.len() as u64) as usize]
                };
                runs.push((
                    id,
                    format!("{root_dotted}.{}{id}", dotted_stamp(at)),
                    Some(root),
                    name,
                    at,
                ));
            }
            for (id, dotted, parent, name, at) in runs {
                let spike = tag == BATCH_A && (150..165).contains(&((at.0 - t0) / MIN_US));
                // Some of batch b's runs have no `metadata.v`.
                let metadata = if tag == BATCH_B && rng.chance(10) {
                    serde_json::json!({})
                } else {
                    serde_json::json!({"v": format!("v{}", rng.below(3))})
                };
                posts.push((
                    Op::Post,
                    RunFields {
                        id: Some(id),
                        trace_id: Some(root),
                        parent_run_id: parent,
                        dotted_order: Some(dotted.clone()),
                        project: Some("p".into()),
                        name: Some(name.into()),
                        run_type: Some(if parent.is_some() { "tool" } else { "chain" }.into()),
                        start_time: Some(at),
                        tags: Some(vec![tag.into()]),
                        metadata: Some(metadata.as_object().unwrap().clone()),
                        ..Default::default()
                    },
                ));
                if rng.chance(8) {
                    continue;
                }
                let ms = 5 + (rng.below(1000) * rng.below(1000) * rng.below(90)) as i64 / 1000;
                let failed = rng.chance(if spike { 80 } else { 5 });
                let end = RunFields {
                    id: Some(id),
                    trace_id: Some(root),
                    dotted_order: Some(dotted),
                    end_time: Some(Ts(at.0 + ms * 1000)),
                    error: failed.then(|| "boom".to_string()),
                    ..Default::default()
                };
                if rng.chance(40) {
                    late.push((Op::Patch, end));
                } else {
                    posts.push((Op::Patch, end));
                }
            }
        }
    }
    posts.extend(late);
    posts
}

/// `batches` loaded into a fresh store with memtables of `memtable_bytes`, flushed as they fill.
pub async fn loaded(seed: u64, traces: u64, memtable_bytes: u64) -> (tempfile::TempDir, Ingest) {
    let (dir, ingest) = open(memtable_bytes);
    send(&ingest, batches(seed, traces)).await;
    settle(&ingest, memtable_bytes).await;
    (dir, ingest)
}
