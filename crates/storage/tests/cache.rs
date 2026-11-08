//! The bounded data cache (M3-FR-12, design §5): least recently used out first, by bytes, never over its
//! capacity; a second read of the same bytes is served from memory; it holds dictionary footers (with their
//! page indexes), dictionary pages, postings chunks, and payload footers and pages.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use serde_json::json;
use tracedb_model::attrs::promote;
use tracedb_model::{Op, RunEvent, RunFields, Source, Ts, merge};
use tracedb_storage::cache::{Cache, Cached, Key};
use tracedb_storage::fts::{Fts, TextField};
use tracedb_storage::segment::{self, Version};
use tracedb_storage::{StorageError, Store};
use uuid::Uuid;

fn bytes(n: usize, fill: u8) -> Cached {
    Cached::Bytes(Bytes::from(vec![fill; n]))
}

fn key(i: u64) -> Key {
    Key::range("p/seg-1.payload.parquet", i * 1000..i * 1000 + 1000)
}

#[tokio::test]
async fn least_recently_used_goes_first_and_the_bound_holds() {
    let cache = Cache::new(64 * 1024);
    let loads = AtomicUsize::new(0);
    let get = |i: u64| {
        let (cache, loads) = (&cache, &loads);
        async move {
            cache
                .get_or_load(key(i), || async {
                    loads.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, StorageError>(bytes(4096, i as u8))
                })
                .await
                .unwrap()
        }
    };
    for i in 0..10 {
        get(i).await;
    }
    let (_, hit) = get(0).await;
    assert!(hit, "a second read hits");
    assert_eq!(loads.load(Ordering::Relaxed), 10);
    for i in 10..100 {
        get(i).await;
        // Key 0 stays the most recently used.
        get(0).await;
        assert!(cache.bytes() <= 64 * 1024, "{} B cached", cache.bytes());
    }
    assert!(
        cache.bytes() >= 48 * 1024,
        "the cache fills up to its bound: {} B",
        cache.bytes()
    );
    assert!(get(0).await.1, "the most recently used survives");
    assert!(!get(10).await.1, "the least recently used went first");
    assert!(cache.hits() >= 91);
}

#[tokio::test]
async fn an_entry_larger_than_an_eighth_of_the_cache_is_not_kept() {
    let cache = Cache::new(64 * 1024);
    let load = || async { Ok::<_, StorageError>(bytes(16 * 1024, 1)) };
    cache.get_or_load(key(1), load).await.unwrap();
    let (_, hit) = cache.get_or_load(key(1), load).await.unwrap();
    assert!(!hit);
    assert_eq!(cache.bytes(), 0);
}

fn uid(n: u128) -> Uuid {
    Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0000 | n)
}

fn version(n: u128) -> Version {
    let id = uid(n);
    let f = RunFields {
        id: Some(id),
        trace_id: Some(id),
        dotted_order: Some(format!("20261105T020000000000Z{id}")),
        project: Some("p".into()),
        name: Some("r".into()),
        run_type: Some("chain".into()),
        start_time: Ts::parse("2026-11-05T02:00:00Z"),
        inputs: Some(json!({"q": format!("question {n} about rate limits"), "pad": "x".repeat(300)})),
        outputs: Some(json!({"a": format!("answer {n}")})),
        ..Default::default()
    };
    let ev = RunEvent {
        op: Op::Post,
        event_seq: n as u64,
        received_at: Ts(0),
        source: Source::Native,
        body_hash: 0,
        fields: f,
    };
    let mut slot = None;
    merge(&mut slot, &ev).unwrap();
    let state = slot.unwrap();
    Version {
        attrs: promote(&state.run),
        state,
        needs_merge: false,
    }
}

#[tokio::test]
async fn text_lookups_and_payload_reads_are_served_from_it_the_second_time() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::local(dir.path()).unwrap();
    let w = segment::write("p", (1..=3_000).map(version).collect(), Ts(0)).unwrap();
    store.put_new(&w.meta.core_path, w.core.clone()).await.unwrap();
    store.put_new(&w.meta.payload_path, w.payload.clone()).await.unwrap();
    let stem = w.meta.fts_path.clone().unwrap();
    store
        .put_new(&tracedb_storage::fts::dict_path(&stem), w.fts.dict.clone())
        .await
        .unwrap();
    store
        .put_new(&tracedb_storage::fts::post_path(&stem), w.fts.post.clone())
        .await
        .unwrap();

    let cache = Arc::new(Cache::new(32 << 20));
    let fts = Fts::with_cache(store.clone(), cache.clone());
    let words = ["question".to_string(), "1234".to_string()];
    let first = fts
        .lookup_within(&w.meta, &words, TextField::Inputs, u32::MAX)
        .await
        .unwrap();
    let rows = first.rows.clone().unwrap();
    assert_eq!(rows.len(), 1);
    let read = |rows: Vec<u32>| {
        let (store, cache, seg) = (store.clone(), cache.clone(), w.meta.clone());
        async move {
            segment::read_payload_rows(&store, &cache, &seg, &rows, true, true, false)
                .await
                .unwrap()
        }
    };
    let got = read(rows.clone()).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1, uid(1234));
    assert_eq!(
        got[0].2.inputs.as_ref().unwrap()["q"],
        "question 1234 about rate limits"
    );
    assert!(got[0].2.events.is_none());

    let kinds = cache.kinds();
    for k in ["footer", "dict_page", "postings", "payload_page"] {
        assert!(kinds.get(k).is_some_and(|&n| n > 0), "{k} cached: {kinds:?}");
    }
    let (hits, misses) = (cache.hits(), cache.misses());
    let again = fts
        .lookup_within(&w.meta, &words, TextField::Inputs, u32::MAX)
        .await
        .unwrap();
    assert_eq!(again.rows, first.rows);
    assert_eq!(read(rows).await, got);
    assert_eq!(cache.misses(), misses, "nothing read from the files the second time");
    assert!(cache.hits() > hits);
    assert!(cache.bytes() <= 32 << 20);
}
