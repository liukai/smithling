use crate::ts::Ts;

/// A filter expression: LangSmith's `filter` grammar plus tracedb's dotted JSON paths and `in()`.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Cmp {
        op: CmpOp,
        path: Path,
        value: Value,
    },
    /// List contains (`has(tags, "x")`).
    Has {
        path: Path,
        value: Value,
    },
    /// One of (`in(name, ["a", "b"])`).
    In {
        path: Path,
        values: Vec<Value>,
    },
    /// The words of `text`, in order, inside one text value of `side`. `words` is `text::tokens(text)`.
    Search {
        text: String,
        words: Vec<String>,
        side: Side,
    },
    /// The path has a value (same result as `neq(path, null)`).
    Exists {
        path: Path,
    },
}

/// Where `search` looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Any,
    Inputs,
    Outputs,
    Error,
}

impl Side {
    /// The name written in `search(side, …)`; `None` for [`Side::Any`], which is written without one.
    pub fn name(self) -> Option<&'static str> {
        match self {
            Side::Any => None,
            Side::Inputs => Some("inputs"),
            Side::Outputs => Some("outputs"),
            Side::Error => Some("error"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Neq,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl CmpOp {
    pub fn name(self) -> &'static str {
        match self {
            CmpOp::Eq => "eq",
            CmpOp::Neq => "neq",
            CmpOp::Gt => "gt",
            CmpOp::Gte => "gte",
            CmpOp::Lt => "lt",
            CmpOp::Lte => "lte",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Path {
    Field(Field),
    /// `metadata.a.b`, `inputs.x`, `outputs.status_code`: `key` is the part after the root.
    Attr {
        root: AttrRoot,
        key: String,
    },
    /// `feedback.<key>`: the mean of the run's feedback scores for `key`; missing if none.
    Feedback(String),
}

impl Path {
    /// The path as written, e.g. `status` or `outputs.status_code`.
    pub fn text(&self) -> String {
        match self {
            Path::Field(f) => f.name().to_string(),
            Path::Attr { root, key } => format!("{}.{key}", root.name()),
            Path::Feedback(key) => format!("feedback.{key}"),
        }
    }

    pub fn kind(&self) -> Kind {
        match self {
            Path::Field(f) => f.kind(),
            Path::Attr { .. } => Kind::Any,
            Path::Feedback(_) => Kind::Num,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AttrRoot {
    Metadata,
    Inputs,
    Outputs,
}

impl AttrRoot {
    pub fn name(self) -> &'static str {
        match self {
            AttrRoot::Metadata => "metadata",
            AttrRoot::Inputs => "inputs",
            AttrRoot::Outputs => "outputs",
        }
    }

    pub const ALL: [AttrRoot; 3] = [AttrRoot::Metadata, AttrRoot::Inputs, AttrRoot::Outputs];
}

/// Built-in run fields a filter can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Field {
    Status,
    Name,
    RunType,
    Model,
    ThreadId,
    Project,
    Error,
    StartTime,
    EndTime,
    /// Seconds, as in LangSmith.
    Latency,
    Tags,
    TotalTokens,
    InputTokens,
    OutputTokens,
    TraceId,
    RunId,
    ParentRunId,
}

/// What kind of value a path holds; used to type-check filters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Str,
    Status,
    Time,
    Num,
    List,
    Id,
    /// A JSON path: any scalar, typed per run.
    Any,
}

impl Field {
    pub const ALL: [Field; 17] = [
        Field::Status,
        Field::Name,
        Field::RunType,
        Field::Model,
        Field::ThreadId,
        Field::Project,
        Field::Error,
        Field::StartTime,
        Field::EndTime,
        Field::Latency,
        Field::Tags,
        Field::TotalTokens,
        Field::InputTokens,
        Field::OutputTokens,
        Field::TraceId,
        Field::RunId,
        Field::ParentRunId,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Field::Status => "status",
            Field::Name => "name",
            Field::RunType => "run_type",
            Field::Model => "model",
            Field::ThreadId => "thread_id",
            Field::Project => "project",
            Field::Error => "error",
            Field::StartTime => "start_time",
            Field::EndTime => "end_time",
            Field::Latency => "latency",
            Field::Tags => "tags",
            Field::TotalTokens => "total_tokens",
            Field::InputTokens => "input_tokens",
            Field::OutputTokens => "output_tokens",
            Field::TraceId => "trace_id",
            Field::RunId => "run_id",
            Field::ParentRunId => "parent_run_id",
        }
    }

    pub fn from_name(s: &str) -> Option<Field> {
        Field::ALL.into_iter().find(|f| f.name() == s)
    }

    pub fn kind(self) -> Kind {
        match self {
            Field::Status => Kind::Status,
            Field::Name | Field::RunType | Field::Model | Field::ThreadId | Field::Project | Field::Error => Kind::Str,
            Field::StartTime | Field::EndTime => Kind::Time,
            Field::Latency | Field::TotalTokens | Field::InputTokens | Field::OutputTokens => Kind::Num,
            Field::Tags => Kind::List,
            Field::TraceId | Field::RunId | Field::ParentRunId => Kind::Id,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Str(String),
    Num(f64),
    Bool(bool),
    Null,
    /// A time literal. `raw` is kept so printing gives back what was written.
    Time {
        raw: String,
        at: TimeAt,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeAt {
    Abs(Ts),
    /// Relative to "now" at evaluation time, in microseconds (negative = in the past).
    Rel(i64),
}

impl TimeAt {
    pub fn resolve(self, now: Ts) -> Ts {
        match self {
            TimeAt::Abs(t) => t,
            TimeAt::Rel(d) => Ts(now.0.saturating_add(d)),
        }
    }

    /// Parses `"-12h"`, `"-30m"`, `"-7d"`, `"-45s"` (also `+`), or an absolute ISO-8601 time.
    pub fn parse(s: &str) -> Option<TimeAt> {
        if let Some(rel) = parse_relative(s) {
            return Some(TimeAt::Rel(rel));
        }
        Ts::parse(s).map(TimeAt::Abs)
    }
}

/// Relative times are bounded to ±100 years so resolving them can't overflow.
const MAX_RELATIVE_US: i64 = 100 * 366 * 86_400_000_000;

fn parse_relative(s: &str) -> Option<i64> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'-' => (-1, &s[1..]),
        b'+' => (1, &s[1..]),
        _ => return None,
    };
    let unit = rest.chars().last()?;
    let digits = &rest[..rest.len() - unit.len_utf8()];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    let us = match unit {
        's' => 1_000_000,
        'm' => 60_000_000,
        'h' => 3_600_000_000,
        'd' => 86_400_000_000,
        _ => return None,
    };
    n.checked_mul(us).filter(|v| *v <= MAX_RELATIVE_US).map(|v| sign * v)
}
