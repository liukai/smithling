mod common;

use common::*;
use proptest::prelude::*;
use serde_json::json;
use smithling_model::{ChangeKind, Op, Reason, RunHeader, RunState, RunType, Status, Warning, merge};

fn apply_all(events: &[smithling_model::RunEvent]) -> Option<RunState> {
    let mut slot = None;
    for ev in events {
        merge(&mut slot, ev).unwrap();
    }
    slot
}

fn fetch_url_post() -> smithling_model::RunEvent {
    let mut f = post_fields(child_ids(), "fetch_url");
    f.project = Some("research-agent".into());
    f.inputs = Some(json!({"url": "https://example.com"}));
    f.tags = Some(vec!["batch=a".into()]);
    f.metadata = Some(json!({"prompt_version": "v7"}).as_object().unwrap().clone());
    event(Op::Post, 1042, f)
}

fn fetch_url_patch() -> smithling_model::RunEvent {
    let mut f = child_ids();
    f.end_time = Some(ts("2026-11-05T02:14:38.200000Z"));
    f.error = Some("HTTPError: 429 Too Many Requests".into());
    f.outputs = Some(json!({"status_code": 429}));
    event(Op::Patch, 1987, f)
}

#[test]
fn worked_example_post_then_patch() {
    let st = apply_all(&[fetch_url_post(), fetch_url_patch()]).unwrap();
    let h = RunHeader::from_state(&st, 0);
    assert_eq!(h.status, Status::Error);
    assert_eq!(h.latency_ms, Some(31_076));
    assert_eq!(h.depth, 1);
    assert!(!h.is_root);
    assert_eq!(h.project, "research-agent");
    assert_eq!(h.name.as_deref(), Some("fetch_url"));
    assert_eq!(h.run_type, RunType::Tool);
    assert_eq!(h.tags, vec!["batch=a"]);
    assert_eq!(h.metadata["prompt_version"], "v7");
    assert_eq!((h.first_seq, h.last_seq, h.event_count), (1042, 1987, 2));
    assert!(h.payload_sizes.inputs > 0 && h.payload_sizes.outputs > 0);
    assert_eq!(st.run.inputs, Some(json!({"url": "https://example.com"})));
}

#[test]
fn absent_fields_never_overwrite() {
    let st = apply_all(&[fetch_url_post(), fetch_url_patch()]).unwrap();
    assert_eq!(
        st.run.inputs,
        Some(json!({"url": "https://example.com"})),
        "patch had no inputs"
    );
    assert_eq!(st.run.tags, vec!["batch=a"], "patch had no tags");
}

#[test]
fn present_fields_replace_whole_values() {
    let mut f = child_ids();
    f.tags = Some(vec!["x".into()]);
    f.events = Some(vec![json!({"name": "t1"}), json!({"name": "t2"})]);
    let mut g = child_ids();
    g.events = Some(vec![
        json!({"name": "t1"}),
        json!({"name": "t2"}),
        json!({"name": "t3"}),
    ]);
    let st = apply_all(&[fetch_url_post(), event(Op::Patch, 2000, f), event(Op::Patch, 2001, g)]).unwrap();
    assert_eq!(st.run.tags, vec!["x"]);
    assert_eq!(
        st.run.events.as_ref().unwrap().len(),
        3,
        "events are replaced, not appended"
    );
}

#[test]
fn change_kinds() {
    let mut slot = None;
    assert_eq!(merge(&mut slot, &fetch_url_post()).unwrap(), ChangeKind::NewChild);
    assert_eq!(merge(&mut slot, &fetch_url_patch()).unwrap(), ChangeKind::Updated);

    let mut root = None;
    let mut early = root_ids();
    early.end_time = Some(ts("2026-11-05T02:55:00Z"));
    assert_eq!(
        merge(&mut root, &event(Op::Patch, 1, early)).unwrap(),
        ChangeKind::Updated
    );
    assert!(
        !root.as_ref().unwrap().has_start,
        "patch before post is not a listed root"
    );
    let mut start = post_fields(root_ids(), "research_task");
    start.project = Some("research-agent".into());
    assert_eq!(
        merge(&mut root, &event(Op::Post, 2, start)).unwrap(),
        ChangeKind::NewRoot
    );
    let st = root.unwrap();
    assert!(st.is_listed_root());
    assert_eq!(st.run.status, Status::Ok);
    assert_eq!(
        st.run.project, "research-agent",
        "project set by a later event is not a conflict"
    );
}

#[test]
fn second_post_acts_like_a_patch() {
    let mut retry = post_fields(child_ids(), "fetch_url_v2");
    retry.start_time = None;
    let st = apply_all(&[fetch_url_post(), event(Op::Post, 1100, retry)]).unwrap();
    assert_eq!(st.run.name.as_deref(), Some("fetch_url_v2"));
    assert!(st.run.start_time.is_some());
}

