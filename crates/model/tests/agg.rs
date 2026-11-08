//! The stats line and `AggSpec` (contracts/filter-language.md, data-model §7).

use proptest::prelude::*;
use serde_json::json;
use smithling_model::agg::{AggSpec, Bucket, Compare, GroupKey, Metric, Pct, SumOf, View, parse_stats, print_stats};
use smithling_model::filter::parse_filter;

fn spec(group_by: Vec<GroupKey>, metrics: Vec<Metric>, top: u32) -> AggSpec {
    AggSpec {
        group_by,
        metrics,
        top,
        ..AggSpec::default()
    }
}

#[test]
fn parses_the_contract_examples() {
    use GroupKey as K;
    use Metric as M;
    for (line, want) in [
        (
            "by name | count, error_rate, p95(latency), sum(cost)",
            spec(
                vec![K::Name],
                vec![M::Count, M::ErrorRate, M::Latency(Pct::P95), M::Sum(SumOf::Cost)],
                10,
            ),
        ),
        (
            "by model | sum(total_tokens), sum(cost)",
            spec(
                vec![K::Model],
                vec![M::Sum(SumOf::TotalTokens), M::Sum(SumOf::Cost)],
                10,
            ),
        ),
        ("by tag | count | top 20", spec(vec![K::Tag], vec![M::Count], 20)),
        (
            "by metadata.prompt_version, name | error_rate",
            spec(
                vec![K::Metadata("prompt_version".into()), K::Name],
                vec![M::ErrorRate],
                10,
            ),
        ),
        (
            "by name | avg(feedback.correctness)",
            spec(vec![K::Name], vec![M::AvgFeedback("correctness".into())], 10),
        ),
        (
            "by run_type, feedback_key | p50(latency), p99(latency), sum(input_tokens), sum(output_tokens)",
            spec(
                vec![K::RunType, K::FeedbackKey],
                vec![
                    M::Latency(Pct::P50),
                    M::Latency(Pct::P99),
                    M::Sum(SumOf::InputTokens),
                    M::Sum(SumOf::OutputTokens),
                ],
                10,
            ),
        ),
        ("by status | count | top 1", spec(vec![K::Status], vec![M::Count], 1)),
        ("count", spec(vec![], vec![M::Count], 10)),
    ] {
        let got = parse_stats(line).unwrap_or_else(|e| panic!("{line}: {e}"));
        assert_eq!(got, want, "{line}");
        assert_eq!(print_stats(&got), line);
    }
    assert_eq!(
        print_stats(&parse_stats("by tag | count | top 10").unwrap()),
        "by tag | count"
    );
}

