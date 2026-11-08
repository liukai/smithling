//! Merging versions equals applying all events in order (the property merge-on-read relies on).

mod common;

use common::*;
use proptest::prelude::*;
use serde_json::json;
use smithling_model::{Op, RunEvent, RunState, Warning, merge, merge_versions};

#[derive(Clone, Debug)]
struct Step {
    name: Option<String>,
    run_type: Option<&'static str>,
    start_minute: Option<u8>,
    inputs: Option<u8>,
    address: Option<u8>,
    error: Option<String>,
    end_minute: Option<u8>,
    tags: Option<Vec<String>>,
    metadata: Option<Option<u8>>,
    output: Option<u32>,
}

fn step() -> impl Strategy<Value = Step> {
    (
        proptest::option::of(prop::sample::select(vec!["tool", "llm", "weird"])),
        proptest::option::of(0u8..59),
        proptest::option::of(0u8..9),
        proptest::option::of(0u8..2),
    )
        .prop_flat_map(|extra| (Just(extra), step_core()))
        .prop_map(|((run_type, start_minute, inputs, address), mut s)| {
            s.run_type = run_type;
            s.start_minute = start_minute;
            s.inputs = inputs;
            s.address = address;
            s
        })
}

fn step_core() -> impl Strategy<Value = Step> {
    (
        proptest::option::of("[a-z]{1,6}"),
        proptest::option::of("[a-z]{1,6}"),
        proptest::option::of(0u8..59),
        proptest::option::of(proptest::collection::vec("[a-z]{1,3}", 0..3)),
        proptest::option::of(proptest::option::of(0u8..5)),
        proptest::option::of(any::<u32>()),
    )
        .prop_map(|(name, error, end_minute, tags, metadata, output)| Step {
            name,
            run_type: None,
            start_minute: None,
            inputs: None,
            address: None,
            error,
            end_minute,
            tags,
            metadata,
            output,
        })
}

fn to_event(seq: u64, post: bool, s: &Step) -> RunEvent {
    let mut f = if post {
        post_fields(child_ids(), "start")
    } else {
        child_ids()
    };
    if s.name.is_some() {
        f.name = s.name.clone();
    }
    if post {
        f.project = Some("eval-agent".into());
    }
    f.error = s.error.clone();
    f.end_time = s.end_minute.map(|m| ts(&format!("2026-11-05T03:{m:02}:00Z")));
    f.tags = s.tags.clone();
    // `Some(None)` sends an empty object: present, and must overwrite (empty ≠ absent).
    f.metadata = s.metadata.map(|m| match m {
        Some(v) => json!({"v": v}).as_object().unwrap().clone(),
        None => serde_json::Map::new(),
    });
    f.outputs = s.output.map(|o| json!({"v": o}));
    if let Some(t) = s.run_type {
        f.run_type = Some(t.into());
    }
    if let Some(m) = s.start_minute {
        f.start_time = Some(ts(&format!("2026-11-05T02:{m:02}:00Z")));
    }
    f.inputs = s.inputs.map(|i| json!({"q": i}));
    // Address is identity: send the same one, sometimes, so first-wins is exercised without conflicts.
    f.address = s.address.map(|_| "lrn:agents/a/environments/local".to_string());
    event(if post { Op::Post } else { Op::Patch }, seq, f)
}

fn apply(events: &[RunEvent]) -> Option<RunState> {
    let mut slot = None;
    for e in events {
        merge(&mut slot, e).unwrap();
    }
    slot
}

/// Compares what a reader sees (the run), not bookkeeping that differs by construction.
fn view(st: &RunState) -> serde_json::Value {
    let mut v = serde_json::to_value(&st.run).unwrap();
    v["_has_start"] = st.has_start.into();
    v["_seq"] = json!([st.first_seq, st.last_seq, st.event_count]);
    v
}

proptest! {
    #[test]
    fn versions_merge_like_their_events(
        steps in proptest::collection::vec(step(), 1..16),
        post_at in 0usize..16,
        cuts in proptest::collection::btree_set(1usize..16, 0..4),
    ) {
        let post_at = post_at % steps.len();
        let events: Vec<RunEvent> = steps.iter().enumerate()
            .map(|(i, s)| to_event(i as u64 + 1, i == post_at, s)).collect();
        let whole = apply(&events).unwrap();

        // Split the event stream at random points (as flushes would) and merge the versions back.
        let mut bounds: Vec<usize> = cuts.into_iter().filter(|c| *c < events.len()).collect();
        bounds.insert(0, 0);
        bounds.push(events.len());
        let mut versions: Vec<RunState> = bounds.windows(2)
            .filter(|w| w[0] < w[1])
            .map(|w| apply(&events[w[0]..w[1]]).unwrap())
            .collect();
        versions.reverse(); // order of arrival must not matter
        let merged = merge_versions(versions).unwrap();

        prop_assert_eq!(view(&merged), view(&whole));
        prop_assert_eq!(merged.present, whole.present);
    }
}

#[test]
fn identity_conflict_keeps_first_and_warns() {
    let mut a = post_fields(child_ids(), "fetch_url");
    a.project = Some("p1".into());
    let first = apply(&[event(Op::Post, 1, a)]).unwrap();
    let mut b = child_ids();
    b.project = Some("p2".into());
    b.error = Some("boom".into());
    let second = apply(&[event(Op::Patch, 2, b)]).unwrap();
    let m = merge_versions(vec![second, first]).unwrap();
    assert_eq!(m.run.project, "p1");
    assert_eq!(m.run.error.as_deref(), Some("boom"));
    assert!(m.run.warnings.contains(&Warning::IdentityConflict));
}

#[test]
fn a_patch_created_version_is_marked() {
    let st = apply(&[event(Op::Patch, 5, child_ids())]).unwrap();
    assert!(st.created_by_patch());
    let st = apply(&[event(Op::Post, 5, post_fields(child_ids(), "x"))]).unwrap();
    assert!(!st.created_by_patch());
}
