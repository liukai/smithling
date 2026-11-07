use super::ast::{Expr, Value};

/// Canonical text for an expression. `parse_filter(&print(e)) == Ok(e)` for every valid `e`.
pub fn print(e: &Expr) -> String {
    let mut s = String::new();
    write_expr(&mut s, e);
    s
}

fn write_expr(s: &mut String, e: &Expr) {
    match e {
        Expr::And(xs) | Expr::Or(xs) => {
            s.push_str(if matches!(e, Expr::And(_)) { "and(" } else { "or(" });
            for (i, x) in xs.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                write_expr(s, x);
            }
            s.push(')');
        }
        Expr::Not(x) => {
            s.push_str("not(");
            write_expr(s, x);
            s.push(')');
        }
        Expr::Cmp { op, path, value } => {
            s.push_str(op.name());
            s.push('(');
            s.push_str(&path.text());
            s.push_str(", ");
            write_value(s, value);
            s.push(')');
        }
        Expr::Has { path, value } => {
            s.push_str("has(");
            s.push_str(&path.text());
            s.push_str(", ");
            write_value(s, value);
            s.push(')');
        }
        Expr::In { path, values } => {
            s.push_str("in(");
            s.push_str(&path.text());
            s.push_str(", [");
            for (i, v) in values.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                write_value(s, v);
            }
            s.push_str("])");
        }
        Expr::Search { text, side, .. } => {
            s.push_str("search(");
            if let Some(side) = side.name() {
                s.push_str(side);
                s.push_str(", ");
            }
            write_value(s, &Value::Str(text.clone()));
            s.push(')');
        }
        Expr::Exists { path } => {
            s.push_str("exists(");
            s.push_str(&path.text());
            s.push(')');
        }
    }
}

pub(crate) fn write_value(s: &mut String, v: &Value) {
    match v {
        Value::Str(x) | Value::Time { raw: x, .. } => s.push_str(&serde_json::to_string(x).expect("string")),
        Value::Num(n) => s.push_str(&format_num(*n)),
        Value::Bool(b) => s.push_str(if *b { "true" } else { "false" }),
        Value::Null => s.push_str("null"),
    }
}

fn format_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}
