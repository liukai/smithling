//! Merge-on-read correctness (SC-09): with random runs, random late updates and random flush points, every
//! query returns exactly what a naive check returns — apply all events in order, then evaluate the filter.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::json;
use tracedb_ingest::{AcceptError, Incoming, Ingest, IngestConfig, Limits};
use tracedb_model::attrs::promote;
use tracedb_model::filter::{EvalStats, Row, eval, parse_filter};
use tracedb_model::{Op, RunEvent, RunFields, RunId, RunState, Source, Ts, merge};
use tracedb_query::{Query, QuerySpec};
use uuid::Uuid;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

fn uid(n: u64) -> Uuid {
    Uuid::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0000 | n as u128)
}

fn dotted_seg(minute: u64, id: Uuid) -> String {
    format!("20261105T{:02}{:02}00000000Z{id}", 2 + minute / 60, minute % 60)
}

/// The event stream of one scenario, in send order.
fn scenario(rng: &mut Rng) -> Vec<Incoming> {
    let mut posts = Vec::new();
    let mut late = Vec::new();
    let traces = 6 + rng.below(10);
    let mut n = 1;
    for t in 0..traces {
        let root = uid(n);
        n += 1;
        let minute = t * 3;
        let root_dotted = dotted_seg(minute, root);
        let project = if rng.chance(70) { "a" } else { "b" };
        let mk = |id: Uuid, dotted: String, parent: Option<Uuid>, name: &str, minute: u64| RunFields {
            id: Some(id),
            trace_id: Some(root),
            parent_run_id: parent,
            dotted_order: Some(dotted),
            project: Some(project.into()),
            name: Some(name.into()),
            run_type: Some(if parent.is_some() { "tool" } else { "chain" }.into()),
            start_time: Ts::parse(&format!("2026-11-05T{:02}:{:02}:00Z", 2 + minute / 60, minute % 60)),
            tags: Some(vec![format!("batch={}", if t % 2 == 0 { "x" } else { "y" })]),
            metadata: Some(json!({"v": format!("v{}", t % 3)}).as_object().unwrap().clone()),
            ..Default::default()
        };
        posts.push((Op::Post, mk(root, root_dotted.clone(), None, "root", minute)));
        let kids = 1 + rng.below(5);
        let mut kid_ids = vec![];
        for k in 0..kids {
            let id = uid(n);
            n += 1;
            let dotted = format!("{root_dotted}.{}", dotted_seg(minute + 1 + k, id));
            let name = ["fetch_url", "llm", "plan"][rng.below(3) as usize];
            posts.push((Op::Post, mk(id, dotted.clone(), Some(root), name, minute + 1 + k)));
            kid_ids.push((id, dotted));
        }
        // Ends: some right away, some late (after other traces), some never; some errors.
        let mut ends = vec![(root, root_dotted.clone())];
        ends.extend(kid_ids);
        for (id, dotted) in ends {
            if rng.chance(15) {
                continue;
            }
            let mut f = RunFields {
                id: Some(id),
                trace_id: Some(root),
                dotted_order: Some(dotted),
                end_time: Ts::parse(&format!(
                    "2026-11-05T{:02}:{:02}:30Z",
                    2 + (minute + 9) / 60,
                    (minute + 9) % 60
                )),
                outputs: Some(json!({"status_code": if rng.chance(30) { 429 } else { 200 }})),
                ..Default::default()
            };
            if rng.chance(25) {
                f.error = Some("HTTPError: 429 Too Many Requests".into());
            }
            if rng.chance(40) {
                late.push((Op::Patch, f));
            } else {
                posts.push((Op::Patch, f));
            }
        }
    }
    // Late updates arrive after everything else, in a shuffled order; some get a second update.
    for i in (1..late.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        late.swap(i, j);
    }
    let mut again = Vec::new();
    for (_, f) in late.iter().take(3) {
        let mut g = RunFields {
            id: f.id,
            trace_id: f.trace_id,
            dotted_order: f.dotted_order.clone(),
            ..Default::default()
        };
        g.tags = Some(vec!["retried".into()]);
        again.push((Op::Patch, g));
    }
    posts
        .into_iter()
        .chain(late)
        .chain(again)
        .enumerate()
        .map(|(i, (op, fields))| Incoming {
            op,
            source: Source::Native,
            body_hash: i as u64,
            fields,
        })
        .collect()
}

