//! The overnight-eval scenario (research M2-R14, M3-R11): deterministic, exact planted counts checked naively over
//! the generated items, and loaded through `/runs/batch` so its checks cross memtable and files.

use smithling_server::testkit as common;

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use smithling_model::cost::{self, Cost, Prices};
use smithling_model::filter::parse_filter;
use smithling_model::{Op, RunEvent, Source, Ts};
use smithling_query::QuerySpec;
use smithling_server::langsmith_map::{Dialect, map_item};
use smithling_tools::scenario::{self, Scenario, generate, load};

/// 2026-11-06T12:00:00Z.
const NOW: i64 = 1_793_966_400_000_000;
const HOUR: i64 = 3_600_000_000;
const MB: u64 = 1024 * 1024;

/// Final state per run id: the post with its patch applied.
fn merged(s: &Scenario) -> HashMap<String, Value> {
    let mut runs: HashMap<String, Value> = HashMap::new();
    for item in s.posts.iter().chain(&s.background) {
        runs.insert(item["id"].as_str().unwrap().into(), item.clone());
    }
    for (_, p) in &s.late {
        let r = runs
            .get_mut(p["id"].as_str().unwrap())
            .expect("a patch follows its post");
        for (k, v) in p.as_object().unwrap() {
            r[k] = v.clone();
        }
    }
    runs
}

fn tagged(r: &Value) -> bool {
    tagged_with(r, scenario::BATCH_TAG)
}

fn tagged_with(r: &Value, tag: &str) -> bool {
    r["tags"].as_array().is_some_and(|t| t.iter().any(|t| t == tag))
}

fn ts(r: &Value, k: &str) -> Option<i64> {
    r[k].as_str().map(|s| Ts::parse(s).expect("an ISO time").0)
}

fn latency_ms(r: &Value) -> i64 {
    (ts(r, "end_time").unwrap() - ts(r, "start_time").unwrap()) / 1000
}

fn err(r: &Value) -> &str {
    r["error"].as_str().unwrap_or("")
}

/// Nearest rank: the smallest value with at least `p`% of values at or below it.
fn p95(mut xs: Vec<i64>) -> i64 {
    xs.sort();
    xs[(xs.len() * 95).div_ceil(100) - 1]
}

const PICO_PER_TOKEN: [(&str, i64, i64); 3] = [
    ("gpt-4o-mini", 150_000, 600_000),
    ("gpt-4o", 2_500_000, 10_000_000),
    ("claude-haiku", 800_000, 4_000_000),
];

/// (picodollars, runs with no price), straight from the items: reported cost first, else tokens × price.
fn naive_cost<'a>(runs: impl Iterator<Item = &'a Value>) -> (i64, usize) {
    let (mut pico, mut no_price) = (0i64, 0usize);
    let usd = |v: &Value| (v.as_f64().unwrap() * 1e12).round() as i64;
    for r in runs.filter(|r| r["run_type"] == "llm") {
        let u = &r["outputs"]["usage_metadata"];
        if !u["total_cost"].is_null() {
            pico += usd(&u["total_cost"]);
        } else if !u["input_cost"].is_null() {
            pico += usd(&u["input_cost"]) + usd(&u["output_cost"]);
        } else {
            let model = r["extra"]["metadata"]["ls_model_name"].as_str().unwrap();
            match PICO_PER_TOKEN.iter().find(|(m, ..)| *m == model) {
                Some((_, i, o)) => {
                    pico += u["input_tokens"].as_i64().unwrap() * i + u["output_tokens"].as_i64().unwrap() * o
                }
                None => no_price += 1,
            }
        }
    }
    (pico, no_price)
}

