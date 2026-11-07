//! Filter language: shared golden cases (also used by the UI), evaluation, and the print/parse round trip.

mod common;

use common::*;
use proptest::prelude::*;
use serde_json::json;
use tracedb_model::agg::{parse_stats, print_stats};
use tracedb_model::attrs::{Attrs, promote};
use tracedb_model::filter::{
    AttrRoot, CmpOp, EvalStats, Expr, Field, Path, Row, Side, TimeAt, Value, eval, eval_with, parse_filter, print,
};
use tracedb_model::{Op, RunState, Ts, merge, text};

#[test]
fn golden_cases() {
    let raw = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/filter-golden.json"
    ))
    .unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    for case in doc["cases"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let parsed = match case["slot"].as_str().unwrap_or("filter") {
            "filter" => parse_filter(input).map(|e| {
                assert_eq!(parse_filter(&print(&e)).unwrap(), e, "re-parse of {input:?}");
                print(&e)
            }),
            "stats" => parse_stats(input).map(|a| {
                assert_eq!(parse_stats(&print_stats(&a)).unwrap(), a, "re-parse of {input:?}");
                print_stats(&a)
            }),
            other => panic!("unknown slot {other:?}"),
        };
        match parsed {
            Ok(text) => {
                let want = case["ok"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{input:?} parsed but expected an error"));
                assert_eq!(text, want, "canonical text of {input:?}");
            }
            Err(err) => {
                let want = &case["error"];
                assert!(!want.is_null(), "{input:?} failed: {err}");
                assert_eq!(
                    (err.start, err.end, err.message.as_str()),
                    (
                        want["start"].as_u64().unwrap() as usize,
                        want["end"].as_u64().unwrap() as usize,
                        want["message"].as_str().unwrap()
                    ),
                    "error for {input:?}"
                );
            }
        }
    }
}

