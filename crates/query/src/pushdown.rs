//! Filter AST → DataFusion expression for file scans.
//!
//! The rule that keeps results exact: a pushed-down expression may keep rows the filter rejects, but it must
//! never drop a row the filter accepts. The final decision is always [`tracedb_model::filter::eval`] on the
//! merged run. So every leaf here either matches [`eval`]'s semantics exactly on single-version rows
//! (absent values never match, types never mix), keeps a known superset (`exact: false`, never negated), or
//! is left out (`None` = "can't push, keep everything").
//!
//! [`eval`]: tracedb_model::filter::eval

use datafusion::arrow::datatypes::DataType;
use datafusion::logical_expr::{Expr as DfExpr, col, lit};
use datafusion::prelude::cast;
use datafusion::scalar::ScalarValue;
use tracedb_model::Ts;
use tracedb_model::attrs::promotable_path;
use tracedb_model::filter::{CmpOp, Expr, Field, Path, Value};

/// A pushed-down expression and whether it is exact (equal to `eval`, not just a superset).
pub struct Pushed {
    pub expr: DfExpr,
    pub exact: bool,
}

/// `None` means nothing can be pushed (every row must be read).
pub fn compile(e: &Expr, now: Ts) -> Option<Pushed> {
    match e {
        Expr::And(xs) => {
            let parts: Vec<Option<Pushed>> = xs.iter().map(|x| compile(x, now)).collect();
            let exact = parts.iter().all(|p| p.as_ref().is_some_and(|p| p.exact));
            let exprs: Vec<DfExpr> = parts.into_iter().flatten().map(|p| p.expr).collect();
            let expr = exprs.into_iter().reduce(DfExpr::and)?;
            Some(Pushed { expr, exact })
        }
        Expr::Or(xs) => {
            // A branch we can't push could match anything, so the whole OR can't be pushed.
            let parts: Option<Vec<Pushed>> = xs.iter().map(|x| compile(x, now)).collect();
            let parts = parts?;
            let exact = parts.iter().all(|p| p.exact);
            let expr = parts.into_iter().map(|p| p.expr).reduce(DfExpr::or)?;
            Some(Pushed { expr, exact })
        }
        Expr::Not(x) => {
            // Only an exact inner expression can be negated. `IS TRUE` turns SQL NULL into false first, so
            // a missing value negates to "matches", as in `eval`.
            let inner = compile(x, now).filter(|p| p.exact)?;
            Some(Pushed {
                expr: !inner.expr.is_true(),
                exact: true,
            })
        }
        Expr::Has {
            path: Path::Field(Field::Tags),
            value: Value::Str(v),
        } => Some(Pushed {
            expr: datafusion::functions_nested::expr_fn::array_has(col("tags"), lit(v.clone())).is_true(),
            exact: true,
        }),
        Expr::Has { .. } => None,
        Expr::In { path, values } => {
            let parts: Option<Vec<Pushed>> = values.iter().map(|v| leaf(CmpOp::Eq, path, v, now)).collect();
            let parts = parts?;
            let exact = parts.iter().all(|p| p.exact);
            Some(Pushed {
                expr: parts.into_iter().map(|p| p.expr).reduce(DfExpr::or)?,
                exact,
            })
        }
        Expr::Cmp { op, path, value } => leaf(*op, path, value, now),
        // `neq(p, null)`: `IS NOT NULL` on a built-in column, nothing on a JSON path.
        Expr::Exists { path } => leaf(CmpOp::Neq, path, &Value::Null, now),
        // Needs payload text, which the core file doesn't have.
        Expr::Search { .. } => None,
    }
}

fn op(o: CmpOp, l: DfExpr, r: DfExpr) -> DfExpr {
    match o {
        CmpOp::Eq => l.eq(r),
        CmpOp::Neq => l.not_eq(r),
        CmpOp::Gt => l.gt(r),
        CmpOp::Gte => l.gt_eq(r),
        CmpOp::Lt => l.lt(r),
        CmpOp::Lte => l.lt_eq(r),
    }
}