/// The same runs as the server would hold them: mapped and merged by the model, costed by `cost::of`.
fn model_cost(s: &Scenario, tag: &str) -> (i64, usize) {
    let mut slots: HashMap<String, Option<smithling_model::RunState>> = HashMap::new();
    let items = s
        .posts
        .iter()
        .map(|p| (Op::Post, p))
        .chain(s.late.iter().map(|(_, p)| (Op::Patch, p)));
    for (seq, (op, item)) in items.enumerate() {
        let ev = RunEvent {
            op,
            event_seq: seq as u64 + 1,
            received_at: Ts(NOW),
            source: Source::LsBatch,
            body_hash: 0,
            fields: map_item(item, Dialect::LangSmith).unwrap(),
        };
        smithling_model::merge(slots.entry(item["id"].as_str().unwrap().into()).or_default(), &ev).unwrap();
    }
    let prices = Prices::embedded();
    let (mut pico, mut no_price) = (0, 0);
    for st in slots
        .into_values()
        .flatten()
        .filter(|st| st.run.tags.iter().any(|t| t == tag))
    {
        match cost::of(&st.run, &prices) {
            Some(Cost::Reported(c) | Cost::Computed(c)) => pico += c,
            Some(Cost::NoPrice) => no_price += 1,
            None => {}
        }
    }
    (pico, no_price)
}

#[test]
fn same_seed_same_data() {
    let (a, b, c) = (generate(7, NOW, MB), generate(7, NOW, MB), generate(8, NOW, MB));
    assert_eq!(a.posts, b.posts);
    assert_eq!(a.late, b.late);
    assert_eq!(a.background, b.background);
    assert_eq!(a.feedback, b.feedback);
    assert_ne!(a.posts, c.posts);
}

#[test]
fn planted_counts_are_exact() {
    for seed in [7, 8, 42] {
        let s = generate(seed, NOW, MB);
        let runs = merged(&s);
        let roots: Vec<&Value> = runs
            .values()
            .filter(|r| r["parent_run_id"].is_null() && tagged(r))
            .collect();
        assert_eq!(roots.len(), 200, "seed {seed}");
        let errored = |r: &Value| !r["error"].is_null();
        let mut trace_errored: HashSet<&str> = HashSet::new();
        for r in runs.values().filter(|r| errored(r)) {
            trace_errored.insert(r["trace_id"].as_str().unwrap());
        }
        let failing: HashSet<String> = roots
            .iter()
            .filter(|r| errored(r))
            .map(|r| r["id"].as_str().unwrap().into())
            .collect();
        let quiet: HashSet<String> = roots
            .iter()
            .filter(|r| !errored(r) && !r["end_time"].is_null())
            .filter(|r| trace_errored.contains(r["trace_id"].as_str().unwrap()))
            .map(|r| r["id"].as_str().unwrap().into())
            .collect();
        assert_eq!(failing.len(), scenario::FAILING_ROOTS, "seed {seed}");
        assert_eq!(quiet.len(), scenario::QUIET_FAILURES, "seed {seed}");
        assert_eq!(failing, s.failing.iter().map(|u| u.to_string()).collect());
        assert_eq!(quiet, s.quiet.iter().map(|u| u.to_string()).collect());
        assert!(roots.iter().all(|r| !r["end_time"].is_null()), "every root finishes");

        let pv = |r: &Value| r["extra"]["metadata"]["prompt_version"].as_str().map(str::to_string);
        let count = |v: &str| runs.values().filter(|r| pv(r).as_deref() == Some(v)).count();
        assert_eq!(runs.values().filter(|r| pv(r).is_some()).count(), 1840);
        assert_eq!((count("v6"), count("v7")), (928, 912), "seed {seed}");
        let v7_429 = runs
            .values()
            .filter(|r| pv(r).as_deref() == Some("v7") && r["outputs"]["status_code"] == 429)
            .count();
        assert_eq!(v7_429, scenario::V7_429, "seed {seed}");

        for (late, p) in &s.late {
            let secs = late.as_secs();
            assert!((30..=120).contains(&secs) || secs >= 40 * 60, "late by {secs} s");
            assert!(!p["end_time"].is_null());
        }
        assert!(s.late.iter().filter(|(d, _)| d.as_secs() >= 40 * 60).count() >= 3);
        assert!(
            s.late.windows(2).all(|w| w[0].0 <= w[1].0),
            "patches go out in arrival order"
        );
    }
}

#[test]
fn background_volume_covers_three_memtables() {
    let s = generate(7, NOW, 2 * MB);
    let bytes: u64 = s
        .background
        .iter()
        .map(|r| 512 + serde_json::to_string(&r["inputs"]).unwrap().len() as u64)
        .sum();
    assert!(bytes >= 3 * 2 * MB, "background {bytes} bytes");
    assert!(
        s.background
            .iter()
            .all(|r| r["extra"]["metadata"]["prompt_version"].is_null())
    );
    assert!(s.background.iter().all(|r| !tagged(r)));
}

