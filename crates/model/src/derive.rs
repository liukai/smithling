use serde_json::Value;

use crate::run::{JsonMap, RunState, Status, Warning, present};

const THREAD_KEYS: [&str; 3] = ["thread_id", "session_id", "conversation_id"];

/// Recomputes every derived field from the stored ones.
pub fn derive(st: &mut RunState) {
    let r = &mut st.run;

    r.status = if r.error.as_deref().is_some_and(|e| !e.is_empty()) {
        Status::Error
    } else if r.end_time.is_some() {
        Status::Ok
    } else {
        Status::Running
    };

    r.warnings.clear();
    r.latency_ms = match (r.start_time, r.end_time) {
        (Some(s), Some(e)) if e.0 >= s.0 => Some(((e.0 - s.0) / 1000) as u64),
        (Some(_), Some(_)) => {
            r.warnings.push(Warning::NegativeLatency);
            None
        }
        _ => None,
    };

    r.thread_id = THREAD_KEYS.iter().find_map(|k| match r.metadata.get(*k) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    });

    r.model = r
        .metadata
        .get("ls_model_name")
        .and_then(Value::as_str)
        .map(str::to_string);

    let usage = st
        .outputs_usage
        .as_ref()
        .or_else(|| r.metadata.get("usage_metadata").and_then(Value::as_object));
    let count = |u: Option<&JsonMap>, k: &str| u.and_then(|u| u.get(k)).and_then(Value::as_u64);
    r.input_tokens = count(usage, "input_tokens");
    r.output_tokens = count(usage, "output_tokens");
    r.total_tokens = count(usage, "total_tokens").or(match (r.input_tokens, r.output_tokens) {
        (None, None) => None,
        (i, o) => Some(i.unwrap_or(0) + o.unwrap_or(0)),
    });

    st.has_start = r.start_time.is_some() && r.name.is_some();
    if st.present & present::CONFLICT != 0 {
        r.warnings.push(Warning::IdentityConflict);
    }
}
