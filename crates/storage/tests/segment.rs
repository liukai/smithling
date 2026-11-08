//! Segment round trip: versions written to Parquet come back identical (minus payloads, which live in the
//! payload file), sorted, with bloom filters, and DataFusion can read them through the store.

use bytes::Bytes;
use datafusion::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use datafusion::parquet::file::reader::{FileReader, SerializedFileReader};
use serde_json::json;
use smithling_model::attrs::promote;
use smithling_model::{Op, RunEvent, RunFields, RunState, Source, Ts, merge, merge_versions};
use smithling_storage::segment::{self, Version, decode_core, decode_payload, start_key};
use smithling_storage::{DedupKeys, Metastore, Store};
use uuid::Uuid;

fn uid(n: u128) -> Uuid {
    Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0000 | n)
}

fn dotted(minute: u32, id: Uuid) -> String {
    format!("20261105T02{minute:02}00000000Z{id}")
}

fn state(n: u128, minute: u32, post: bool) -> RunState {
    let id = uid(n);
    let f = RunFields {
        id: Some(id),
        trace_id: Some(id),
        dotted_order: Some(dotted(minute, id)),
        project: post.then(|| "eval-agent".to_string()),
        name: post.then(|| format!("run-{n}")),
        run_type: post.then(|| "chain".to_string()),
        start_time: post.then(|| Ts::parse(&format!("2026-11-05T02:{minute:02}:00Z")).unwrap()),
        end_time: (!post).then(|| Ts::parse("2026-11-05T03:00:00Z").unwrap()),
        error: (n.is_multiple_of(3) && !post).then(|| "boom".to_string()),
        tags: post.then(|| vec!["batch=2026-11-05-a".to_string()]),
        metadata: post.then(|| {
            json!({"prompt_version": if n.is_multiple_of(2) { "v6" } else { "v7" }})
                .as_object()
                .unwrap()
                .clone()
        }),
        inputs: post.then(|| json!({"q": format!("question {n}")})),
        outputs: (!post).then(|| json!({"status_code": 429})),
        ..Default::default()
    };
    let ev = RunEvent {
        op: if post { Op::Post } else { Op::Patch },
        event_seq: n as u64 * 10 + u64::from(!post),
        received_at: Ts(1_000 + n as i64),
        source: Source::Native,
        body_hash: 0,
        fields: f,
    };
    let mut slot = None;
    merge(&mut slot, &ev).unwrap();
    slot.unwrap()
}

fn version(st: RunState) -> Version {
    let attrs = promote(&st.run);
    let needs_merge = st.created_by_patch();
    Version {
        state: st,
        attrs,
        needs_merge,
    }
}

