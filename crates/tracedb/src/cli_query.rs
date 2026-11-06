//! `tracedb query`: one `POST /api/query` against a running server, printed as a table or JSON lines.

use std::io::Write;
use std::time::Duration;

use serde_json::{Value, json};

#[derive(clap::Args, Debug)]
pub struct QueryArgs {
    #[arg(long, default_value = "http://localhost:7070", env = "TRACEDB_URL")]
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
    /// One JSON object per run, per line.
    #[arg(long)]
    pub json: bool,
    /// Conditions on the run itself, e.g. `eq(status, "error")`.
    pub filter: Option<String>,
}

/// Runs the query and prints it; returns the exit code (contracts/cli.md: 1 = failure, 2 = not reachable).
pub async fn run(a: &QueryArgs, out: &mut impl Write, err: &mut impl Write) -> u8 {
    let body = json!({
        "project": a.project,
        "time": a.time.as_ref().map(|t| json!({"last": t})),
        "filter": a.filter,
        "trace_filter": a.trace_filter,
        "tree_filter": a.tree_filter,
        "is_root": a.root.then_some(true),
        "limit": a.limit,
    });
    let url = format!("{}/api/query", a.url.trim_end_matches('/'));
    let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build();
    let sent = match http {
        Ok(h) => h.post(&url).json(&body).send().await,
        Err(e) => Err(e),
    };
    let resp = match sent {
        Ok(r) => r,
        Err(e) if e.is_connect() || e.is_timeout() => {
            let _ = writeln!(err, "tracedb query: tracedb is not reachable at {}: {e}", a.url);
            return 2;
        }
        Err(e) => {
            let _ = writeln!(err, "tracedb query: {e}");
            return 1;
        }
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        let _ = writeln!(err, "tracedb query: {url} answered {status}: {}", text.trim());
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
    let runs = v["runs"].as_array().map_or(&[][..], Vec::as_slice);
    let printed = if a.json {
        runs.iter().try_for_each(|r| writeln!(out, "{r}"))
    } else {
        table(runs, v["stats"]["matched"].as_u64().unwrap_or(runs.len() as u64), out)
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
        _ if s("code") == "bad_time" => {
            return writeln!(
                err,
                "tracedb query: --time {:?}: use e.g. 12h, 30m or 7d",
                a.time.as_deref().unwrap_or("")
            );
        }
        _ => {
            return writeln!(err, "tracedb query: {}: {}", s("code"), s("message"));
        }
    };
    writeln!(err, "tracedb query: {flag}: {}", s("message"))?;
    if let (Some(text), Some(start), Some(end)) = (text, e["start"].as_u64(), e["end"].as_u64()) {
        let at = |i: u64| text.get(..i as usize).map_or(0, |p| p.chars().count());
        let (from, to) = (at(start), at(end).max(at(start) + 1));
        writeln!(err, "  {text}\n  {}{}", " ".repeat(from), "^".repeat(to - from))?;
    }
    Ok(())
}

fn table(runs: &[Value], matched: u64, out: &mut impl Write) -> std::io::Result<()> {
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
    if matched > rows.len() as u64 {
        writeln!(out, "showing {} of {matched} {noun} (--limit up to 200)", rows.len())
    } else {
        writeln!(out, "{matched} {noun}")
    }
}
