//! Text search is exact (M3-FR-03, design §4.2): with random runs and text, late updates that replace outputs,
//! random flush points and some files without a text index, `search` through the index returns exactly what a
//! naive phrase check over the fully merged runs returns, in every slot and under `not`/`or`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use smithling_ingest::{AcceptError, Incoming, Ingest, IngestConfig, Limits};
use smithling_model::agg::{AggSpec, GroupKey, Metric};
use smithling_model::attrs::promote;
use smithling_model::cost::Prices;
use smithling_model::filter::{EvalStats, Row, eval, parse_filter};
use smithling_model::{Op, RunEvent, RunFields, RunId, RunState, Source, Ts, merge};
use smithling_query::{Query, QuerySpec, QueryWarning};
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

const WORDS: &[&str] = &[
    "429", "Too", "many", "REQUESTS", "rate", "limit", "timeout", "ok", "Café", "限", "流", "😀", "the",
];

/// A few words, often a planted phrase, sometimes the same phrase split across two leaves.
fn sentence(rng: &mut Rng) -> String {
    let mut s: Vec<&str> = (0..1 + rng.below(5))
        .map(|_| WORDS[rng.below(WORDS.len() as u64) as usize])
        .collect();
    match rng.below(5) {
        0 => s.extend(["429", "Too", "Many", "Requests"]),
        1 => s.extend(["rate", "limit"]),
        2 => s.extend(["限", "流"]),
        _ => {}
    }
    s.join(if rng.chance(50) { " " } else { ", " })
}

fn text(rng: &mut Rng) -> Value {
    match rng.below(4) {
        0 => {
            let code = [200, 429][rng.below(2) as usize];
            json!({"body": sentence(rng), "status_code": code})
        }
        1 => json!({"messages": [{"content": sentence(rng)}, {"content": sentence(rng)}]}),
        2 => json!({"a": "429 Too", "b": "Many Requests", "q": sentence(rng)}),
        _ => json!({"q": sentence(rng)}),
    }
}

