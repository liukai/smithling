//! Shows how a run's start and update messages become one run.
//! Run with: cargo run -p tracedb-model --example merge_demo
//!
//! The story: an overnight eval of a research agent. For task t-117 the agent starts at 02:14:00 (the root run),
//! and 7 s later calls its `fetch_url` tool (a child run). The API rate-limits it and the call fails 31 s later:
//!
//! ```text
//! trace 0b9f…11                      one task, start to finish
//! └── run 0b9f…11  research_task     root run: its id IS the trace id (not in this demo)
//!     └── run 7c1e…a4  fetch_url     this demo: start message, then a failing update
//! ```
//!
//! The SDK sends two messages for `fetch_url`: one when it starts, one when it ends. tracedb merges them.

use serde_json::json;
use tracedb_model::{Op, RunEvent, RunFields, RunHeader, Source, Ts, merge, validate};

/// This step: the `fetch_url` tool call. Every message about this step carries it.
const RUN_ID: &str = "7c1e0000-0000-4000-8000-0000000000a4";

/// The whole task this step belongs to. It equals the root run's id, so every run in the tree shares it.
const TRACE_ID: &str = "0b9f0000-0000-4000-8000-000000000011";

/// The path from the root down to this run: one `<start time><run id>` segment per level, joined by `.`.
/// Two segments, so depth 1. The second-to-last segment is the parent, so `parent_run_id` can be left out.
/// Sorting runs by this string lists a trace in tree order, each parent before its children.
const DOTTED_ORDER: &str = concat!(
    "20261105T021400000000Z",
    "0b9f0000-0000-4000-8000-000000000011", // root: started 02:14:00.000000
    ".",
    "20261105T021407123456Z",
    "7c1e0000-0000-4000-8000-0000000000a4", // this run: started 02:14:07.123456
);

fn main() {
    // Message 1, sent when the tool call starts: who it is, where it sits, what it was asked to do.
    let start: RunFields = serde_json::from_value(json!({
        "id": RUN_ID,
        "trace_id": TRACE_ID,
        "dotted_order": DOTTED_ORDER,
        "project": "research-agent",
        "name": "fetch_url",
        "run_type": "tool",
        "start_time": "2026-11-05T02:14:07.123456Z",
        "inputs": {"url": "https://api.example.com/papers?q=llm"},
        "tags": ["batch=2026-11-05-a"],
        "metadata": {"prompt_version": "v7", "task_id": "t-117"}
    }))
    .unwrap();
    // Message 2, sent 31 s later when it fails. Same three ids, so tracedb knows it's the same step.
    // Fields not repeated here (name, inputs, tags…) keep their values from message 1.
    let update: RunFields = serde_json::from_value(json!({
        "id": RUN_ID,
        "trace_id": TRACE_ID,
        "dotted_order": DOTTED_ORDER,
        "end_time": "2026-11-05T02:14:38.200000Z",
        "error": "HTTPError: 429 Too Many Requests",
        "outputs": {"status_code": 429, "body": "Too Many Requests"}
    }))
    .unwrap();

    let mut slot = None;
    for (step, op, seq, mut fields) in [(1, Op::Post, 1042, start), (2, Op::Patch, 1987, update)] {
        validate(op, &mut fields).expect("valid");
        let ev = RunEvent {
            op,
            event_seq: seq,
            received_at: Ts::now(),
            source: Source::Native,
            body_hash: seq,
            fields,
        };
        let kind = merge(&mut slot, &ev).expect("merges");
        let st = slot.as_ref().unwrap();
        println!(
            "after message {step} ({op:?}): {kind:?} → status={:?} latency_ms={:?} error={:?}",
            st.run.status, st.run.latency_ms, st.run.error
        );
    }

    let header = RunHeader::from_state(slot.as_ref().unwrap(), 0);
    println!("\nmerged run, as the runs list and trace view will see it:");
    println!("{}", serde_json::to_string_pretty(&header).unwrap());

    // Only an id: no trace_id or dotted_order, so tracedb can't place it in any tree.
    let mut bad: RunFields = serde_json::from_value(json!({"id": RUN_ID})).unwrap();
    println!(
        "\na malformed message is rejected with a reason: {:?}",
        validate(Op::Patch, &mut bad).unwrap_err()
    );
}