#[test]
fn m3_errors_are_byte_exact() {
    for (input, want) in [
        ("search()", "7..7 search needs a string"),
        (r#"search("")"#, "7..9 search needs at least one word"),
        (r#"search(body, "x")"#, "7..11 search side is inputs, outputs or error"),
        (r#"exists("x")"#, "7..10 exists() needs a field path"),
        ("gt(feedback, 1)", "3..11 feedback needs a key: feedback.<key>"),
    ] {
        assert_eq!(parse_filter(input).unwrap_err().to_string(), want, "{input}");
    }
    for (input, want) in [
        (
            "by name | p90(latency)",
            r#"10..13 unknown metric "p90"; expected p50, p95 or p99"#,
        ),
        ("by a, b, c | count", "9..10 at most 2 group keys"),
    ] {
        assert_eq!(parse_stats(input).unwrap_err().to_string(), want, "{input}");
    }
}

#[test]
fn search_keeps_text_and_words() {
    let e = parse_filter(r#"search(outputs, "429, Too  Many")"#).unwrap();
    assert_eq!(
        e,
        Expr::Search {
            text: "429, Too  Many".into(),
            words: vec!["429".into(), "too".into(), "many".into()],
            side: Side::Outputs,
        }
    );
    assert_eq!(print(&e), r#"search(outputs, "429, Too  Many")"#);
    let any = parse_filter(r#"search("x")"#).unwrap();
    assert!(matches!(any, Expr::Search { side: Side::Any, .. }));
}

#[test]
fn feedback_pair_is_rewritten() {
    assert_eq!(
        parse_filter(r#"and(eq(feedback_key, "correctness"), gt(feedback_score, 0.5))"#).unwrap(),
        Expr::Cmp {
            op: CmpOp::Gt,
            path: Path::Feedback("correctness".into()),
            value: Value::Num(0.5),
        }
    );
}

fn fetch_url() -> RunState {
    let mut f = post_fields(child_ids(), "fetch_url");
    f.tags = Some(vec!["batch=2026-11-05-a".into()]);
    f.metadata = Some(
        json!({"prompt_version": "v7", "deep": {"a": {"b": {"c": {"d": 1}}}}})
            .as_object()
            .unwrap()
            .clone(),
    );
    let mut slot = None;
    merge(&mut slot, &event(Op::Post, 1, f)).unwrap();
    let mut p = child_ids();
    p.end_time = Some(ts("2026-11-05T02:14:38.199999Z"));
    p.error = Some("HTTPError: 429 Too Many Requests".into());
    p.outputs = Some(json!({"status_code": 429, "body": "x"}));
    merge(&mut slot, &event(Op::Patch, 2, p)).unwrap();
    slot.unwrap()
}

fn matches(st: &RunState, filter: &str) -> bool {
    let attrs: Attrs = promote(&st.run);
    let e = parse_filter(filter).unwrap();
    eval(
        &e,
        &Row {
            run: &st.run,
            attrs: &attrs,
        },
        Ts::now(),
        &mut EvalStats::default(),
    )
}

#[test]
fn evaluates_fields_attrs_and_null() {
    let st = fetch_url();
    for (f, want) in [
        (r#"eq(status, "error")"#, true),
        (r#"eq(status, "ok")"#, false),
        (r#"and(eq(name, "fetch_url"), has(tags, "batch=2026-11-05-a"))"#, true),
        (r#"gt(latency, 30)"#, true),
        (r#"gt(latency, 32)"#, false),
        (r#"neq(error, null)"#, true),
        (r#"eq(model, null)"#, true),
        (r#"eq(metadata.prompt_version, "v7")"#, true),
        (r#"eq(outputs.status_code, 429)"#, true),
        (r#"gte(outputs.status_code, 400)"#, true),
        (r#"eq(outputs.missing, null)"#, true),
        (r#"eq(outputs.status_code, "429")"#, false),
        (r#"in(name, ["web_search", "fetch_url"])"#, true),
        (r#"not(eq(run_type, "tool"))"#, false),
        (r#"gt(start_time, "2026-11-05T02:00:00Z")"#, true),
        (r#"gt(start_time, "-1h")"#, false),
        (r#"eq(trace_id, "0b9f0000-0000-4000-8000-000000000011")"#, true),
        (r#"eq(metadata.deep.a.b.c, null)"#, false),
        (r#"neq(metadata.deep.a.b.c, null)"#, true),
        (r#"eq(metadata.deep.a.b.c.d, 1)"#, false),
        (r#"eq(metadata.deep.a.b.c.d, null)"#, false),
    ] {
        assert_eq!(matches(&st, f), want, "{f}");
    }
}

#[test]
fn evaluates_exists_and_feedback() {
    let st = fetch_url();
    let attrs = promote(&st.run);
    let row = Row {
        run: &st.run,
        attrs: &attrs,
    };
    for (f, same) in [
        ("exists(metadata.prompt_version)", "neq(metadata.prompt_version, null)"),
        ("exists(error)", "neq(error, null)"),
        ("exists(tags)", "has(tags, \"batch=2026-11-05-a\")"),
        ("exists(outputs.status_code)", "neq(outputs.status_code, null)"),
        ("not(exists(model))", "eq(model, null)"),
        ("not(exists(metadata.missing))", "eq(metadata.missing, null)"),
        (
            "not(exists(metadata.deep.a.b.c.d))",
            "not(neq(metadata.deep.a.b.c.d, null))",
        ),
        ("not(exists(feedback.correctness))", "eq(feedback.correctness, null)"),
    ] {
        assert!(matches(&st, f), "{f}");
        assert!(matches(&st, same), "{same}");
    }
    let mean = |key: &str| (key == "correctness").then_some(0.25);
    let check = |f: &str| {
        eval_with(
            &parse_filter(f).unwrap(),
            &row,
            &mean,
            Ts::now(),
            &mut EvalStats::default(),
        )
    };
    assert!(check("lt(feedback.correctness, 0.5)"));
    assert!(check("exists(feedback.correctness)"));
    assert!(!check("gt(feedback.correctness, 0.5)"));
    assert!(!check("exists(feedback.helpfulness)"));
    assert!(check("eq(feedback.helpfulness, null)"));
    assert!(check(
        r#"and(eq(feedback_key, "correctness"), lte(feedback_score, 0.25))"#
    ));
    assert!(
        !matches(&st, "lt(feedback.correctness, 0.5)"),
        "no lookup: feedback is missing"
    );
}

#[test]
fn evaluates_search_on_the_merged_text() {
    let st = fetch_url();
    for (f, want) in [
        (r#"search("429 Too Many Requests")"#, true),
        (r#"search(error, "too many")"#, true),
        (r#"search(outputs, "too many")"#, false),
        (r#"search(outputs, "429")"#, true),
        (r#"search(inputs, "429")"#, false),
        (r#"search("Requests Too")"#, false),
        (r#"and(search("429"), eq(name, "fetch_url"))"#, true),
        (r#"and(search("429"), eq(name, "llm"))"#, false),
        (r#"or(eq(name, "llm"), search("httperror"))"#, true),
        (r#"not(search("timeout"))"#, true),
        (r#"not(search("429"))"#, false),
    ] {
        assert_eq!(matches(&st, f), want, "{f}");
    }
    let bare = st.without_payload();
    assert!(
        !matches(&bare, r#"search(outputs, "429")"#),
        "a run read without its payload has no outputs text"
    );
    assert!(
        matches(&bare, r#"search(error, "429")"#),
        "error text is in the core row"
    );
}

#[test]
fn mixed_types_are_counted() {
    let st = fetch_url();
    let attrs = promote(&st.run);
    let e = parse_filter(r#"eq(outputs.status_code, "429")"#).unwrap();
    let mut stats = EvalStats::default();
    assert!(!eval(
        &e,
        &Row {
            run: &st.run,
            attrs: &attrs
        },
        Ts::now(),
        &mut stats
    ));
    assert_eq!(stats.mixed_types["outputs.status_code"]["number"], 1);
}

#[test]
fn unindexed_values_are_present_and_reported() {
    let mut st = fetch_url();
    st.run.metadata.insert("long".into(), json!("x".repeat(300)));
    st.run.metadata.insert("list".into(), json!([1, 2]));
    st.run.metadata.insert("a.b".into(), json!(1));
    let attrs = promote(&st.run);
    assert!(!attrs.contains_key("metadata.a.b"), "keys with dots aren't addressable");
    let row = Row {
        run: &st.run,
        attrs: &attrs,
    };
    let mut stats = EvalStats::default();
    let check = |f: &str, stats: &mut EvalStats| eval(&parse_filter(f).unwrap(), &row, Ts::now(), stats);
    assert!(check(r#"neq(metadata.long, null)"#, &mut stats));
    assert!(!check(r#"eq(metadata.long, null)"#, &mut stats));
    assert!(!check(r#"eq(metadata.long, "x")"#, &mut stats));
    assert!(!check(r#"eq(metadata.deep.a.b.c.d, 1)"#, &mut stats));
    assert_eq!(stats.not_indexed["metadata.long"], 1);
    assert_eq!(stats.not_indexed["metadata.deep.a.b.c.d"], 1);
}

#[test]
fn parser_edge_cases() {
    for (input, ok) in [
        (r#"eq(name, "\b\f")"#, true),
        (r#"eq(name, "😀")"#, true),
        (r#"eq(name, "😀")"#, true),
        (r#"eq(name, "\u+041")"#, false),
        (r#"eq(metadata.ключ, 1)"#, true),
        (r#"gt(start_time, "--5h")"#, false),
        (r#"gt(start_time, "+106751990d")"#, false),
        (r#"eq(name,"\é")"#, false),
    ] {
        assert_eq!(parse_filter(input).is_ok(), ok, "{input}");
        if let Err(e) = parse_filter(input) {
            assert!(
                input.is_char_boundary(e.start) && input.is_char_boundary(e.end),
                "{input}: {e}"
            );
        }
    }
    assert_eq!(
        parse_filter(r#"eq(name, "😀")"#).unwrap(),
        parse_filter(r#"eq(name, "😀")"#).unwrap()
    );
    let ctl = parse_filter(r#"eq(name, "\b")"#).unwrap();
    assert_eq!(parse_filter(&print(&ctl)).unwrap(), ctl);
}

#[test]
fn promotion_limits() {
    let st = fetch_url();
    let attrs = promote(&st.run);
    assert!(attrs.contains_key("metadata.prompt_version"));
    assert!(attrs.contains_key("metadata.deep.a.b.c") || !attrs.contains_key("metadata.deep.a.b.c.d"));
    assert!(!attrs.contains_key("metadata.deep.a.b.c.d"), "metadata depth ≤ 4");
    assert!(attrs.contains_key("outputs.status_code"));
}

// Round trip: random ASTs print to text that parses back to the same AST.

fn path() -> impl Strategy<Value = Path> {
    prop_oneof![
        prop::sample::select(vec![Field::Name, Field::Model, Field::Error, Field::Project]).prop_map(Path::Field),
        (
            "[a-z_]{1,6}(\\.[a-z0-9_]{1,4}){0,2}",
            prop::sample::select(AttrRoot::ALL.to_vec())
        )
            .prop_map(|(key, root)| Path::Attr { root, key }),
        "[a-zA-Z_][a-zA-Z0-9_-]{0,6}(\\.[a-z0-9_]{1,4})?".prop_map(Path::Feedback),
    ]
}

fn value_for(p: &Path) -> BoxedStrategy<Value> {
    match p {
        Path::Field(_) => "[ -~]{0,8}".prop_map(Value::Str).boxed(),
        Path::Feedback(_) => (-1e3f64..1e3)
            .prop_map(|n| Value::Num((n * 100.0).round() / 100.0))
            .boxed(),
        Path::Attr { .. } => prop_oneof![
            "[ -~]{0,8}".prop_map(Value::Str),
            (-1e9f64..1e9).prop_map(|n| Value::Num((n * 100.0).round() / 100.0)),
            any::<bool>().prop_map(Value::Bool),
        ]
        .boxed(),
    }
}

fn leaf() -> impl Strategy<Value = Expr> {
    prop_oneof![
        (path(), prop::sample::select(vec![CmpOp::Eq, CmpOp::Neq])).prop_flat_map(|(p, op)| {
            value_for(&p).prop_map(move |value| Expr::Cmp {
                op,
                path: p.clone(),
                value,
            })
        }),
        "[a-z=0-9-]{1,10}".prop_map(|t| Expr::Has {
            path: Path::Field(Field::Tags),
            value: Value::Str(t)
        }),
        (-48i64..48).prop_map(|h| Expr::Cmp {
            op: CmpOp::Gt,
            path: Path::Field(Field::StartTime),
            value: Value::Time {
                raw: format!("-{}h", h.abs()),
                at: TimeAt::Rel(-h.abs() * 3_600_000_000)
            },
        }),
        prop::sample::select(vec!["running", "ok", "error"]).prop_map(|s| Expr::Cmp {
            op: CmpOp::Eq,
            path: Path::Field(Field::Status),
            value: Value::Str(s.into()),
        }),
        (
            "[a-zA-Z0-9]{1,6}[ -~]{0,8}|[ -~]{0,4}(ключ|é|😀|429)[ -~]{0,4}",
            prop::sample::select(vec![Side::Any, Side::Inputs, Side::Outputs, Side::Error])
        )
            .prop_filter("search needs a word", |(t, _)| !text::tokens(t).is_empty())
            .prop_map(|(t, side)| Expr::Search {
                words: text::tokens(&t),
                text: t,
                side,
            }),
        prop_oneof![path(), Just(Path::Field(Field::Tags))].prop_map(|path| Expr::Exists { path }),
    ]
}

fn expr() -> impl Strategy<Value = Expr> {
    leaf().prop_recursive(4, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 1..4).prop_map(Expr::And),
            prop::collection::vec(inner.clone(), 1..4).prop_map(Expr::Or),
            inner.prop_map(|e| Expr::Not(Box::new(e))),
        ]
    })
}

proptest! {
    #[test]
    fn print_then_parse_is_identity(e in expr()) {
        let text = print(&e);
        let back = parse_filter(&text);
        prop_assert_eq!(back.as_ref(), Ok(&e), "text: {}", text);
        prop_assert_eq!(print(&back.unwrap()), text, "printing is canonical");
    }
}
