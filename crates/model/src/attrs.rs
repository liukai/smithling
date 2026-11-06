//! Promoted attributes: scalar leaves of `metadata`, `inputs` and `outputs`, keyed by dotted path, so filters
//! never need the full JSON (data-model §4).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::run::Run;

pub const METADATA_DEPTH: usize = 4;
pub const PAYLOAD_DEPTH: usize = 3;
pub const PAYLOAD_LEAVES: usize = 64;
pub const MAX_STRING_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    Bool(bool),
    Num(f64),
    Str(String),
    /// The value exists but wasn't promoted (a string over 256 B, an array, or past the leaf budget), so
    /// filters can't answer from attributes and report `not_indexed`. Serialized as `null`.
    Unindexed,
}

impl Scalar {
    pub fn type_name(&self) -> &'static str {
        match self {
            Scalar::Str(_) => "string",
            Scalar::Num(_) => "number",
            Scalar::Bool(_) => "bool",
            Scalar::Unindexed => "unindexed",
        }
    }

    pub fn display(&self) -> String {
        match self {
            Scalar::Str(s) => s.clone(),
            Scalar::Num(n) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{}", *n as i64),
            Scalar::Num(n) => n.to_string(),
            Scalar::Bool(b) => b.to_string(),
            Scalar::Unindexed => "…".into(),
        }
    }
}

/// `path → value`, e.g. `metadata.prompt_version → "v7"`, `outputs.status_code → 429`.
pub type Attrs = BTreeMap<String, Scalar>;

/// Promotes the run's scalar leaves. Strings longer than 256 bytes, arrays, and paths past the depth
/// limits are left out (filters on them report `not_indexed`).
pub fn promote(run: &Run) -> Attrs {
    let mut out = Attrs::new();
    let mut budget = usize::MAX;
    for (k, v) in run.metadata.iter().filter(|(k, _)| !k.contains('.')) {
        walk(&mut out, &format!("metadata.{k}"), v, 1, METADATA_DEPTH, &mut budget);
    }
    for (root, v) in [("inputs", &run.inputs), ("outputs", &run.outputs)] {
        if let Some(Value::Object(m)) = v {
            let mut budget = PAYLOAD_LEAVES;
            for (k, v) in m.iter().filter(|(k, _)| !k.contains('.')) {
                walk(&mut out, &format!("{root}.{k}"), v, 1, PAYLOAD_DEPTH, &mut budget);
            }
        }
    }
    out
}

fn walk(out: &mut Attrs, path: &str, v: &Value, depth: usize, max_depth: usize, budget: &mut usize) {
    let scalar = match v {
        Value::Null => return,
        _ if *budget == 0 => Scalar::Unindexed,
        Value::String(s) if s.len() <= MAX_STRING_BYTES => Scalar::Str(s.clone()),
        Value::Number(n) => n.as_f64().map_or(Scalar::Unindexed, Scalar::Num),
        Value::Bool(b) => Scalar::Bool(*b),
        Value::Object(m) if depth < max_depth => {
            for (k, v) in m {
                // A key containing "." would collide with a nested path; it isn't addressable.
                if !k.contains('.') {
                    walk(out, &format!("{path}.{k}"), v, depth + 1, max_depth, budget);
                }
            }
            return;
        }
        _ => Scalar::Unindexed,
    };
    if scalar != Scalar::Unindexed {
        *budget -= 1;
    }
    out.insert(path.to_string(), scalar);
}

/// Whether a path of this root and key could have been promoted (depth within the limit).
pub fn promotable(root: &str, key: &str) -> bool {
    let depth = key.split('.').count();
    match root {
        "metadata" => depth <= METADATA_DEPTH,
        _ => depth <= PAYLOAD_DEPTH,
    }
}

/// Same as [`promotable`], for a full path like `outputs.a.b`.
pub fn promotable_path(path: &str) -> bool {
    match path.split_once('.') {
        Some((root, key)) => promotable(root, key),
        None => true,
    }
}
