//! Compaction's job core (M4 plan U2): which flushed files a job takes, and that merging them gives each run
//! exactly the state merging all its versions gives.

use std::collections::HashMap;
use std::sync::Arc;

use proptest::prelude::*;
use serde_json::json;
use smithling_ingest::compact::{self, CompactConfig};
use smithling_ingest::{IngestConfig, Storage};
use smithling_model::attrs::promote;
use smithling_model::run::RunFields;
use smithling_model::{Op, RunEvent, RunState, Source, Ts, merge, merge_versions};
use smithling_storage::SegmentMeta;
use smithling_storage::segment::{self, Version as Row};
use uuid::Uuid;

const HOUR_US: i64 = 3_600_000_000;

fn meta(id: &str, created_at: i64, rows: u64, max_start_key: i64, tier: u8) -> Arc<SegmentMeta> {
    Arc::new(SegmentMeta {
        id: id.into(),
        project: "p".into(),
        core_path: format!("p/{id}.core.parquet"),
        payload_path: format!("p/{id}.payload.parquet"),
        min_start_key: max_start_key - 1,
        max_start_key,
        min_trace_id: Uuid::nil(),
        max_trace_id: Uuid::max(),
        min_seq: created_at as u64,
        max_seq: created_at as u64 + 1,
        row_count: rows,
        needs_merge_rows: 0,
        core_bytes: 1,
        payload_bytes: 1,
        created_at,
        fts_path: None,
        fts_bytes: 0,
        tier,
    })
}

#[test]
fn a_job_takes_whole_flushes_in_log_order_while_their_windows_are_closed() {
    let cfg = CompactConfig {
        window_us: HOUR_US,
        settle_us: HOUR_US / 4,
        max_rows: 100,
        ..CompactConfig::default()
    };
    // Flush 1 (two projects' files), flush 2, flush 3 still has a row in the open window, flush 4 is closed again
    // (only late updates) but comes after 3, so it waits.
    let now = 10 * HOUR_US;
    let closed = 8 * HOUR_US + 10;
    let segs = vec![
        meta("f2", 2, 10, closed, 0),
        meta("f1a", 1, 10, closed, 0),
        meta("f1b", 1, 10, closed - 5, 0),
        meta("f3", 3, 10, 9 * HOUR_US + 10, 0),
        meta("f4", 4, 10, closed, 0),
        meta("l1", 0, 10, closed, 1),
    ];
    let ids = |v: Vec<Arc<SegmentMeta>>| v.iter().map(|s| s.id.clone()).collect::<Vec<_>>();
    let none = |_: &str| false;
    assert_eq!(ids(compact::pick(&segs, &none, now, &cfg, false)), ["f1a", "f1b", "f2"]);
    // A window closes once its end plus the settle age has passed.
    assert_eq!(
        ids(compact::pick(&segs, &none, 9 * HOUR_US + HOUR_US / 4 - 1, &cfg, false)),
        Vec::<String>::new()
    );
    assert_eq!(
        ids(compact::pick(&segs, &none, 9 * HOUR_US + HOUR_US / 4, &cfg, false)),
        ["f1a", "f1b", "f2"]
    );
    // `force` treats every window as closed; L1 files are never inputs.
    assert_eq!(
        ids(compact::pick(&segs, &none, now, &cfg, true)),
        ["f1a", "f1b", "f2", "f3", "f4"]
    );
    // A damaged file stops the prefix before its flush.
    assert_eq!(
        ids(compact::pick(&segs, &|id| id == "f2", now, &cfg, true)),
        ["f1a", "f1b"]
    );
    // The size cap stops at a flush boundary, but a job always takes at least one flush.
    let small = CompactConfig {
        max_rows: 25,
        ..cfg.clone()
    };
    assert_eq!(ids(compact::pick(&segs, &none, now, &small, true)), ["f1a", "f1b"]);
    let tiny = CompactConfig {
        max_rows: 1,
        ..cfg.clone()
    };
    assert_eq!(ids(compact::pick(&segs, &none, now, &tiny, true)), ["f1a", "f1b"]);

    // Log order is seq order, not the wall clock: a flush stamped earlier (the clock stepped back) but holding
    // later events comes later.
    let mut skewed = segs.clone();
    let mut late = (*meta("f5", 0, 10, closed, 0)).clone();
    (late.min_seq, late.max_seq) = (100, 101);
    skewed.push(Arc::new(late));
    assert_eq!(
        ids(compact::pick(&skewed, &none, now, &cfg, true)),
        ["f1a", "f1b", "f2", "f3", "f4", "f5"]
    );
}

fn uid(n: u32) -> Uuid {
    Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0000 | n as u128)
}

