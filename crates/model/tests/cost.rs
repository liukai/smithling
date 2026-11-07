//! Run cost (research M3-R8, data-model §5): reported → tokens × price → no price, in integer picodollars.

mod common;

use common::*;
use serde_json::{Value, json};
use tracedb_model::cost::{self, Cost, PICO_PER_USD, PriceError, Prices};
use tracedb_model::{Op, Run, merge};

fn run(run_type: &str, metadata: Value, outputs: Option<Value>, top: Option<Value>) -> Run {
    let mut f = post_fields(child_ids(), "step");
    f.run_type = Some(run_type.into());
    f.metadata = metadata.as_object().cloned();
    f.outputs = outputs;
    if let Some(Value::Object(other)) = top {
        f.extra = Some(json!({"other": other}).as_object().unwrap().clone());
    }
    let mut slot = None;
    merge(&mut slot, &event(Op::Post, 1, f)).unwrap();
    slot.unwrap().run
}

fn llm(model: &str, input: u64, output: u64) -> Run {
    run(
        "llm",
        json!({"ls_model_name": model}),
        Some(json!({"usage_metadata": {"input_tokens": input, "output_tokens": output}})),
        None,
    )
}

fn usd(x: f64) -> i64 {
    (x * PICO_PER_USD as f64).round() as i64
}

#[test]
fn tokens_times_price_in_picodollars() {
    let p = Prices::embedded();
    // gpt-4o-mini: 0.15 / 0.60 USD per 1M tokens → 150,000 / 600,000 pico per token.
    assert_eq!(
        cost::of(&llm("gpt-4o-mini", 4000, 750), &p),
        Some(Cost::Computed(1_050_000_000))
    );
    assert_eq!(
        cost::of(&llm("gpt-4o", 4000, 500), &p),
        Some(Cost::Computed(15_000_000_000))
    );
    let mini = p.get("gpt-4o-mini").unwrap();
    assert_eq!((mini.input_pico, mini.output_pico), (150_000, 600_000));
}

#[test]
fn reported_cost_wins_in_source_order() {
    let p = Prices::embedded();
    let usage = |u: Value| {
        run(
            "llm",
            json!({"ls_model_name": "claude-haiku"}),
            Some(json!({"usage_metadata": u})),
            None,
        )
    };
    let total = usage(json!({"input_tokens": 4000, "output_tokens": 500, "total_cost": 0.0028}));
    assert_eq!(total.reported_cost_pico, Some(2_800_000_000));
    assert_eq!(
        cost::of(&total, &p),
        Some(Cost::Reported(2_800_000_000)),
        "tokens would give 5.2e9"
    );
    let parts = usage(json!({"input_tokens": 4000, "output_tokens": 500, "input_cost": 0.0015, "output_cost": 0.0008}));
    assert_eq!(cost::of(&parts, &p), Some(Cost::Reported(2_300_000_000)));
    let both = usage(json!({"total_cost": 0.01, "input_cost": 0.0015, "output_cost": 0.0008}));
    assert_eq!(
        both.reported_cost_pico,
        Some(usd(0.01)),
        "usage total_cost before its parts"
    );
    let top = run(
        "llm",
        json!({"ls_model_name": "claude-haiku"}),
        Some(json!({"usage_metadata": {"input_tokens": 4000, "output_tokens": 500}})),
        Some(json!({"total_cost": 0.004})),
    );
    assert_eq!(
        cost::of(&top, &p),
        Some(Cost::Reported(usd(0.004))),
        "the run's own total_cost"
    );
    let in_metadata = run(
        "llm",
        json!({"ls_model_name": "gpt-4o", "usage_metadata": {"input_tokens": 1, "total_cost": 0.5}}),
        None,
        None,
    );
    assert_eq!(cost::of(&in_metadata, &p), Some(Cost::Reported(usd(0.5))));
}

#[test]
fn unknown_model_is_no_price_never_zero() {
    let p = Prices::embedded();
    assert_eq!(cost::of(&llm("local-llama", 4000, 500), &p), Some(Cost::NoPrice));
    let no_model = run(
        "llm",
        json!({}),
        Some(json!({"usage_metadata": {"input_tokens": 10}})),
        None,
    );
    assert_eq!(cost::of(&no_model, &p), Some(Cost::NoPrice));
    let no_usage = run("llm", json!({"ls_model_name": "gpt-4o"}), None, None);
    assert_eq!(cost::of(&no_usage, &p), Some(Cost::NoPrice));
}