#[test]
fn json_matches_data_model() {
    let full = AggSpec {
        group_by: vec![GroupKey::Name],
        metrics: vec![
            Metric::Count,
            Metric::ErrorRate,
            Metric::Latency(Pct::P95),
            Metric::Sum(SumOf::Cost),
        ],
        view: View::Series,
        top: 20,
        bucket: Bucket::M5,
        compare: Compare::Filter(parse_filter(r#"has(tags, "batch=2026-11-06-b")"#).unwrap()),
    };
    let j = json!({
        "group_by": ["name"],
        "metrics": ["count", "error_rate", "p95(latency)", "sum(cost)"],
        "view": "series",
        "top": 20,
        "bucket": "5m",
        "compare": {"filter": "has(tags, \"batch=2026-11-06-b\")"},
    });
    assert_eq!(serde_json::to_value(&full).unwrap(), j);
    assert_eq!(serde_json::from_value::<AggSpec>(j).unwrap(), full);

    let min: AggSpec = serde_json::from_value(json!({"metrics": ["count"]})).unwrap();
    assert_eq!(min, spec(vec![], vec![Metric::Count], 10));
    assert_eq!(
        (min.view, min.bucket, &min.compare),
        (View::Table, Bucket::Auto, &Compare::None)
    );
    assert_eq!(
        serde_json::to_value(&min).unwrap(),
        json!({"group_by": [], "metrics": ["count"], "view": "table", "top": 10, "bucket": "auto"})
    );

    let shifted: AggSpec = serde_json::from_value(json!({
        "group_by": ["metadata.prompt_version", "tag"],
        "metrics": ["avg(feedback.correctness)"],
        "view": "dist",
        "compare": {"shift": "1d"},
    }))
    .unwrap();
    assert_eq!(shifted.compare, Compare::Shift("1d".into()));
    assert_eq!(
        serde_json::to_value(&shifted).unwrap()["compare"],
        json!({"shift": "1d"})
    );
}

#[test]
fn json_rejects_bad_specs() {
    for (body, want) in [
        (json!({"metrics": []}), "at least one metric"),
        (
            json!({"group_by": ["name", "model", "tag"], "metrics": ["count"]}),
            "at most 2 group keys",
        ),
        (
            json!({"metrics": ["count"], "top": 0}),
            "top is a whole number from 1 to 50",
        ),
        (
            json!({"metrics": ["count"], "top": 51}),
            "top is a whole number from 1 to 50",
        ),
        (json!({"metrics": ["p90(latency)"]}), "unknown metric \"p90\""),
        (
            json!({"group_by": ["foo"], "metrics": ["count"]}),
            "unknown group key \"foo\"",
        ),
        (json!({"metrics": ["count"], "bucket": "2m"}), "unknown variant `2m`"),
        (json!({"metrics": ["count"], "view": "pie"}), "unknown variant `pie`"),
        (
            json!({"metrics": ["count"], "compare": {"shift": "yesterday"}}),
            "shift",
        ),
        (
            json!({"metrics": ["count"], "compare": {"filter": "eq(staus, 1)"}}),
            "unknown field",
        ),
        (
            json!({"metrics": ["count"], "compare": {"filter": "x", "shift": "1d"}}),
            "compare",
        ),
        (
            json!({"group_by": ["tag", "tag"], "metrics": ["count"]}),
            "\"tag\" is listed twice",
        ),
        (
            json!({"metrics": ["sum(cost)", "count", "sum( cost )"]}),
            "\"sum(cost)\" is listed twice",
        ),
    ] {
        let err = serde_json::from_value::<AggSpec>(body.clone()).unwrap_err().to_string();
        assert!(err.contains(want), "{body}: {err}");
    }
}

fn group_key() -> impl Strategy<Value = GroupKey> {
    prop_oneof![
        prop::sample::select(vec![
            GroupKey::Name,
            GroupKey::RunType,
            GroupKey::Model,
            GroupKey::Status,
            GroupKey::Tag,
            GroupKey::FeedbackKey,
        ]),
        "[a-z_]{1,6}(\\.[a-z0-9_]{1,4}){0,2}".prop_map(GroupKey::Metadata),
    ]
}

fn metric() -> impl Strategy<Value = Metric> {
    prop_oneof![
        Just(Metric::Count),
        Just(Metric::ErrorRate),
        prop::sample::select(vec![Pct::P50, Pct::P95, Pct::P99]).prop_map(Metric::Latency),
        prop::sample::select(vec![
            SumOf::InputTokens,
            SumOf::OutputTokens,
            SumOf::TotalTokens,
            SumOf::Cost
        ])
        .prop_map(Metric::Sum),
        "[a-zA-Z_][a-zA-Z0-9_-]{0,6}".prop_map(Metric::AvgFeedback),
    ]
}

fn dedup<T: PartialEq>(xs: Vec<T>) -> Vec<T> {
    let mut out = Vec::new();
    for x in xs {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    out
}

proptest! {
    #[test]
    fn print_then_parse_is_identity(
        group_by in prop::collection::vec(group_key(), 0..=2).prop_map(dedup),
        metrics in prop::collection::vec(metric(), 1..5).prop_map(dedup),
        top in 1u32..=50,
    ) {
        let a = spec(group_by, metrics, top);
        let text = print_stats(&a);
        let back = parse_stats(&text);
        prop_assert_eq!(back.as_ref(), Ok(&a), "text: {}", text);
        prop_assert_eq!(print_stats(&back.unwrap()), text, "printing is canonical");
        let j = serde_json::to_value(&a).unwrap();
        prop_assert_eq!(serde_json::from_value::<AggSpec>(j).unwrap(), a);
    }
}
