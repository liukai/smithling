//! The memtable limit counts heap, not JSON bytes (M3 q1): `RunStore::bytes` stays within 25% of what the
//! allocator reports for the same runs, for runs without payload, chat-like payloads, one large text and
//! event lists.

mod common;

use common::*;
use serde_json::{Value, json};
use tikv_jemalloc_ctl::{epoch, stats};
use tracedb_ingest::store::RunStore;
use tracedb_model::{Op, RunFields, validate};

#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn allocated() -> u64 {
    epoch::advance().unwrap();
    stats::allocated::read().unwrap() as u64
}

fn prose(n: usize, salt: usize) -> String {
    let words = [
        "fetch",
        "the",
        "page",
        "and",
        "summarize",
        "429",
        "retry",
        "model",
        "answer",
        "quickly",
    ];
    (0..n)
        .map(|i| words[(i * 7 + salt) % words.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

fn with_payload(mut f: RunFields, shape: &str, i: usize) -> RunFields {
    let obj = |v: Value| v.as_object().unwrap().clone();
    match shape {
        "headers" => {}
        "chat" => {
            f.inputs = Some(json!({"messages": [
                {"role": "system", "content": prose(30, i)},
                {"role": "user", "content": prose(40, i + 1)},
            ], "temperature": 0.2}));
            f.outputs = Some(json!({"text": prose(80, i + 2),
                "usage_metadata": {"input_tokens": 120 + i, "output_tokens": 80, "total_tokens": 200 + i}}));
            f.metadata = Some(obj(
                json!({"ls_model_name": "gpt-4o", "ls_provider": "openai", "batch": "a",
                "attempt": i % 3, "region": "us-east-1", "user": format!("u{}", i % 97)}),
            ));
            f.tags = Some(vec!["eval".into(), format!("shard-{}", i % 8)]);
        }
        "large" => f.inputs = Some(json!({"document": prose(1500, i)})),
        "events" => {
            f.events = Some(
                (0..10)
                    .map(|k| json!({"name": "new_token", "time": "2026-11-05T02:00:01Z", "kwargs": {"token": k}}))
                    .collect(),
            )
        }
        _ => unreachable!(),
    }
    f
}

#[test]
fn memtable_bytes_track_its_heap() {
    for shape in ["headers", "chat", "large", "events"] {
        let mut events = Vec::new();
        for i in 0..20_000usize {
            let t = uuid(i as u128 * 2 + 1);
            let f = if i % 4 == 0 {
                root(t, "2026-11-05T02:00:00Z", "agent")
            } else {
                child(
                    uuid((i - i % 4) as u128 * 2 + 1),
                    "2026-11-05T02:00:00Z",
                    t,
                    "2026-11-05T02:00:01Z",
                )
            };
            let mut f = with_payload(f, shape, i);
            validate(Op::Post, &mut f).unwrap();
            events.push(event(i as u64 + 1, &post(f)));
        }
        let before = allocated();
        let mut store = RunStore::new();
        for ev in &events {
            store.apply(ev).unwrap();
        }
        let heap = allocated() - before;
        let counted = store.bytes();
        let ratio = counted as f64 / heap as f64;
        eprintln!(
            "{shape:>8}: counted {:.1} MB, heap {:.1} MB ({ratio:.2})",
            counted as f64 / 1e6,
            heap as f64 / 1e6
        );
        assert!(
            (0.75..=1.25).contains(&ratio),
            "{shape}: counted {counted} B, heap {heap} B"
        );
        drop(store);
    }
}
