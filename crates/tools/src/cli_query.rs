//! `smithling query`: one `POST /api/query` against a running server, printed as a table or JSON lines; with
//! `--group-by` or `--metrics`, one `POST /api/aggregate` printed as an aggregate table.

use std::io::Write;
use std::time::Duration;

use serde_json::{Value, json};

#[derive(clap::Args, Debug)]
pub struct QueryArgs {
    #[arg(long, default_value = "http://localhost:7070", env = "SMITHLING_URL")]
    pub url: String,
    #[arg(long)]
    pub project: Option<String>,
    /// Only runs that started in the last span, e.g. `12h`, `30m`, `7d`.
    #[arg(long)]
    pub time: Option<String>,
    /// Only root runs.
    #[arg(long)]
    pub root: bool,
    /// Conditions on the root of each run's trace.
    #[arg(long)]
    pub trace_filter: Option<String>,
    /// Conditions on any run in each run's trace.
    #[arg(long)]
    pub tree_filter: Option<String>,
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=200))]
    pub limit: u16,
    /// One JSON object per run, per line (with `--group-by`/`--metrics`: the aggregate response).
    #[arg(long)]
    pub json: bool,
    /// Group by up to two keys: name, run_type, model, status, tag, metadata.<key>.
    #[arg(long, value_name = "KEY[,KEY]")]
    pub group_by: Option<String>,
    /// e.g. `count,error_rate,p95(latency),sum(cost)` (default `count`).
    #[arg(long, value_name = "LIST")]
    pub metrics: Option<String>,
    /// Groups shown before `(other)`.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=50))]
    pub top: u32,
    /// Words or an exact phrase in inputs, outputs or error: adds `search("TEXT")` to the filter.
    #[arg(long, value_name = "TEXT")]
    pub search: Option<String>,
    /// Side b of a comparison: these runs instead of the filter's, e.g. `has(tags, "batch=b")`.
    #[arg(long, value_name = "EXPR", conflicts_with = "shift")]
    pub compare: Option<String>,
    /// Side b of a comparison: the same window this much earlier, e.g. `1d`.
    #[arg(long, value_name = "SPAN")]
    pub shift: Option<String>,
    /// Conditions on the run itself, e.g. `eq(status, "error")`.
    pub filter: Option<String>,
}

/// The filter sent: `--search` adds `search("TEXT")` to EXPR as `and(EXPR, search("TEXT"))`.
fn filter_text(a: &QueryArgs) -> Option<String> {
    let search = a.search.as_ref().map(|t| {
        let quoted = serde_json::to_string(t).expect("a string serializes");
        format!("search({quoted})")
    });
    match (&a.filter, search) {
        // A bad EXPR is sent alone, so the server's error points into it (and `a, b` can't pass inside `and`).
        (Some(f), Some(_)) if smithling_model::filter::parse_filter(f).is_err() => Some(f.clone()),
        (Some(f), Some(s)) => Some(format!("and({f}, {s})")),
        (f, s) => s.or_else(|| f.clone()),
    }
}

