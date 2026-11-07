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

// Phrases and excerpts (M3-R3, data-model §6): consecutive words within one leaf, on the merged run's text.

use tracedb_model::filter::Side;
use tracedb_model::text::{EXCERPT_CHARS, Text, excerpt, phrase_in};

fn w(s: &str) -> Vec<String> {
    tokens(s)
}

fn outputs(v: &serde_json::Value) -> Text<'_> {
    Text {
        outputs: Some(v),
        ..Text::default()
    }
}

/// The excerpt's marked part, by character offsets.
fn marked(m: &tracedb_model::text::Match) -> String {
    m.excerpt.chars().skip(m.start).take(m.end - m.start).collect()
}

#[test]
fn consecutive_words_in_one_leaf() {
    let v = json!({"body": "got 429, Too MANY requests from api.example.com"});
    assert!(phrase_in(&outputs(&v), &w("429 Too Many Requests"), Side::Any));
    assert!(phrase_in(&outputs(&v), &w("many"), Side::Any));
    assert!(!phrase_in(&outputs(&v), &w("Too 429"), Side::Any), "order matters");
    assert!(
        !phrase_in(&outputs(&v), &w("429 Many"), Side::Any),
        "words must be consecutive"
    );
    assert!(!phrase_in(&outputs(&v), &w("requests from api.example"), Side::Any));
    assert!(phrase_in(&outputs(&v), &w("requests from api.example.com"), Side::Any));
    assert!(!phrase_in(&outputs(&v), &w("req"), Side::Any), "whole words only");
}

#[test]
fn a_phrase_split_across_leaves_does_not_match() {
    let v = json!({"a": "429 Too", "b": "Many Requests", "list": ["rate", "limit"]});
    assert!(!phrase_in(&outputs(&v), &w("429 Too Many Requests"), Side::Any));
    assert!(!phrase_in(&outputs(&v), &w("rate limit"), Side::Any));
    assert!(phrase_in(&outputs(&v), &w("too"), Side::Any));
    let v = json!({"status_code": 429, "ok": false});
    assert!(phrase_in(&outputs(&v), &w("429"), Side::Any), "numbers are text");
    assert!(phrase_in(&outputs(&v), &w("FALSE"), Side::Any));
    assert!(
        !phrase_in(&outputs(&v), &w("status_code"), Side::Any),
        "keys aren't searched"
    );
}

#[test]
fn sides() {
    let (i, o) = (json!({"q": "rate limit"}), json!({"text": "fine"}));
    let t = Text {
        inputs: Some(&i),
        outputs: Some(&o),
        error: Some("Timeout: upstream"),
    };
    let rate = w("rate limit");
    assert!(phrase_in(&t, &rate, Side::Any));
    assert!(phrase_in(&t, &rate, Side::Inputs));
    assert!(!phrase_in(&t, &rate, Side::Outputs));
    assert!(!phrase_in(&t, &rate, Side::Error));
    assert!(phrase_in(&t, &w("timeout"), Side::Error));
    assert!(!phrase_in(&t, &w("timeout"), Side::Inputs));
    assert!(!phrase_in(&Text::default(), &w("x"), Side::Any), "no text, no match");
}

#[test]
fn excerpt_of_a_short_value_is_the_whole_value() {
    let o = json!({"body": "HTTPError: 429 Too Many Requests"});
    let t = Text {
        outputs: Some(&o),
        error: Some("HTTPError: 429 Too Many Requests"),
        ..Text::default()
    };
    let m = excerpt(&t, &w("429 too many requests"), Side::Any).unwrap();
    assert_eq!(
        (m.path.as_str(), m.excerpt.as_str(), m.start, m.end),
        ("error", "HTTPError: 429 Too Many Requests", 11, 32),
        "the error is shown first (contracts/http-api.md)"
    );
    let m = excerpt(&t, &w("429 too many requests"), Side::Outputs).unwrap();
    assert_eq!(m.path, "outputs.body");
    let i = json!({"messages": [{"content": "x"}, {"content": "say Too Many"}]});
    let t = Text {
        inputs: Some(&i),
        ..Text::default()
    };
    let m = excerpt(&t, &w("too many"), Side::Any).unwrap();
    assert_eq!(
        (m.path.as_str(), marked(&m).as_str()),
        ("inputs.messages.1.content", "Too Many")
    );
    assert!(excerpt(&t, &w("absent"), Side::Any).is_none());
}

