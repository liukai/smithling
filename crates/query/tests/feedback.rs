//! `feedback.<key>` in filters and aggregates (M3 T098, q3): a run's value is the mean of its scores for the key.
//! Random runs (late updates, random flush points) and random feedback, some for runs that never arrive, checked
//! against a naive answer over the merged runs: filters (and LangSmith's pair form), `avg(feedback.<key>)` in
//! totals, groups, series and comparisons, `by feedback_key` with its `where`, `/api/fields` paths and columns.

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use common::{Rng, batches, open, send, settle, uid};
use serde_json::{Value as J, json};
use tracedb_ingest::Feedback;
use tracedb_model::agg::{AggSpec, Compare, GroupKey, Metric, View};
use tracedb_model::filter::{Expr, Path, parse_filter};
use tracedb_model::{Op, RunEvent, RunFields, RunId, RunState, Source, Status, Ts, merge};
use tracedb_query::{Cells, Query, QuerySpec};

const KEYS: [&str; 2] = ["correctness", "helpfulness"];

fn naive_runs(events: &[(Op, RunFields)]) -> HashMap<RunId, RunState> {
    let mut runs: HashMap<RunId, Option<RunState>> = HashMap::new();
    for (i, (op, f)) in events.iter().enumerate() {
        let ev = RunEvent {
            op: *op,
            event_seq: i as u64 + 1,
            received_at: Ts(i as i64),
            source: Source::Native,
            body_hash: i as u64,
            fields: f.clone(),
        };
        merge(runs.entry(f.id.unwrap()).or_default(), &ev).unwrap();
    }
    runs.into_iter().map(|(k, v)| (k, v.unwrap())).collect()
}

/// Random items on about half the runs and a few unknown ids; returns `(run, key) → mean score`.
async fn plant(q: &Query, rng: &mut Rng, runs: &[RunId], tag: &str) -> HashMap<(RunId, String), f64> {
    let mut scores: HashMap<(RunId, String), Vec<f64>> = HashMap::new();
    let orphans: Vec<RunId> = (0..5).map(|i| uid(900_000 + i + tag.len() as u64 * 10)).collect();
    let mut items = Vec::new();
    for (i, run) in runs.iter().chain(&orphans).enumerate() {
        if !rng.chance(50) {
            continue;
        }
        for k in 0..1 + rng.below(3) {
            let key = KEYS[rng.below(2) as usize];
            let score = [J::from(0), J::from(0.25), J::from(0.5), J::from(1), J::Null][rng.below(5) as usize].clone();
            if let Some(s) = score.as_f64() {
                scores.entry((*run, key.to_string())).or_default().push(s);
            }
            let body = json!({"id": format!("f-{tag}-{i}-{k}"), "run_id": run, "key": key, "score": score});
            items.push(Feedback::from_upload(body, None, Ts::now()).unwrap());
        }
    }
    for chunk in items.chunks(50) {
        q.ingest().put_feedback(chunk.to_vec()).await.unwrap();
    }
    scores
        .into_iter()
        .map(|(k, v)| (k, v.iter().sum::<f64>() / v.len() as f64))
        .collect()
}

async fn ids(q: &Query, spec: &QuerySpec) -> BTreeSet<RunId> {
    let mut out = BTreeSet::new();
    let mut spec = QuerySpec {
        limit: Some(200),
        ..spec.clone()
    };
    loop {
        let page = q.query(&spec).await.unwrap();
        out.extend(page.runs.iter().map(|r| r.header.run_id));
        match page.next_cursor {
            Some(c) => spec.cursor = Some(c),
            None => return out,
        }
    }
}

fn close(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => (a - b).abs() < 1e-9,
        (None, None) => true,
        _ => false,
    }
}

/// `avg(feedback.key)` over `runs`: the mean of the run means, over runs with a score.
fn naive_avg(runs: &[&RunState], means: &HashMap<(RunId, String), f64>, key: &str) -> (Option<f64>, u64) {
    let vs: Vec<f64> = runs
        .iter()
        .filter_map(|r| means.get(&(r.run.run_id, key.to_string())).copied())
        .collect();
    let n = vs.len() as u64;
    ((n > 0).then(|| vs.iter().sum::<f64>() / n as f64), n)
}

fn assert_avg(c: &Cells, want: (Option<f64>, u64), label: &str) {
    assert!(
        close(c.feedback_avg, want.0) && c.feedback_n == want.1,
        "{label}: got avg {:?} n {}, want {:?} n {}",
        c.feedback_avg,
        c.feedback_n,
        want.0,
        want.1
    );
}