fn exact(expr: DfExpr) -> Option<Pushed> {
    // `IS TRUE` so that a NULL comparison (absent value) is false, never NULL.
    Some(Pushed {
        expr: expr.is_true(),
        exact: true,
    })
}

fn leaf(o: CmpOp, path: &Path, value: &Value, now: Ts) -> Option<Pushed> {
    let f = match path {
        Path::Field(f) => f,
        Path::Attr { .. } => return attr(o, path, value),
        Path::Feedback(_) => return None,
    };
    // Fields whose stored column can differ from what `eval` sees for an absent value are not pushed:
    // project (absent = "default"), run_type (absent = "other"), error ("" counts as absent).
    let column = match f {
        Field::Status => "status",
        Field::Name => "name",
        Field::Model => "model",
        Field::ThreadId => "thread_id",
        Field::StartTime => "start_time",
        Field::EndTime => "end_time",
        Field::Latency => "latency_ms",
        Field::TotalTokens => "total_tokens",
        Field::InputTokens => "input_tokens",
        Field::OutputTokens => "output_tokens",
        Field::TraceId => "trace_id",
        Field::RunId => "run_id",
        Field::ParentRunId => "parent_run_id",
        Field::Project | Field::RunType | Field::Error | Field::Tags => return None,
    };
    if *value == Value::Null {
        return match o {
            CmpOp::Eq => exact(col(column).is_null()),
            CmpOp::Neq => exact(col(column).is_not_null()),
            _ => None,
        };
    }
    match (f, value) {
        (Field::Status | Field::Name | Field::Model | Field::ThreadId, Value::Str(s)) => {
            exact(op(o, col(column), lit(s.clone())))
        }
        (Field::StartTime | Field::EndTime, Value::Time { at, .. }) => {
            exact(op(o, col(column), lit(at.resolve(now).0)))
        }
        (Field::Latency, Value::Num(n)) => exact(op(o, cast(col(column), DataType::Float64) / lit(1000.0), lit(*n))),
        (Field::TotalTokens | Field::InputTokens | Field::OutputTokens, Value::Num(n)) => {
            exact(op(o, cast(col(column), DataType::Float64), lit(*n)))
        }
        (Field::TraceId | Field::RunId | Field::ParentRunId, Value::Str(s)) if matches!(o, CmpOp::Eq | CmpOp::Neq) => {
            let id = uuid::Uuid::try_parse(s).ok()?;
            exact(op(o, col(column), id_lit(id)))
        }
        _ => None,
    }
}

/// `eq` on a promoted string, as a text search of the `attrs` JSON column (`"path":"value"`); a superset.
/// Rows where the path holds a value of another type pass too, so `eval` still reports mixed types.
fn attr(o: CmpOp, path: &Path, value: &Value) -> Option<Pushed> {
    let (CmpOp::Eq, Value::Str(v)) = (o, value) else {
        return None;
    };
    let p = path.text();
    if !promotable_path(&p) {
        return None;
    }
    let key = format!("{}:", serde_json::to_string(&p).ok()?);
    let has = |text: &str| {
        let escaped: String = text
            .chars()
            .flat_map(|c| match c {
                '%' | '_' | '\\' => vec!['\\', c],
                c => vec![c],
            })
            .collect();
        col("attrs").like(lit(format!("%{escaped}%")))
    };
    let hit = has(&format!("{key}{}", serde_json::to_string(v).ok()?));
    let other_type = has(&key).and(!has(&format!("{key}\"")));
    Some(Pushed {
        expr: hit.or(other_type),
        exact: false,
    })
}

pub fn id_lit(id: uuid::Uuid) -> DfExpr {
    lit(ScalarValue::FixedSizeBinary(16, Some(id.as_bytes().to_vec())))
}