#[test]
fn batch_a_planted_figures() {
    for seed in [7, 8, 42] {
        let s = generate(seed, NOW, MB);
        let runs = merged(&s);
        let batch: Vec<&Value> = runs.values().filter(|r| tagged(r)).collect();
        let fetch: Vec<&Value> = batch.iter().copied().filter(|r| r["name"] == "fetch_url").collect();
        assert_eq!(fetch.len(), 300, "seed {seed}");
        let errored: Vec<&Value> = fetch.iter().copied().filter(|r| !err(r).is_empty()).collect();
        let limited: Vec<&Value> = errored
            .iter()
            .copied()
            .filter(|r| err(r) == "HTTPError: 429 Too Many Requests")
            .collect();
        let timeouts: Vec<&Value> = errored
            .iter()
            .copied()
            .filter(|r| err(r).starts_with("ReadTimeout"))
            .collect();
        assert_eq!(
            (errored.len(), limited.len(), timeouts.len()),
            (42, 31, 11),
            "seed {seed}"
        );
        assert_eq!(
            format!("{:.1}%", errored.len() as f64 * 100.0 / fetch.len() as f64),
            "14.0%"
        );
        let step_errors = batch
            .iter()
            .filter(|r| !r["parent_run_id"].is_null() && !err(r).is_empty())
            .count();
        assert_eq!(step_errors, 42, "only fetch_url steps fail");

        // 14 failing roots each with a 429 (5 also retried into a timeout); 23 quiet = 17 × 429 + 6 × timeout.
        let trace_of = |r: &&Value| r["trace_id"].as_str().unwrap().to_string();
        let with_429: HashSet<String> = limited.iter().map(trace_of).collect();
        let with_timeout: HashSet<String> = timeouts.iter().map(trace_of).collect();
        let roots_in = |ids: &[uuid::Uuid]| ids.iter().map(|u| u.to_string()).collect::<HashSet<_>>();
        let (failing, quiet) = (roots_in(&s.failing), roots_in(&s.quiet));
        assert!(failing.iter().all(|t| with_429.contains(t)));
        assert_eq!(failing.intersection(&with_timeout).count(), 5);
        assert_eq!(quiet.intersection(&with_429).count(), 17);
        assert_eq!(quiet.intersection(&with_timeout).count(), 6);
        assert_eq!(
            with_429.len() + with_timeout.len(),
            14 + 23 + 5,
            "one planted step per kind per trace"
        );

        // The phrase: in `outputs.body` and `error` of the 31 only, anywhere in the scenario.
        let phrase = scenario::PHRASE;
        let hits: Vec<&Value> = runs
            .values()
            .filter(|r| serde_json::to_string(r).unwrap().contains(phrase))
            .collect();
        assert_eq!(hits.len(), 31, "seed {seed}");
        assert!(hits.iter().all(|r| r["name"] == "fetch_url" && tagged(r)));
        assert!(
            hits.iter()
                .all(|r| err(r).contains(phrase) && r["outputs"]["body"].as_str().is_some_and(|b| b.contains(phrase)))
        );

        let pv = |r: &Value| r["extra"]["metadata"]["prompt_version"].as_str().map(str::to_string);
        let status_429: Vec<&Value> = runs.values().filter(|r| r["outputs"]["status_code"] == 429).collect();
        assert_eq!(status_429.len(), 31);
        let v7 = status_429.iter().filter(|r| pv(r).as_deref() == Some("v7")).count();
        let v6 = status_429.iter().filter(|r| pv(r).as_deref() == Some("v6")).count();
        assert_eq!((v7, v6), (19, 12), "seed {seed}");

        // Latencies: 258 ok 0.2–4 s and never late; 11 × 30 s; 22 × 31 s; 9 at 35–95 s → p95 = 31 s.
        let late: HashSet<&str> = s.late.iter().map(|(_, p)| p["id"].as_str().unwrap()).collect();
        let ok: Vec<&Value> = fetch.iter().copied().filter(|r| err(r).is_empty()).collect();
        assert_eq!(ok.len(), 258);
        assert!(ok.iter().all(|r| (200..=4_000).contains(&latency_ms(r))), "seed {seed}");
        assert!(
            ok.iter().all(|r| !late.contains(r["id"].as_str().unwrap())),
            "ok fetch_url never late"
        );
        assert!(timeouts.iter().all(|r| latency_ms(r) == 30_000));
        assert_eq!(limited.iter().filter(|r| latency_ms(r) == 31_000).count(), 22);
        assert_eq!(
            limited
                .iter()
                .filter(|r| (35_000..=95_000).contains(&latency_ms(r)))
                .count(),
            9
        );
        assert_eq!(
            p95(fetch.iter().map(|r| latency_ms(r)).collect()),
            31_000,
            "seed {seed}"
        );

        // Models and cost.
        let llm: Vec<&Value> = batch.iter().copied().filter(|r| r["run_type"] == "llm").collect();
        let model = |r: &Value| {
            r["extra"]["metadata"]["ls_model_name"]
                .as_str()
                .unwrap_or("")
                .to_string()
        };
        let tokens = |m: &str, k: &str| -> i64 {
            llm.iter()
                .filter(|r| model(r) == m)
                .map(|r| r["outputs"]["usage_metadata"][k].as_i64().unwrap())
                .sum()
        };
        for (m, n) in [
            ("gpt-4o-mini", 800),
            ("gpt-4o", 200),
            ("claude-haiku", 100),
            ("local-llama", 12),
        ] {
            assert_eq!(llm.iter().filter(|r| model(r) == m).count(), n, "{m}");
        }
        assert_eq!(llm.len(), 1112, "every llm run names a model");
        assert_eq!(
            (
                tokens("gpt-4o-mini", "input_tokens"),
                tokens("gpt-4o-mini", "output_tokens")
            ),
            (3_200_000, 600_000)
        );
        assert_eq!(
            (tokens("gpt-4o", "input_tokens"), tokens("gpt-4o", "output_tokens")),
            (800_000, 100_000)
        );
        assert!(
            llm.iter()
                .filter(|r| model(r) == "claude-haiku")
                .all(|r| r["outputs"]["usage_metadata"]["total_cost"] == 0.0028)
        );
        let haiku_by_tokens: i64 =
            tokens("claude-haiku", "input_tokens") * 800_000 + tokens("claude-haiku", "output_tokens") * 4_000_000;
        assert_eq!(cost::display(haiku_by_tokens), "$0.52", "reported $0.28 must win");
        let (pico, no_price) = naive_cost(batch.iter().copied());
        assert_eq!(
            (cost::display(pico), no_price),
            ("$4.12".to_string(), 12),
            "seed {seed}"
        );
        assert_eq!(pico, scenario::BATCH_A_COST_PICO);
        assert_eq!(
            model_cost(&s, scenario::BATCH_TAG),
            (pico, 12),
            "the model's cost path agrees"
        );

        // Feedback: correctness = 0 on two failing roots, one sent before its run.
        assert_eq!(s.feedback.len(), 2);
        let on: HashSet<&str> = s.feedback.iter().map(|f| f.body["run_id"].as_str().unwrap()).collect();
        assert_eq!(on.len(), 2);
        assert!(on.iter().all(|id| failing.contains(*id)));
        assert!(
            s.feedback
                .iter()
                .all(|f| f.body["key"] == "correctness" && f.body["score"] == 0.0)
        );
        assert_eq!(s.feedback.iter().filter(|f| f.before_run).count(), 1);
    }
}

