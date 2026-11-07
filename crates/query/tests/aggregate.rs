//! Aggregates == naive accumulation over fully merged runs (M3 T053): random runs, late updates, second starts
//! and random flush points; built-in keys, tags (unnested, totals apart), metadata with `(none)`, top N with
//! `(other)`, every metric; no run counted twice; each group's `where` selects exactly its runs.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::{Value as J, json};
use tracedb_ingest::{AcceptError, Incoming, Ingest, IngestConfig, Limits};
use tracedb_model::agg::{AggSpec, GroupKey, Metric, Pct, SumOf};
use tracedb_model::attrs::{Scalar, promote};
use tracedb_model::cost::{self, Cost, Prices};
use tracedb_model::filter::{EvalStats, Expr, Row, eval, parse_filter, status_name};
use tracedb_model::run::run_type_str;
use tracedb_model::{Op, RunEvent, RunFields, RunId, RunState, Source, Status, Ts, merge};
use tracedb_query::{Cells, Query, QuerySpec, TimeRange};
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

fn at(minute: u64, sec: u64) -> Option<Ts> {
    let m = minute + sec / 60;
    Ts::parse(&format!("2026-11-05T{:02}:{:02}:{:02}Z", 2 + m / 60, m % 60, sec % 60))
}

fn dotted_seg(minute: u64, id: Uuid) -> String {
    format!("20261105T{:02}{:02}00000000Z{id}", 2 + minute / 60, minute % 60)
}

const NAMES: [&str; 7] = ["fetch_url", "llm", "plan", "parse", "search", "rank", "embed"];
const TAGS: [&str; 5] = ["batch=x", "batch=y", "t1", "t2", "t3"];
const MODELS: [&str; 3] = ["gpt-4o-mini", "gpt-4o", "unknown-model"];