#[test]
fn no_rollup_to_parent_runs() {
    let p = Prices::embedded();
    let chain = run(
        "chain",
        json!({"ls_model_name": "gpt-4o"}),
        Some(json!({"usage_metadata": {"input_tokens": 4000, "output_tokens": 500}})),
        None,
    );
    assert_eq!(cost::of(&chain, &p), None, "a chain's tokens are its children's");
    assert_eq!(cost::of(&run("tool", json!({}), None, None), &p), None);
    let reported_chain = run(
        "chain",
        json!({}),
        Some(json!({"usage_metadata": {"total_cost": 0.002}})),
        None,
    );
    assert_eq!(reported_chain.reported_cost_pico, Some(usd(0.002)));
    assert_eq!(
        cost::of(&reported_chain, &p),
        None,
        "a chain's reported cost repeats its children's"
    );
}

#[test]
fn tokens_too_large_to_price_are_no_price() {
    let p = Prices::embedded();
    assert_eq!(cost::of(&llm("gpt-4o", u64::MAX, 0), &p), Some(Cost::NoPrice));
    assert_eq!(cost::of(&llm("gpt-4o", 0, 1 << 60), &p), Some(Cost::NoPrice));
}

#[test]
fn lookup_is_exact_then_aliases_without_prefix_matching() {
    let p = Prices::embedded();
    assert_eq!(
        cost::of(&llm("gpt-4o-mini-2024-07-18", 4000, 750), &p),
        Some(Cost::Computed(1_050_000_000))
    );
    assert!(p.get("gpt-4o-2024-11-20").is_some());
    assert!(p.get("claude-3-5-haiku-latest").is_some());
    assert!(p.get("gpt-4o-mini-2099-01-01").is_none());
    assert!(p.get("gpt-4").is_none());
    assert!(p.get("GPT-4o").is_none());
    assert_eq!(p.get("gpt-4o-mini").unwrap().input_pico, 150_000, "not gpt-4o's price");
}

#[test]
fn cents_round_half_up() {
    assert_eq!(cost::cents_half_up(4_120_000_000_000), 412);
    assert_eq!(cost::cents_half_up(4_115_000_000_000), 412);
    assert_eq!(cost::cents_half_up(4_114_999_999_999), 411);
    assert_eq!(cost::cents_half_up(0), 0);
    assert_eq!(cost::display(4_120_000_000_000), "$4.12");
    assert_eq!(cost::display(70_000_000_000), "$0.07");
    assert_eq!(cost::display(1_234_565_000_000_000), "$1234.57");
}

#[test]
fn user_file_replaces_entries_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prices.toml");
    std::fs::write(
        &path,
        "version = 1\n\n[models.\"gpt-4o\"]\ninput = 5.0\noutput = 15.0\n\n[models.\"local-llama\"]\ninput = 0\noutput = 0\naliases = [\"llama-3\"]\n",
    )
    .unwrap();
    let mut p = Prices::embedded();
    p.merge_user(&path).unwrap();
    assert_eq!(p.get("gpt-4o").unwrap().input_pico, 5_000_000);
    assert!(
        p.get("gpt-4o-2024-08-06").is_none(),
        "the user's entry replaced its aliases too"
    );
    assert_eq!(p.get("gpt-4o-mini").unwrap().input_pico, 150_000, "others kept");
    assert_eq!(
        cost::of(&llm("llama-3", 10, 10), &p),
        Some(Cost::Computed(0)),
        "priced at 0 by the user"
    );
}

#[test]
fn a_parse_error_names_the_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prices.toml");
    for (text, line) in [
        ("version = 1\n\n[models.\"x\"]\ninput = 0.1\noutput = = 2\n", 5),
        ("version = 1\n[models.\"x\"]\ninput = \"cheap\"\noutput = 1\n", 3),
        ("version = 1\n[models.\"x\"]\ninput = -1\noutput = 1\n", 3),
        ("version = 2\n", 1),
    ] {
        std::fs::write(&path, text).unwrap();
        let err = Prices::embedded().merge_user(&path).unwrap_err();
        assert!(
            matches!(err, PriceError::Parse { line: l, .. } if l == line),
            "{text:?}: {err}"
        );
        assert!(err.to_string().contains(&format!("line {line}")), "{err}");
        assert!(err.to_string().contains("prices.toml"), "{err}");
    }
    let missing = Prices::embedded()
        .merge_user(&dir.path().join("nope.toml"))
        .unwrap_err();
    assert!(matches!(missing, PriceError::Read { .. }), "{missing}");
}
