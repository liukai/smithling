//! Progressive answers (M3 T076): an estimate over a 100% sample == the exact answer; a smaller sample scales
//! counts and sums by rows in scope / rows read, not rates or percentiles, and marks every cell `estimated`;
//! small scopes get only the result; dropping the stream stops the clean and dirty loops.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use futures_util::StreamExt;
use smithling_model::agg::{AggSpec, Compare, GroupKey, Metric, Pct, SumOf, View};
use smithling_model::cost::Prices;
use smithling_model::filter::parse_filter;
use smithling_query::aggregate::{AggMsg, StreamOpts};
use smithling_query::{AggResult, Cells, Query, QuerySpec};

fn every_file() -> StreamOpts {
    StreamOpts {
        min_rows: 0,
        sample: 1.0,
        budget: Duration::from_secs(60),
        fill_wait: Duration::from_secs(60),
    }
}

async fn messages(q: &Query, spec: &QuerySpec, a: &AggSpec, opts: StreamOpts) -> Vec<AggMsg> {
    q.aggregate_stream(spec.clone(), a.clone(), Arc::new(Prices::embedded()), opts)
        .collect()
        .await
}

fn unmark(c: &Cells) -> Cells {
    Cells {
        estimated: false,
        ..c.clone()
    }
}

/// `r` with every estimate mark taken off.
fn as_exact(r: &AggResult) -> AggResult {
    let mut r = r.clone();
    r.query_id = String::new();
    r.partial = false;
    r.sampled_pct = None;
    r.stats.ms = 0;
    r.totals = unmark(&r.totals);
    for g in &mut r.groups {
        g.cells = unmark(&g.cells);
    }
    if let Some(s) = &mut r.series {
        for b in &mut s.buckets {
            for c in b.per_group.iter_mut().flatten() {
                *c = unmark(c);
            }
        }
    }
    if let Some(c) = &mut r.compare {
        *c.side_b = as_exact(&c.side_b);
    }
    r
}

fn all_cells(r: &AggResult) -> Vec<&Cells> {
    let mut v: Vec<&Cells> = vec![&r.totals];
    v.extend(r.groups.iter().map(|g| &g.cells).filter(|c| c.runs > 0));
    v.extend(
        r.series
            .iter()
            .flat_map(|s| s.buckets.iter().flat_map(|b| b.per_group.iter().flatten())),
    );
    if let Some(c) = &r.compare {
        v.extend(all_cells(&c.side_b));
    }
    v
}

fn split(msgs: Vec<AggMsg>) -> (AggResult, AggResult) {
    let mut it = msgs.into_iter();
    match (it.next(), it.next(), it.next()) {
        (Some(AggMsg::Estimate(e)), Some(AggMsg::Result(r)), None) => (e, r),
        other => panic!("expected an estimate then the result: {other:?}"),
    }
}