fn scenario(rng: &mut Rng) -> Vec<Incoming> {
    let (mut posts, mut late) = (Vec::new(), Vec::new());
    let traces = 6 + rng.below(10);
    let mut n = 1;
    for t in 0..traces {
        let root = uid(n);
        n += 1;
        let minute = t * 3;
        let root_dotted = dotted_seg(minute, root);
        // Some traces never name a project (filed as "default").
        let project = [Some("a"), Some("a"), Some("b"), None][rng.below(4) as usize];
        let mut runs = vec![(root, root_dotted.clone(), None, "root".to_string(), minute)];
        for k in 0..1 + rng.below(6) {
            let id = uid(n);
            n += 1;
            let dotted = format!("{root_dotted}.{}", dotted_seg(minute + 1 + k, id));
            let name = NAMES[rng.below(NAMES.len() as u64) as usize].to_string();
            runs.push((id, dotted, Some(root), name, minute + 1 + k));
        }
        for (id, dotted, parent, name, minute) in runs {
            let llm = name == "llm";
            let mut md = serde_json::Map::new();
            if rng.chance(70) {
                md.insert("v".into(), json!(format!("v{}", rng.below(4))));
            }
            if rng.chance(50) {
                md.insert(
                    "n".into(),
                    [json!(rng.below(4)), json!(2.5)][rng.below(2) as usize].clone(),
                );
            }
            if rng.chance(30) {
                md.insert("flag".into(), json!(rng.chance(50)));
            }
            if rng.chance(15) {
                md.insert("long".into(), json!("x".repeat(300)));
            }
            if llm && rng.chance(85) {
                md.insert("ls_model_name".into(), json!(MODELS[rng.below(3) as usize]));
            }
            let tags: Vec<String> = (0..rng.below(4))
                .map(|_| TAGS[rng.below(TAGS.len() as u64) as usize].to_string())
                .collect();
            let run_type = match (llm, parent.is_some(), rng.chance(10)) {
                (true, ..) => "llm",
                (_, _, true) => "custom_step",
                (_, true, _) => "tool",
                _ => "chain",
            };
            posts.push((
                Op::Post,
                RunFields {
                    id: Some(id),
                    trace_id: Some(root),
                    parent_run_id: parent,
                    dotted_order: Some(dotted.clone()),
                    project: project.map(str::to_string),
                    name: Some(name),
                    run_type: Some(run_type.into()),
                    start_time: at(minute, 0),
                    tags: (!tags.is_empty() || rng.chance(50)).then_some(tags),
                    metadata: Some(md),
                    ..Default::default()
                },
            ));
            if rng.chance(15) {
                continue;
            }
            let mut outputs = json!({"status_code": if rng.chance(30) { 429 } else { 200 }});
            if llm {
                let mut usage = json!({"input_tokens": rng.below(5_000), "output_tokens": rng.below(900)});
                if rng.chance(20) {
                    usage["total_cost"] = json!(0.0125);
                }
                outputs["usage_metadata"] = usage;
            }
            let mut f = RunFields {
                id: Some(id),
                trace_id: Some(root),
                dotted_order: Some(dotted),
                end_time: at(minute, 1 + rng.below(400)),
                outputs: Some(outputs),
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
    for i in (1..late.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        late.swap(i, j);
    }
    let mut again = Vec::new();
    for (i, (_, f)) in late.iter().take(4).enumerate() {
        let mut g = RunFields {
            id: f.id,
            trace_id: f.trace_id,
            dotted_order: f.dotted_order.clone(),
            tags: Some(vec!["retried".into()]),
            ..Default::default()
        };
        if i % 2 == 0 {
            // A second start for a run whose first one may already be in a file (D-025).
            g.name = Some("restarted".into());
            g.run_type = Some("tool".into());
            g.start_time = Ts::parse("2026-11-05T02:00:00Z");
            again.push((Op::Post, g));
        } else {
            again.push((Op::Patch, g));
        }
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

struct Case {
    from: Option<&'static str>,
    to: Option<&'static str>,
    filter: Option<&'static str>,
    tree: Option<&'static str>,
    trace: Option<&'static str>,
    root: Option<bool>,
    project: Option<&'static str>,
}

const fn c(filter: Option<&'static str>) -> Case {
    Case {
        from: None,
        to: None,
        filter,
        tree: None,
        trace: None,
        root: None,
        project: None,
    }
}

const CASES: &[Case] = &[
    c(None),
    // Exact pushdowns: accumulated straight from the columns.
    c(Some(r#"has(tags, "batch=x")"#)),
    c(Some(r#"and(eq(name, "fetch_url"), eq(status, "error"))"#)),
    c(Some("gt(latency, 100)")),
    // Not exact: decoded and evaluated.
    c(Some(r#"eq(metadata.v, "v1")"#)),
    c(Some(r#"or(neq(error, null), has(tags, "retried"))"#)),
    c(Some(r#"eq(run_type, "llm")"#)),
    Case {
        project: Some("a"),
        root: Some(false),
        ..c(None)
    },
    Case {
        from: Some("2026-11-05T02:20:00Z"),
        ..c(Some(r#"eq(status, "ok")"#))
    },
    Case {
        tree: Some(r#"eq(status, "error")"#),
        ..c(None)
    },
    Case {
        trace: Some(r#"has(tags, "batch=y")"#),
        project: Some("b"),
        ..c(Some(r#"neq(name, "plan")"#))
    },
    Case {
        project: Some("default"),
        ..c(None)
    },
    Case {
        from: Some("2026-11-05T02:10:00Z"),
        to: Some("2026-11-05T02:30:00Z"),
        ..c(Some("exists(name)"))
    },
];

fn key_sets() -> Vec<Vec<GroupKey>> {
    let m = |k: &str| GroupKey::Metadata(k.into());
    vec![
        vec![],
        vec![GroupKey::Name],
        vec![GroupKey::Tag],
        vec![m("v")],
        vec![m("n")],
        vec![m("long")],
        vec![m("flag")],
        vec![GroupKey::Status],
        vec![GroupKey::RunType],
        vec![GroupKey::Model],
        vec![GroupKey::Name, GroupKey::Tag],
        vec![m("v"), GroupKey::Status],
        vec![GroupKey::Tag, m("n")],
        vec![GroupKey::Model, GroupKey::Name],
    ]
}

fn metrics() -> Vec<Metric> {
    let mut m = vec![Metric::Count, Metric::ErrorRate];
    m.extend(Pct::ALL.map(Metric::Latency));
    m.extend(SumOf::ALL.map(Metric::Sum));
    m
}

/// Independent of the engine's accumulators: counted straight from the merged runs.
fn naive_cells(runs: &[&RunState], prices: &Prices) -> Cells {
    let finished: Vec<&&RunState> = runs.iter().filter(|r| r.run.status != Status::Running).collect();
    let errors = runs.iter().filter(|r| r.run.status == Status::Error).count() as u64;
    let mut lat: Vec<i64> = finished
        .iter()
        .filter_map(|r| r.run.latency_ms.map(|v| v as i64))
        .collect();
    lat.sort();
    let pct = |p: usize| (!lat.is_empty()).then(|| lat[(lat.len() * p).div_ceil(100) - 1]);
    let sum = |f: fn(&RunState) -> Option<u64>| {
        let vs: Vec<u64> = runs.iter().filter_map(|r| f(r)).collect();
        (!vs.is_empty()).then(|| vs.iter().sum())
    };
    let (mut pico, mut priced, mut no_price) = (0i64, 0, 0);
    for r in runs {
        match cost::of(&r.run, prices) {
            Some(Cost::Reported(c) | Cost::Computed(c)) => {
                pico += c;
                priced += 1;
            }
            Some(Cost::NoPrice) => no_price += 1,
            None => {}
        }
    }
    Cells {
        runs: runs.len() as u64,
        finished: finished.len() as u64,
        running: runs.len() as u64 - finished.len() as u64,
        errors,
        error_rate: (!finished.is_empty()).then(|| errors as f64 / finished.len() as f64),
        p50: pct(50),
        p95: pct(95),
        p99: pct(99),
        n_latency: lat.len() as u64,
        input_tokens: sum(|r| r.run.input_tokens),
        output_tokens: sum(|r| r.run.output_tokens),
        total_tokens: sum(|r| r.run.total_tokens),
        cost_pico: (priced > 0).then_some(pico),
        no_price_runs: no_price,
        ..Cells::default()
    }
}

/// A group-key value as the response writes it; `None` for a value that can't be a group (unindexed).
fn naive_keys(k: &GroupKey, st: &RunState) -> Vec<Option<J>> {
    let s = |v: Option<&str>| Some(v.map_or(J::from("(none)"), J::from));
    let r = &st.run;
    match k {
        GroupKey::Name => vec![s(r.name.as_deref())],
        GroupKey::Model => vec![s(r.model.as_deref())],
        GroupKey::Status => vec![s(Some(status_name(r.status)))],
        GroupKey::RunType => vec![s(Some(r.run_type_raw.as_deref().unwrap_or(run_type_str(r.run_type))))],
        GroupKey::Tag => {
            let t: BTreeSet<&String> = r.tags.iter().collect();
            if t.is_empty() {
                vec![s(None)]
            } else {
                t.into_iter().map(|t| Some(J::from(t.as_str()))).collect()
            }
        }
        GroupKey::Metadata(m) => vec![match promote(r).get(&format!("metadata.{m}")) {
            None => s(None),
            Some(Scalar::Str(v)) => Some(J::from(v.as_str())),
            Some(Scalar::Num(n)) => Some(serde_json::Number::from_f64(*n).unwrap().into()),
            Some(Scalar::Bool(b)) => Some(J::from(*b)),
            Some(Scalar::Unindexed) => None,
        }],
        GroupKey::FeedbackKey => unreachable!(),
    }
}

fn groups_of(keys: &[GroupKey], st: &RunState) -> Vec<Vec<Option<J>>> {
    let mut out = vec![vec![]];
    for k in keys {
        out = out
            .into_iter()
            .flat_map(|t| {
                naive_keys(k, st).into_iter().map(move |v| {
                    let mut t = t.clone();
                    t.push(v);
                    t
                })
            })
            .collect();
    }
    out
}

fn ks(k: &[Option<J>]) -> String {
    serde_json::to_string(k).unwrap()
}

fn ev(e: &Expr, st: &RunState) -> bool {
    let attrs = promote(&st.run);
    eval(
        e,
        &Row {
            run: &st.run,
            attrs: &attrs,
        },
        Ts::now(),
        &mut EvalStats::default(),
    )
}

fn matching<'a>(runs: &'a HashMap<RunId, RunState>, c: &Case) -> Vec<&'a RunState> {
    let m = |st: &RunState, f: &str| ev(&parse_filter(f).unwrap(), st);
    let in_project = |s: &&RunState| c.project.is_none_or(|p| s.run.project == p);
    let tree: Option<HashSet<_>> = c.tree.map(|f| {
        runs.values()
            .filter(in_project)
            .filter(|s| m(s, f))
            .map(|s| s.run.trace_id)
            .collect()
    });
    let trace: Option<HashSet<_>> = c.trace.map(|f| {
        runs.values()
            .filter(in_project)
            .filter(|s| s.run.is_root && m(s, f))
            .map(|s| s.run.trace_id)
            .collect()
    });
    let (from, to) = (
        c.from.map(|f| Ts::parse(f).unwrap().0),
        c.to.map(|t| Ts::parse(t).unwrap().0),
    );
    let key = |s: &RunState| tracedb_storage::segment::start_key(&s.run.dotted_order);
    runs.values()
        .filter(|s| from.is_none_or(|f| key(s) >= f) && to.is_none_or(|t| key(s) <= t))
        .filter(in_project)
        .filter(|s| c.root.is_none_or(|r| s.run.is_root == r))
        .filter(|s| c.filter.is_none_or(|f| m(s, f)))
        .filter(|s| tree.as_ref().is_none_or(|t| t.contains(&s.run.trace_id)))
        .filter(|s| trace.as_ref().is_none_or(|t| t.contains(&s.run.trace_id)))
        .collect()
}

fn spec_of(c: &Case) -> QuerySpec {
    QuerySpec {
        time: TimeRange {
            from: c.from.map(|f| tracedb_model::filter::TimeAt::parse(f).unwrap()),
            to: c.to.map(|t| tracedb_model::filter::TimeAt::parse(t).unwrap()),
        },
        project: c.project.map(str::to_string),
        filter: c.filter.map(|f| parse_filter(f).unwrap()),
        tree_filter: c.tree.map(|f| parse_filter(f).unwrap()),
        trace_filter: c.trace.map(|f| parse_filter(f).unwrap()),
        is_root: c.root,
        ..QuerySpec::default()
    }
}

/// Runs the query with `where` added to its filter and returns how many runs match.
async fn count_where(q: &Query, spec: &QuerySpec, w: &str) -> usize {
    let w = parse_filter(w).unwrap();
    let filter = Some(match &spec.filter {
        Some(f) => Expr::And(vec![f.clone(), w]),
        None => w,
    });
    let spec = QuerySpec {
        filter,
        count: true,
        limit: Some(1),
        ..spec.clone()
    };
    q.query(&spec).await.unwrap().stats.matched
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aggregates_match_naive_accumulation_whatever_was_flushed() {
    let prices = Prices::embedded();
    let (mut with_files, mut clean, mut dirty, mut cut_seen, mut where_checked) = (0, 0, 0, 0, 0);
    for seed in 1..=12u64 {
        let mut rng = Rng(seed * 7919);
        let events = scenario(&mut rng);
        let runs = naive(&events);
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = IngestConfig::new(dir.path());
        cfg.limits = Limits {
            memtable_bytes: [1024, 2048, 4096, 16 * 1024, u64::MAX / 4][(seed % 5) as usize],
            dedup_window_us: 0,
            ..Limits::default()
        };
        let (ingest, _) = Ingest::open(cfg).unwrap();
        send_all(&ingest, &events).await;
        if seed % 2 == 0 {
            let deadline = Instant::now() + Duration::from_secs(10);
            while ingest.health().frozen && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        if ingest.health().segments > 0 {
            with_files += 1;
        }
        let q = Query::new(ingest.clone());
        let keysets = key_sets();
        for (ci, case) in CASES.iter().enumerate() {
            let spec = spec_of(case);
            let want = matching(&runs, case);
            for _ in 0..3 {
                let keys = keysets[rng.below(keysets.len() as u64) as usize].clone();
                let top = 1 + rng.below(4) as u32;
                let agg = AggSpec {
                    group_by: keys.clone(),
                    metrics: metrics(),
                    top,
                    ..AggSpec::default()
                };
                let label = format!("seed {seed} case {ci} by {:?} top {top}", keys);
                let got = q.aggregate(&spec, &agg, &prices).await.unwrap();
                clean += got.stats.clean_rows;
                dirty += got.stats.dirty_runs;
                assert_eq!(got.totals, naive_cells(&want, &prices), "{label}: totals");
                assert_eq!(
                    (got.stats.clean_rows + got.stats.dirty_runs) as u64,
                    got.totals.runs,
                    "{label}: each run on one side"
                );
                assert_eq!(got.overlap, keys.contains(&GroupKey::Tag), "{label}");
                if keys.is_empty() {
                    assert!(got.groups.is_empty() && !got.cut, "{label}");
                    continue;
                }
                // group → members
                let mut naive_groups: BTreeMap<String, (Vec<Option<J>>, Vec<&RunState>)> = BTreeMap::new();
                for st in &want {
                    for g in groups_of(&keys, st) {
                        naive_groups.entry(ks(&g)).or_insert_with(|| (g, vec![])).1.push(st);
                    }
                }
                let single = keys.len() == 1;
                let none_key = vec![Some(J::from("(none)"))];
                let rankable = |g: &Vec<Option<J>>| g.iter().all(Option::is_some) && !(single && *g == none_key);
                let other_key = vec![J::from("(other)"); keys.len()];
                let shown: Vec<&tracedb_query::Group> = got.groups.iter().filter(|g| g.key != other_key).collect();
                let mut shown_keys = HashSet::new();
                let mut least = u64::MAX;
                for g in &shown {
                    let k: Vec<Option<J>> = g.key.iter().cloned().map(Some).collect();
                    let members = &naive_groups
                        .get(&ks(&k))
                        .unwrap_or_else(|| panic!("{label}: no group {k:?}"))
                        .1;
                    assert_eq!(g.cells, naive_cells(members, &prices), "{label}: group {k:?}");
                    if k != none_key || !single {
                        least = least.min(g.cells.runs);
                    }
                    let w = g
                        .where_
                        .as_deref()
                        .unwrap_or_else(|| panic!("{label}: {k:?} has no where"));
                    let we = parse_filter(w).unwrap();
                    let sel: BTreeSet<RunId> = want.iter().filter(|s| ev(&we, s)).map(|s| s.run.run_id).collect();
                    let mem: BTreeSet<RunId> = members.iter().map(|s| s.run.run_id).collect();
                    assert_eq!(sel, mem, "{label}: where {w}");
                    assert!(rankable(&k) || (single && k == none_key), "{label}: {k:?} shown");
                    shown_keys.insert(ks(&k));
                }
                let ranked = naive_groups.values().filter(|(g, _)| rankable(g)).count();
                let shown_ranked = shown
                    .iter()
                    .filter(|g| g.key.iter().all(|v| v != "(none)") || !single)
                    .count();
                assert_eq!(shown_ranked, ranked.min(top as usize), "{label}: top N");
                for (k, (g, members)) in &naive_groups {
                    if rankable(g) && !shown_keys.contains(k) {
                        assert!(members.len() as u64 <= least, "{label}: {k:?} outranks a shown group");
                    }
                }
                let mut other: Vec<&RunState> = want
                    .iter()
                    .copied()
                    .filter(|st| groups_of(&keys, st).iter().any(|g| !shown_keys.contains(&ks(g))))
                    .collect();
                other.sort_by_key(|s| s.run.run_id);
                other.dedup_by_key(|s| s.run.run_id);
                let got_other = got.groups.iter().find(|g| g.key == other_key);
                assert_eq!(got.cut, !other.is_empty(), "{label}: cut");
                if let Some(o) = got_other {
                    cut_seen += 1;
                    assert_eq!(o.cells, naive_cells(&other, &prices), "{label}: (other)");
                    match &o.where_ {
                        Some(w) => {
                            let we = parse_filter(w).unwrap();
                            let sel: BTreeSet<RunId> =
                                want.iter().filter(|s| ev(&we, s)).map(|s| s.run.run_id).collect();
                            let mem: BTreeSet<RunId> = other.iter().map(|s| s.run.run_id).collect();
                            assert_eq!(sel, mem, "{label}: (other) where {w}");
                        }
                        None => assert!(keys.contains(&GroupKey::Tag), "{label}: (other) has no where"),
                    }
                } else {
                    assert!(other.is_empty(), "{label}: (other) missing");
                }
                // Click-through through the query path itself.
                if let Some(g) = got.groups.iter().find(|g| g.where_.is_some()) {
                    let n = count_where(&q, &spec, g.where_.as_deref().unwrap()).await;
                    assert_eq!(n as u64, g.cells.runs, "{label}: query with where {:?}", g.where_);
                    where_checked += 1;
                }
            }
        }
        // Past the value cap, percentiles are approximate and every count stays exact.
        let capped = Query::new(ingest.clone()).with_value_cap(3);
        let agg = AggSpec {
            group_by: vec![GroupKey::Name],
            metrics: metrics(),
            ..AggSpec::default()
        };
        let exact = q.aggregate(&QuerySpec::default(), &agg, &prices).await.unwrap();
        let approx = capped.aggregate(&QuerySpec::default(), &agg, &prices).await.unwrap();
        assert!(approx.totals.approximate && !exact.totals.approximate, "seed {seed}");
        assert_eq!(
            (approx.totals.runs, approx.totals.errors, approx.totals.n_latency),
            (exact.totals.runs, exact.totals.errors, exact.totals.n_latency)
        );
        for (e, x) in [
            (exact.totals.p50, approx.totals.p50),
            (exact.totals.p99, approx.totals.p99),
        ] {
            let (e, x) = (e.unwrap() as f64, x.unwrap() as f64);
            assert!((x - e).abs() / e < 0.05, "seed {seed}: histogram {x} vs exact {e}");
        }
        // Only some traces (live views).
        let some: HashSet<_> = runs
            .values()
            .filter(|s| s.run.is_root)
            .map(|s| s.run.trace_id)
            .take(3)
            .collect();
        let spec = QuerySpec {
            traces: Some(some.clone()),
            ..QuerySpec::default()
        };
        let want: Vec<&RunState> = runs.values().filter(|s| some.contains(&s.run.trace_id)).collect();
        let got = q.aggregate(&spec, &agg, &prices).await.unwrap();
        assert_eq!(got.totals, naive_cells(&want, &prices), "seed {seed}: traces");
    }
    eprintln!(
        "seeds with files {with_files}/12; clean rows {clean}; dirty runs {dirty}; (other) rows {cut_seen}; where via query {where_checked}"
    );
    assert!(with_files >= 6, "most seeds must read from data files");
    assert!(clean > 500 && dirty > 500, "both sides must be exercised");
    assert!(cut_seen > 50 && where_checked > 200);
}
