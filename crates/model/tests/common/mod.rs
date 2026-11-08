#![allow(dead_code)]

use smithling_model::{Op, RunEvent, RunFields, Source, Ts};
use uuid::Uuid;

pub const TRACE: &str = "0b9f0000-0000-4000-8000-000000000011";
pub const RUN: &str = "7c1e0000-0000-4000-8000-0000000000a4";

pub fn trace_id() -> Uuid {
    Uuid::parse_str(TRACE).unwrap()
}

pub fn run_id() -> Uuid {
    Uuid::parse_str(RUN).unwrap()
}

pub fn root_dotted() -> String {
    format!("20261105T021400000000Z{TRACE}")
}

pub fn child_dotted() -> String {
    format!("{}.20261105T021407123456Z{RUN}", root_dotted())
}

pub fn ts(s: &str) -> Ts {
    Ts::parse(s).unwrap()
}

/// Identity-only fields for the `fetch_url` child run.
pub fn child_ids() -> RunFields {
    RunFields {
        id: Some(run_id()),
        trace_id: Some(trace_id()),
        parent_run_id: Some(trace_id()),
        dotted_order: Some(child_dotted()),
        ..Default::default()
    }
}

pub fn root_ids() -> RunFields {
    RunFields {
        id: Some(trace_id()),
        trace_id: Some(trace_id()),
        dotted_order: Some(root_dotted()),
        ..Default::default()
    }
}

pub fn post_fields(mut f: RunFields, name: &str) -> RunFields {
    f.name = Some(name.into());
    f.run_type = Some("tool".into());
    f.start_time = Some(ts("2026-11-05T02:14:07.123456Z"));
    f
}

pub fn event(op: Op, seq: u64, fields: RunFields) -> RunEvent {
    RunEvent {
        op,
        event_seq: seq,
        received_at: Ts(seq as i64),
        source: Source::Native,
        body_hash: seq,
        fields,
    }
}