#[test]
fn excerpt_is_80_characters_each_side_cut_on_character_boundaries() {
    // Multi-byte characters on both sides: a byte cut would split one.
    let before = "é".repeat(200);
    let after = "限".repeat(200);
    let s = format!("{before} 429 Too Many Requests {after}");
    let v = json!({"page": s});
    let m = excerpt(&outputs(&v), &w("429 too many requests"), Side::Any).unwrap();
    assert_eq!(marked(&m), "429 Too Many Requests");
    assert_eq!(m.start, 1 + EXCERPT_CHARS, "an ellipsis, then 80 characters");
    assert_eq!(m.excerpt.chars().count(), 1 + EXCERPT_CHARS + 21 + EXCERPT_CHARS + 1);
    assert!(m.excerpt.starts_with('…') && m.excerpt.ends_with('…'));
    assert_eq!(
        m.excerpt,
        format!(
            "…{} 429 Too Many Requests {}…",
            "é".repeat(EXCERPT_CHARS - 1),
            "限".repeat(EXCERPT_CHARS - 1)
        )
    );
}

#[test]
fn a_1_mb_value_gives_a_short_excerpt() {
    let mut s = "lorem ipsum ".repeat(90_000);
    s.push_str("the upstream said 429 Too Many Requests, retrying");
    s.push_str(&" dolor sit".repeat(10_000));
    assert!(s.len() > 1_000_000);
    let v = json!({"dump": s});
    let t = outputs(&v);
    assert!(phrase_in(&t, &w("429 Too Many Requests"), Side::Any));
    let m = excerpt(&t, &w("429 Too Many Requests"), Side::Any).unwrap();
    assert!(
        m.excerpt.chars().count() <= 2 * EXCERPT_CHARS + 23,
        "{}",
        m.excerpt.len()
    );
    assert_eq!(marked(&m), "429 Too Many Requests");
}

#[test]
fn non_english_words_accents_emoji_and_case() {
    let v = json!({"zh": "上游返回了限流错误", "fr": "Le CAFÉ est fermé", "ja": "レート制限に達しました", "e": "rate 🚦 Limit hit"});
    let t = outputs(&v);
    assert!(
        phrase_in(&t, &w("限流"), Side::Any),
        "CJK: one word per character, consecutive"
    );
    assert!(!phrase_in(&t, &w("流限"), Side::Any));
    assert!(phrase_in(&t, &w("café"), Side::Any));
    assert!(phrase_in(&t, &w("Est Fermé"), Side::Any));
    assert!(!phrase_in(&t, &w("cafe"), Side::Any), "accents aren't folded");
    assert!(phrase_in(&t, &w("レート制限"), Side::Any));
    let g = json!("Ο ΣΊΣΥΦΟΣ ανεβαίνει");
    assert!(
        phrase_in(&outputs(&g), &w("σίσυφος ΑΝΕΒΑΊΝΕΙ"), Side::Any),
        "final sigma as tokens() folds it"
    );
    assert!(phrase_in(&t, &w("RATE limit"), Side::Any), "emoji aren't words");
    let k = json!("the \u{212A}elvin scale");
    assert!(
        phrase_in(&outputs(&k), &w("kelvin scale"), Side::Any),
        "a non-ASCII letter that lower-cases to ASCII"
    );
    let m = excerpt(&t, &w("限流"), Side::Any).unwrap();
    assert_eq!((m.path.as_str(), marked(&m).as_str()), ("outputs.zh", "限流"));
    assert_eq!(m.excerpt, "上游返回了限流错误");
    let m = excerpt(&t, &w("rate limit"), Side::Any).unwrap();
    assert_eq!(marked(&m), "rate 🚦 Limit");
}
