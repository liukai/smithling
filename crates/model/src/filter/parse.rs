use std::fmt;

use super::ast::{AttrRoot, CmpOp, Expr, Field, Kind, Path, Side, TimeAt, Value};

/// A parse or type error, pointing at a byte range of the input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub start: usize,
    pub end: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{} {}", self.start, self.end, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Parses one filter expression, e.g. `and(eq(status, "error"), gt(latency, 30))`.
pub fn parse_filter(src: &str) -> Result<Expr, ParseError> {
    let mut p = Parser {
        src,
        toks: lex(src, false)?,
        pos: 0,
    };
    let e = p.expr()?;
    if let Some(t) = p.toks.get(p.pos) {
        return Err(err(t.start, t.end, "expected end of filter"));
    }
    Ok(e)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Tok {
    Ident(String),
    Str(String),
    Num(f64),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    /// `|`, only in the stats line.
    Pipe,
}

#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub tok: Tok,
    pub start: usize,
    pub end: usize,
}

pub(crate) fn err(start: usize, end: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        start,
        end,
        message: message.into(),
    }
}

/// Tokens of `src`; `|` is a token only when `pipe` is set (the stats line).
pub(crate) fn lex(src: &str, pipe: bool) -> Result<Vec<Token>, ParseError> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        let single = match c {
            b'(' => Some(Tok::LParen),
            b')' => Some(Tok::RParen),
            b'[' => Some(Tok::LBracket),
            b']' => Some(Tok::RBracket),
            b',' => Some(Tok::Comma),
            b'|' if pipe => Some(Tok::Pipe),
            _ => None,
        };
        if let Some(tok) = single {
            out.push(Token { tok, start, end: i + 1 });
            i += 1;
        } else if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'"' {
            let (s, end) = lex_string(src, i)?;
            out.push(Token {
                tok: Tok::Str(s),
                start,
                end,
            });
            i = end;
        } else if c == b'-' || c.is_ascii_digit() {
            i += 1;
            while i < b.len() && (b[i].is_ascii_digit() || matches!(b[i], b'.' | b'e' | b'E' | b'+' | b'-')) {
                i += 1;
            }
            let text = &src[start..i];
            let n: f64 = text
                .parse()
                .map_err(|_| err(start, i, format!("bad number {text:?}")))?;
            if !n.is_finite() {
                return Err(err(start, i, format!("number {text:?} is out of range")));
            }
            out.push(Token {
                tok: Tok::Num(n),
                start,
                end: i,
            });
        } else if c.is_ascii_alphabetic()
            || c == b'_'
            || !c.is_ascii() && src[i..].chars().next().is_some_and(char::is_alphabetic)
        {
            // Identifiers: letters (any script, so metadata keys like `ключ` work), digits, `_ . -`.
            for ch in src[i..].chars() {
                if ch.is_alphanumeric() || matches!(ch, '_' | '.' | '-') {
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            out.push(Token {
                tok: Tok::Ident(src[start..i].to_string()),
                start,
                end: i,
            });
        } else {
            let ch = src[i..].chars().next().unwrap();
            return Err(err(i, i + ch.len_utf8(), format!("unexpected character {ch:?}")));
        }
    }
    Ok(out)
}

/// A JSON-style string starting at `i` (the opening quote). Returns the value and the end offset.
fn lex_string(src: &str, i: usize) -> Result<(String, usize), ParseError> {
    let mut out = String::new();
    let mut chars = src[i + 1..].char_indices().peekable();
    let pos = |off: usize| i + 1 + off;
    while let Some((off, ch)) = chars.next() {
        let at = pos(off);
        match ch {
            '"' => return Ok((out, at + 1)),
            '\\' => {
                let Some((eoff, e)) = chars.next() else { break };
                let esc_end = pos(eoff) + e.len_utf8();
                match e {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    'b' => out.push('\u{8}'),
                    'f' => out.push('\u{c}'),
                    'u' => {
                        let (hi, end) = hex4(src, esc_end)
                            .ok_or_else(|| err(at, esc_end, "bad \\u escape; expected 4 hex digits"))?;
                        skip_to(&mut chars, end, i);
                        let ch = if (0xD800..0xDC00).contains(&hi) {
                            // A surrogate pair, as JSON writes characters outside the BMP.
                            let low = src
                                .get(end..end + 2)
                                .filter(|p| *p == "\\u")
                                .and_then(|_| hex4(src, end + 2));
                            match low {
                                Some((lo, end2)) if (0xDC00..0xE000).contains(&lo) => {
                                    skip_to(&mut chars, end2, i);
                                    char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                                }
                                _ => None,
                            }
                        } else {
                            char::from_u32(hi)
                        };
                        out.push(ch.ok_or_else(|| err(at, end, "bad \\u escape"))?);
                    }
                    other => return Err(err(at, esc_end, format!("unknown escape \\{other}"))),
                }
            }
            c => out.push(c),
        }
    }
    Err(err(i, src.len(), "unterminated string; expected \""))
}

