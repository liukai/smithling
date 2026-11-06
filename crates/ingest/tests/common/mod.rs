#![allow(dead_code)]

use tracedb_ingest::Incoming;
use tracedb_model::{Op, RunEvent, RunFields, Source, Ts};
use uuid::Uuid;

pub fn uuid(n: u128) -> Uuid {
    Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0000 | n)
}

fn seg(ts: &str, id: Uuid) -> String {
    format!("{ts}{id}")
}

/// Root run `trace` (a run whose id is the trace id).
pub fn root(trace: Uuid, start: &str, name: &str) -> RunFields {
    RunFields {
        id: Some(trace),
        trace_id: Some(trace),
        dotted_order: Some(seg(&dotted_ts(start), trace)),
        project: Some("p".into()),
        name: Some(name.into()),
        run_type: Some("chain".into()),
        start_time: Some(Ts::parse(start).unwrap()),
        ..Default::default()
    }
}

/// Child run `id` under root `trace`.
pub fn child(trace: Uuid, root_start: &str, id: Uuid, start: &str) -> RunFields {
    RunFields {
        id: Some(id),
        trace_id: Some(trace),
        parent_run_id: Some(trace),
        dotted_order: Some(format!(
            "{}.{}",
            seg(&dotted_ts(root_start), trace),
            seg(&dotted_ts(start), id)
        )),
        project: Some("p".into()),
        name: Some("step".into()),
        run_type: Some("tool".into()),
        start_time: Some(Ts::parse(start).unwrap()),
        ..Default::default()
    }
}

/// `2026-11-05T02:14:07Z` → `20261105T021407000000Z`
pub fn dotted_ts(iso: &str) -> String {
    let t = Ts::parse(iso).unwrap().to_string();
    let d: String = t.chars().filter(|c| c.is_ascii_digit()).collect();
    format!("{}T{}Z", &d[..8], &d[8..20])
}

pub fn post(fields: RunFields) -> Incoming {
    let body_hash = hash(&fields);
    Incoming {
        op: Op::Post,
        source: Source::Native,
        body_hash,
        fields,
    }
}

pub fn patch(fields: RunFields) -> Incoming {
    let body_hash = hash(&fields);
    Incoming {
        op: Op::Patch,
        source: Source::Native,
        body_hash,
        fields,
    }
}

fn hash(f: &RunFields) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    serde_json::to_string(f).unwrap().hash(&mut h);
    h.finish()
}

pub fn event(seq: u64, it: &Incoming) -> RunEvent {
    RunEvent {
        op: it.op,
        event_seq: seq,
        received_at: Ts(seq as i64),
        source: it.source,
        body_hash: it.body_hash,
        fields: it.fields.clone(),
    }
}