/// Runs the query and prints it; returns the exit code (contracts/cli.md: 1 = failure, 2 = not reachable).
pub async fn run(a: &QueryArgs, out: &mut impl Write, err: &mut impl Write) -> u8 {
    let mut body = json!({
        "project": a.project,
        "time": a.time.as_ref().map(|t| json!({"last": t})),
        "filter": filter_text(a),
        "trace_filter": a.trace_filter,
        "tree_filter": a.tree_filter,
        "is_root": a.root.then_some(true),
    });
    let grouped = a.group_by.is_some() || a.metrics.is_some() || a.compare.is_some() || a.shift.is_some();
    let list = |s: &str| -> Vec<String> {
        s.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    };
    // Canonical names (`p95(latency)`) for the table's columns; the server reports any error.
    let given = a.metrics.as_deref().unwrap_or("count");
    let metrics = match smithling_model::agg::parse_stats(&list(given).join(", ")) {
        Ok(spec) => spec.metrics.iter().map(smithling_model::agg::Metric::text).collect(),
        Err(_) => list(given),
    };
    if grouped {
        body["group_by"] = json!(list(a.group_by.as_deref().unwrap_or("")));
        body["metrics"] = json!(metrics);
        body["top"] = json!(a.top);
        if let Some(c) = &a.compare {
            body["compare"] = json!({"filter": c});
        } else if let Some(s) = &a.shift {
            body["compare"] = json!({"shift": s});
        }
    } else {
        body["limit"] = json!(a.limit);
    }
    let path = if grouped { "/api/aggregate" } else { "/api/query" };
    let url = format!("{}{path}", a.url.trim_end_matches('/'));
    let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build();
    let sent = match http {
        Ok(h) => h.post(&url).json(&body).send().await,
        Err(e) => Err(e),
    };
    let resp = match sent {
        Ok(r) => r,
        Err(e) if e.is_connect() || e.is_timeout() => {
            let _ = writeln!(err, "smithling query: smithling is not reachable at {}: {e}", a.url);
            return 2;
        }
        Err(e) => {
            let _ = writeln!(err, "smithling query: {e}");
            return 1;
        }
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        let _ = writeln!(err, "smithling query: {url} answered {status}: {}", text.trim());
        return 1;
    };
    let ok = status.is_success();
    if !ok {
        let _ = report_error(a, &v["error"], err);
        return 1;
    }
    for w in v["warnings"].as_array().into_iter().flatten() {
        let _ = writeln!(err, "warning: {w}");
    }
    if let Some(ids) = v["skipped_files"].as_array() {
        let _ = writeln!(err, "warning: damaged data files skipped: {ids:?}");
    }
    if grouped {
        let printed = if a.json {
            writeln!(out, "{v}")
        } else {
            let keys = list(a.group_by.as_deref().unwrap_or(""));
            agg_table(&v, &keys, &metrics, out)
        };
        return if printed.is_ok() { 0 } else { 1 };
    }
    let runs = v["runs"].as_array().map_or(&[][..], Vec::as_slice);
    let printed = if a.json {
        runs.iter().try_for_each(|r| writeln!(out, "{r}"))
    } else {
        let matched = v["stats"]["matched"].as_u64().unwrap_or(runs.len() as u64);
        table(runs, matched, v["stats"]["complete"].as_bool().unwrap_or(true), out)
    };
    match printed {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

fn report_error(a: &QueryArgs, e: &Value, err: &mut impl Write) -> std::io::Result<()> {
    let s = |k: &str| e[k].as_str().unwrap_or("");
    let (flag, text) = match s("slot") {
        "trace_filter" => ("--trace-filter", &a.trace_filter),
        "tree_filter" => ("--tree-filter", &a.tree_filter),
        "filter" => ("EXPR", &a.filter),
        _ if s("code") == "bad_stats" => {
            return writeln!(err, "smithling query: --group-by/--metrics: {}", s("message"));
        }
        _ if s("code") == "bad_time" => {
            return writeln!(
                err,
                "smithling query: --time {:?}: use e.g. 12h, 30m or 7d",
                a.time.as_deref().unwrap_or("")
            );
        }
        _ => {
            return writeln!(err, "smithling query: {}: {}", s("code"), s("message"));
        }
    };
    let mut at = (e["start"].as_u64(), e["end"].as_u64());
    if s("slot") == "filter" && filter_text(a).is_some_and(|f| a.filter.as_ref() != Some(&f)) {
        // Sent as `and(EXPR, search(…))`, or the search alone.
        let expr = a.filter.as_ref().map_or(0, |f| f.len() as u64 + 4);
        if at.0.is_none_or(|start| start >= expr) {
            return writeln!(err, "smithling query: --search: {}", s("message"));
        }
        at = (None, None);
    }
    writeln!(err, "smithling query: {flag}: {}", s("message"))?;
    if let (Some(text), Some(start), Some(end)) = (text, at.0, at.1) {
        let at = |i: u64| text.get(..i as usize).map_or(0, |p| p.chars().count());
        let (from, to) = (at(start), at(end).max(at(start) + 1));
        writeln!(err, "  {text}\n  {}{}", " ".repeat(from), "^".repeat(to - from))?;
    }
    Ok(())
}

fn table(runs: &[Value], matched: u64, complete: bool, out: &mut impl Write) -> std::io::Result<()> {
    let cell = |r: &Value, k: &str| r[k].as_str().unwrap_or("-").to_string();
    let rows: Vec<[String; 6]> = runs
        .iter()
        .map(|r| {
            let start = r["start_time"]
                .as_str()
                .map_or("-".into(), |t| t.get(..19).unwrap_or(t).replace('T', " "));
            let latency = r["latency_ms"]
                .as_u64()
                .map_or("-".into(), |ms| format!("{:.1}s", ms as f64 / 1000.0));
            let name: String = cell(r, "name").chars().take(40).collect();
            [
                start,
                cell(r, "status"),
                latency,
                cell(r, "run_type"),
                name,
                cell(r, "run_id"),
            ]
        })
        .collect();
    let head = ["START (UTC)", "STATUS", "LATENCY", "TYPE", "NAME", "RUN ID"];
    let mut width = head.map(|h| h.chars().count());
    for r in &rows {
        for (w, c) in width.iter_mut().zip(r) {
            *w = (*w).max(c.chars().count());
        }
    }
    let line = |cells: [&str; 6]| {
        let parts: Vec<String> = cells.iter().zip(width).map(|(c, w)| format!("{c:<w$}")).collect();
        parts.join("  ").trim_end().to_string()
    };
    writeln!(out, "{}", line(head))?;
    for r in &rows {
        writeln!(out, "{}", line(r.each_ref().map(String::as_str)))?;
    }
    let noun = if matched == 1 { "run" } else { "runs" };
    if !complete {
        writeln!(out, "showing {} runs; more match (--limit up to 200)", rows.len())
    } else if matched > rows.len() as u64 {
        writeln!(out, "showing {} of {matched} {noun} (--limit up to 200)", rows.len())
    } else {
        writeln!(out, "{matched} {noun}")
    }
}

/// `1,112`.
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

type Cell = Box<dyn Fn(&Value) -> String>;

fn dash() -> String {
    "—".to_string()
}

/// One column per metric (and `runs`); one row per group, then `total`.
fn agg_table(v: &Value, keys: &[String], metrics: &[String], out: &mut impl Write) -> std::io::Result<()> {
    let mut cols: Vec<(String, Cell)> = vec![(
        "runs".into(),
        Box::new(|c: &Value| c["runs"].as_u64().map_or_else(dash, thousands)),
    )];
    for m in metrics.iter().filter(|m| *m != "count") {
        let (head, cell): (String, Cell) = match m.as_str() {
            "error_rate" => (
                m.clone(),
                Box::new(|c: &Value| {
                    c["error_rate"]
                        .as_f64()
                        .map_or_else(dash, |r| format!("{:.1}%", r * 100.0))
                }),
            ),
            "sum(cost)" => (
                "cost".into(),
                Box::new(|c: &Value| {
                    c["cost_pico"]
                        .as_i64()
                        .map_or_else(dash, smithling_model::cost::display)
                }),
            ),
            p if p.ends_with("(latency)") => {
                let k = p.trim_end_matches("(latency)").to_string();
                (
                    k.clone(),
                    Box::new(move |c: &Value| {
                        let mark = if c["approximate"] == true { "~" } else { "" };
                        c[&k]
                            .as_f64()
                            .map_or_else(dash, |ms| format!("{mark}{:.1} s", ms / 1000.0))
                    }),
                )
            }
            s if s.starts_with("sum(") => {
                let k = s.trim_start_matches("sum(").trim_end_matches(')').to_string();
                (
                    k.clone(),
                    Box::new(move |c: &Value| c[&k].as_u64().map_or_else(dash, thousands)),
                )
            }
            other => (other.to_string(), Box::new(|_: &Value| dash())),
        };
        cols.push((head, cell));
    }
    let key_text = |k: &Value| k.as_str().map_or_else(|| k.to_string(), str::to_string);
    // Side b of a comparison: its cells for the same group (`null`, shown as —, when it has none).
    let b = v.get("compare").map(|c| &c["side_b"]);
    let b_cells = |key: &Value| -> &Value {
        b.and_then(|b| b["groups"].as_array())
            .and_then(|gs| gs.iter().find(|g| g["key"] == *key))
            .map_or(&Value::Null, |g| &g["cells"])
    };
    let with_b = |r: &mut Vec<String>, cells: &Value| {
        if b.is_some() {
            r.push("│".into());
            r.extend(cols.iter().map(|(_, f)| f(cells)));
        }
    };
    let mut rows: Vec<Vec<String>> = Vec::new();
    for g in v["groups"].as_array().into_iter().flatten() {
        let mut r: Vec<String> = g["key"].as_array().into_iter().flatten().map(key_text).collect();
        r.extend(cols.iter().map(|(_, f)| f(&g["cells"])));
        with_b(&mut r, b_cells(&g["key"]));
        rows.push(r);
    }
    let mut total: Vec<String> = vec!["total".into()];
    total.extend((1..keys.len()).map(|_| String::new()));
    total.extend(cols.iter().map(|(_, f)| f(&v["totals"])));
    with_b(&mut total, b.map_or(&Value::Null, |b| &b["totals"]));
    rows.push(total);
    let mut head: Vec<String> = if keys.is_empty() {
        vec![String::new()]
    } else {
        keys.to_vec()
    };
    head.extend(cols.iter().map(|(h, _)| h.clone()));
    let nkeys = head.len() - cols.len();
    if b.is_some() {
        head.push("│ b:".into());
        head.extend(cols.iter().map(|(h, _)| h.clone()));
    }
    let sep = nkeys + cols.len();
    let mut width: Vec<usize> = head.iter().map(|h| h.chars().count()).collect();
    for r in &rows {
        for (w, c) in width.iter_mut().zip(r) {
            *w = (*w).max(c.chars().count());
        }
    }
    let line = |cells: &[String]| {
        let parts: Vec<String> = cells
            .iter()
            .zip(&width)
            .enumerate()
            .map(|(i, (c, w))| {
                let pad = w - c.chars().count();
                if i < nkeys || i == sep {
                    format!("{c}{}", " ".repeat(pad))
                } else {
                    format!("{}{c}", " ".repeat(pad))
                }
            })
            .collect();
        parts.join("   ").trim_end().to_string()
    };
    writeln!(out, "{}", line(&head))?;
    let last = rows.len() - 1;
    for (i, r) in rows.iter().enumerate() {
        let mut l = line(r);
        let no_price = v["totals"]["no_price_runs"].as_u64().unwrap_or(0);
        if i == last && no_price > 0 && metrics.iter().any(|m| m == "sum(cost)") {
            let noun = if no_price == 1 { "run" } else { "runs" };
            l.push_str(&format!("   {} {noun} had no price", thousands(no_price)));
        }
        writeln!(out, "{l}")?;
    }
    if v["overlap"] == true {
        writeln!(
            out,
            "groups overlap (a run with several tags is in each); total counts each run once"
        )?;
    }
    Ok(())
}