fn metrics() -> Vec<Metric> {
    vec![
        Metric::Count,
        Metric::ErrorRate,
        Metric::Latency(Pct::P95),
        Metric::Sum(SumOf::TotalTokens),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_estimate_over_every_file_is_the_exact_answer() {
    let (_dir, ingest) = loaded(5, 150, 16 * 1024).await;
    let q = Query::new(ingest);
    let a_filter = parse_filter(&format!("has(tags, {BATCH_A:?})")).unwrap();
    let b_filter = parse_filter(&format!("has(tags, {BATCH_B:?})")).unwrap();
    let spec = QuerySpec {
        filter: Some(a_filter),
        ..QuerySpec::default()
    };
    for (view, compare) in [
        (View::Table, Compare::None),
        (View::Series, Compare::None),
        (View::Table, Compare::Filter(b_filter.clone())),
        (View::Series, Compare::Filter(b_filter)),
    ] {
        let a = AggSpec {
            group_by: vec![GroupKey::Name],
            metrics: metrics(),
            view,
            top: 3,
            compare,
            ..AggSpec::default()
        };
        let (est, res) = split(messages(&q, &spec, &a, every_file()).await);
        assert!(est.partial && !res.partial);
        assert_eq!(est.sampled_pct, Some(100.0));
        assert_eq!(est.query_id, res.query_id, "one id per request");
        assert!(all_cells(&est).iter().all(|c| c.estimated));
        assert!(all_cells(&res).iter().all(|c| !c.estimated));
        let exact = q.aggregate(&spec, &a, &Prices::embedded()).await.unwrap();
        assert_eq!(as_exact(&est), as_exact(&exact), "{view:?}");
        assert_eq!(as_exact(&res), as_exact(&exact), "{view:?}");
        assert!(res.stats.dirty_runs > 0 && res.stats.clean_rows > 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sample_scales_counts_and_sums_but_not_rates_or_percentiles() {
    let (_dir, ingest) = loaded(6, 300, 16 * 1024).await;
    let files = ingest.health().segments;
    assert!(files >= 10, "{files} files");
    let q = Query::new(ingest);
    let spec = QuerySpec::default();
    let a = AggSpec {
        group_by: vec![GroupKey::Name],
        metrics: metrics(),
        ..AggSpec::default()
    };
    let opts = StreamOpts {
        sample: 0.3,
        ..every_file()
    };
    let (est, res) = split(messages(&q, &spec, &a, opts).await);
    let pct = est.sampled_pct.unwrap() as f64;
    assert!(pct > 5.0 && pct < 70.0, "{pct}% sampled");
    assert!(all_cells(&est).iter().all(|c| c.estimated));
    let read = (est.stats.clean_rows + est.stats.dirty_runs) as f64;
    let t = &est.totals;
    assert!(
        (t.runs as f64 - read * 100.0 / pct).abs() <= 2.0,
        "{} runs from {read} read",
        t.runs
    );
    assert!(est.stats.clean_rows < res.stats.clean_rows && est.stats.dirty_runs < res.stats.dirty_runs);
    assert!((t.runs as f64) > 0.6 * res.totals.runs as f64 && (t.runs as f64) < 1.6 * res.totals.runs as f64);
    // A rate is the sample's own (up to rounding the scaled counts), not scaled.
    let rate = t.error_rate.unwrap();
    assert!((rate - t.errors as f64 / t.finished as f64).abs() < 0.01 && rate < 1.0);
    assert!(t.p95.unwrap() > 0 && t.p95 <= Some(90_005));
    let tokens = t.total_tokens.unwrap_or(0);
    assert_eq!(tokens, 0, "no run reports tokens");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cold_estimate_skips_multi_version_runs_and_the_result_is_still_exact() {
    let (_dir, ingest) = loaded(9, 150, 16 * 1024).await;
    let a = AggSpec {
        group_by: vec![GroupKey::Name],
        metrics: metrics(),
        ..AggSpec::default()
    };
    let spec = QuerySpec::default();
    // A fresh server: its merged rows of multi-version runs aren't read yet, and the estimate doesn't wait.
    let cold = Query::new(ingest.clone());
    let opts = StreamOpts {
        fill_wait: Duration::ZERO,
        ..every_file()
    };
    let (est, res) = split(messages(&cold, &spec, &a, opts).await);
    let exact = Query::new(ingest)
        .aggregate(&spec, &a, &Prices::embedded())
        .await
        .unwrap();
    assert!(est.partial && all_cells(&est).iter().all(|c| c.estimated));
    assert!(
        est.stats.dirty_runs < res.stats.dirty_runs,
        "multi-version runs left out"
    );
    assert!(
        est.totals.runs > est.stats.clean_rows as u64 + est.stats.dirty_runs as u64,
        "scaled up for them"
    );
    let off = (est.totals.runs as f64 - exact.totals.runs as f64).abs() / exact.totals.runs as f64;
    assert!(off < 0.25, "{} vs {}", est.totals.runs, exact.totals.runs);
    assert_eq!(as_exact(&res), as_exact(&exact));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_small_scope_gets_only_the_result() {
    let (_dir, ingest) = loaded(7, 40, 16 * 1024).await;
    let q = Query::new(ingest);
    let a = AggSpec {
        group_by: vec![GroupKey::Name],
        metrics: metrics(),
        ..AggSpec::default()
    };
    let msgs = messages(&q, &QuerySpec::default(), &a, StreamOpts::default()).await;
    assert!(matches!(msgs.as_slice(), [AggMsg::Result(r)] if !r.partial), "{msgs:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_stream_stops_reading() {
    let (_dir, ingest) = loaded(8, 2500, 256 * 1024).await;
    let files: u64 = ingest.health().segments as u64;
    assert!(files >= 8, "{files} files");
    let q = Query::new(ingest);
    // An inexact filter: rows are decoded and evaluated, so the scan takes a while.
    let spec = QuerySpec {
        filter: Some(parse_filter(r#"or(eq(metadata.v, "v1"), neq(error, null))"#).unwrap()),
        ..QuerySpec::default()
    };
    let a = AggSpec {
        group_by: vec![GroupKey::Metadata("v".into())],
        metrics: metrics(),
        ..AggSpec::default()
    };
    let opts = StreamOpts {
        min_rows: u64::MAX,
        ..StreamOpts::default()
    };
    let full = q.aggregate_stream(spec.clone(), a.clone(), Arc::new(Prices::embedded()), opts);
    let full_token = full.cancel_token();
    assert_eq!(full.collect::<Vec<_>>().await.len(), 1);
    let total = full_token.rows_read();
    let stream = q.aggregate_stream(spec.clone(), a.clone(), Arc::new(Prices::embedded()), opts);
    let token = stream.cancel_token();
    let deadline = Instant::now() + Duration::from_secs(20);
    while token.rows_read() == 0 {
        assert!(Instant::now() < deadline, "never started reading");
        tokio::task::yield_now().await;
    }
    drop(stream);
    assert!(token.is_cancelled(), "the stream owns the token");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stopped = token.rows_read();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(token.rows_read(), stopped, "rows read stop growing");
    // Both loops look at the token: the clean scan stops within a batch or so, long before the end.
    assert!(stopped < total / 2, "stopped early: {stopped} of {total}");
}

/// SC-06 at 2M runs: time to the first estimate on a cold server (merged rows of multi-version runs not read
/// yet) and a warm one, and to the exact result. Slow: `cargo test --release -p smithling-query --test progressive
/// -- --ignored --nocapture`; `SMITHLING_2M_DIR` keeps the data between runs, `SMITHLING_2M_MEMTABLE_MB` sizes files.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timings_at_2m_runs() {
    use smithling_ingest::{Incoming, Ingest, IngestConfig};
    use smithling_model::{Op, RunFields, Source, Ts};
    const RUNS: u64 = 2_000_000;
    let dir = std::env::var("SMITHLING_2M_DIR").unwrap_or_else(|_| "/tmp/smithling-2m-u7".into());
    // About unit 4's spike shape (38 files) by default.
    let mb: u64 = std::env::var("SMITHLING_2M_MEMTABLE_MB").map_or(24, |v| v.parse().unwrap());
    let open = || {
        let mut cfg = IngestConfig::new(&dir);
        cfg.limits.memtable_bytes = mb * 1024 * 1024;
        Ingest::open(cfg).unwrap().0
    };
    let ingest = open();
    if ingest.health().segment_rows < RUNS {
        let t0 = ts("2026-11-05T00:00:00Z").0;
        let mut rng = Rng(11);
        let run = |n: u64, rng: &mut Rng| {
            let trace = n / 10 * 10;
            let at = Ts(t0 + (n as i64) * 50_000);
            let id = uid(n);
            let dotted = if n == trace {
                format!("{}{id}", at.to_string().replace(['-', ':', '.'], ""))
            } else {
                let root_at = Ts(t0 + (trace as i64) * 50_000);
                format!(
                    "{}{}.{}{id}",
                    root_at.to_string().replace(['-', ':', '.'], ""),
                    uid(trace),
                    at.to_string().replace(['-', ':', '.'], "")
                )
            };
            let start = RunFields {
                id: Some(id),
                trace_id: Some(uid(trace)),
                parent_run_id: (n != trace).then(|| uid(trace)),
                dotted_order: Some(dotted.clone()),
                project: Some("p".into()),
                name: Some(NAMES[(n % 4) as usize].into()),
                run_type: Some("tool".into()),
                start_time: Some(at),
                tags: Some(vec![if rng.chance(50) { BATCH_A } else { BATCH_B }.into()]),
                ..Default::default()
            };
            let end = RunFields {
                id: Some(id),
                trace_id: Some(uid(trace)),
                dotted_order: Some(dotted),
                end_time: Some(Ts(at.0 + 1000 * (5 + rng.below(30_000) as i64))),
                error: rng.chance(4).then(|| "boom".into()),
                ..Default::default()
            };
            (start, end)
        };
        let item = |op, fields| Incoming {
            op,
            source: Source::Native,
            body_hash: 0,
            fields,
        };
        let started = Instant::now();
        let mut late: Vec<Incoming> = Vec::new();
        for chunk in (0..RUNS).collect::<Vec<_>>().chunks(5000) {
            let mut items = Vec::new();
            for &n in chunk {
                let (s, e) = run(n, &mut rng);
                if rng.chance(10) {
                    items.push(item(Op::Post, s));
                    late.push(item(Op::Patch, e));
                } else {
                    items.push(item(
                        Op::Post,
                        RunFields {
                            end_time: e.end_time,
                            error: e.error,
                            ..s
                        },
                    ));
                }
            }
            loop {
                match ingest.accept(items.clone()).await {
                    Ok(_) => break,
                    Err(smithling_ingest::AcceptError::Busy) => tokio::time::sleep(Duration::from_millis(20)).await,
                    Err(e) => panic!("{e}"),
                }
            }
        }
        // The late ends after every start, most of which are in files by then: multi-version runs.
        for c in late.chunks(5000) {
            while ingest.accept(c.to_vec()).await.is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        settle(&ingest, mb * 1024 * 1024).await;
        eprintln!("loaded {RUNS} runs in {:.0?}", started.elapsed());
    }
    drop(ingest);
    let ingest = open();
    let h = ingest.health();
    eprintln!(
        "{} files, {} file rows, {} memtable runs",
        h.segments, h.segment_rows, h.runs
    );
    let q = Query::new(ingest);
    let a = AggSpec {
        group_by: vec![GroupKey::Name],
        metrics: vec![
            Metric::Count,
            Metric::ErrorRate,
            Metric::Latency(Pct::P95),
            Metric::Sum(SumOf::Cost),
        ],
        ..AggSpec::default()
    };
    let spec = QuerySpec::default();
    let timed = |label: &'static str| {
        let (q, spec, a) = (q.clone(), spec.clone(), a.clone());
        async move {
            let started = Instant::now();
            let mut s = q.aggregate_stream(spec, a, Arc::new(Prices::embedded()), StreamOpts::default());
            let mut first = None;
            while let Some(m) = s.next().await {
                match m {
                    AggMsg::Estimate(e) => {
                        first = Some(started.elapsed());
                        eprintln!(
                            "{label}: estimate {:.0?} ({:.1}% sampled, {} runs est.)",
                            started.elapsed(),
                            e.sampled_pct.unwrap(),
                            e.totals.runs
                        );
                    }
                    AggMsg::Result(r) => {
                        eprintln!("{label}: result {:.0?} ({} runs)", started.elapsed(), r.totals.runs)
                    }
                    AggMsg::Error { error, .. } => panic!("{error}"),
                }
            }
            first.expect("an estimate")
        }
    };
    let cold = timed("cold").await;
    let warm = timed("warm").await;
    let started = Instant::now();
    let exact = q.aggregate(&spec, &a, &Prices::embedded()).await.unwrap();
    eprintln!(
        "exact (POST, warm): {:.0?}, {} runs",
        started.elapsed(),
        exact.totals.runs
    );
    assert!(
        cold < Duration::from_secs(1) && warm < Duration::from_secs(1),
        "SC-06: {cold:?} / {warm:?}"
    );
}
