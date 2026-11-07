//! What an aggregate adds up ([`AggSpec`], data-model §7) and the stats line that writes it,
//! `by name | count, p95(latency) | top 20` (contracts/filter-language.md).

use serde::{Deserialize, Serialize};

use crate::filter::parse::{ParseError, Tok, Token, err, lex, resolve_path};
use crate::filter::{AttrRoot, Expr, Path, parse_filter, print};

pub const DEFAULT_TOP: u32 = 10;
pub const MAX_TOP: u32 = 50;
pub const MAX_GROUP_KEYS: usize = 2;

/// The aggregate part of a request; the runs it adds up come from the query fields next to it.
/// `Default` has no metrics, so it is only a base to fill in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawSpec", into = "RawSpec")]
pub struct AggSpec {
    /// 0 to [`MAX_GROUP_KEYS`].
    pub group_by: Vec<GroupKey>,
    /// At least one.
    pub metrics: Vec<Metric>,
    pub view: View,
    /// 1 to [`MAX_TOP`].
    pub top: u32,
    pub bucket: Bucket,
    pub compare: Compare,
}

impl Default for AggSpec {
    fn default() -> AggSpec {
        AggSpec {
            group_by: Vec::new(),
            metrics: Vec::new(),
            view: View::Table,
            top: DEFAULT_TOP,
            bucket: Bucket::Auto,
            compare: Compare::None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum GroupKey {
    Name,
    RunType,
    Model,
    Status,
    /// One group per tag; a run with several tags is in several groups.
    Tag,
    FeedbackKey,
    /// `metadata.<key>`.
    Metadata(String),
}

const NAMED_KEYS: [(&str, GroupKey); 6] = [
    ("name", GroupKey::Name),
    ("run_type", GroupKey::RunType),
    ("model", GroupKey::Model),
    ("status", GroupKey::Status),
    ("tag", GroupKey::Tag),
    ("feedback_key", GroupKey::FeedbackKey),
];

impl GroupKey {
    pub fn text(&self) -> String {
        match self {
            GroupKey::Metadata(k) => format!("metadata.{k}"),
            named => NAMED_KEYS
                .iter()
                .find(|(_, k)| k == named)
                .map(|(n, _)| n.to_string())
                .expect("every other key is named"),
        }
    }

    fn resolve(text: &str, start: usize, end: usize) -> Result<GroupKey, ParseError> {
        if let Some((_, k)) = NAMED_KEYS.iter().find(|(n, _)| *n == text) {
            return Ok(k.clone());
        }
        if text.starts_with("metadata.")
            && let Path::Attr {
                root: AttrRoot::Metadata,
                key,
            } = resolve_path(text, start, end)?
        {
            return Ok(GroupKey::Metadata(key));
        }
        Err(err(
            start,
            end,
            format!(
                "unknown group key {text:?}; expected name, run_type, model, status, tag, feedback_key or metadata.<key>"
            ),
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Metric {
    Count,
    /// Errored / finished runs.
    ErrorRate,
    /// Nearest-rank latency percentile over finished runs.
    Latency(Pct),
    Sum(SumOf),
    /// `avg(feedback.<key>)`.
    AvgFeedback(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pct {
    P50,
    P95,
    P99,
}

impl Pct {
    pub const ALL: [Pct; 3] = [Pct::P50, Pct::P95, Pct::P99];

    pub fn name(self) -> &'static str {
        match self {
            Pct::P50 => "p50",
            Pct::P95 => "p95",
            Pct::P99 => "p99",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SumOf {
    InputTokens,
    OutputTokens,
    TotalTokens,
    Cost,
}

impl SumOf {
    pub const ALL: [SumOf; 4] = [SumOf::InputTokens, SumOf::OutputTokens, SumOf::TotalTokens, SumOf::Cost];

    pub fn name(self) -> &'static str {
        match self {
            SumOf::InputTokens => "input_tokens",
            SumOf::OutputTokens => "output_tokens",
            SumOf::TotalTokens => "total_tokens",
            SumOf::Cost => "cost",
        }
    }
}

impl Metric {
    pub fn text(&self) -> String {
        match self {
            Metric::Count => "count".into(),
            Metric::ErrorRate => "error_rate".into(),
            Metric::Latency(p) => format!("{}(latency)", p.name()),
            Metric::Sum(s) => format!("sum({})", s.name()),
            Metric::AvgFeedback(k) => format!("avg(feedback.{k})"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    #[default]
    Table,
    Series,
    Dist,
}

/// Time-series bucket width.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Bucket {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "1m")]
    M1,
    #[serde(rename = "5m")]
    M5,
    #[serde(rename = "15m")]
    M15,
    #[serde(rename = "1h")]
    H1,
    #[serde(rename = "6h")]
    H6,
    #[serde(rename = "1d")]
    D1,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Compare {
    #[default]
    None,
    /// Side b is the same query with this filter in place of `filter`.
    Filter(Expr),
    /// Side b is the same window this much earlier, e.g. `"1d"` (`<n>s|m|h|d`).
    Shift(String),
}

/// Parses a stats line: `[by <key>[, <key>] |] <metric>, … [| top <n>]`. Fields other than group_by, metrics and
/// top are left at their defaults.
pub fn parse_stats(src: &str) -> Result<AggSpec, ParseError> {
    let mut c = Cursor {
        toks: lex(src, true)?,
        pos: 0,
        end: src.len(),
    };
    let mut spec = AggSpec::default();
    if c.peek().is_some_and(|t| t.tok == Tok::Ident("by".into())) {
        c.pos += 1;
        let mut keys = vec![c.key_token()?];
        while c.eat(&Tok::Comma) {
            keys.push(c.key_token()?);
        }
        if let Some((_, s, e)) = keys.get(MAX_GROUP_KEYS) {
            return Err(err(*s, *e, format!("at most {MAX_GROUP_KEYS} group keys")));
        }
        spec.group_by = keys
            .iter()
            .map(|(t, s, e)| GroupKey::resolve(t, *s, *e))
            .collect::<Result<_, _>>()?;
        if let Some(i) = repeat(&spec.group_by) {
            let (_, s, e) = keys[i];
            return Err(err(s, e, twice(&spec.group_by[i].text())));
        }
        c.expect(&Tok::Pipe, "\"|\"")?;
    }
    loop {
        let first = c.pos;
        let m = c.metric()?;
        if spec.metrics.contains(&m) {
            return Err(err(c.toks[first].start, c.toks[c.pos - 1].end, twice(&m.text())));
        }
        spec.metrics.push(m);
        if !c.eat(&Tok::Comma) {
            break;
        }
    }
    if c.eat(&Tok::Pipe) {
        let t = c.next("top")?;
        if t.tok != Tok::Ident("top".into()) {
            return Err(err(t.start, t.end, "expected top"));
        }
        let t = c.next("a number")?;
        spec.top = match t.tok {
            Tok::Num(n) if n.fract() == 0.0 && (1.0..=MAX_TOP as f64).contains(&n) => n as u32,
            _ => return Err(err(t.start, t.end, TOP_RANGE)),
        };
    }
    if let Some(t) = c.peek() {
        return Err(err(t.start, t.end, "expected end of stats line"));
    }
    Ok(spec)
}

/// Canonical stats line; `| top <n>` only when it isn't the default. `parse_stats(&print_stats(a))` gives back
/// `a`'s group_by, metrics and top.
pub fn print_stats(a: &AggSpec) -> String {
    let mut s = String::new();
    if !a.group_by.is_empty() {
        s.push_str("by ");
        s.push_str(&a.group_by.iter().map(GroupKey::text).collect::<Vec<_>>().join(", "));
        s.push_str(" | ");
    }
    s.push_str(&a.metrics.iter().map(Metric::text).collect::<Vec<_>>().join(", "));
    if a.top != DEFAULT_TOP {
        s.push_str(&format!(" | top {}", a.top));
    }
    s
}

const TOP_RANGE: &str = "top is a whole number from 1 to 50";

/// The index of the first item equal to an earlier one.
fn repeat<T: PartialEq>(items: &[T]) -> Option<usize> {
    (1..items.len()).find(|&i| items[..i].contains(&items[i]))
}

fn twice(text: &str) -> String {
    format!("{text:?} is listed twice")
}

struct Cursor {
    toks: Vec<Token>,
    pos: usize,
    end: usize,
}

impl Cursor {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        let hit = self.peek().is_some_and(|t| t.tok == *tok);
        self.pos += usize::from(hit);
        hit
    }

    fn next(&mut self, expected: &str) -> Result<Token, ParseError> {
        let t = self
            .peek()
            .cloned()
            .ok_or_else(|| err(self.end, self.end, format!("expected {expected}")))?;
        self.pos += 1;
        Ok(t)
    }

    fn expect(&mut self, tok: &Tok, shown: &str) -> Result<Token, ParseError> {
        let t = self.next(shown)?;
        if t.tok == *tok {
            Ok(t)
        } else {
            Err(err(t.start, t.end, format!("expected {shown}")))
        }
    }

    fn key_token(&mut self) -> Result<(String, usize, usize), ParseError> {
        match self.next("a group key")? {
            Token {
                tok: Tok::Ident(s),
                start,
                end,
            } => Ok((s, start, end)),
            t => Err(err(t.start, t.end, "expected a group key")),
        }
    }

    /// The argument of `fname(arg)` as an identifier; anything else is `wrong`.
    fn arg(&mut self, wrong: &str) -> Result<(String, usize, usize), ParseError> {
        self.expect(&Tok::LParen, "\"(\"")?;
        let t = self.next(wrong).map_err(|e| err(e.start, e.end, wrong))?;
        let Tok::Ident(a) = t.tok else {
            return Err(err(t.start, t.end, wrong));
        };
        self.expect(&Tok::RParen, "\")\"")?;
        Ok((a, t.start, t.end))
    }

    fn metric(&mut self) -> Result<Metric, ParseError> {
        const WANT: &str = "a metric such as count";
        let t = self.next(WANT)?;
        let Tok::Ident(name) = &t.tok else {
            return Err(err(t.start, t.end, format!("expected {WANT}")));
        };
        if let Some(p) = Pct::ALL.into_iter().find(|p| p.name() == name) {
            let msg = format!("{name}() works on latency");
            let (a, s, e) = self.arg(&msg)?;
            return if a == "latency" {
                Ok(Metric::Latency(p))
            } else {
                Err(err(s, e, msg))
            };
        }
        match name.as_str() {
            "count" => Ok(Metric::Count),
            "error_rate" => Ok(Metric::ErrorRate),
            "sum" => {
                let msg = "sum() works on input_tokens, output_tokens, total_tokens or cost";
                let (a, s, e) = self.arg(msg)?;
                SumOf::ALL
                    .into_iter()
                    .find(|x| x.name() == a)
                    .map(Metric::Sum)
                    .ok_or_else(|| err(s, e, msg))
            }
            "avg" => {
                let msg = "avg() works on feedback.<key>";
                let (a, s, e) = self.arg(msg)?;
                if a == "feedback" || a.starts_with("feedback.") {
                    let Path::Feedback(k) = resolve_path(&a, s, e)? else {
                        unreachable!("feedback paths resolve to Path::Feedback")
                    };
                    return Ok(Metric::AvgFeedback(k));
                }
                Err(err(s, e, msg))
            }
            n if n.len() > 1 && n.starts_with('p') && n[1..].bytes().all(|b| b.is_ascii_digit()) => Err(err(
                t.start,
                t.end,
                format!("unknown metric {n:?}; expected p50, p95 or p99"),
            )),
            n => Err(err(
                t.start,
                t.end,
                format!("unknown metric {n:?}; expected count, error_rate, p50, p95, p99, sum or avg"),
            )),
        }
    }
}

/// One stats-line item on its own (a JSON `group_by` or `metrics` entry).
fn single<T>(text: &str, what: &str, f: impl FnOnce(&mut Cursor) -> Result<T, ParseError>) -> Result<T, String> {
    let mut c = Cursor {
        toks: lex(text, true).map_err(|e| format!("{what}: {}", e.message))?,
        pos: 0,
        end: text.len(),
    };
    let v = f(&mut c).map_err(|e| format!("{what}: {}", e.message))?;
    match c.peek() {
        Some(_) => Err(format!("{what}: {text:?} is not one {what}")),
        None => Ok(v),
    }
}

#[derive(Serialize, Deserialize)]
struct RawSpec {
    #[serde(default)]
    group_by: Vec<String>,
    metrics: Vec<String>,
    #[serde(default)]
    view: View,
    #[serde(default = "default_top")]
    top: i64,
    #[serde(default)]
    bucket: Bucket,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compare: Option<RawCompare>,
}

#[derive(Serialize, Deserialize)]
struct RawCompare {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shift: Option<String>,
}

fn default_top() -> i64 {
    DEFAULT_TOP.into()
}

impl TryFrom<RawSpec> for AggSpec {
    type Error = String;

    fn try_from(r: RawSpec) -> Result<AggSpec, String> {
        if r.group_by.len() > MAX_GROUP_KEYS {
            return Err(format!("at most {MAX_GROUP_KEYS} group keys"));
        }
        if r.metrics.is_empty() {
            return Err("at least one metric".into());
        }
        let group_by: Vec<GroupKey> = r
            .group_by
            .iter()
            .map(|k| {
                single(k, "group key", |c| {
                    let (t, s, e) = c.key_token()?;
                    GroupKey::resolve(&t, s, e)
                })
            })
            .collect::<Result<_, _>>()?;
        let metrics: Vec<Metric> = r
            .metrics
            .iter()
            .map(|m| single(m, "metric", Cursor::metric))
            .collect::<Result<_, _>>()?;
        if let Some(i) = repeat(&group_by) {
            return Err(format!("group key: {}", twice(&group_by[i].text())));
        }
        if let Some(i) = repeat(&metrics) {
            return Err(format!("metric: {}", twice(&metrics[i].text())));
        }
        let top = u32::try_from(r.top)
            .ok()
            .filter(|t| (1..=MAX_TOP).contains(t))
            .ok_or(TOP_RANGE)?;
        let compare = match r.compare {
            None => Compare::None,
            Some(RawCompare {
                filter: Some(f),
                shift: None,
            }) => Compare::Filter(parse_filter(&f).map_err(|e| format!("compare.filter: {e}"))?),
            Some(RawCompare {
                filter: None,
                shift: Some(s),
            }) => {
                if !valid_shift(&s) {
                    return Err(format!(
                        "compare.shift {s:?}: expected a duration such as \"1d\" or \"6h\""
                    ));
                }
                Compare::Shift(s)
            }
            Some(_) => return Err("compare needs exactly one of filter or shift".into()),
        };
        Ok(AggSpec {
            group_by,
            metrics,
            view: r.view,
            top,
            bucket: r.bucket,
            compare,
        })
    }
}

/// `<n>s|m|h|d` with n > 0, within the relative-time range filters accept.
fn valid_shift(s: &str) -> bool {
    !s.starts_with(['+', '-'])
        && matches!(crate::filter::TimeAt::parse(&format!("-{s}")), Some(crate::filter::TimeAt::Rel(us)) if us < 0)
}

impl From<AggSpec> for RawSpec {
    fn from(a: AggSpec) -> RawSpec {
        RawSpec {
            group_by: a.group_by.iter().map(GroupKey::text).collect(),
            metrics: a.metrics.iter().map(Metric::text).collect(),
            view: a.view,
            top: a.top.into(),
            bucket: a.bucket,
            compare: match a.compare {
                Compare::None => None,
                Compare::Filter(e) => Some(RawCompare {
                    filter: Some(print(&e)),
                    shift: None,
                }),
                Compare::Shift(s) => Some(RawCompare {
                    filter: None,
                    shift: Some(s),
                }),
            },
        }
    }
}
