use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::ts::Ts;

pub type RunId = Uuid;
pub type TraceId = Uuid;
pub type JsonMap = Map<String, Value>;

pub const DEFAULT_PROJECT: &str = "default";

/// The fields an event may carry. Absent and `null` mean "no change".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunFields {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<RunId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<TraceId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<RunId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dotted_order: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// SDK 0.14+ agent address (`lrn:agents/<id>/environments/<env>`), kept as sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Raw value; normalized into `RunType` on merge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time: Option<Ts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_time: Option<Ts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<JsonMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<Value>>,
    /// Fields smithling received but does not store in M1 (e.g. `serialized`, attachments), with their sizes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped: Option<BTreeMap<String, u64>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunType {
    Llm,
    Chain,
    Tool,
    Retriever,
    Embedding,
    Prompt,
    Parser,
    Agent,
    Other,
}

impl RunType {
    /// Unknown values become `Other`; the original is returned so it can be kept in `run_type_raw`.
    pub fn from_raw(raw: &str) -> (RunType, Option<String>) {
        let t = match raw {
            "llm" => RunType::Llm,
            "chain" => RunType::Chain,
            "tool" => RunType::Tool,
            "retriever" => RunType::Retriever,
            "embedding" => RunType::Embedding,
            "prompt" => RunType::Prompt,
            "parser" => RunType::Parser,
            "agent" => RunType::Agent,
            _ => return (RunType::Other, Some(raw.to_string())),
        };
        (t, None)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Ok,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Warning {
    NegativeLatency,
    /// A later version disagreed on trace id, `dotted_order`, project or address; the first version's
    /// identity is kept (design §8, question 1).
    IdentityConflict,
}

/// One step: the merge of all its events. Derived fields are recomputed after every merge.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub run_id: RunId,
    pub trace_id: TraceId,
    pub parent_run_id: Option<RunId>,
    pub dotted_order: String,
    pub depth: u16,
    pub is_root: bool,

    pub project: String,
    pub address: Option<String>,
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
    /// The cost the run reported, in picodollars (`cost::reported_pico`).
    pub reported_cost_pico: Option<i64>,

    pub inputs: Option<Value>,
    pub outputs: Option<Value>,
    pub events: Option<Vec<Value>>,
    pub dropped: BTreeMap<String, u64>,

    pub warnings: Vec<Warning>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadSizes {
    pub inputs: u64,
    pub outputs: u64,
    pub events: u64,
}

impl PayloadSizes {
    pub fn total(&self) -> u64 {
        self.inputs + self.outputs + self.events
    }
}

/// A run plus the store's bookkeeping about it.
#[derive(Clone, Debug, PartialEq)]
pub struct RunState {
    pub run: Run,
    pub first_seq: u64,
    pub last_seq: u64,
    pub created_at: Ts,
    pub updated_at: Ts,
    pub event_count: u32,
    pub payload_sizes: PayloadSizes,
    /// True once `start_time` and `name` are known; until then the run is not listed as a root.
    pub has_start: bool,
    /// Whether `project` came from an event (it is immutable once set) or is still the default.
    pub project_set: bool,
    /// `outputs.usage_metadata` of the latest outputs, kept apart from the payload so token counts can be
    /// derived when the payload isn't loaded (versions read from files).
    pub outputs_usage: Option<JsonMap>,
    /// Which fields some event set (`present::*`). Parquet can't tell absent from empty, and the merge rule
    /// needs to ("absent never overwrites"), so versions carry this bitmask.
    pub present: u32,
}

/// Bits of [`RunState::present`].
pub mod present {
    pub const PROJECT: u32 = 1 << 0;
    pub const ADDRESS: u32 = 1 << 1;
    pub const NAME: u32 = 1 << 2;
    pub const RUN_TYPE: u32 = 1 << 3;
    pub const START: u32 = 1 << 4;
    pub const END: u32 = 1 << 5;
    pub const ERROR: u32 = 1 << 6;
    pub const TAGS: u32 = 1 << 7;
    pub const METADATA: u32 = 1 << 8;
    pub const EXTRA: u32 = 1 << 9;
    pub const INPUTS: u32 = 1 << 10;
    pub const OUTPUTS: u32 = 1 << 11;
    pub const EVENTS: u32 = 1 << 12;
    pub const DROPPED: u32 = 1 << 13;
    /// The version was started by an update, not a start: the run's start lives elsewhere.
    pub const CREATED_BY_PATCH: u32 = 1 << 29;
    /// Versions disagreed on an identity field.
    pub const CONFLICT: u32 = 1 << 30;
}

impl RunState {
    pub fn payload_bytes(&self) -> u64 {
        self.payload_sizes.total()
    }

    pub fn is_listed_root(&self) -> bool {
        self.run.is_root && self.has_start
    }

    /// A copy without `inputs`, `outputs` and `events` (their `present` bits and sizes are kept), for reads
    /// that only need headers.
    pub fn without_payload(&self) -> RunState {
        let r = &self.run;
        RunState {
            run: Run {
                run_id: r.run_id,
                trace_id: r.trace_id,
                parent_run_id: r.parent_run_id,
                dotted_order: r.dotted_order.clone(),
                depth: r.depth,
                is_root: r.is_root,
                project: r.project.clone(),
                address: r.address.clone(),
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
                reported_cost_pico: r.reported_cost_pico,
                inputs: None,
                outputs: None,
                events: None,
                dropped: r.dropped.clone(),
                warnings: r.warnings.clone(),
            },
            first_seq: self.first_seq,
            last_seq: self.last_seq,
            created_at: self.created_at,
            updated_at: self.updated_at,
            event_count: self.event_count,
            payload_sizes: self.payload_sizes,
            has_start: self.has_start,
            project_set: self.project_set,
            outputs_usage: self.outputs_usage.clone(),
            present: self.present,
        }
    }

    pub fn created_by_patch(&self) -> bool {
        self.present & present::CREATED_BY_PATCH != 0
    }

    /// The fields this version set, as if they arrived in one event.
    pub fn to_fields(&self) -> RunFields {
        let r = &self.run;
        let has = |bit: u32| self.present & bit != 0;
        let opt = |bit: u32, v: Option<String>| if has(bit) { v } else { None };
        RunFields {
            id: Some(r.run_id),
            trace_id: Some(r.trace_id),
            parent_run_id: r.parent_run_id,
            dotted_order: Some(r.dotted_order.clone()),
            project: has(present::PROJECT).then(|| r.project.clone()),
            address: opt(present::ADDRESS, r.address.clone()),
            name: opt(present::NAME, r.name.clone()),
            run_type: has(present::RUN_TYPE).then(|| {
                r.run_type_raw
                    .clone()
                    .unwrap_or_else(|| run_type_str(r.run_type).into())
            }),
            start_time: if has(present::START) { r.start_time } else { None },
            end_time: if has(present::END) { r.end_time } else { None },
            error: opt(present::ERROR, r.error.clone()),
            tags: has(present::TAGS).then(|| r.tags.clone()),
            metadata: has(present::METADATA).then(|| r.metadata.clone()),
            extra: has(present::EXTRA).then(|| r.extra.clone()),
            inputs: if has(present::INPUTS) { r.inputs.clone() } else { None },
            outputs: if has(present::OUTPUTS) { r.outputs.clone() } else { None },
            events: if has(present::EVENTS) { r.events.clone() } else { None },
            dropped: has(present::DROPPED).then(|| r.dropped.clone()),
        }
    }
}

pub fn run_type_str(t: RunType) -> &'static str {
    match t {
        RunType::Llm => "llm",
        RunType::Chain => "chain",
        RunType::Tool => "tool",
        RunType::Retriever => "retriever",
        RunType::Embedding => "embedding",
        RunType::Prompt => "prompt",
        RunType::Parser => "parser",
        RunType::Agent => "agent",
        RunType::Other => "other",
    }
}
