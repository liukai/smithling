//! Time series and distribution (M3 T063): `bucket: auto` ≈ 100 buckets on a round step, or the given one;
//! empty buckets `null`; one series per group including `(other)`; the 02:30 error spike; fixed log-latency
//! bins; every bucket and bin carries a `where` that selects exactly its runs (checked against the table
//! aggregate, itself checked against naive accumulation in `aggregate.rs`).

mod common;

use common::*;
use smithling_model::agg::{AggSpec, Bucket, GroupKey, Metric, Pct, View};
use smithling_model::cost::Prices;
use smithling_model::filter::{Expr, parse_filter};
use smithling_query::aggregate::accum::BIN_LOWER_MS;
use smithling_query::aggregate::series::AUTO_STEPS_MIN;
use smithling_query::{AggResult, Cells, Query, QuerySpec};

fn batch_a() -> QuerySpec {
    QuerySpec {
        filter: Some(parse_filter(&format!("has(tags, {BATCH_A:?})")).unwrap()),
        ..QuerySpec::default()
    }
}

fn metrics() -> Vec<Metric> {
    vec![Metric::Count, Metric::ErrorRate, Metric::Latency(Pct::P95)]
}

/// The table aggregate of `spec` with `wheres` added to its filter.
async fn totals_where(q: &Query, spec: &QuerySpec, wheres: &[&str], metrics: &[Metric]) -> Cells {
    let mut all: Vec<Expr> = spec.filter.iter().cloned().collect();
    all.extend(wheres.iter().map(|w| parse_filter(w).unwrap()));
    let spec = QuerySpec {
        filter: Some(Expr::And(all)),
        ..spec.clone()
    };
    let agg = AggSpec {
        metrics: metrics.to_vec(),
        ..AggSpec::default()
    };
    q.aggregate(&spec, &agg, &Prices::embedded()).await.unwrap().totals
}