#[test]
fn batch_b_and_timing() {
    for seed in [7, 8, 42] {
        let s = generate(seed, NOW, MB);
        let runs = merged(&s);
        let b: Vec<&Value> = runs
            .values()
            .filter(|r| tagged_with(r, scenario::BATCH_B_TAG))
            .collect();
        assert_eq!(
            b.iter().filter(|r| r["parent_run_id"].is_null()).count(),
            200,
            "seed {seed}"
        );
        assert!(b.iter().all(|r| !tagged(r)));
        let fetch: Vec<&Value> = b.iter().copied().filter(|r| r["name"] == "fetch_url").collect();
        assert_eq!(fetch.len(), 300);
        let errors: Vec<&Value> = b.iter().copied().filter(|r| !err(r).is_empty()).collect();
        assert_eq!(errors.len(), 6, "seed {seed}");
        assert!(
            errors
                .iter()
                .all(|r| r["name"] == "fetch_url" && err(r).starts_with("ReadTimeout"))
        );
        assert_eq!(
            format!("{:.1}%", errors.len() as f64 * 100.0 / fetch.len() as f64),
            "2.0%"
        );
        assert!(b.iter().all(|r| r["outputs"]["status_code"] != 429));
        assert_eq!(p95(fetch.iter().map(|r| latency_ms(r)).collect()), 9_000, "seed {seed}");
        let (pico, no_price) = naive_cost(b.iter().copied());
        assert_eq!((cost::display(pico), no_price), ("$4.07".to_string(), 0), "seed {seed}");
        assert_eq!(pico, scenario::BATCH_B_COST_PICO);
        assert_eq!(model_cost(&s, scenario::BATCH_B_TAG), (pico, 0));
        assert!(
            b.iter()
                .all(|r| !serde_json::to_string(r).unwrap().contains(scenario::PHRASE))
        );
        assert!(b.iter().all(|r| r["extra"]["metadata"]["prompt_version"].is_null()));

        let end = |tag: &str| {
            runs.values()
                .filter(|r| tagged_with(r, tag))
                .filter_map(|r| ts(r, "end_time"))
                .max()
                .unwrap()
        };
        assert_eq!(end(scenario::BATCH_TAG), NOW - 30 * HOUR, "seed {seed}");
        assert_eq!(end(scenario::BATCH_B_TAG), NOW - 6 * HOUR, "seed {seed}");
        assert!(
            runs.values().all(|r| ts(r, "start_time").unwrap() <= NOW),
            "no run starts in the future"
        );
        assert!(runs.values().filter_map(|r| ts(r, "end_time")).all(|e| e <= NOW));

        let bg_errors: Vec<&str> = s
            .background
            .iter()
            .filter(|r| !r["parent_run_id"].is_null())
            .map(err)
            .filter(|e| !e.is_empty())
            .collect();
        assert!(!bg_errors.is_empty());
        assert!(
            bg_errors.iter().all(|e| *e == "HTTPError: 503 Service Unavailable"),
            "{bg_errors:?}"
        );
        assert!(
            s.background
                .iter()
                .all(|r| r["outputs"]["status_code"] != 429 && !err(r).contains("429"))
        );
    }
}

