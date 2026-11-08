//! Comparison (M3 T070): `compare.filter` and `compare.shift` over one pin == two separate aggregates; side b
//! pairs with side a's groups, a group missing on one side is `null` there (not 0); series share an offset axis
//! from each side's first run (two filters) or window start (a shift), so a 12 h and a 10 h batch line up from 0.

mod common;

use common::*;
use serde_json::Value;
use smithling_model::agg::{AggSpec, Bucket, Compare, GroupKey, Metric, Pct, View};
use smithling_model::cost::Prices;
use smithling_model::filter::{TimeAt, parse_filter};
use smithling_query::{AggResult, Query, QuerySpec, TimeRange};

fn tagged(tag: &str) -> QuerySpec {
    QuerySpec {
        filter: Some(parse_filter(&format!("has(tags, {tag:?})")).unwrap()),
        ..QuerySpec::default()
    }
}

fn by_name(view: View, compare: Compare) -> AggSpec {
    AggSpec {
        group_by: vec![GroupKey::Name],
        metrics: vec![Metric::Count, Metric::ErrorRate, Metric::Latency(Pct::P95)],
        view,
        bucket: Bucket::H1,
        compare,
        ..AggSpec::default()
    }
}

fn plain(a: &AggSpec) -> AggSpec {
    AggSpec {
        compare: Compare::None,
        view: View::Table,
        ..a.clone()
    }
}

