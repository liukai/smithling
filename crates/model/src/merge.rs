use std::collections::BTreeMap;

use serde_json::Map;

use crate::derive::derive;
use crate::dotted_order::DottedOrder;
use crate::event::RunEvent;
use crate::event::{Op, Source};
use crate::run::{DEFAULT_PROJECT, PayloadSizes, Run, RunState, RunType, Status, present};
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
        reported_cost_pico: None,
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
        outputs_usage: None,
        has_start: false,
        project_set: false,
        present: if ev.op == Op::Patch {
            present::CREATED_BY_PATCH
        } else {
            0
        },
    };
    apply(&mut st, ev);
    Ok(st)
}

/// The identity checks of the merge rule, usable before an event is applied (the writer checks before
/// writing to the WAL, so a rejected event leaves no trace).
pub fn check_identity(st: &RunState, ev: &RunEvent) -> Result<(), Reject> {
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
        st.present |= present::PROJECT;
    }
    if let Some(a) = &f.address
        && r.address.is_none()
    {
        r.address = Some(a.clone());
        st.present |= present::ADDRESS;
    }
    if let Some(v) = &f.name {
        r.name = Some(v.clone());
        st.present |= present::NAME;
    }
    if let Some(raw) = &f.run_type {
        (r.run_type, r.run_type_raw) = RunType::from_raw(raw);
        st.present |= present::RUN_TYPE;
    }
    if let Some(v) = f.start_time {
        r.start_time = Some(v);
        st.present |= present::START;
    }
    if let Some(v) = f.end_time {
        r.end_time = Some(v);
        st.present |= present::END;
    }
    if let Some(v) = &f.error {
        r.error = Some(v.clone());
        st.present |= present::ERROR;
    }
    if let Some(v) = &f.tags {
        r.tags = v.clone();
        st.present |= present::TAGS;
    }
    if let Some(v) = &f.metadata {
        r.metadata = v.clone();
        st.present |= present::METADATA;
    }
    if let Some(v) = &f.extra {
        r.extra = v.clone();
        st.present |= present::EXTRA;
    }
    if let Some(v) = &f.inputs {
        st.payload_sizes.inputs = json_size(v);
        r.inputs = Some(v.clone());
        st.present |= present::INPUTS;
    }
    if let Some(v) = &f.outputs {
        st.payload_sizes.outputs = json_size(v);
        st.outputs_usage = v.get("usage_metadata").and_then(serde_json::Value::as_object).cloned();
        r.outputs = Some(v.clone());
        st.present |= present::OUTPUTS;
    }
    if let Some(v) = &f.events {
        st.payload_sizes.events = json_size(v);
        r.events = Some(v.clone());
        st.present |= present::EVENTS;
    }
    if let Some(v) = &f.dropped {
        r.dropped = v.clone();
        st.present |= present::DROPPED;
    }
    st.last_seq = ev.event_seq;
    st.updated_at = ev.received_at;
    st.event_count += 1;
    derive(st);
}

/// Merges versions of one run (from memtables and files) into one state, in `first_seq` order.
///
/// Precondition: versions cover disjoint `event_seq` ranges (each memtable holds a contiguous range and is
/// flushed whole), so ordering by `first_seq` is ordering by time.
///
/// Same rule as applying their events in order: a present value replaces, an absent one never does.
/// Identity fields (trace id, `dotted_order`, parent, project, address) keep the first version's value; a
/// later disagreement is kept as an `identity_conflict` warning instead of a rejection (design §8, q1).
pub fn merge_versions(mut versions: Vec<RunState>) -> Option<RunState> {
    versions.sort_by_key(|v| (v.first_seq, v.last_seq));
    debug_assert!(
        versions.windows(2).all(|w| w[0].last_seq < w[1].first_seq),
        "versions must cover disjoint seq ranges"
    );
    let mut it = versions.into_iter();
    let first = it.next()?;
    Some(it.fold(first, fold))
}

fn fold(a: RunState, b: RunState) -> RunState {
    let fa = a.to_fields();
    let fb = b.to_fields();
    let (ra, rb) = (&a.run, &b.run);
    let conflict = ra.trace_id != rb.trace_id
        || ra.dotted_order != rb.dotted_order
        || ra.parent_run_id != rb.parent_run_id
        || (a.project_set && b.project_set && ra.project != rb.project)
        || matches!((&ra.address, &rb.address), (Some(x), Some(y)) if x != y);
    let fields = crate::run::RunFields {
        id: fa.id,
        trace_id: fa.trace_id,
        parent_run_id: fa.parent_run_id,
        dotted_order: fa.dotted_order,
        project: fa.project.or(fb.project),
        address: fa.address.or(fb.address),
        name: fb.name.or(fa.name),
        run_type: fb.run_type.or(fa.run_type),
        start_time: fb.start_time.or(fa.start_time),
        end_time: fb.end_time.or(fa.end_time),
        error: fb.error.or(fa.error),
        tags: fb.tags.or(fa.tags),
        metadata: fb.metadata.or(fa.metadata),
        extra: fb.extra.or(fa.extra),
        // Payload fields: the newer version's value whenever it set the field, even if that payload isn't
        // loaded (then the merged payload is unloaded too, never an older one).
        inputs: if b.present & present::INPUTS != 0 {
            fb.inputs
        } else {
            fa.inputs
        },
        outputs: if b.present & present::OUTPUTS != 0 {
            fb.outputs
        } else {
            fa.outputs
        },
        events: if b.present & present::EVENTS != 0 {
            fb.events
        } else {
            fa.events
        },
        dropped: fb.dropped.or(fa.dropped),
    };
    let ev = RunEvent {
        op: if a.created_by_patch() { Op::Patch } else { Op::Post },
        event_seq: b.last_seq.max(a.last_seq),
        received_at: b.updated_at.max(a.updated_at),
        source: Source::Native,
        body_hash: 0,
        fields,
    };
    let mut slot = None;
    merge(&mut slot, &ev).expect("a version always has id, trace_id and a valid dotted_order");
    let mut st = slot.expect("merge fills the slot");
    st.first_seq = a.first_seq.min(b.first_seq);
    st.last_seq = a.last_seq.max(b.last_seq);
    st.created_at = a.created_at.min(b.created_at);
    st.updated_at = a.updated_at.max(b.updated_at);
    st.event_count = a.event_count + b.event_count;
    // Versions read from files carry payload bits and sizes without the payload itself; keep them.
    for (bit, size) in [
        (present::INPUTS, &mut st.payload_sizes.inputs),
        (present::OUTPUTS, &mut st.payload_sizes.outputs),
        (present::EVENTS, &mut st.payload_sizes.events),
    ] {
        let pick = |v: &RunState| match bit {
            present::INPUTS => v.payload_sizes.inputs,
            present::OUTPUTS => v.payload_sizes.outputs,
            _ => v.payload_sizes.events,
        };
        if b.present & bit != 0 {
            *size = pick(&b);
        } else if a.present & bit != 0 {
            *size = pick(&a);
        }
    }
    st.present |= (a.present | b.present) & (present::INPUTS | present::OUTPUTS | present::EVENTS);
    st.outputs_usage = if b.present & present::OUTPUTS != 0 {
        b.outputs_usage.clone()
    } else {
        a.outputs_usage.clone()
    };
    st.present = (st.present & !present::CREATED_BY_PATCH)
        | (a.present & present::CREATED_BY_PATCH)
        | ((a.present | b.present) & present::CONFLICT)
        | if conflict { present::CONFLICT } else { 0 };
    derive(&mut st);
    st
}