fn filtered<'a>(
    runs: &'a HashMap<RunId, RunState>,
    f: Option<&Expr>,
    means: &HashMap<(RunId, String), f64>,
) -> Vec<&'a RunState> {
    runs.values()
        .filter(|st| {
            f.is_none_or(|f| {
                let attrs = tracedb_model::attrs::promote(&st.run);
                let fb = |k: &str| means.get(&(st.run.run_id, k.to_string())).copied();
                tracedb_model::filter::eval_with(
                    f,
                    &tracedb_model::filter::Row {
                        run: &st.run,
                        attrs: &attrs,
                    },
                    &fb,
                    Ts::now(),
                    &mut Default::default(),
                )
            })
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn feedback_filters_and_averages_match_the_naive_mean() {
    let (mut groups_checked, mut clean_with_feedback) = (0, 0);
    for seed in 1..=6u64 {
        let mem = [2048, 8192, 32 * 1024, u64::MAX / 4][(seed % 4) as usize];
        let events = batches(seed, 25);
        let runs = naive_runs(&events);
        let (_dir, ingest) = open(mem);
        let q = Query::new(ingest.clone());
        let mut rng = Rng(seed * 31);
        let all: Vec<RunId> = {
            let mut v: Vec<RunId> = runs.keys().copied().collect();
            v.sort();
            v
        };
        // Half the feedback before its run arrives.
        let (before, after) = all.split_at(all.len() / 2);
        let mut means = plant(&q, &mut rng, before, "a").await;
        send(&ingest, events.clone()).await;
        settle(&ingest, mem).await;
        means.extend(plant(&q, &mut rng, after, "bb").await);
        let label = format!("seed {seed} memtable {mem}");

        // Filters, after the merge: the plain form and LangSmith's pair give the same runs.
        for f in [
            "gt(feedback.correctness, 0.5)",
            "lte(feedback.correctness, 0.25)",
            "exists(feedback.helpfulness)",
            "and(eq(feedback.correctness, 0), eq(status, \"error\"))",
            "or(gte(feedback.helpfulness, 1), not(exists(feedback.correctness)))",
            "and(eq(feedback_key, \"correctness\"), gt(feedback_score, 0.5))",
        ] {
            let e = parse_filter(f).unwrap();
            let want: BTreeSet<RunId> = filtered(&runs, Some(&e), &means).iter().map(|s| s.run.run_id).collect();
            let spec = QuerySpec {
                filter: Some(e),
                ..QuerySpec::default()
            };
            assert_eq!(ids(&q, &spec).await, want, "{label}: {f}");
        }

        // avg(feedback.<key>) in totals and groups, under exact and inexact filters.
        for f in [
            None,
            Some("eq(status, \"error\")"),
            Some("eq(metadata.v, \"v1\")"),
            Some("exists(feedback.helpfulness)"),
        ] {
            let e = f.map(|f| parse_filter(f).unwrap());
            let want = filtered(&runs, e.as_ref(), &means);
            let spec = QuerySpec {
                filter: e.clone(),
                ..QuerySpec::default()
            };
            for key in KEYS {
                let agg = AggSpec {
                    group_by: vec![GroupKey::Name],
                    metrics: vec![Metric::Count, Metric::AvgFeedback(key.into())],
                    top: 50,
                    ..AggSpec::default()
                };
                let got = q
                    .aggregate(&spec, &agg, &tracedb_model::cost::Prices::embedded())
                    .await
                    .unwrap();
                let l = format!("{label} filter {f:?} avg {key}");
                assert_eq!(got.totals.runs, want.len() as u64, "{l}");
                if f.is_some_and(|f| f.contains("feedback")) {
                    clean_with_feedback += got.stats.clean_rows;
                }
                assert_avg(&got.totals, naive_avg(&want, &means, key), &format!("{l}: totals"));
                for g in &got.groups {
                    let name = g.key[0].as_str().unwrap();
                    let members: Vec<&RunState> = want
                        .iter()
                        .copied()
                        .filter(|s| s.run.name.as_deref().unwrap_or("(none)") == name)
                        .collect();
                    assert_avg(
                        &g.cells,
                        naive_avg(&members, &means, key),
                        &format!("{l}: group {name}"),
                    );
                    groups_checked += 1;
                }
            }
        }

        // A time series and a comparison carry the average too.
        let agg = AggSpec {
            metrics: vec![Metric::Count, Metric::AvgFeedback("correctness".into())],
            view: View::Series,
            compare: Compare::Filter(parse_filter("eq(status, \"error\")").unwrap()),
            ..AggSpec::default()
        };
        let got = q
            .aggregate(&QuerySpec::default(), &agg, &tracedb_model::cost::Prices::embedded())
            .await
            .unwrap();
        let every: Vec<&RunState> = runs.values().collect();
        assert_avg(
            &got.totals,
            naive_avg(&every, &means, "correctness"),
            &format!("{label}: a"),
        );
        let n: u64 = got
            .series
            .as_ref()
            .unwrap()
            .buckets
            .iter()
            .flat_map(|b| b.per_group.iter().flatten())
            .map(|c| c.feedback_n)
            .sum();
        assert_eq!(n, got.totals.feedback_n, "{label}: series buckets add up");
        let errors: Vec<&RunState> = runs.values().filter(|s| s.run.status == Status::Error).collect();
        let b = &got.compare.as_ref().unwrap().side_b;
        assert_avg(
            &b.totals,
            naive_avg(&errors, &means, "correctness"),
            &format!("{label}: side b"),
        );

        // By feedback_key: one group per key a run has a score for (overlapping), `(none)` for the rest.
        let agg = AggSpec {
            group_by: vec![GroupKey::FeedbackKey],
            metrics: vec![Metric::Count, Metric::AvgFeedback("correctness".into())],
            ..AggSpec::default()
        };
        let got = q
            .aggregate(&QuerySpec::default(), &agg, &tracedb_model::cost::Prices::embedded())
            .await
            .unwrap();
        assert!(got.overlap, "{label}");
        assert_eq!(
            got.totals.runs,
            runs.len() as u64,
            "{label}: totals count each run once"
        );
        let mut naive_groups: BTreeMap<String, Vec<&RunState>> = BTreeMap::new();
        for st in runs.values() {
            let keys: Vec<&str> = KEYS
                .into_iter()
                .filter(|k| means.contains_key(&(st.run.run_id, k.to_string())))
                .collect();
            if keys.is_empty() {
                naive_groups.entry("(none)".into()).or_default().push(st);
            }
            for k in keys {
                naive_groups.entry(k.into()).or_default().push(st);
            }
        }
        let got_groups: BTreeMap<String, &tracedb_query::Group> = got
            .groups
            .iter()
            .map(|g| (g.key[0].as_str().unwrap().to_string(), g))
            .collect();
        assert_eq!(
            got_groups.keys().collect::<Vec<_>>(),
            naive_groups.keys().collect::<Vec<_>>(),
            "{label}: feedback_key groups"
        );
        for (k, members) in &naive_groups {
            let g = got_groups[k];
            assert_eq!(g.cells.runs, members.len() as u64, "{label}: by feedback_key {k}");
            assert_avg(
                &g.cells,
                naive_avg(members, &means, "correctness"),
                &format!("{label}: {k}"),
            );
            match (k.as_str(), &g.where_) {
                ("(none)", w) => assert!(w.is_none(), "{label}: (none) has no where: {w:?}"),
                (_, Some(w)) => {
                    let spec = QuerySpec {
                        filter: Some(parse_filter(w).unwrap()),
                        count: true,
                        limit: Some(1),
                        ..QuerySpec::default()
                    };
                    assert_eq!(
                        q.query(&spec).await.unwrap().stats.matched,
                        members.len(),
                        "{label}: {w}"
                    );
                }
                (_, None) => panic!("{label}: {k} has no where"),
            }
        }

        // The field catalog lists the keys as numbers, counting runs that have arrived.
        let fields = q.fields(None, Default::default(), "feedback.").await.unwrap();
        for key in KEYS {
            let n = means.keys().filter(|(r, k)| k == key && runs.contains_key(r)).count() as u64;
            let f = fields.fields.iter().find(|f| f.path == format!("feedback.{key}"));
            match f {
                Some(f) => assert_eq!(
                    f.types.iter().map(|t| (t.ty.as_str(), t.count)).collect::<Vec<_>>(),
                    [("number", n)],
                    "{label}: fields {key}"
                ),
                None => assert_eq!(n, 0, "{label}: feedback.{key} not listed"),
            }
        }

        // As a Table column: the run's mean.
        let spec = QuerySpec {
            columns: vec![Path::Feedback("correctness".into())],
            limit: Some(200),
            ..QuerySpec::default()
        };
        for r in q.query(&spec).await.unwrap().runs {
            let want = means.get(&(r.header.run_id, "correctness".into())).copied();
            let got = r.columns.get("feedback.correctness").and_then(J::as_f64);
            assert!(close(got, want), "{label}: column on {}", r.header.run_id);
        }
    }
    assert!(groups_checked > 100, "{groups_checked}");
    assert!(
        clean_with_feedback > 100,
        "file rows decoded for a feedback filter: {clean_with_feedback}"
    );
}
