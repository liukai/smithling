//! Words for search: Unicode word boundaries, lower-cased, no normalization or stemming (M3-R2). Search terms
//! and the text index both use [`tokens`], so a phrase matches exactly when its words appear in order.

use unicode_segmentation::UnicodeSegmentation;

/// The words of `s`: segments with a letter or digit, lower-cased. `"429, Too"` gives `["429", "too"]`.
pub fn tokens(s: &str) -> Vec<String> {
    s.split_word_bounds()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .map(str::to_lowercase)
        .collect()
}

/// The words of one JSON scalar; numbers and booleans count as their text. Objects, arrays and null give none.
pub fn scalar_tokens(v: &serde_json::Value) -> Vec<String> {
    use serde_json::Value as J;
    match v {
        J::String(s) => tokens(s),
        J::Number(n) => tokens(&n.to_string()),
        J::Bool(b) => vec![b.to_string()],
        J::Null | J::Array(_) | J::Object(_) => Vec::new(),
    }
}
