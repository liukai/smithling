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
    /// Fields tracedb received but does not store in M0 (e.g. `serialized`, attachments), with their sizes.
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
}

impl RunState {
    pub fn payload_bytes(&self) -> u64 {
        self.payload_sizes.total()
    }

    pub fn is_listed_root(&self) -> bool {
        self.run.is_root && self.has_start
    }
}
