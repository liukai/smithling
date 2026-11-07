//! The word tokenizer shared by search terms and the text index (M3-R2).

use serde_json::json;
use tracedb_model::text::{scalar_tokens, tokens};

fn t(s: &str) -> Vec<String> {
    tokens(s)
}

#[test]
fn unicode_words_lower_cased() {
    for (input, want) in [
        (
            "HTTPError: 429 Too Many Requests",
            vec!["httperror", "429", "too", "many", "requests"],
        ),
        ("429, Too", vec!["429", "too"]),
        ("  rate-limit!!  ", vec!["rate", "limit"]),
        (
            "https://api.example.com/v1?q=x",
            vec!["https", "api.example.com", "v1", "q", "x"],
        ),
        ("Café ÉCOLE naïve", vec!["café", "école", "naïve"]),
        ("ΣΊΣΥΦΟΣ", vec!["σίσυφος"]),
        ("限流错误", vec!["限", "流", "错", "误"]),
        ("レート制限", vec!["レート", "制", "限"]),
        ("rate 😀 limit 🚦", vec!["rate", "limit"]),
        ("3.14 and -2,5", vec!["3.14", "and", "2,5"]),
        ("snake_case don't", vec!["snake_case", "don't"]),
        ("", vec![]),
        ("!? ... — 😀", vec![]),
    ] {
        assert_eq!(t(input), want, "{input:?}");
    }
}

#[test]
fn punctuation_is_ignored_and_case_folded() {
    assert_eq!(t("429, Too"), t("429 too"));
    assert_eq!(t("Rate LIMIT"), t("rate limit"));
}

#[test]
fn no_normalization_or_stemming() {
    assert_ne!(
        t("cafe\u{301}"),
        t("café"),
        "no NFKC: combining accents stay as written"
    );
    assert_eq!(t("requests"), vec!["requests"]);
    assert_eq!(t("ﬁle"), vec!["ﬁle"], "ligatures aren't expanded");
}

#[test]
fn scalars_as_text() {
    assert_eq!(scalar_tokens(&json!("Too Many")), vec!["too", "many"]);
    assert_eq!(scalar_tokens(&json!(429)), vec!["429"]);
    assert_eq!(scalar_tokens(&json!(0.5)), vec!["0.5"]);
    assert_eq!(scalar_tokens(&json!(true)), vec!["true"]);
    assert_eq!(scalar_tokens(&json!(false)), vec!["false"]);
    assert!(scalar_tokens(&json!(null)).is_empty());
    assert!(
        scalar_tokens(&json!({"a": "b"})).is_empty(),
        "only scalars; callers walk the leaves"
    );
    assert!(scalar_tokens(&json!(["a"])).is_empty());
}
