//! Words for search: Unicode word boundaries, lower-cased, no normalization or stemming (M3-R2). Search terms
//! and the text index both use [`tokens`], so a phrase matches exactly when its words appear in order.

use std::borrow::Cow;

use unicode_segmentation::UnicodeSegmentation;

use crate::filter::Side;

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

/// Characters shown on each side of a match.
pub const EXCERPT_CHARS: usize = 80;

/// A run's searchable text: every string and scalar leaf of `inputs` and `outputs`, and `error` whole.
#[derive(Clone, Copy, Debug, Default)]
pub struct Text<'a> {
    pub inputs: Option<&'a serde_json::Value>,
    pub outputs: Option<&'a serde_json::Value>,
    pub error: Option<&'a str>,
}

impl<'a> Text<'a> {
    pub fn of(run: &'a crate::Run) -> Text<'a> {
        Text {
            inputs: run.inputs.as_ref(),
            outputs: run.outputs.as_ref(),
            error: run.error.as_deref(),
        }
    }
}

/// Where a phrase was first found: the leaf's path and an excerpt with the match at `start..end` (characters).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Match {
    pub path: String,
    pub excerpt: String,
    pub start: usize,
    pub end: usize,
}

/// Whether `words` appear consecutively within one text value of `side`.
pub fn phrase_in(t: &Text, words: &[String], side: Side) -> bool {
    first(t, words, side).is_some()
}

/// The first match of `words` in `side` (error, then outputs, then inputs), with ±[`EXCERPT_CHARS`] around it.
pub fn excerpt(t: &Text, words: &[String], side: Side) -> Option<Match> {
    first(t, words, side).map(|(path, leaf, span)| cut(path, &leaf, span))
}

/// The path, text and byte span of the first leaf holding the phrase.
type Found<'a> = (String, Cow<'a, str>, (usize, usize));

fn first<'a>(t: &Text<'a>, words: &[String], side: Side) -> Option<Found<'a>> {
    if words.is_empty() {
        return None;
    }
    let on = |s: Side| side == Side::Any || side == s;
    if on(Side::Error)
        && let Some(e) = t.error
        && let Some(span) = find(e, words)
    {
        return Some(("error".to_string(), Cow::Borrowed(e), span));
    }
    [
        (Side::Outputs, "outputs", t.outputs),
        (Side::Inputs, "inputs", t.inputs),
    ]
    .into_iter()
    .filter(|(s, _, _)| on(*s))
    .find_map(|(_, root, v)| walk(v?, &mut vec![Cow::Borrowed(root)], words))
}

fn walk<'a>(v: &'a serde_json::Value, path: &mut Vec<Cow<'a, str>>, words: &[String]) -> Option<Found<'a>> {
    use serde_json::Value as J;
    let leaf = |s: Cow<'a, str>, path: &Vec<Cow<'a, str>>| find(&s, words).map(|span| (path.join("."), s, span));
    let mut inner = |key: Cow<'a, str>, x: &'a serde_json::Value| {
        path.push(key);
        let hit = walk(x, path, words);
        path.pop();
        hit
    };
    match v {
        J::Object(o) => o.iter().find_map(|(k, x)| inner(Cow::Borrowed(k), x)),
        J::Array(a) => a
            .iter()
            .enumerate()
            .find_map(|(i, x)| inner(Cow::Owned(i.to_string()), x)),
        J::String(s) => leaf(Cow::Borrowed(s), path),
        J::Number(n) => leaf(Cow::Owned(n.to_string()), path),
        J::Bool(b) => leaf(Cow::Borrowed(if *b { "true" } else { "false" }), path),
        J::Null => None,
    }
}

/// Byte span from the first to the last word of the first place `words` appear consecutively in `s`.
fn find(s: &str, words: &[String]) -> Option<(usize, usize)> {
    // ASCII text can only hold an ASCII word as a substring: most leaves are rejected without splitting them.
    if s.is_ascii()
        && let Some(w) = words.iter().max_by_key(|w| w.len())
        && (!w.is_ascii() || !contains_ignore_case(s.as_bytes(), w.as_bytes()))
    {
        return None;
    }
    // Spans of the last `words.len()` words seen, oldest first.
    let mut last: std::collections::VecDeque<(usize, usize)> = std::collections::VecDeque::with_capacity(words.len());
    for (at, w) in s.split_word_bound_indices() {
        if !w.chars().any(char::is_alphanumeric) {
            continue;
        }
        if last.len() == words.len() {
            last.pop_front();
        }
        last.push_back((at, at + w.len()));
        if last.len() == words.len()
            && last
                .iter()
                .rev()
                .zip(words.iter().rev())
                .all(|(&(a, b), w)| same(&s[a..b], w))
        {
            return Some((last[0].0, last[last.len() - 1].1));
        }
    }
    None
}

/// `word` lower-cased as [`tokens`] does it (`str::to_lowercase`: a final sigma differs from per-char) is `lower`.
fn same(word: &str, lower: &str) -> bool {
    if word.is_ascii() {
        word.eq_ignore_ascii_case(lower)
    } else {
        word.to_lowercase() == lower
    }
}

fn contains_ignore_case(hay: &[u8], needle: &[u8]) -> bool {
    needle.is_empty() || hay.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

fn cut(path: String, s: &str, (from, to): (usize, usize)) -> Match {
    let before: Vec<(usize, char)> = s[..from].char_indices().rev().take(EXCERPT_CHARS + 1).collect();
    let start_byte = if before.len() > EXCERPT_CHARS {
        before[EXCERPT_CHARS - 1].0
    } else {
        0
    };
    let mut end_byte = s.len();
    for (n, (i, _)) in s[to..].char_indices().enumerate() {
        if n == EXCERPT_CHARS {
            end_byte = to + i;
            break;
        }
    }
    let mut excerpt = String::new();
    if start_byte > 0 {
        excerpt.push('…');
    }
    let start = excerpt.chars().count() + s[start_byte..from].chars().count();
    excerpt.push_str(&s[start_byte..end_byte]);
    let end = start + s[from..to].chars().count();
    if end_byte < s.len() {
        excerpt.push('…');
    }
    Match {
        path,
        excerpt,
        start,
        end,
    }
}