fn read_all(bytes: &Bytes) -> Vec<datafusion::arrow::record_batch::RecordBatch> {
    ParquetRecordBatchReaderBuilder::try_new(bytes.clone())
        .unwrap()
        .build()
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn versions_round_trip_and_merge_like_the_originals() {
    let posts: Vec<RunState> = (1..=40).map(|n| state(n, (n % 60) as u32, true)).collect();
    let patches: Vec<RunState> = (1..=40).map(|n| state(n, (n % 60) as u32, false)).collect();
    let rows: Vec<Version> = posts.iter().chain(&patches).cloned().map(version).collect();
    let w = segment::write("eval-agent", rows, Ts(1_700_000_000_000_000)).unwrap();
    assert_eq!(w.meta.row_count, 80);
    assert_eq!(w.meta.needs_merge_rows, 40, "patch-created versions are marked");
    assert!(
        w.meta.core_path.starts_with("eval-agent-") && w.meta.core_path.contains("/2023-11-14/seg-"),
        "{}",
        w.meta.core_path
    );

    let stored: Vec<_> = read_all(&w.core).iter().flat_map(|b| decode_core(b).unwrap()).collect();
    assert_eq!(stored.len(), 80);
    let ids: Vec<Uuid> = stored.iter().map(|r| r.state.run.trace_id).collect();
    assert!(ids.windows(2).all(|p| p[0] <= p[1]), "sorted by trace_id");

    for n in 1..=40u128 {
        let back: Vec<RunState> = stored
            .iter()
            .filter(|r| r.state.run.run_id == uid(n))
            .map(|r| r.state.clone())
            .collect();
        assert_eq!(back.len(), 2);
        let merged = merge_versions(back).unwrap();
        let mut want = merge_versions(vec![posts[n as usize - 1].clone(), patches[n as usize - 1].clone()]).unwrap();
        // Payloads aren't in the core file; everything else must match exactly.
        want.run.inputs = None;
        want.run.outputs = None;
        assert_eq!(merged, want, "run {n}");
        let row = stored
            .iter()
            .find(|r| r.state.run.run_id == uid(n) && !r.needs_merge)
            .unwrap();
        assert_eq!(row.attrs, promote(&posts[n as usize - 1].run));
    }

    let payloads: Vec<_> = read_all(&w.payload)
        .iter()
        .flat_map(|b| decode_payload(b).unwrap())
        .collect();
    let first = &stored[0];
    let (pid, p) = &payloads[first.payload_row as usize];
    assert_eq!(*pid, first.state.run.run_id, "payload_row points at the same run");
    assert!(p.inputs.is_some() || p.outputs.is_some());
}

#[test]
fn bloom_filters_and_page_statistics_are_written() {
    let rows: Vec<Version> = (1..=10).map(|n| version(state(n, 1, true))).collect();
    let w = segment::write("p", rows, Ts(0)).unwrap();
    let reader = SerializedFileReader::new(w.core.clone()).unwrap();
    let md = reader.metadata();
    let schema = md.file_metadata().schema_descr();
    let col = |name: &str| {
        (0..schema.num_columns())
            .find(|i| schema.column(*i).name() == name)
            .unwrap()
    };
    let rg = md.row_group(0);
    assert!(
        rg.column(col("run_id")).bloom_filter_offset().is_some(),
        "bloom on run_id"
    );
    assert!(
        rg.column(col("trace_id")).bloom_filter_offset().is_some(),
        "bloom on trace_id"
    );
    assert!(rg.column(col("start_key")).statistics().is_some());
    assert!(
        md.offset_index().is_some()
            || md.column_index().is_some()
            || rg.column(col("start_key")).column_index_offset().is_some(),
        "page index written"
    );
}

#[test]
fn start_key_comes_from_the_runs_own_segment() {
    let root = uid(1);
    let child = uid(2);
    let d = format!("{}.20261105T021407123456Z{child}", dotted(0, root));
    assert_eq!(start_key(&d), Ts::parse("2026-11-05T02:14:07.123456Z").unwrap().0);
}

#[test]
fn field_stats_count_types_and_top_values() {
    let rows: Vec<Version> = (1..=20).map(|n| version(state(n, 1, true))).collect();
    let w = segment::write("p", rows, Ts(0)).unwrap();
    let pv = w.stats.iter().find(|s| s.path == "metadata.prompt_version").unwrap();
    assert_eq!((pv.ty.as_str(), pv.count, pv.other_count), ("string", 20, 0));
    assert_eq!(pv.top_values.len(), 2);
    assert_eq!(pv.top_values.iter().map(|v| v.1).sum::<u64>(), 20);
    assert!(w.stats.iter().any(|s| s.path == "tags" && s.ty == "list"));
}

#[test]
fn stats_resume_from_their_full_listing() {
    use smithling_storage::segment::StatsBuilder;
    let runs: Vec<RunState> = (1..=30).map(|n| state(n, 1, n % 4 != 0)).collect();
    let add = |b: &mut StatsBuilder, rs: &[RunState]| rs.iter().for_each(|st| b.add_run(st, &promote(&st.run)));
    let mut whole = StatsBuilder::default();
    add(&mut whole, &runs);
    let mut first = StatsBuilder::default();
    add(&mut first, &runs[..12]);
    let mut resumed = StatsBuilder::from_stats(&first.finish_all("s"));
    add(&mut resumed, &runs[12..]);
    assert_eq!(resumed.finish_all("s"), whole.finish_all("s"));
}

#[tokio::test]
async fn store_is_write_once_and_readable_by_datafusion() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::local(&dir.path().join("data")).unwrap();
    let rows: Vec<Version> = (1..=5).map(|n| version(state(n, 1, true))).collect();
    let w = segment::write("p q/x", rows, Ts(0)).unwrap();
    assert!(
        w.meta.core_path.starts_with("p_q_x-"),
        "project names are sanitized for paths"
    );
    let other = segment::write("p_q_x", vec![version(state(9, 1, true))], Ts(0)).unwrap();
    assert_ne!(
        other.meta.core_path.split('/').next(),
        w.meta.core_path.split('/').next(),
        "different projects never share a folder"
    );
    store.put_new(&w.meta.core_path, w.core.clone()).await.unwrap();
    assert!(
        store.put_new(&w.meta.core_path, w.core.clone()).await.is_err(),
        "names are never reused"
    );
    assert_eq!(store.get(&w.meta.core_path).await.unwrap(), w.core);
    assert_eq!(store.list().await.unwrap(), vec![w.meta.core_path.clone()]);

    let ctx = datafusion::prelude::SessionContext::new();
    store.register(&ctx);
    let df = ctx
        .read_parquet(
            store.url(&w.meta.core_path),
            datafusion::prelude::ParquetReadOptions::default(),
        )
        .await
        .unwrap();
    let batches = df.collect().await.unwrap();
    let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
    assert_eq!(rows, 5);
    assert_eq!(decode_core(&batches[0]).unwrap()[0].state.run.project, "eval-agent");

    store.delete(&w.meta.core_path).await.unwrap();
    assert!(store.list().await.unwrap().is_empty());
}