#[test]
fn identity_conflicts_are_rejected_and_leave_the_run_unchanged() {
    let other = uuid::Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
    let mut cases = Vec::new();
    let mut f = child_ids();
    f.trace_id = Some(other);
    cases.push(f);
    let mut f = child_ids();
    f.parent_run_id = Some(other);
    cases.push(f);
    let mut f = child_ids();
    f.dotted_order = Some(format!(
        "{}.20261105T021407123456Z{RUN}",
        format_args!("20261105T021400000000Z{other}")
    ));
    cases.push(f);
    let mut f = child_ids();
    f.project = Some("another-project".into());
    cases.push(f);

    for fields in cases {
        let mut slot = Some(apply_all(&[fetch_url_post()]).unwrap());
        let before = slot.clone();
        let err = merge(&mut slot, &event(Op::Patch, 5000, fields)).unwrap_err();
        assert_eq!(err.reason, Reason::IdentityConflict);
        assert_eq!(slot, before);
    }
}

#[test]
fn address_is_kept_and_immutable() {
    let lrn = "lrn:agents/research-bot/environments/dev";
    let mut start = post_fields(child_ids(), "fetch_url");
    start.address = Some(lrn.into());
    let mut slot = None;
    merge(&mut slot, &event(Op::Post, 1, start)).unwrap();
    assert_eq!(slot.as_ref().unwrap().run.address.as_deref(), Some(lrn));
    assert_eq!(
        RunHeader::from_state(slot.as_ref().unwrap(), 0).address.as_deref(),
        Some(lrn)
    );

    let mut same = child_ids();
    same.address = Some(lrn.into());
    merge(&mut slot, &event(Op::Patch, 2, same)).expect("same address is fine");

    let mut other = child_ids();
    other.address = Some("lrn:agents/other/environments/prod".into());
    let before = slot.clone();
    assert_eq!(
        merge(&mut slot, &event(Op::Patch, 3, other)).unwrap_err().reason,
        Reason::IdentityConflict
    );
    assert_eq!(slot, before);
}

#[test]
fn address_can_arrive_on_a_later_event() {
    let mut slot = Some(apply_all(&[fetch_url_post()]).unwrap());
    let mut late = child_ids();
    late.address = Some("lrn:agents/a/environments/dev".into());
    merge(&mut slot, &event(Op::Patch, 2000, late)).unwrap();
    assert_eq!(
        slot.unwrap().run.address.as_deref(),
        Some("lrn:agents/a/environments/dev")
    );
}

#[test]
fn status_transitions() {
    let mut slot = None;
    merge(&mut slot, &fetch_url_post()).unwrap();
    assert_eq!(slot.as_ref().unwrap().run.status, Status::Running);

    let mut done = child_ids();
    done.end_time = Some(ts("2026-11-05T02:14:10Z"));
    merge(&mut slot, &event(Op::Patch, 2, done)).unwrap();
    assert_eq!(slot.as_ref().unwrap().run.status, Status::Ok);

    let mut failed = child_ids();
    failed.error = Some("boom".into());
    merge(&mut slot, &event(Op::Patch, 3, failed)).unwrap();
    assert_eq!(slot.as_ref().unwrap().run.status, Status::Error);

    let mut later = child_ids();
    later.end_time = Some(ts("2026-11-05T02:14:20Z"));
    merge(&mut slot, &event(Op::Patch, 4, later)).unwrap();
    assert_eq!(
        slot.as_ref().unwrap().run.status,
        Status::Error,
        "an error can't be cleared by omission"
    );
}

#[test]
fn negative_latency_is_a_warning() {
    let mut f = child_ids();
    f.end_time = Some(ts("2026-11-05T02:00:00Z"));
    let st = apply_all(&[fetch_url_post(), event(Op::Patch, 2, f)]).unwrap();
    assert_eq!(st.run.latency_ms, None);
    assert_eq!(st.run.warnings, vec![Warning::NegativeLatency]);
}

#[test]
fn unknown_run_type_is_other_with_raw_kept() {
    let mut f = post_fields(child_ids(), "x");
    f.run_type = Some("planner".into());
    let st = apply_all(&[event(Op::Post, 1, f)]).unwrap();
    assert_eq!(st.run.run_type, RunType::Other);
    assert_eq!(st.run.run_type_raw.as_deref(), Some("planner"));
}