fn series_of(r: &AggResult) -> &smithling_query::aggregate::Series {
    r.series.as_ref().expect("view: series adds series")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn series_buckets_on_a_round_step_and_match_the_table_per_bucket() {
    let (_dir, ingest) = loaded(1, 150, 16 * 1024).await;
    assert!(ingest.health().segments >= 3);
    let q = Query::new(ingest);
    let spec = batch_a();
    let agg = AggSpec {
        group_by: vec![GroupKey::Name],
        metrics: metrics(),
        view: View::Series,
        top: 3,
        ..AggSpec::default()
    };
    let r = q.aggregate(&spec, &agg, &Prices::embedded()).await.unwrap();
    let s = series_of(&r);
    assert!(!s.offset);
    let step_min = s.step_ms / 60_000;
    assert!(AUTO_STEPS_MIN.contains(&step_min), "round step: {step_min} min");
    assert!(
        (34..=100).contains(&s.buckets.len()),
        "≈ 100 buckets: {}",
        s.buckets.len()
    );
    assert!(
        r.cut && r.groups.last().unwrap().key[0] == "(other)",
        "(other) has a series too"
    );
    for (i, b) in s.buckets.iter().enumerate() {
        let t = b.t.expect("alone, buckets are times").0;
        assert_eq!(t % (s.step_ms * 1000), 0, "on whole steps");
        assert_eq!(t, s.buckets[0].t.unwrap().0 + i as i64 * s.step_ms * 1000);
        assert_eq!(b.per_group.len(), r.groups.len());
        let t1 = smithling_model::Ts(t + s.step_ms * 1000);
        assert_eq!(
            b.where_,
            format!(
                "and(gte(start_time, {:?}), lt(start_time, {:?}))",
                b.t.unwrap().to_string(),
                t1.to_string()
            )
        );
        // Each cell == the table aggregate of the group's and the bucket's runs; `null` when it has none.
        for (g, cell) in r.groups.iter().zip(&b.per_group) {
            let want = totals_where(&q, &spec, &[g.where_.as_deref().unwrap(), &b.where_], &agg.metrics).await;
            match cell {
                None => assert_eq!(want.runs, 0, "bucket {i} group {:?}", g.key),
                Some(c) => assert_eq!(*c, want, "bucket {i} group {:?}", g.key),
            }
        }
    }
    // Every run is in one bucket.
    for (gi, g) in r.groups.iter().enumerate() {
        let runs: u64 = s
            .buckets
            .iter()
            .filter_map(|b| b.per_group[gi].as_ref())
            .map(|c| c.runs)
            .sum();
        assert_eq!(runs, g.cells.runs, "{:?}", g.key);
    }
    // No run started between 05:00 and 06:00: those buckets are null, not 0.
    let (gap0, gap1) = (ts("2026-11-05T05:00:00Z").0, ts("2026-11-05T06:00:00Z").0);
    let gap: Vec<_> = s
        .buckets
        .iter()
        .filter(|b| (gap0..gap1).contains(&b.t.unwrap().0))
        .collect();
    assert!(!gap.is_empty());
    assert!(gap.iter().all(|b| b.per_group.iter().all(Option::is_none)));
    let json = serde_json::to_value(&r).unwrap();
    let empty = json["series"]["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["t"].as_str().unwrap().starts_with("2026-11-05T05:30"))
        .unwrap();
    assert!(
        empty["per_group"].as_array().unwrap().iter().all(|c| c.is_null()),
        "{empty}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_given_step_shows_the_0230_spike_and_bins_are_fixed_log_latency() {
    let (_dir, ingest) = loaded(2, 300, 16 * 1024).await;
    let q = Query::new(ingest);
    let spec = batch_a();
    let agg = AggSpec {
        metrics: vec![Metric::ErrorRate],
        view: View::Series,
        bucket: Bucket::M15,
        ..AggSpec::default()
    };
    let r = q.aggregate(&spec, &agg, &Prices::embedded()).await.unwrap();
    let s = series_of(&r);
    assert_eq!(s.step_ms, 15 * 60_000);
    assert!(
        s.buckets.iter().all(|b| b.per_group.len() == 1),
        "not grouped: the totals"
    );
    let worst = s
        .buckets
        .iter()
        .filter(|b| b.per_group[0].as_ref().is_some_and(|c| c.finished >= 20))
        .max_by(|x, y| {
            let rate =
                |b: &&smithling_query::aggregate::SeriesBucket| b.per_group[0].as_ref().and_then(|c| c.error_rate);
            rate(x).partial_cmp(&rate(y)).unwrap()
        })
        .unwrap();
    assert_eq!(worst.t, Some(ts("2026-11-05T02:30:00Z")), "the spike's bucket");
    assert!(worst.per_group[0].as_ref().unwrap().error_rate.unwrap() > 0.5);

    let agg = AggSpec {
        group_by: vec![GroupKey::Name],
        metrics: metrics(),
        view: View::Dist,
        ..AggSpec::default()
    };
    let r = q.aggregate(&spec, &agg, &Prices::embedded()).await.unwrap();
    let d = r.dist.as_ref().expect("view: dist adds dist");
    let first = BIN_LOWER_MS.iter().position(|&b| b == d.bins_ms[0]).unwrap();
    assert_eq!(
        d.bins_ms,
        BIN_LOWER_MS[first..first + d.bins_ms.len()],
        "contiguous fixed bins"
    );
    assert_eq!(BIN_LOWER_MS[1], 10, "from 10 ms");
    assert_eq!(BIN_LOWER_MS[9], 20, "8 per doubling");
    assert!(d.bins_ms.len() > 40, "5 ms … 90 s spans many bins");
    assert_eq!(
        (d.per_group.len(), d.where_per_bin.len()),
        (r.groups.len(), d.bins_ms.len())
    );
    for (g, counts) in r.groups.iter().zip(&d.per_group) {
        assert_eq!(counts.iter().sum::<u64>(), g.cells.n_latency, "{:?}", g.key);
    }
    let secs = |ms: i64| format!("{}", ms as f64 / 1000.0);
    assert_eq!(
        d.where_per_bin[3],
        format!(
            "and(gte(latency, {}), lt(latency, {}))",
            secs(d.bins_ms[3]),
            secs(d.bins_ms[4])
        )
    );
    for (gi, g) in r.groups.iter().enumerate() {
        for b in (0..d.bins_ms.len()).step_by(5) {
            let want = totals_where(
                &q,
                &spec,
                &[g.where_.as_deref().unwrap(), &d.where_per_bin[b]],
                &agg.metrics,
            )
            .await;
            assert_eq!(
                d.per_group[gi][b], want.n_latency,
                "group {:?} bin {}",
                g.key, d.bins_ms[b]
            );
        }
    }
}