fn keys(flushed_seq: u64, newest_at: i64) -> DedupKeys {
    DedupKeys {
        flushed_seq,
        newest_at,
        keys: vec![flushed_seq as u8; 33],
    }
}

#[tokio::test]
async fn metastore_flush_is_atomic_and_persistent() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("meta.sqlite").display());
    let m = Metastore::open(&url).await.unwrap();
    assert_eq!(m.flushed_seq().await.unwrap(), 0);
    let rows: Vec<Version> = (1..=5).map(|n| version(state(n, 1, true))).collect();
    let w = segment::write("p", rows, Ts(0)).unwrap();
    m.commit_flush(std::slice::from_ref(&w.meta), &w.stats, 42, &keys(42, 1_000), 0)
        .await
        .unwrap();

    // A failing flush (duplicate segment id) changes nothing.
    assert!(
        m.commit_flush(std::slice::from_ref(&w.meta), &w.stats, 99, &keys(99, 2_000), 0)
            .await
            .is_err()
    );
    // flushed_seq never moves backwards.
    let w2 = segment::write("p", vec![version(state(7, 1, true))], Ts(0)).unwrap();
    assert!(
        m.commit_flush(std::slice::from_ref(&w2.meta), &w2.stats, 10, &keys(10, 2_000), 0)
            .await
            .is_err()
    );
    drop(m);
    let m = Metastore::open(&url).await.unwrap();
    assert_eq!(m.flushed_seq().await.unwrap(), 42);
    let segs = m.active_segments().await.unwrap();
    assert_eq!(segs, vec![w.meta.clone()]);
    let stats = m.field_stats().await.unwrap();
    assert_eq!(stats.len(), w.stats.len());
    assert_eq!(m.dedup_keys().await.unwrap(), vec![keys(42, 1_000)]);
}

#[tokio::test]
async fn dedup_keys_are_pruned_by_age_but_the_newest_flush_keeps_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("meta.sqlite").display());
    let m = Metastore::open(&url).await.unwrap();
    for (seq, at) in [(10, 100), (20, 200), (30, 300)] {
        let w = segment::write("p", vec![version(state(seq as u128, 1, true))], Ts(0)).unwrap();
        m.commit_flush(std::slice::from_ref(&w.meta), &w.stats, seq, &keys(seq, at), 150)
            .await
            .unwrap();
    }
    assert_eq!(m.dedup_keys().await.unwrap(), vec![keys(20, 200), keys(30, 300)]);
    // Even past the window, the newest flush's keys stay (start-up uses them to rebuild the multi-version set).
    let w = segment::write("p", vec![version(state(40, 1, true))], Ts(0)).unwrap();
    m.commit_flush(std::slice::from_ref(&w.meta), &w.stats, 40, &keys(40, 400), 1_000)
        .await
        .unwrap();
    assert_eq!(m.dedup_keys().await.unwrap(), vec![keys(40, 400)]);
    // Saving again for the same flush replaces the set.
    m.save_dedup_keys(&keys(40, 500)).await.unwrap();
    assert_eq!(m.dedup_keys().await.unwrap(), vec![keys(40, 500)]);
}

#[tokio::test]
async fn a_catalog_from_before_dedup_keys_is_upgraded_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("meta.sqlite").display());
    drop(Metastore::open(&url).await.unwrap());
    let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
    sqlx::query("DROP TABLE dedup_keys").execute(&pool).await.unwrap();
    pool.close().await;
    let m = Metastore::open(&url).await.unwrap();
    assert!(m.dedup_keys().await.unwrap().is_empty());
    m.save_dedup_keys(&keys(1, 1)).await.unwrap();
}

