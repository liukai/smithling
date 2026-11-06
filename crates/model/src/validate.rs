use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dotted_order::DottedOrder;
use crate::event::Op;
use crate::run::RunFields;

pub const MAX_PAYLOAD_FIELD_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_METADATA_BYTES: u64 = 1024 * 1024;
pub const MAX_ERROR_BYTES: usize = 1024 * 1024;
pub const MAX_TAGS: usize = 100;
pub const MAX_TAG_CHARS: usize = 256;
pub const MAX_NAME_CHARS: usize = 512;
pub const MAX_PROJECT_CHARS: usize = 128;
pub const MAX_ADDRESS_CHARS: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    MissingField,
    InvalidUuid,
    InvalidTimestamp,
    InvalidDottedOrder,
    IdentityConflict,
    TooLarge,
    Capacity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{reason:?}: {detail}")]
pub struct Reject {
    pub reason: Reason,
    pub detail: String,
}

impl Reject {
    pub fn new(reason: Reason, detail: impl Into<String>) -> Reject {
        Reject {
            reason,
            detail: detail.into(),
        }
    }
}

/// Checks one event's fields and normalizes them in place: a missing `parent_run_id` is filled in from
/// `dotted_order`, and an empty `project` counts as absent. `invalid_uuid` and `invalid_timestamp` are
/// raised while parsing wire JSON (see [`parse_uuid`], [`parse_ts`]), before fields reach this function.
pub fn validate(op: Op, f: &mut RunFields) -> Result<DottedOrder, Reject> {
    let missing = |name: &str| Reject::new(Reason::MissingField, name.to_string());
    let run_id = f.id.ok_or_else(|| missing("id"))?;
    let trace_id = f.trace_id.ok_or_else(|| missing("trace_id"))?;
    let dotted = f.dotted_order.as_deref().ok_or_else(|| missing("dotted_order"))?;
    if op == Op::Post {
        if f.name.as_deref().is_none_or(str::is_empty) {
            return Err(missing("name"));
        }
        if f.run_type.as_deref().is_none_or(str::is_empty) {
            return Err(missing("run_type"));
        }
        if f.start_time.is_none() {
            return Err(missing("start_time"));
        }
    }

    let order = DottedOrder::parse(dotted).map_err(|e| Reject::new(Reason::InvalidDottedOrder, e))?;
    let bad_order = |what: &str| Reject::new(Reason::InvalidDottedOrder, what.to_string());
    if order.trace_id() != trace_id {
        return Err(bad_order("first segment does not match trace_id"));
    }
    if order.run_id() != run_id {
        return Err(bad_order("last segment does not match id"));
    }
    match (f.parent_run_id, order.parent_id()) {
        (Some(p), Some(q)) if p != q => return Err(bad_order("second-to-last segment does not match parent_run_id")),
        (Some(_), None) => return Err(bad_order("parent_run_id set but dotted_order has one segment")),
        (None, parent) => f.parent_run_id = parent,
        _ => {}
    }

    if f.project.as_deref() == Some("") {
        f.project = None;
    }
    check_limits(f)?;
    Ok(order)
}

fn check_limits(f: &RunFields) -> Result<(), Reject> {
    let too_large = |what: String| Reject::new(Reason::TooLarge, what);
    if let Some(name) = &f.name
        && name.chars().count() > MAX_NAME_CHARS
    {
        return Err(too_large(format!("name over {MAX_NAME_CHARS} chars")));
    }
    if let Some(project) = &f.project
        && project.chars().count() > MAX_PROJECT_CHARS
    {
        return Err(too_large(format!("project over {MAX_PROJECT_CHARS} chars")));
    }
    if let Some(address) = &f.address
        && address.chars().count() > MAX_ADDRESS_CHARS
    {
        return Err(too_large(format!("address over {MAX_ADDRESS_CHARS} chars")));
    }
    if let Some(tags) = &f.tags {
        if tags.len() > MAX_TAGS {
            return Err(too_large(format!("more than {MAX_TAGS} tags")));
        }
        if tags.iter().any(|t| t.chars().count() > MAX_TAG_CHARS) {
            return Err(too_large(format!("tag over {MAX_TAG_CHARS} chars")));
        }
    }
    if let Some(e) = &f.error
        && e.len() > MAX_ERROR_BYTES
    {
        return Err(too_large("error over 1 MB".into()));
    }
    if let Some(m) = &f.metadata
        && json_size(m) > MAX_METADATA_BYTES
    {
        return Err(too_large("metadata over 1 MB".into()));
    }
    for (name, size) in [
        ("inputs", f.inputs.as_ref().map(json_size)),
        ("outputs", f.outputs.as_ref().map(json_size)),
        ("events", f.events.as_ref().map(json_size)),
    ] {
        if size.is_some_and(|s| s > MAX_PAYLOAD_FIELD_BYTES) {
            return Err(too_large(format!("{name} over 20 MB")));
        }
    }
    Ok(())
}

pub fn parse_uuid(field: &str, v: &Value) -> Result<uuid::Uuid, Reject> {
    v.as_str()
        .and_then(|s| uuid::Uuid::try_parse(s).ok())
        .ok_or_else(|| Reject::new(Reason::InvalidUuid, format!("{field}: {v}")))
}

pub fn parse_ts(field: &str, v: &Value) -> Result<crate::Ts, Reject> {
    v.as_str()
        .and_then(crate::Ts::parse)
        .ok_or_else(|| Reject::new(Reason::InvalidTimestamp, format!("{field}: {v}")))
}

/// Serialized JSON size in bytes, without allocating the serialized form.
pub fn json_size<T: Serialize + ?Sized>(v: &T) -> u64 {
    struct Counter(u64);
    impl io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0 += buf.len() as u64;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut c = Counter(0);
    let _ = serde_json::to_writer(&mut c, v);
    c.0
}