/// Side b holds side a's groups in a's order, each with b's own cells, then b's `(other)`.
fn assert_paired(a: &AggResult, b: &AggResult, b_alone: &AggResult, label: &str) {
    assert_eq!(b.totals, b_alone.totals, "{label}: totals");
    let shown: Vec<&Value> = a
        .groups
        .iter()
        .filter(|g| g.key[0] != "(other)")
        .map(|g| &g.key[0])
        .collect();
    let b_shown: Vec<&Value> = b
        .groups
        .iter()
        .filter(|g| g.key[0] != "(other)")
        .map(|g| &g.key[0])
        .collect();
    assert_eq!(shown, b_shown, "{label}: pairs by key, in a's order");
    for g in &b.groups {
        let alone = b_alone.groups.iter().find(|x| x.key == g.key && g.key[0] != "(other)");
        match alone {
            Some(x) => assert_eq!(g.cells, x.cells, "{label}: {:?}", g.key),
            None if g.key[0] == "(other)" => {
                let rest: u64 = b_alone
                    .groups
                    .iter()
                    .filter(|x| !shown.contains(&&x.key[0]) && x.key[0] != "(other)")
                    .map(|x| x.cells.runs)
                    .sum();
                assert_eq!(
                    g.cells.runs, rest,
                    "{label}: b's (other) is b's runs outside a's groups"
                );
            }
            None => assert_eq!(g.cells.runs, 0, "{label}: {:?} has no runs in b", g.key),
        }
    }
    assert!(b.compare.is_none(), "{label}: no nested compare");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_batches_pair_by_group_and_line_up_from_their_first_run() {
    let (_dir, ingest) = loaded(3, 200, 16 * 1024).await;
    let q = Query::new(ingest);
    let prices = Prices::embedded();
    let (a_spec, b_spec) = (tagged(BATCH_A), tagged(BATCH_B));
    let cmp = Compare::Filter(b_spec.filter.clone().unwrap());
    let agg = by_name(View::Table, cmp.clone());
    let r = q.aggregate(&a_spec, &agg, &prices).await.unwrap();
    let a_alone = q.aggregate(&a_spec, &plain(&agg), &prices).await.unwrap();
    let b_alone = q.aggregate(&b_spec, &plain(&agg), &prices).await.unwrap();
    assert_eq!(
        (&r.totals, &r.groups),
        (&a_alone.totals, &a_alone.groups),
        "side a as alone"
    );
    let b = &r.compare.as_ref().expect("compare adds side b").side_b;
    assert_paired(&r, b, &b_alone, "filter");
    let only_a = b
        .groups
        .iter()
        .find(|g| g.key[0] == "only_a")
        .expect("a's group, on b's side");
    assert_eq!(only_a.cells.runs, 0);
    let json = serde_json::to_value(&r).unwrap();
    let side_b = &json["compare"]["side_b"];
    let missing = side_b["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["key"][0] == "only_a")
        .unwrap();
    assert!(missing["cells"].is_null(), "empty, not zero: {missing}");
    assert!(side_b.get("compare").is_none());
    assert!(
        side_b["groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["key"][0] == "(other)"),
        "only_b is in b's (other)"
    );

    // Series: both sides count from their first run, on one axis.
    let agg = by_name(View::Series, cmp);
    let r = q.aggregate(&a_spec, &agg, &prices).await.unwrap();
    let (sa, sb) = (
        r.series.as_ref().unwrap(),
        r.compare.as_ref().unwrap().side_b.series.as_ref().unwrap(),
    );
    assert!(sa.offset && sb.offset);
    assert_eq!((sa.step_ms, sb.step_ms), (3_600_000, 3_600_000));
    assert_eq!(sa.buckets.len(), sb.buckets.len(), "one axis");
    assert!((12..=13).contains(&sa.buckets.len()), "12 h of batch a");
    for (i, (x, y)) in sa.buckets.iter().zip(&sb.buckets).enumerate() {
        assert_eq!(
            (x.offset_ms, y.offset_ms),
            (Some(i as i64 * 3_600_000), Some(i as i64 * 3_600_000))
        );
        assert!(x.t.is_none() && y.t.is_none());
    }
    let any = |b: &smithling_query::aggregate::SeriesBucket| b.per_group.iter().any(Option::is_some);
    assert!(any(&sa.buckets[0]) && any(&sb.buckets[0]), "both start at 0");
    assert!(any(&sa.buckets[11]) && !any(&sb.buckets[11]), "b ran 10 h");
    // Side b's buckets select b's runs (its own times).
    let first = &sb.buckets[0];
    assert!(first.where_.contains("2026-11-06T00:"), "{}", first.where_);
    let mut f = vec![b_spec.filter.clone().unwrap(), parse_filter(&first.where_).unwrap()];
    f.push(parse_filter(r.groups[0].where_.as_deref().unwrap()).unwrap());
    let spec = QuerySpec {
        filter: Some(smithling_model::filter::Expr::And(f)),
        ..QuerySpec::default()
    };
    let n = q.aggregate(&spec, &plain(&agg), &prices).await.unwrap().totals.runs;
    let gi = r
        .compare
        .as_ref()
        .unwrap()
        .side_b
        .groups
        .iter()
        .position(|g| g.key == r.groups[0].key)
        .unwrap();
    assert_eq!(first.per_group[gi].as_ref().map_or(0, |c| c.runs), n);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn side_b_other_where_holds_its_none_runs_when_side_a_had_none() {
    let (_dir, ingest) = loaded(10, 100, 16 * 1024).await;
    let q = Query::new(ingest);
    let prices = Prices::embedded();
    let (a_spec, b_spec) = (tagged(BATCH_A), tagged(BATCH_B));
    let agg = AggSpec {
        group_by: vec![GroupKey::Metadata("v".into())],
        metrics: vec![Metric::Count],
        top: 2,
        compare: Compare::Filter(b_spec.filter.clone().unwrap()),
        ..AggSpec::default()
    };
    let r = q.aggregate(&a_spec, &agg, &prices).await.unwrap();
    assert!(r.groups.iter().all(|g| g.key[0] != "(none)"), "every run of a has v");
    let b = &r.compare.as_ref().unwrap().side_b;
    let other = b
        .groups
        .iter()
        .find(|g| g.key[0] == "(other)")
        .expect("b's third value and its (none)");
    let w = parse_filter(other.where_.as_deref().unwrap()).unwrap();
    let spec = QuerySpec {
        filter: Some(smithling_model::filter::Expr::And(vec![
            b_spec.filter.clone().unwrap(),
            w,
        ])),
        ..QuerySpec::default()
    };
    let n = q.aggregate(&spec, &plain(&agg), &prices).await.unwrap().totals.runs;
    assert_eq!(n, other.cells.runs, "where {:?}", other.where_);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shift_compares_the_same_window_a_day_earlier() {
    let (_dir, ingest) = loaded(4, 200, 16 * 1024).await;
    let q = Query::new(ingest);
    let prices = Prices::embedded();
    let window = |from: &str, to: &str| QuerySpec {
        time: TimeRange {
            from: TimeAt::parse(from),
            to: TimeAt::parse(to),
        },
        ..QuerySpec::default()
    };
    let today = window("2026-11-06T00:00:00Z", "2026-11-06T12:00:00Z");
    let yesterday = window("2026-11-05T00:00:00Z", "2026-11-05T12:00:00Z");
    let agg = by_name(View::Series, Compare::Shift("1d".into()));
    let r = q.aggregate(&today, &agg, &prices).await.unwrap();
    let alone = |s: &QuerySpec| {
        let (q, s, a, p) = (&q, s.clone(), plain(&agg), &prices);
        async move { q.aggregate(&s, &a, p).await.unwrap() }
    };
    let (a_alone, b_alone) = (alone(&today).await, alone(&yesterday).await);
    assert_eq!(r.totals, a_alone.totals);
    assert!(a_alone.groups.iter().any(|g| g.key[0] == "only_b") && b_alone.groups.iter().any(|g| g.key[0] == "only_a"));
    let b = &r.compare.as_ref().unwrap().side_b;
    assert_paired(&r, b, &b_alone, "shift");
    let (sa, sb) = (r.series.as_ref().unwrap(), b.series.as_ref().unwrap());
    assert!(sa.offset && sb.offset);
    assert_eq!(
        (sa.buckets.len(), sb.buckets.len()),
        (13, 13),
        "the 12 h window, from its start"
    );
    assert!(
        sa.buckets[0].where_.contains("2026-11-06T00:00:00"),
        "{}",
        sa.buckets[0].where_
    );
    assert!(
        sb.buckets[0].where_.contains("2026-11-05T00:00:00"),
        "{}",
        sb.buckets[0].where_
    );
    // Batch b ran 10 h of today's window: its late buckets are empty while yesterday's aren't.
    let any = |b: &smithling_query::aggregate::SeriesBucket| b.per_group.iter().any(Option::is_some);
    assert!(!any(&sa.buckets[11]) && any(&sb.buckets[11]));
}
