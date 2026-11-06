use serde::{Deserialize, Serialize};

use crate::run::RunFields;
use crate::ts::Ts;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Post,
    Patch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    LsBatch,
    LsSingle,
    Native,
}

/// One upload message about one run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    pub op: Op,
    /// Assigned by the writer; strictly increasing across the store and defines merge order.
    pub event_seq: u64,
    pub received_at: Ts,
    pub source: Source,
    /// Hash of the canonical item JSON, used for dedup.
    pub body_hash: u64,
    pub fields: RunFields,
}
