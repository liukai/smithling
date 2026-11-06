use std::collections::BTreeMap;

use serde_json::Map;

use crate::derive::derive;
use crate::dotted_order::DottedOrder;
use crate::event::RunEvent;
use crate::run::{DEFAULT_PROJECT, PayloadSizes, Run, RunState, RunType, Status};
use crate::validate::{Reason, Reject, json_size};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    /// A root run became listable (its start arrived).
    NewRoot,
    /// A non-root run was seen for the first time.
    NewChild,
    Updated,
}

/// Applies one validated event to the run's slot (empty if the run is new).
///
/// Present fields replace old values; absent fields leave them. Identity fields are immutable: a
/// different `trace_id`, `dotted_order`, `parent_run_id`, `address` or explicit `project` is `identity_conflict`
/// and leaves the run unchanged.
pub fn merge(slot: &mut Option<RunState>, ev: &RunEvent) -> Result<ChangeKind, Reject> {
    let kind = match slot {
        None => {
            let st = new_state(ev)?;
            let kind = match (st.run.is_root, st.is_listed_root()) {
                (true, true) => ChangeKind::NewRoot,
                (true, false) => ChangeKind::Updated,
                (false, _) => ChangeKind::NewChild,
            };
            *slot = Some(st);
            kind
        }
        Some(st) => {
            check_identity(st, ev)?;
            let was_listed = st.is_listed_root();
            apply(st, ev);
            if !was_listed && st.is_listed_root() {
                ChangeKind::NewRoot
            } else {
                ChangeKind::Updated
            }
        }
    };
    Ok(kind)
}

fn new_state(ev: &RunEvent) -> Result<RunState, Reject> {
    let f = &ev.fields;
    let missing = |name: &str| Reject::new(Reason::MissingField, name.to_string());
    let run_id = f.id.ok_or_else(|| missing("id"))?;
    let trace_id = f.trace_id.ok_or_else(|| missing("trace_id"))?;
    let dotted = f.dotted_order.clone().ok_or_else(|| missing("dotted_order"))?;
    let order = DottedOrder::parse(&dotted).map_err(|e| Reject::new(Reason::InvalidDottedOrder, e))?;
    let parent_run_id = f.parent_run_id.or(order.parent_id());
    let run = Run {
        run_id,
        trace_id,
        parent_run_id,
        dotted_order: dotted,
        depth: order.depth(),
        is_root: parent_run_id.is_none(),
        project: DEFAULT_PROJECT.to_string(),
        address: None,
        tags: Vec::new(),
        metadata: Map::new(),
        extra: Map::new(),
        thread_id: None,
        name: None,
        run_type: RunType::Other,
        run_type_raw: None,
        start_time: None,
        end_time: None,
        error: None,
        status: Status::Running,
        latency_ms: None,
        model: None,
        input_tokens: None,
        output_tokens: None,
        total_tokens: None,
        inputs: None,
        outputs: None,
        events: None,
        dropped: BTreeMap::new(),
        warnings: Vec::new(),
    };
    let mut st = RunState {
        run,
        first_seq: ev.event_seq,
        last_seq: ev.event_seq,
        created_at: ev.received_at,
        updated_at: ev.received_at,
        event_count: 0,
        payload_sizes: PayloadSizes::default(),
        has_start: false,
        project_set: false,
    };
    apply(&mut st, ev);
    Ok(st)
}

fn check_identity(st: &RunState, ev: &RunEvent) -> Result<(), Reject> {
    let f = &ev.fields;
    let r = &st.run;
    let conflict = |field: &str| Reject::new(Reason::IdentityConflict, format!("{field} differs from the stored run"));
    if f.trace_id.is_some_and(|t| t != r.trace_id) {
        return Err(conflict("trace_id"));
    }
    if f.dotted_order.as_deref().is_some_and(|d| d != r.dotted_order) {
        return Err(conflict("dotted_order"));
    }
    if f.parent_run_id.is_some() && f.parent_run_id != r.parent_run_id {
        return Err(conflict("parent_run_id"));
    }
    if let (Some(old), Some(new)) = (&r.address, &f.address)
        && old != new
    {
        return Err(conflict("address"));
    }
    if st.project_set && f.project.as_deref().is_some_and(|p| p != r.project) {
        return Err(conflict("project"));
    }
    Ok(())
}

fn apply(st: &mut RunState, ev: &RunEvent) {
    let f = &ev.fields;
    let r = &mut st.run;
    if let Some(p) = &f.project
        && !st.project_set
    {
        r.project = p.clone();
        st.project_set = true;
    }
    if let Some(a) = &f.address
        && r.address.is_none()
    {
        r.address = Some(a.clone());
    }
    if let Some(v) = &f.name {
        r.name = Some(v.clone());
    }
    if let Some(raw) = &f.run_type {
        (r.run_type, r.run_type_raw) = RunType::from_raw(raw);
    }
    if let Some(v) = f.start_time {
        r.start_time = Some(v);
    }
    if let Some(v) = f.end_time {
        r.end_time = Some(v);
    }
    if let Some(v) = &f.error {
        r.error = Some(v.clone());
    }
    if let Some(v) = &f.tags {
        r.tags = v.clone();
    }
    if let Some(v) = &f.metadata {
        r.metadata = v.clone();
    }
    if let Some(v) = &f.extra {
        r.extra = v.clone();
    }
    if let Some(v) = &f.inputs {
        st.payload_sizes.inputs = json_size(v);
        r.inputs = Some(v.clone());
    }
    if let Some(v) = &f.outputs {
        st.payload_sizes.outputs = json_size(v);
        r.outputs = Some(v.clone());
    }
    if let Some(v) = &f.events {
        st.payload_sizes.events = json_size(v);
        r.events = Some(v.clone());
    }
    if let Some(v) = &f.dropped {
        r.dropped = v.clone();
    }
    st.last_seq = ev.event_seq;
    st.updated_at = ev.received_at;
    st.event_count += 1;
    derive(st);
}
