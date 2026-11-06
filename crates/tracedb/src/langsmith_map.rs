//! Turns uploaded JSON items into `RunFields`. The only code that knows LangSmith field names.
//!
//! Each item is parsed on its own so one bad item can't fail a batch. `null` counts as absent.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use tracedb_ingest::Incoming;
use tracedb_model::validate::{json_size, parse_ts, parse_uuid};
use tracedb_model::{JsonMap, Op, Reason, Reject, RunFields, Source};
use xxhash_rust::xxh64::xxh64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// LangSmith SDK runs: `session_name`, `extra.metadata`, …
    LangSmith,
    /// tracedb's own field names (`project`, `metadata`, …), as in `RunFields`.
    Native,
}

/// Received but not stored in M0; their sizes are recorded in `dropped`.
const DROPPED: [&str; 6] = [
    "serialized",
    "attachments",
    "input_attachments",
    "output_attachments",
    "inputs_s3_urls",
    "outputs_s3_urls",
];

const LRN_PREFIX: &str = "lrn:agents/";

pub fn incoming(
    op: Op,
    source: Source,
    item: &Value,
    dialect: Dialect,
) -> Result<Incoming, (Option<uuid::Uuid>, Reject)> {
    let body_hash = xxh64(&serde_json::to_vec(item).unwrap_or_default(), 0);
    let fields = map_item(item, dialect)
        .map_err(|r| (item.get("id").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()), r))?;
    Ok(Incoming {
        op,
        source,
        body_hash,
        fields,
    })
}

pub fn map_item(item: &Value, dialect: Dialect) -> Result<RunFields, Reject> {
    let obj = item
        .as_object()
        .ok_or_else(|| Reject::new(Reason::InvalidField, "item is not a JSON object"))?;
    let get = |k: &str| obj.get(k).filter(|v| !v.is_null());
    let mut f = RunFields::default();
    let mut used: Vec<&str> = Vec::new();
    let mut take = |k: &'static str| {
        used.push(k);
        get(k)
    };

    if let Some(v) = take("id") {
        f.id = Some(parse_uuid("id", v)?);
    }
    if let Some(v) = take("trace_id") {
        f.trace_id = Some(parse_uuid("trace_id", v)?);
    }
    if let Some(v) = take("parent_run_id") {
        f.parent_run_id = Some(parse_uuid("parent_run_id", v)?);
    }
    f.dotted_order = take("dotted_order").map(string).transpose()?;
    f.name = take("name").map(string).transpose()?;
    f.run_type = take("run_type").map(string).transpose()?;
    f.error = take("error").map(string).transpose()?;
    f.address = take("address").map(string).transpose()?;
    if let Some(v) = take("start_time") {
        f.start_time = Some(parse_ts("start_time", v)?);
    }
    if let Some(v) = take("end_time") {
        f.end_time = Some(parse_ts("end_time", v)?);
    }
    if let Some(v) = take("tags") {
        f.tags = Some(strings(v)?);
    }
    f.inputs = take("inputs").cloned();
    f.outputs = take("outputs").cloned();
    if let Some(v) = take("events") {
        f.events = Some(
            v.as_array()
                .cloned()
                .ok_or_else(|| Reject::new(Reason::InvalidField, "events must be an array"))?,
        );
    }

    match dialect {
        Dialect::Native => {
            f.project = take("project").map(string).transpose()?;
            f.metadata = take("metadata").map(object).transpose()?;
            f.extra = take("extra").map(object).transpose()?;
        }
        Dialect::LangSmith => {
            let session_name = take("session_name").map(string).transpose()?;
            let session_id = take("session_id").map(string).transpose()?;
            f.project = session_name
                .or(session_id)
                .or_else(|| f.address.as_deref().and_then(project_from_address));
            if let Some(extra) = take("extra").map(object).transpose()? {
                let mut extra = extra;
                if let Some(Value::Object(m)) = extra.remove("metadata") {
                    f.metadata = Some(m);
                }
                f.extra = Some(extra);
            }
        }
    }

    let mut dropped = BTreeMap::new();
    for k in DROPPED {
        used.push(k);
        if let Some(v) = get(k) {
            dropped.insert(k.to_string(), json_size(v));
        }
    }
    if !dropped.is_empty() {
        f.dropped = Some(dropped);
    }

    let unknown: Map<String, Value> = obj
        .iter()
        .filter(|(k, v)| !v.is_null() && !used.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if !unknown.is_empty() {
        f.extra
            .get_or_insert_with(Map::new)
            .insert("other".into(), Value::Object(unknown));
    }
    Ok(f)
}

/// `lrn:agents/<id>/environments/<env>` → `<id>/<env>`.
pub fn project_from_address(lrn: &str) -> Option<String> {
    let rest = lrn.strip_prefix(LRN_PREFIX)?;
    let (agent, env) = rest.split_once("/environments/")?;
    (!agent.is_empty() && !env.is_empty() && !env.contains('/')).then(|| format!("{agent}/{env}"))
}

fn string(v: &Value) -> Result<String, Reject> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| Reject::new(Reason::InvalidField, format!("expected a string, got {v}")))
}

fn strings(v: &Value) -> Result<Vec<String>, Reject> {
    v.as_array()
        .and_then(|a| {
            a.iter()
                .map(|x| x.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| Reject::new(Reason::InvalidField, "tags must be an array of strings"))
}

fn object(v: &Value) -> Result<JsonMap, Reject> {
    v.as_object()
        .cloned()
        .ok_or_else(|| Reject::new(Reason::InvalidField, format!("expected an object, got {v}")))
}
