use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::run::{JsonMap, PayloadSizes, RunId, RunState, RunType, Status, TraceId, Warning};
use crate::ts::Ts;

/// Every run field except the payloads (`inputs`, `outputs`, `events`), plus their sizes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunHeader {
    pub run_id: RunId,
    pub trace_id: TraceId,
    pub parent_run_id: Option<RunId>,
    pub dotted_order: String,
    pub depth: u16,
    pub is_root: bool,
    pub project: String,
    pub tags: Vec<String>,
    pub metadata: JsonMap,
    pub extra: JsonMap,
    pub thread_id: Option<String>,
    pub name: Option<String>,
    pub run_type: RunType,
    pub run_type_raw: Option<String>,
    pub start_time: Option<Ts>,
    pub end_time: Option<Ts>,
    pub error: Option<String>,
    pub status: Status,
    pub latency_ms: Option<u64>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub dropped: BTreeMap<String, u64>,
    pub warnings: Vec<Warning>,
    pub payload_sizes: PayloadSizes,
    pub child_count: u32,
    pub first_seq: u64,
    pub last_seq: u64,
    pub event_count: u32,
}

impl RunHeader {
    pub fn from_state(st: &RunState, child_count: u32) -> RunHeader {
        let r = &st.run;
        RunHeader {
            run_id: r.run_id,
            trace_id: r.trace_id,
            parent_run_id: r.parent_run_id,
            dotted_order: r.dotted_order.clone(),
            depth: r.depth,
            is_root: r.is_root,
            project: r.project.clone(),
            tags: r.tags.clone(),
            metadata: r.metadata.clone(),
            extra: r.extra.clone(),
            thread_id: r.thread_id.clone(),
            name: r.name.clone(),
            run_type: r.run_type,
            run_type_raw: r.run_type_raw.clone(),
            start_time: r.start_time,
            end_time: r.end_time,
            error: r.error.clone(),
            status: r.status,
            latency_ms: r.latency_ms,
            model: r.model.clone(),
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            total_tokens: r.total_tokens,
            dropped: r.dropped.clone(),
            warnings: r.warnings.clone(),
            payload_sizes: st.payload_sizes,
            child_count,
            first_seq: st.first_seq,
            last_seq: st.last_seq,
            event_count: st.event_count,
        }
    }
}

/// A header plus the payload fields that were asked for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunView {
    #[serde(flatten)]
    pub header: RunHeader,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<serde_json::Value>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceView {
    pub root: Option<RunHeader>,
    /// Sorted by `dotted_order` (depth-first, siblings by start time).
    pub runs: Vec<RunHeader>,
    pub run_count: usize,
    pub has_running: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RootsPage {
    pub runs: Vec<RunHeader>,
    pub next_cursor: Option<String>,
}