/// The naive answer: apply every event in send order, per run.
fn naive(events: &[Incoming]) -> HashMap<RunId, RunState> {
    let mut runs: HashMap<RunId, Option<RunState>> = HashMap::new();
    for (i, it) in events.iter().enumerate() {
        let ev = RunEvent {
            op: it.op,
            event_seq: i as u64 + 1,
            received_at: Ts(i as i64),
            source: it.source,
            body_hash: it.body_hash,
            fields: it.fields.clone(),
        };
        merge(runs.entry(it.fields.id.unwrap()).or_default(), &ev).unwrap();
    }
    runs.into_iter().map(|(k, v)| (k, v.unwrap())).collect()
}

struct Case {
    filter: Option<&'static str>,
    tree: Option<&'static str>,
    trace: Option<&'static str>,
    root: Option<bool>,
    project: Option<&'static str>,
}

const CASES: &[Case] = &[
    Case {
        filter: Some(r#"eq(status, "error")"#),
        tree: None,
        trace: None,
        root: None,
        project: None,
    },
    Case {
        filter: Some(r#"eq(status, "ok")"#),
        tree: Some(r#"eq(status, "error")"#),
        trace: None,
        root: Some(true),
        project: None,
    },
    Case {
        filter: Some(r#"and(has(tags, "batch=x"), eq(status, "running"))"#),
        tree: None,
        trace: None,
        root: None,
        project: Some("a"),
    },
    Case {
        filter: Some(r#"eq(outputs.status_code, 429)"#),
        tree: None,
        trace: None,
        root: None,
        project: None,
    },
    Case {
        filter: Some(r#"or(neq(error, null), has(tags, "retried"))"#),
        tree: None,
        trace: None,
        root: None,
        project: None,
    },
    Case {
        filter: Some(r#"not(eq(name, "llm"))"#),
        tree: None,
        trace: None,
        root: Some(false),
        project: None,
    },
    Case {
        filter: Some(r#"and(eq(metadata.v, "v1"), gt(latency, 60))"#),
        tree: None,
        trace: None,
        root: None,
        project: None,
    },
    Case {
        filter: Some(r#"eq(name, "fetch_url")"#),
        tree: None,
        trace: Some(r#"eq(status, "error")"#),
        root: None,
        project: None,
    },
    Case {
        filter: None,
        tree: None,
        trace: None,
        root: Some(true),
        project: Some("b"),
    },
    Case {
        filter: Some(r#"eq(end_time, null)"#),
        tree: None,
        trace: None,
        root: None,
        project: None,
    },
];

fn expected(runs: &HashMap<RunId, RunState>, c: &Case) -> BTreeSet<RunId> {
    let now = Ts::now();
    let m = |st: &RunState, f: &str| {
        let attrs = promote(&st.run);
        eval(
            &parse_filter(f).unwrap(),
            &Row {
                run: &st.run,
                attrs: &attrs,
            },
            now,
            &mut EvalStats::default(),
        )
    };
    let tree: Option<HashSet<_>> = c
        .tree
        .map(|f| runs.values().filter(|s| m(s, f)).map(|s| s.run.trace_id).collect());
    let trace: Option<HashSet<_>> = c.trace.map(|f| {
        runs.values()
            .filter(|s| s.run.is_root && m(s, f))
            .map(|s| s.run.trace_id)
            .collect()
    });
    runs.values()
        .filter(|s| c.project.is_none_or(|p| s.run.project == p))
        .filter(|s| c.root.is_none_or(|r| s.run.is_root == r))
        .filter(|s| c.filter.is_none_or(|f| m(s, f)))
        .filter(|s| tree.as_ref().is_none_or(|t| t.contains(&s.run.trace_id)))
        .filter(|s| trace.as_ref().is_none_or(|t| t.contains(&s.run.trace_id)))
        .map(|s| s.run.run_id)
        .collect()
}

async fn send_all(ingest: &Ingest, events: &[Incoming]) {
    for it in events {
        loop {
            match ingest.accept(vec![it.clone()]).await {
                Ok(r) => {
                    assert_eq!(r.accepted, 1, "{:?}", r.rejected);
                    break;
                }
                Err(AcceptError::Busy) => tokio::time::sleep(Duration::from_millis(5)).await,
                Err(e) => panic!("{e}"),
            }
        }
    }
}

async fn all_pages(q: &Query, mut spec: QuerySpec) -> Vec<RunId> {
    spec.limit = Some(7);
    let mut out = Vec::new();
    loop {
        let page = q.query(&spec).await.unwrap();
        out.extend(page.runs.iter().map(|r| r.header.run_id));
        match page.next_cursor {
            Some(c) => spec.cursor = Some(c),
            None => return out,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queries_match_the_naive_answer_whatever_was_flushed() {
    let (mut with_files, mut merged) = (0, 0);
    for seed in 1..=12u64 {
        let mut rng = Rng(seed * 7919);
        let events = scenario(&mut rng);
        let runs = naive(&events);
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = IngestConfig::new(dir.path());
        cfg.limits = Limits {
            // From "flush every few events" to "never flush": exercises every mix of files and memory.
            memtable_bytes: [1024, 2048, 4096, 16 * 1024, u64::MAX / 4][(seed % 5) as usize],
            dedup_window_us: 0,
            ..Limits::default()
        };
        let (ingest, _) = Ingest::open(cfg).unwrap();
        send_all(&ingest, &events).await;
        // Sometimes wait for the last flush, sometimes query while one may be running.
        if seed % 2 == 0 {
            let deadline = Instant::now() + Duration::from_secs(10);
            while ingest.health().frozen && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        let q = Query::new(ingest.clone());
        let segments = ingest.health().segments;
        if segments > 0 {
            with_files += 1;
        }
        merged += q.query(&QuerySpec::default()).await.unwrap().stats.rows_merged;
        for (i, c) in CASES.iter().enumerate() {
            let spec = QuerySpec {
                project: c.project.map(str::to_string),
                filter: c.filter.map(|f| parse_filter(f).unwrap()),
                tree_filter: c.tree.map(|f| parse_filter(f).unwrap()),
                trace_filter: c.trace.map(|f| parse_filter(f).unwrap()),
                is_root: c.root,
                ..QuerySpec::default()
            };
            let got = all_pages(&q, spec).await;
            let unique: BTreeSet<RunId> = got.iter().copied().collect();
            assert_eq!(unique.len(), got.len(), "seed {seed} case {i}: a run appeared twice");
            assert_eq!(unique, expected(&runs, c), "seed {seed} case {i} ({segments} segments)");
        }
        // Trace views and single runs agree with the naive state too.
        for st in runs.values().take(5) {
            let t = q.trace(st.run.trace_id).await.unwrap().unwrap();
            let want = runs.values().filter(|s| s.run.trace_id == st.run.trace_id).count();
            assert_eq!(t.runs.len(), want, "seed {seed}: trace size");
            let r = q
                .run(
                    st.run.run_id,
                    tracedb_ingest::Include {
                        inputs: true,
                        outputs: true,
                        events: true,
                    },
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                r.header.status, st.run.status,
                "seed {seed}: status of {}",
                st.run.run_id
            );
            assert_eq!(r.header.tags, st.run.tags, "seed {seed}: tags of {}", st.run.run_id);
            assert_eq!(r.outputs, st.run.outputs, "seed {seed}: outputs of {}", st.run.run_id);
        }
    }
    eprintln!("seeds with data files: {with_files}/12; versions merged: {merged}");
    assert!(with_files >= 6, "most seeds must read from data files");
    assert!(merged > 20, "runs split across files and memory must be merged");
}