#[test]
fn derived_model_tokens_and_thread() {
    let mut f = post_fields(child_ids(), "llm");
    f.metadata = Some(
        json!({"ls_model_name": "gpt-x", "thread_id": "chat-42"})
            .as_object()
            .unwrap()
            .clone(),
    );
    f.outputs = Some(json!({"usage_metadata": {"input_tokens": 1200, "output_tokens": 350}}));
    let st = apply_all(&[event(Op::Post, 1, f)]).unwrap();
    assert_eq!(st.run.model.as_deref(), Some("gpt-x"));
    assert_eq!(st.run.thread_id.as_deref(), Some("chat-42"));
    assert_eq!(
        (st.run.input_tokens, st.run.output_tokens, st.run.total_tokens),
        (Some(1200), Some(350), Some(1550))
    );

    let mut g = post_fields(child_ids(), "llm");
    g.metadata = Some(
        json!({"usage_metadata": {"input_tokens": 5, "output_tokens": 6, "total_tokens": 20}})
            .as_object()
            .unwrap()
            .clone(),
    );
    let st = apply_all(&[event(Op::Post, 1, g)]).unwrap();
    assert_eq!(
        st.run.total_tokens,
        Some(20),
        "reported total wins over the sum; metadata is the fallback"
    );
}

// Property tests: a run is the field-wise "latest present value" of its events, in event_seq order.

#[derive(Clone, Debug)]
struct Partial {
    name: Option<String>,
    error: Option<String>,
    end_minute: Option<u8>,
    tags: Option<Vec<String>>,
    output: Option<u32>,
}

fn partial() -> impl Strategy<Value = Partial> {
    (
        proptest::option::of("[a-z]{1,8}"),
        proptest::option::of("[a-z]{1,8}"),
        proptest::option::of(0u8..59),
        proptest::option::of(proptest::collection::vec("[a-z]{1,4}", 0..3)),
        proptest::option::of(any::<u32>()),
    )
        .prop_map(|(name, error, end_minute, tags, output)| Partial {
            name,
            error,
            end_minute,
            tags,
            output,
        })
}

fn to_event(seq: u64, first: bool, p: &Partial) -> smithling_model::RunEvent {
    let mut f = if first {
        post_fields(child_ids(), "start")
    } else {
        child_ids()
    };
    if p.name.is_some() {
        f.name = p.name.clone();
    }
    f.error = p.error.clone();
    f.end_time = p.end_minute.map(|m| ts(&format!("2026-11-05T03:{m:02}:00Z")));
    f.tags = p.tags.clone();
    f.outputs = p.output.map(|o| json!({"v": o}));
    event(if first { Op::Post } else { Op::Patch }, seq, f)
}

proptest! {
    #[test]
    fn run_is_latest_present_value_per_field(parts in proptest::collection::vec(partial(), 1..20)) {
        let events: Vec<_> = parts.iter().enumerate().map(|(i, p)| to_event(i as u64 + 1, i == 0, p)).collect();
        let st = apply_all(&events).unwrap();

        let last = |get: &dyn Fn(&smithling_model::RunEvent) -> Option<serde_json::Value>| {
            events.iter().rev().find_map(get)
        };
        let name = last(&|e| e.fields.name.clone().map(Into::into));
        let error = last(&|e| e.fields.error.clone().map(Into::into));
        let end = last(&|e| e.fields.end_time.map(|t| t.0.into()));
        let tags = last(&|e| e.fields.tags.clone().map(Into::into));
        let outputs = last(&|e| e.fields.outputs.clone());

        prop_assert_eq!(st.run.name.clone().map(Into::into), name);
        prop_assert_eq!(st.run.error.clone().map(Into::into), error.clone());
        prop_assert_eq!(st.run.end_time.map(|t| t.0.into()), end.clone());
        prop_assert_eq!(serde_json::Value::from(st.run.tags.clone()), tags.unwrap_or(serde_json::json!([])));
        prop_assert_eq!(st.run.outputs.clone(), outputs);
        let expected_status = if error.is_some() { Status::Error } else if end.is_some() { Status::Ok } else { Status::Running };
        prop_assert_eq!(st.run.status, expected_status);
        prop_assert_eq!(st.event_count as usize, events.len());
        prop_assert_eq!(st.last_seq, events.len() as u64);
    }

    #[test]
    fn arrival_order_is_irrelevant_once_sorted_by_seq(parts in proptest::collection::vec(partial(), 1..12), seed in any::<u64>()) {
        let events: Vec<_> = parts.iter().enumerate().map(|(i, p)| to_event(i as u64 + 1, i == 0, p)).collect();
        let mut shuffled = events.clone();
        let n = shuffled.len();
        for i in (1..n).rev() {
            shuffled.swap(i, (seed.wrapping_mul(i as u64 + 7) % (i as u64 + 1)) as usize);
        }
        shuffled.sort_by_key(|e| e.event_seq);
        prop_assert_eq!(apply_all(&events), apply_all(&shuffled));
    }

    #[test]
    fn reapplying_an_event_changes_only_bookkeeping(parts in proptest::collection::vec(partial(), 1..8)) {
        let events: Vec<_> = parts.iter().enumerate().map(|(i, p)| to_event(i as u64 + 1, i == 0, p)).collect();
        let once = apply_all(&events).unwrap();
        let mut twice_events = events.clone();
        twice_events.push(events.last().unwrap().clone());
        let twice = apply_all(&twice_events).unwrap();
        prop_assert_eq!(once.run, twice.run);
    }
}