/// Version `k` of run `n`: k = 0 is the start (named project unless `projectless`), later ones are updates.
fn version(n: u32, k: usize, seq: u64, start_minute: u32, projectless: bool) -> RunState {
    let id = uid(n);
    let hour = start_minute / 60;
    let minute = start_minute % 60;
    let start = format!("2026-11-05T{:02}:{minute:02}:00Z", 2 + hour);
    let dotted = format!("20261105T{:02}{minute:02}00000000Z{id}", 2 + hour);
    let post = k == 0;
    let f = RunFields {
        id: Some(id),
        trace_id: Some(id),
        dotted_order: Some(dotted),
        project: (post && !projectless).then(|| format!("proj-{}", n % 2)),
        name: post.then(|| format!("run-{n}")),
        run_type: post.then(|| "tool".to_string()),
        start_time: post.then(|| Ts::parse(&start).unwrap()),
        end_time: (k == 2).then(|| Ts::parse("2026-11-05T09:00:00Z").unwrap()),
        error: (k == 2 && n.is_multiple_of(3)).then(|| "429 Too Many Requests".to_string()),
        tags: (k == 1).then(|| vec![format!("tag-{n}")]),
        inputs: post.then(|| json!({"q": format!("question {n}")})),
        outputs: (k >= 1).then(|| json!({"step": k, "n": n})),
        ..Default::default()
    };
    let ev = RunEvent {
        op: if post { Op::Post } else { Op::Patch },
        event_seq: seq,
        received_at: Ts(seq as i64),
        source: Source::Native,
        body_hash: 0,
        fields: f,
    };
    let mut slot = None;
    merge(&mut slot, &ev).unwrap();
    slot.unwrap()
}

/// (run, number of versions 1..=3, flush of each later version after the first, start minute, project-less first)
fn runs() -> impl Strategy<Value = Vec<(u32, usize, Vec<usize>, u32, bool)>> {
    prop::collection::vec(
        (
            1usize..=3,
            prop::collection::vec(0usize..3, 2),
            0u32..180,
            any::<bool>(),
        ),
        1..40,
    )
    .prop_map(|v| {
        v.into_iter()
            .enumerate()
            .map(|(i, (k, gaps, minute, projectless))| (i as u32 + 1, k, gaps, minute, projectless))
            .collect()
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn merged_rows_equal_merging_every_version(spec in runs()) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(&IngestConfig::new(dir.path())).unwrap();
        // Spread versions over flushes: a run's later versions go to the same or a later flush.
        let mut flushes: Vec<Vec<RunState>> = vec![Vec::new(); 8];
        let mut want: HashMap<Uuid, Vec<RunState>> = HashMap::new();
        let mut seq = 0u64;
        let mut placed: Vec<(usize, u32, usize, u32, bool)> = Vec::new();
        for (n, k, gaps, minute, projectless) in &spec {
            let mut f = 0;
            for j in 0..*k {
                if j > 0 { f += gaps[j - 1]; }
                placed.push((f, *n, j, *minute, *projectless && j == 0));
            }
        }
        placed.sort_by_key(|p| (p.0, p.1, p.2));
        for (f, n, j, minute, projectless) in placed {
            seq += 1;
            let st = version(n, j, seq, minute, projectless);
            want.entry(uid(n)).or_default().push(st.clone());
            flushes[f].push(st);
        }
        let mut inputs = Vec::new();
        for (f, states) in flushes.into_iter().enumerate().filter(|(_, s)| !s.is_empty()) {
            let mut by_project: HashMap<String, Vec<Row>> = HashMap::new();
            for st in states {
                let project = if st.project_set { st.run.project.clone() } else { String::new() };
                by_project.entry(project).or_default().push(Row {
                    attrs: promote(&st.run),
                    needs_merge: st.created_by_patch(),
                    state: st,
                });
            }
            for (project, rows) in by_project {
                let w = segment::write(&project, rows, Ts(1_000 + f as i64)).unwrap();
                let (store, m) = (storage.store.clone(), w.meta.clone());
                let (core, payload) = (w.core.clone(), w.payload.clone());
                storage.block(async move {
                    store.put_new(&m.core_path, core).await?;
                    store.put_new(&m.payload_path, payload).await
                }).unwrap();
                inputs.push(Arc::new(w.meta));
            }
        }
        let cfg = CompactConfig { window_us: HOUR_US, ..CompactConfig::default() };
        let picked = compact::pick(&inputs, &|_| false, 0, &cfg, true);
        prop_assert_eq!(picked.len(), inputs.len());
        let built = compact::build(&storage, picked, &[], &cfg, Ts(9_999)).unwrap();
        prop_assert_eq!(built.rows_in as usize, want.values().map(Vec::len).sum::<usize>());

        let mut got: HashMap<Uuid, RunState> = HashMap::new();
        for w in &built.outputs {
            prop_assert_eq!(w.meta.tier, 1);
            prop_assert!(segment::is_compacted(&w.meta.core_path));
            let rows = segment::read_all(w.core.clone(), w.payload.clone()).unwrap();
            let windows: std::collections::HashSet<i64> = rows
                .iter()
                .map(|r| compact::window_of(segment::start_key(&r.state.run.dotted_order), HOUR_US))
                .collect();
            prop_assert_eq!(windows.len(), 1, "one window per file");
            for r in rows {
                prop_assert!(!r.needs_merge, "no earlier L1 file holds these runs");
                let project = if r.state.project_set { r.state.run.project.clone() } else { String::new() };
                prop_assert_eq!(&project, &w.meta.project, "filed under the merged run's project");
                prop_assert!(got.insert(r.state.run.run_id, r.state).is_none(), "one row per run");
            }
        }
        prop_assert_eq!(got.len(), want.len());
        for (id, versions) in want {
            prop_assert_eq!(&got[&id], &merge_versions(versions).unwrap(), "run {}", id);
        }
    }
}