#[test]
fn passwords_are_redacted() {
    assert_eq!(
        smithling_storage::redact("error connecting to postgres://tdb:s3cret@db:5432/x failed"),
        "error connecting to postgres://tdb:***@db:5432/x failed"
    );
    assert_eq!(
        smithling_storage::redact("sqlite:///tmp/meta.sqlite"),
        "sqlite:///tmp/meta.sqlite"
    );
}

#[test]
fn tokens_survive_a_round_trip_without_the_payload() {
    let id = uid(77);
    let mut slot = None;
    let mut f = RunFields {
        id: Some(id),
        trace_id: Some(id),
        dotted_order: Some(dotted(5, id)),
        name: Some("llm".into()),
        run_type: Some("llm".into()),
        start_time: Ts::parse("2026-11-05T02:05:00Z"),
        ..Default::default()
    };
    f.outputs = Some(json!({"usage_metadata": {"input_tokens": 120, "output_tokens": 30, "total_tokens": 150}}));
    merge(
        &mut slot,
        &RunEvent {
            op: Op::Post,
            event_seq: 1,
            received_at: Ts(1),
            source: Source::Native,
            body_hash: 0,
            fields: f,
        },
    )
    .unwrap();
    let st = slot.unwrap();
    assert_eq!(st.run.total_tokens, Some(150));
    let w = segment::write("p", vec![version(st.clone())], Ts(0)).unwrap();
    let back = decode_core(&read_all(&w.core)[0]).unwrap().remove(0).state;
    assert!(back.run.outputs.is_none(), "payload isn't loaded from the core file");
    assert_eq!((back.run.input_tokens, back.run.total_tokens), (Some(120), Some(150)));

    // Merging with a later header-only version keeps them, and keeps the payload unloaded.
    let mut later = None;
    let p = RunFields {
        id: Some(id),
        trace_id: Some(id),
        dotted_order: Some(dotted(5, id)),
        end_time: Ts::parse("2026-11-05T02:06:00Z"),
        ..Default::default()
    };
    merge(
        &mut later,
        &RunEvent {
            op: Op::Patch,
            event_seq: 2,
            received_at: Ts(2),
            source: Source::Native,
            body_hash: 0,
            fields: p,
        },
    )
    .unwrap();
    let m = merge_versions(vec![back, later.unwrap()]).unwrap();
    assert_eq!(m.run.total_tokens, Some(150));
    assert_eq!(m.payload_sizes, st.payload_sizes);
}

#[test]
fn a_state_without_its_payload_is_never_rewritten() {
    let st = state(3, 1, true);
    let w = segment::write("p", vec![version(st)], Ts(0)).unwrap();
    let back = decode_core(&read_all(&w.core)[0]).unwrap().remove(0);
    let err = segment::write(
        "p",
        vec![Version {
            state: back.state,
            attrs: back.attrs,
            needs_merge: false,
        }],
        Ts(0),
    );
    assert!(err.is_err(), "writing would silently drop the payload");
}

#[test]
fn only_missing_or_undecodable_files_count_as_damaged() {
    use datafusion::parquet::errors::ParquetError;
    use smithling_storage::StorageError;
    use smithling_storage::segment::damage;
    let not_found = || object_store::Error::NotFound {
        path: "a".into(),
        source: "gone".into(),
    };
    let flaky = || object_store::Error::Generic {
        store: "S3",
        source: "503 Slow Down".into(),
    };
    assert_eq!(damage(&StorageError::Object(not_found())), Some("missing"));
    assert_eq!(damage(&StorageError::Object(flaky())), None);
    let pq = |e: ParquetError| StorageError::Parquet(e);
    assert_eq!(
        damage(&pq(ParquetError::General("bad footer".into()))),
        Some("unreadable")
    );
    assert_eq!(
        damage(&pq(ParquetError::External(Box::new(not_found())))),
        Some("missing")
    );
    assert_eq!(
        damage(&pq(ParquetError::External(Box::new(flaky())))),
        None,
        "a ranged read failing"
    );
    let io = std::io::Error::other("too many open files");
    assert_eq!(damage(&pq(ParquetError::External(Box::new(io)))), None);
    assert_eq!(damage(&StorageError::Format("no run_id".into())), Some("unreadable"));
    assert_eq!(damage(&StorageError::Metastore("down".into())), None);
}