#[tokio::test]
async fn loads_through_the_batch_api_across_memtable_and_files() {
    let server = common::start_with(|c| c.limits.memtable_bytes = MB).await;
    let s = generate(7, Ts::now().0, MB);
    let loaded = load(&format!("http://{}", server.addr), &s).await.unwrap();
    let health = server.state.ingest.health();
    assert!(health.flushes >= 2, "flushes {}", health.flushes);
    assert_eq!(loaded.flushes, health.flushes);
    assert!(loaded.in_files);
    assert!(loaded.posts > s.posts.len() && loaded.posts <= s.posts.len() + s.background.len());
    assert_eq!(loaded.patches, s.late.len());

    let spec = QuerySpec {
        is_root: Some(true),
        filter: Some(parse_filter(r#"and(has(tags, "batch=2026-11-05-a"), eq(status, "error"))"#).unwrap()),
        limit: Some(200),
        ..QuerySpec::default()
    };
    let page = server.state.query.query(&spec).await.unwrap();
    assert_eq!(page.stats.matched, scenario::FAILING_ROOTS);
    let flushed = health.flushed_seq;
    assert!(
        page.runs
            .iter()
            .all(|r| r.header.first_seq <= flushed && r.header.last_seq > flushed),
        "each failing root started in a file and ended in the memtable"
    );
}

#[tokio::test]
async fn refuses_a_server_that_already_holds_the_batch() {
    let server = common::start_with(|c| c.limits.memtable_bytes = MB).await;
    let url = format!("http://{}", server.addr);
    let s = generate(7, Ts::now().0, MB);
    load(&url, &s).await.unwrap();
    let err = load(&url, &s).await.unwrap_err();
    assert!(matches!(err, smithling_tools::seed::SeedError::Refused(..)), "{err}");
}

#[tokio::test]
async fn unreachable_server_is_reported() {
    let err = load("http://127.0.0.1:9", &generate(1, NOW, MB)).await.unwrap_err();
    assert!(matches!(err, smithling_tools::seed::SeedError::Unreachable(..)));
}