/// Exactly four hex digits at `at`: the value and the offset after them.
fn hex4(src: &str, at: usize) -> Option<(u32, usize)> {
    let h = src.get(at..at + 4)?;
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((u32::from_str_radix(h, 16).ok()?, at + 4))
}

fn skip_to(chars: &mut std::iter::Peekable<std::str::CharIndices>, end: usize, i: usize) {
    while chars.peek().is_some_and(|(off, _)| i + 1 + off < end) {
        chars.next();
    }
}

/// What a function call parses to: an expression, or one half of LangSmith's
/// `and(eq(feedback_key, "k"), op(feedback_score, x))`, which only its `and(…)` can turn into `op(feedback.k, x)`.
enum Node {
    Expr(Expr),
    FeedbackKey {
        key: String,
        span: (usize, usize),
    },
    FeedbackScore {
        op: CmpOp,
        value: Value,
        span: (usize, usize),
    },
}

const PAIR: &str = "feedback_key and feedback_score go together in one and(…); or write feedback.<key>";

/// The arguments of one `and(…)`: one `feedback_key` and at least one `feedback_score` become `feedback.<key>`
/// comparisons where the scores were; an `and` left with one argument is that argument.
fn feedback_pair(nodes: Vec<Node>) -> Result<Expr, ParseError> {
    let mut keys = nodes.iter().filter_map(|n| match n {
        Node::FeedbackKey { key, span } => Some((key, *span)),
        _ => None,
    });
    let key = keys.next();
    if let Some((_, span)) = keys.next() {
        return Err(err(span.0, span.1, PAIR));
    }
    let first_score = nodes.iter().find_map(|n| match n {
        Node::FeedbackScore { span, .. } => Some(*span),
        _ => None,
    });
    let key = match (key, first_score) {
        (None, None) => None,
        (Some((k, _)), Some(_)) => Some(k.clone()),
        (Some((_, span)), None) | (None, Some(span)) => return Err(err(span.0, span.1, PAIR)),
    };
    let mut args: Vec<Expr> = nodes
        .into_iter()
        .filter_map(|n| match n {
            Node::Expr(e) => Some(e),
            Node::FeedbackKey { .. } => None,
            Node::FeedbackScore { op, value, .. } => Some(Expr::Cmp {
                op,
                path: Path::Feedback(key.clone().expect("checked above")),
                value,
            }),
        })
        .collect();
    Ok(if key.is_some() && args.len() == 1 {
        args.pop().expect("one")
    } else {
        Expr::And(args)
    })
}

/// A key that prints and lexes back as `feedback.<key>`.
fn valid_key(key: &str) -> bool {
    !key.split('.').any(str::is_empty) && key.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    pos: usize,
}