fn scenario(rng: &mut Rng) -> Vec<Incoming> {
    let mut posts = Vec::new();
    let mut late = Vec::new();
    let mut n = 1;
    for t in 0..6 + rng.below(10) {
        let root = uid(n);
        n += 1;
        let minute = t * 3;
        let root_dotted = dotted_seg(minute, root);
        let project = if rng.chance(70) { "a" } else { "b" };
        let mut runs = vec![(root, root_dotted.clone(), None, "root", minute)];
        for k in 0..1 + rng.below(5) {
            let id = uid(n);
            n += 1;
            let name = ["fetch_url", "llm", "plan"][rng.below(3) as usize];
            runs.push((
                id,
                format!("{root_dotted}.{}", dotted_seg(minute + 1 + k, id)),
                Some(root),
                name,
                minute + 1 + k,
            ));
        }
        for (id, dotted, parent, name, minute) in runs {
            let mut f = RunFields {
                id: Some(id),
                trace_id: Some(root),
                parent_run_id: parent,
                dotted_order: Some(dotted.clone()),
                project: Some(project.into()),
                name: Some(name.into()),
                run_type: Some(if parent.is_some() { "tool" } else { "chain" }.into()),
                start_time: Ts::parse(&format!("2026-11-05T{:02}:{:02}:00Z", 2 + minute / 60, minute % 60)),
                inputs: Some(text(rng)),
                ..Default::default()
            };
            if rng.chance(30) {
                f.outputs = Some(text(rng));
            }
            posts.push((Op::Post, f));
            if rng.chance(15) {
                continue;
            }
            let mut end = RunFields {
                id: Some(id),
                trace_id: Some(root),
                dotted_order: Some(dotted),
                end_time: Ts::parse("2026-11-05T03:00:00Z"),
                outputs: Some(text(rng)),
                ..Default::default()
            };
            if rng.chance(25) {
                end.error = Some(if rng.chance(50) {
                    "HTTPError: 429 Too Many Requests".into()
                } else {
                    sentence(rng)
                });
            }
            if rng.chance(40) {
                late.push((Op::Patch, end));
            } else {
                posts.push((Op::Patch, end));
            }
        }
    }
    for i in (1..late.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        late.swap(i, j);
    }
    // Some late updates replace outputs again (the old text must stop matching) or the error.
    let mut again = Vec::new();
    for (_, f) in late.iter().take(6) {
        again.push((
            Op::Patch,
            RunFields {
                id: f.id,
                trace_id: f.trace_id,
                dotted_order: f.dotted_order.clone(),
                outputs: Some(json!({"body": "replaced"})),
                error: rng.chance(50).then(|| "ReadTimeout: rate limited".to_string()),
                ..Default::default()
            },
        ));
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

/// (filter, tree_filter, trace_filter, project)
type Case = (
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
);

const CASES: &[Case] = &[
    (Some(r#"search("429 Too Many Requests")"#), None, None, None),
    (Some(r#"search(outputs, "too many")"#), None, None, None),
    (Some(r#"search(inputs, "rate limit")"#), None, None, Some("a")),
    (Some(r#"search(error, "429")"#), None, None, None),
    (Some(r#"search("限流")"#), None, None, None),
    (Some(r#"search("CAFÉ")"#), None, None, None),
    (Some(r#"search("replaced")"#), None, None, None),
    // Common words: most files skip the index for them.
    (Some(r#"search("the")"#), None, None, None),
    (Some(r#"search(outputs, "429")"#), None, None, None),
    (
        Some(r#"and(search("rate limit"), eq(name, "fetch_url"))"#),
        None,
        None,
        None,
    ),
    (
        Some(r#"and(eq(status, "error"), search(outputs, "requests"))"#),
        None,
        None,
        None,
    ),
    (Some(r#"not(search("429 too many requests"))"#), None, None, None),
    (
        Some(r#"or(eq(name, "plan"), search(error, "timeout"))"#),
        None,
        None,
        None,
    ),
    (
        Some(r#"and(search("too"), not(search("many")))"#),
        None,
        None,
        Some("b"),
    ),
    (None, Some(r#"search(error, "429 Too Many Requests")"#), None, None),
    (Some(r#"eq(name, "llm")"#), Some(r#"search("rate limit")"#), None, None),
    (Some(r#"search("limit")"#), None, Some(r#"search(inputs, "429")"#), None),
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
    let tree: Option<HashSet<_>> =
        c.1.map(|f| runs.values().filter(|s| m(s, f)).map(|s| s.run.trace_id).collect());
    let trace: Option<HashSet<_>> = c.2.map(|f| {
        runs.values()
            .filter(|s| s.run.is_root && m(s, f))
            .map(|s| s.run.trace_id)
            .collect()
    });
    runs.values()
        .filter(|s| c.3.is_none_or(|p| s.run.project == p))
        .filter(|s| c.0.is_none_or(|f| m(s, f)))
        .filter(|s| tree.as_ref().is_none_or(|t| t.contains(&s.run.trace_id)))
        .filter(|s| trace.as_ref().is_none_or(|t| t.contains(&s.run.trace_id)))
        .map(|s| s.run.run_id)
        .collect()
}

fn spec(c: &Case) -> QuerySpec {
    QuerySpec {
        filter: c.0.map(|f| parse_filter(f).unwrap()),
        tree_filter: c.1.map(|f| parse_filter(f).unwrap()),
        trace_filter: c.2.map(|f| parse_filter(f).unwrap()),
        project: c.3.map(str::to_string),
        ..QuerySpec::default()
    }
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

/// Every page in order; checks each run appears once and a search's runs carry a match.
async fn all_pages(q: &Query, mut spec: QuerySpec, label: &str) -> BTreeSet<RunId> {
    spec.limit = Some(8);
    let mut out = Vec::new();
    let mut keys = Vec::new();
    let positive = spec.filter.as_ref().is_some_and(|f| {
        let t = smithling_model::filter::print(f);
        t.starts_with("search") || t.starts_with("and(search")
    });
    loop {
        let page = q.query(&spec).await.unwrap();
        for r in &page.runs {
            if positive {
                let m = r
                    .r#match
                    .as_ref()
                    .unwrap_or_else(|| panic!("{label}: no match on {}", r.header.run_id));
                assert!(m.start < m.end && m.end <= m.excerpt.chars().count(), "{label}: {m:?}");
            }
        }
        out.extend(page.runs.iter().map(|r| r.header.run_id));
        keys.extend(page.runs.iter().map(|r| {
            (
                -smithling_storage::segment::start_key(&r.header.dotted_order),
                r.header.run_id,
            )
        }));
        match page.next_cursor {
            Some(c) => spec.cursor = Some(c),
            None => break,
        }
    }
    assert!(keys.is_sorted(), "{label}: pages out of order");
    let unique: BTreeSet<RunId> = out.iter().copied().collect();
    assert_eq!(unique.len(), out.len(), "{label}: a run appeared twice");
    unique
}

fn config(dir: &Path, memtable_bytes: u64) -> IngestConfig {
    let mut cfg = IngestConfig::new(dir);
    cfg.limits = Limits {
        memtable_bytes,
        dedup_window_us: 0,
        ..Limits::default()
    };
    cfg
}

/// Turns every other data file into an M2 file: no sidecar on disk, no `fts_path` in the catalog.
fn strip_every_other_index(dir: &Path) -> Vec<String> {
    let url = format!("sqlite://{}", dir.join("meta.sqlite").display());
    let stems: Vec<(String, String)> = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
                let rows: Vec<(String, String)> =
                    sqlx::query_as("SELECT id, fts_path FROM segments WHERE fts_path IS NOT NULL ORDER BY id")
                        .fetch_all(&pool)
                        .await
                        .unwrap();
                let strip: Vec<(String, String)> = rows.into_iter().step_by(2).collect();
                for (id, _) in &strip {
                    sqlx::query("UPDATE segments SET fts_path = NULL, fts_bytes = NULL WHERE id = ?")
                        .bind(id)
                        .execute(&pool)
                        .await
                        .unwrap();
                }
                pool.close().await;
                strip
            })
    })
    .join()
    .unwrap();
    for (_, stem) in &stems {
        for f in [format!("{stem}.dict.parquet"), format!("{stem}.post")] {
            std::fs::remove_file(dir.join("data").join(&f))
                .or_else(|_| std::fs::remove_file(dir.join(&f)))
                .unwrap_or_else(|e| panic!("{f}: {e}"));
        }
    }
    stems.into_iter().map(|(id, _)| id).collect()
}

async fn check_all(q: &Query, scan: &Query, runs: &HashMap<RunId, RunState>, label: &str) {
    for (i, c) in CASES.iter().enumerate() {
        let want = expected(runs, c);
        let got = all_pages(q, spec(c), &format!("{label} case {i}")).await;
        assert_eq!(got, want, "{label} case {i} {c:?}: index");
        let got = all_pages(scan, spec(c), &format!("{label} case {i} scan")).await;
        assert_eq!(got, want, "{label} case {i} {c:?}: scan");
        let counted = q
            .query(&QuerySpec {
                count: true,
                limit: Some(1),
                ..spec(c)
            })
            .await
            .unwrap();
        assert_eq!(counted.stats.matched, want.len(), "{label} case {i}: count");
        assert!(counted.stats.complete, "{label} case {i}");
        // As an aggregate's filter too.
        let agg = AggSpec {
            group_by: vec![GroupKey::Name],
            metrics: vec![Metric::Count],
            ..AggSpec::default()
        };
        let got = q.aggregate(&spec(c), &agg, &Prices::embedded()).await.unwrap();
        assert_eq!(got.totals.runs, want.len() as u64, "{label} case {i}: aggregate");
        let mut by_name: BTreeMap<String, u64> = BTreeMap::new();
        for id in &want {
            *by_name
                .entry(runs[id].run.name.clone().unwrap_or_default())
                .or_default() += 1;
        }
        let groups: BTreeMap<String, u64> = got
            .groups
            .iter()
            .map(|g| (g.key[0].as_str().unwrap_or_default().to_string(), g.cells.runs))
            .collect();
        assert_eq!(groups, by_name, "{label} case {i}: aggregate groups");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_matches_a_naive_phrase_check_whatever_was_flushed_or_indexed() {
    let (mut with_files, mut stripped_total, mut hits) = (0, 0, 0);
    for seed in 1..=6u64 {
        let mut rng = Rng(seed * 104_729);
        let events = scenario(&mut rng);
        let runs = naive(&events);
        let dir = tempfile::tempdir().unwrap();
        let memtable = [1024, 2048, 4096, 16 * 1024, u64::MAX / 4][(seed % 5) as usize];
        {
            let (ingest, _) = Ingest::open(config(dir.path(), memtable)).unwrap();
            send_all(&ingest, &events).await;
            let deadline = Instant::now() + Duration::from_secs(10);
            while ingest.health().frozen && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if ingest.health().segments > 0 {
                with_files += 1;
            }
            let q = Query::new(ingest.clone()).with_first_window(1);
            let scan = Query::new(ingest.clone()).without_text_index();
            check_all(&q, &scan, &runs, &format!("seed {seed}")).await;
            let any = q
                .query(&QuerySpec {
                    count: true,
                    ..spec(&CASES[0])
                })
                .await
                .unwrap();
            hits += any.stats.matched;
        }
        // The same folder with half its files as M2 wrote them.
        let stripped = strip_every_other_index(dir.path());
        stripped_total += stripped.len();
        let (ingest, _) = Ingest::open(config(dir.path(), u64::MAX / 4)).unwrap();
        let q = Query::new(ingest.clone()).with_first_window(1);
        let scan = Query::new(ingest.clone()).without_text_index();
        check_all(&q, &scan, &runs, &format!("seed {seed} (M2 files)")).await;
        if !stripped.is_empty() {
            let page = q.query(&spec(&CASES[0])).await.unwrap();
            let named: BTreeSet<String> = page
                .warnings
                .iter()
                .find_map(|w| match w {
                    QueryWarning::NotIndexedFiles { files } => Some(files.iter().cloned().collect()),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("seed {seed}: no not_indexed warning: {:?}", page.warnings));
            let in_scope: BTreeSet<String> = stripped.iter().cloned().collect();
            assert!(
                named.is_subset(&in_scope) && !named.is_empty(),
                "seed {seed}: {named:?}"
            );
            assert!(page.stats.scan_files >= 1);
        }
    }
    eprintln!("seeds with files: {with_files}/6; files stripped: {stripped_total}; phrase hits: {hits}");
    assert!(with_files >= 4);
    assert!(stripped_total >= 6);
    assert!(hits >= 10, "the planted phrase must be found");
}

/// A run's outputs replaced by a late update after its first outputs were flushed: the old text stops
/// matching, the new text is found (Story 1 scenario 2), and the index still names the old file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replaced_text_stops_matching() {
    let dir = tempfile::tempdir().unwrap();
    let (ingest, _) = Ingest::open(config(dir.path(), 4096)).unwrap();
    let id = uid(1);
    let dotted = dotted_seg(0, id);
    let base = RunFields {
        id: Some(id),
        trace_id: Some(id),
        dotted_order: Some(dotted.clone()),
        ..Default::default()
    };
    let start = RunFields {
        project: Some("p".into()),
        name: Some("fetch_url".into()),
        run_type: Some("tool".into()),
        start_time: Ts::parse("2026-11-05T02:00:00Z"),
        outputs: Some(json!({"body": "rate limit exceeded"})),
        ..base.clone()
    };
    let mut events = vec![(Op::Post, start)];
    // Filler so the first version is flushed before the update arrives.
    for n in 2..60 {
        let f = RunFields {
            id: Some(uid(n)),
            trace_id: Some(uid(n)),
            dotted_order: Some(dotted_seg(n, uid(n))),
            project: Some("p".into()),
            name: Some("filler".into()),
            run_type: Some("chain".into()),
            start_time: Ts::parse("2026-11-05T02:30:00Z"),
            inputs: Some(json!({"pad": "x".repeat(200)})),
            ..Default::default()
        };
        events.push((Op::Post, f));
    }
    let items: Vec<Incoming> = events
        .into_iter()
        .enumerate()
        .map(|(i, (op, fields))| Incoming {
            op,
            source: Source::Native,
            body_hash: i as u64,
            fields,
        })
        .collect();
    send_all(&ingest, &items).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while ingest.health().segments == 0 || ingest.health().frozen {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let q = Query::new(ingest.clone());
    let s = |f: &str| QuerySpec {
        filter: Some(parse_filter(f).unwrap()),
        ..QuerySpec::default()
    };
    let page = q.query(&s(r#"search("rate limit")"#)).await.unwrap();
    assert_eq!(page.runs.len(), 1);
    let m = page.runs[0].r#match.clone().unwrap();
    assert_eq!(
        (m.path.as_str(), m.excerpt.as_str()),
        ("outputs.body", "rate limit exceeded")
    );
    assert_eq!((m.start, m.end), (0, 10));

    let update = Incoming {
        op: Op::Patch,
        source: Source::Native,
        body_hash: 999,
        fields: RunFields {
            outputs: Some(json!({"body": "HTTP 429 Too Many Requests"})),
            end_time: Ts::parse("2026-11-05T02:00:31Z"),
            ..base
        },
    };
    send_all(&ingest, &[update]).await;
    for _ in 0..2 {
        let page = q.query(&s(r#"search("rate limit")"#)).await.unwrap();
        assert!(page.runs.is_empty(), "the replaced outputs no longer match");
        let page = q
            .query(&s(r#"search(outputs, "429 too many requests")"#))
            .await
            .unwrap();
        assert_eq!(page.runs.len(), 1);
        assert_eq!(page.runs[0].header.run_id, id);
        assert!(page.stats.candidates >= 1);
        // Again once the update is in a file too.
        let filler: Vec<Incoming> = (100..160)
            .map(|n| Incoming {
                op: Op::Post,
                source: Source::Native,
                body_hash: n,
                fields: RunFields {
                    id: Some(uid(n)),
                    trace_id: Some(uid(n)),
                    dotted_order: Some(dotted_seg(n % 50, uid(n))),
                    project: Some("p".into()),
                    name: Some("filler".into()),
                    run_type: Some("chain".into()),
                    start_time: Ts::parse("2026-11-05T02:30:00Z"),
                    inputs: Some(json!({"pad": "y".repeat(200)})),
                    ..Default::default()
                },
            })
            .collect();
        send_all(&ingest, &filler).await;
        let deadline = Instant::now() + Duration::from_secs(10);
        while ingest.health().runs > 40 || ingest.health().frozen {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