const LOGIC: [&str; 3] = ["and", "or", "not"];
const LEAF_FUNCS: [&str; 4] = ["has", "in", "exists", "search"];
const CMP: [(&str, CmpOp); 6] = [
    ("eq", CmpOp::Eq),
    ("neq", CmpOp::Neq),
    ("gt", CmpOp::Gt),
    ("gte", CmpOp::Gte),
    ("lt", CmpOp::Lt),
    ("lte", CmpOp::Lte),
];

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }

    fn end_pos(&self) -> usize {
        self.src.len()
    }

    fn next(&mut self, expected: &str) -> Result<Token, ParseError> {
        match self.toks.get(self.pos) {
            Some(t) => {
                self.pos += 1;
                Ok(t.clone())
            }
            None => Err(err(self.end_pos(), self.end_pos(), format!("expected {expected}"))),
        }
    }

    fn expect(&mut self, tok: Tok, shown: &str) -> Result<Token, ParseError> {
        let t = self.next(shown)?;
        if t.tok == tok {
            Ok(t)
        } else {
            Err(err(t.start, t.end, format!("expected {shown}")))
        }
    }

    fn expr(&mut self) -> Result<Expr, ParseError> {
        match self.node()? {
            Node::Expr(e) => Ok(e),
            Node::FeedbackKey { span, .. } | Node::FeedbackScore { span, .. } => Err(err(span.0, span.1, PAIR)),
        }
    }

    fn node(&mut self) -> Result<Node, ParseError> {
        let t = self.next("a function such as eq( or and(")?;
        let Tok::Ident(name) = &t.tok else {
            return Err(err(t.start, t.end, "expected a function such as eq( or and("));
        };
        let name = name.as_str();
        if !(LOGIC.contains(&name) || CMP.iter().any(|(n, _)| *n == name) || LEAF_FUNCS.contains(&name)) {
            return Err(err(
                t.start,
                t.end,
                format!(
                    "unknown function {name:?}; expected and, or, not, eq, neq, gt, gte, lt, lte, has, in, exists or search"
                ),
            ));
        }
        self.expect(Tok::LParen, "\"(\"")?;
        let e = match name {
            "and" => {
                let mut nodes = vec![self.node()?];
                while self.peek().is_some_and(|t| t.tok == Tok::Comma) {
                    self.pos += 1;
                    nodes.push(self.node()?);
                }
                feedback_pair(nodes)?
            }
            "or" => {
                let mut args = vec![self.expr()?];
                while self.peek().is_some_and(|t| t.tok == Tok::Comma) {
                    self.pos += 1;
                    args.push(self.expr()?);
                }
                Expr::Or(args)
            }
            "not" => Expr::Not(Box::new(self.expr()?)),
            "exists" => {
                let at = self.peek().map_or((self.end_pos(), self.end_pos()), |t| match t.tok {
                    Tok::RParen => (t.start, t.start),
                    _ => (t.start, t.end),
                });
                if !self.peek().is_some_and(|t| matches!(t.tok, Tok::Ident(_))) {
                    return Err(err(at.0, at.1, "exists() needs a field path"));
                }
                Expr::Exists { path: self.path()?.0 }
            }
            "search" => self.search()?,
            "has" => {
                let (path, ps, pe) = self.path()?;
                self.expect(Tok::Comma, "\",\"")?;
                let (value, vs, ve) = self.value()?;
                if path != Path::Field(Field::Tags) {
                    return Err(err(
                        ps,
                        pe,
                        format!("has() works on lists; {:?} isn't one (use eq)", path.text()),
                    ));
                }
                if !matches!(value, Value::Str(_)) {
                    return Err(err(vs, ve, "has(tags, …) needs a string value"));
                }
                Expr::Has { path, value }
            }
            "in" => {
                let (path, ps, pe) = self.path()?;
                self.expect(Tok::Comma, "\",\"")?;
                self.expect(Tok::LBracket, "\"[\"")?;
                let mut values = Vec::new();
                if !self.peek().is_some_and(|t| t.tok == Tok::RBracket) {
                    loop {
                        let (v, vs, ve) = self.value()?;
                        values.push(check_value(&path, (ps, pe), CmpOp::Eq, v, (vs, ve))?);
                        if self.peek().is_some_and(|t| t.tok == Tok::Comma) {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                }
                self.expect(Tok::RBracket, "\"]\"")?;
                if values.is_empty() {
                    return Err(err(ps, pe, "in() needs at least one value"));
                }
                Expr::In { path, values }
            }
            _ => {
                let op = CMP.iter().find(|(n, _)| *n == name).unwrap().1;
                if let Some(n) = self.feedback_pair_part(op)? {
                    self.expect(Tok::RParen, "\")\"")?;
                    return Ok(n);
                }
                let (path, ps, pe) = self.path()?;
                self.expect(Tok::Comma, "\",\"")?;
                let (value, vs, ve) = self.value()?;
                let value = check_value(&path, (ps, pe), op, value, (vs, ve))?;
                Expr::Cmp { op, path, value }
            }
        };
        self.expect(Tok::RParen, "\")\"")?;
        Ok(Node::Expr(e))
    }

    /// `search(text)` or `search(side, text)`, after the `(`.
    fn search(&mut self) -> Result<Expr, ParseError> {
        let mut side = Side::Any;
        if let Some(Token {
            tok: Tok::Ident(name),
            start,
            end,
        }) = self.peek().cloned()
        {
            side = [Side::Inputs, Side::Outputs, Side::Error]
                .into_iter()
                .find(|s| s.name() == Some(name.as_str()))
                .ok_or_else(|| err(start, end, "search side is inputs, outputs or error"))?;
            self.pos += 1;
            if self.peek().is_some_and(|t| t.tok != Tok::RParen) {
                self.expect(Tok::Comma, "\",\"")?;
            }
        }
        let (start, end) = match self.peek() {
            None => (self.end_pos(), self.end_pos()),
            Some(t) if t.tok == Tok::RParen => (t.start, t.start),
            Some(t) => (t.start, t.end),
        };
        let Some(Tok::Str(text)) = self.peek().map(|t| t.tok.clone()) else {
            return Err(err(start, end, "search needs a string"));
        };
        self.pos += 1;
        let words = crate::text::tokens(&text);
        if words.is_empty() {
            return Err(err(start, end, "search needs at least one word"));
        }
        Ok(Expr::Search { text, words, side })
    }

    /// `eq(feedback_key, "k")` or `op(feedback_score, x)` of the LangSmith pair, after the `(`; `None` (nothing
    /// read) for any other path.
    fn feedback_pair_part(&mut self, op: CmpOp) -> Result<Option<Node>, ParseError> {
        let Some(Token {
            tok: Tok::Ident(name),
            start: ps,
            end: pe,
        }) = self.peek().cloned()
        else {
            return Ok(None);
        };
        if !matches!(name.as_str(), "feedback_key" | "feedback_score") {
            return Ok(None);
        }
        self.pos += 1;
        self.expect(Tok::Comma, "\",\"")?;
        let (value, vs, ve) = self.value()?;
        let span = (ps, pe);
        if name == "feedback_score" {
            let value = check_value_as("feedback_score", Kind::Num, span, op, value, (vs, ve))?;
            return Ok(Some(Node::FeedbackScore { op, value, span }));
        }
        let (CmpOp::Eq, Value::Str(key)) = (op, value) else {
            return Err(err(ps, pe, "feedback_key is compared with eq and a \"string\""));
        };
        if !valid_key(&key) {
            return Err(err(
                vs,
                ve,
                format!("feedback key {key:?} can't be written as feedback.<key>; use letters, digits, _ . or -"),
            ));
        }
        Ok(Some(Node::FeedbackKey { key, span }))
    }

    fn path(&mut self) -> Result<(Path, usize, usize), ParseError> {
        let t = self.next("a field name")?;
        let Tok::Ident(text) = &t.tok else {
            return Err(err(t.start, t.end, "expected a field name"));
        };
        Ok((resolve_path(text, t.start, t.end)?, t.start, t.end))
    }

    fn value(&mut self) -> Result<(Value, usize, usize), ParseError> {
        let t = self.next("a value")?;
        let v = match &t.tok {
            Tok::Str(s) => Value::Str(s.clone()),
            Tok::Num(n) => Value::Num(*n),
            Tok::Ident(s) if s == "true" => Value::Bool(true),
            Tok::Ident(s) if s == "false" => Value::Bool(false),
            Tok::Ident(s) if s == "null" => Value::Null,
            _ => {
                return Err(err(
                    t.start,
                    t.end,
                    "expected a value: a \"string\", number, true, false or null",
                ));
            }
        };
        Ok((v, t.start, t.end))
    }
}

const FEEDBACK_NEEDS_KEY: &str = "feedback needs a key: feedback.<key>";

pub(crate) fn resolve_path(text: &str, start: usize, end: usize) -> Result<Path, ParseError> {
    if matches!(text, "feedback_key" | "feedback_score") {
        return Err(err(start, end, PAIR));
    }
    if text == "feedback" || text == "feedback." {
        return Err(err(start, end, FEEDBACK_NEEDS_KEY));
    }
    if let Some(key) = text.strip_prefix("feedback.") {
        if !valid_key(key) {
            return Err(err(start, end, format!("bad path {text:?}")));
        }
        return Ok(Path::Feedback(key.to_string()));
    }
    if let Some((root, key)) = text.split_once('.') {
        let root = AttrRoot::ALL.into_iter().find(|r| r.name() == root).ok_or_else(|| {
            err(
                start,
                end,
                format!("unknown field {text:?}; JSON paths start with metadata., inputs. or outputs."),
            )
        })?;
        if key.is_empty() || key.split('.').any(str::is_empty) {
            return Err(err(start, end, format!("bad path {text:?}")));
        }
        return Ok(Path::Attr {
            root,
            key: key.to_string(),
        });
    }
    if let Some(f) = Field::from_name(text) {
        return Ok(Path::Field(f));
    }
    let names = Field::ALL
        .iter()
        .map(|f| f.name())
        .chain(AttrRoot::ALL.iter().map(|r| r.name()));
    let best = names
        .map(|n| (edit_distance(n, text), n))
        .min()
        .filter(|(d, _)| *d <= 2);
    let hint = best.map(|(_, n)| format!(" (did you mean {n:?}?)")).unwrap_or_default();
    Err(err(start, end, format!("unknown field {text:?}{hint}")))
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

const STATUSES: [&str; 3] = ["running", "ok", "error"];

/// Type-checks `op(path, value)` and converts time strings into time values.
fn check_value(
    path: &Path,
    ps: (usize, usize),
    op: CmpOp,
    value: Value,
    vs: (usize, usize),
) -> Result<Value, ParseError> {
    check_value_as(&path.text(), path.kind(), ps, op, value, vs)
}

fn check_value_as(
    name: &str,
    kind: Kind,
    (ps, pe): (usize, usize),
    op: CmpOp,
    value: Value,
    (vs, ve): (usize, usize),
) -> Result<Value, ParseError> {
    let ordered = !matches!(op, CmpOp::Eq | CmpOp::Neq);
    if value == Value::Null {
        return if ordered {
            Err(err(
                vs,
                ve,
                format!("null can only be used with eq or neq, not {}()", op.name()),
            ))
        } else {
            Ok(value)
        };
    }
    let mismatch = |what: &str| {
        err(
            ps,
            pe,
            format!("{name:?} is {what}; {}() needs {}", op.name(), needs(kind)),
        )
    };
    match kind {
        Kind::Str => match value {
            Value::Str(_) => Ok(value),
            _ => Err(mismatch("a string")),
        },
        Kind::Status => match &value {
            _ if ordered => Err(err(
                ps,
                pe,
                format!("status can only be compared with eq, neq or in, not {}()", op.name()),
            )),
            Value::Str(s) if STATUSES.contains(&s.as_str()) => Ok(value),
            _ => Err(err(vs, ve, "status is one of \"running\", \"ok\", \"error\"")),
        },
        Kind::Time => match &value {
            Value::Str(s) => match TimeAt::parse(s) {
                Some(at) => Ok(Value::Time { raw: s.clone(), at }),
                None => Err(err(
                    vs,
                    ve,
                    format!("{name} needs an ISO-8601 time or a relative time like \"-12h\""),
                )),
            },
            _ => Err(mismatch("a time")),
        },
        Kind::Num => match value {
            Value::Num(_) => Ok(value),
            _ => Err(mismatch("a number")),
        },
        Kind::List => Err(err(ps, pe, format!("{name} is a list; use has({name}, …)"))),
        Kind::Id => match &value {
            _ if ordered => Err(err(ps, pe, format!("{name} can only be compared with eq, neq or in"))),
            Value::Str(s) if uuid::Uuid::try_parse(s).is_ok() => Ok(value),
            _ => Err(err(vs, ve, format!("{name} needs a UUID string"))),
        },
        Kind::Any => match value {
            Value::Bool(_) if ordered => Err(err(
                vs,
                ve,
                format!("true/false can only be used with eq or neq, not {}()", op.name()),
            )),
            _ => Ok(value),
        },
    }
}

fn needs(kind: Kind) -> &'static str {
    match kind {
        Kind::Str => "a \"string\" value",
        Kind::Time => "a time such as \"2026-11-05T00:00:00Z\" or \"-12h\"",
        Kind::Num => "a number",
        _ => "a value of the same type",
    }
}
