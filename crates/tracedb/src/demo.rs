//! `tracedb demo`: loads the overnight-eval scenario and checks the M2 numbers through the real read API
//! (contracts/cli.md). Without `--url` it runs its own server on a temporary folder (or `--data`).

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tracedb_ingest::Limits;

use crate::scenario::{self, Loaded, PROJECT, Scenario};
use crate::seed::SeedError;
use crate::{ServeConfig, app, parse_size};

#[derive(clap::Args, Debug)]
pub struct DemoArgs {
    #[arg(long, default_value_t = 7)]
    pub seed: u64,
    /// Load into this running tracedb instead of starting one.
    #[arg(long, conflicts_with_all = ["data", "storage", "metastore"])]
    pub url: Option<String>,
    /// Data folder for the demo's own server (default: a temporary folder, removed afterwards).
    #[arg(long)]
    pub data: Option<PathBuf>,
    /// As `serve --storage`.
    #[arg(long)]
    pub storage: Option<String>,
    /// As `serve --metastore`.
    #[arg(long)]
    pub metastore: Option<String>,
    /// The demo server's memtable size; the background volume is three of these (match the server's with `--url`).
    #[arg(long, default_value = "4MB", value_parser = parse_size)]
    pub memtable: u64,
    #[arg(long, default_value = "m2", value_parser = ["m2"])]
    pub checks: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub name: String,
    pub expected: String,
    pub actual: String,
    pub ms: Option<u64>,
}

impl Check {
    pub fn new(name: &str, expected: &str, actual: &str, ms: Option<u64>) -> Check {
        Check {
            name: name.into(),
            expected: expected.into(),
            actual: actual.into(),
            ms,
        }
    }

    pub fn passed(&self) -> bool {
        self.expected == self.actual
    }
}

/// Prints the pass/fail table; returns whether every check passed.
pub fn report(header: &str, checks: &[Check], out: &mut impl Write) -> std::io::Result<bool> {
    writeln!(out, "{header}")?;
    let width = checks
        .iter()
        .map(|c| c.actual.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);
    for c in checks {
        let mark = if c.passed() { "✓" } else { "✗" };
        let ms = c.ms.map_or(String::new(), |ms| format!("{ms:>6} ms"));
        let line = format!("  {mark} {:<36} {:<width$} {ms}", c.name, c.actual);
        writeln!(out, "{}", line.trim_end())?;
    }
    let failed: Vec<&Check> = checks.iter().filter(|c| !c.passed()).collect();
    if failed.is_empty() {
        writeln!(out, "all {} checks passed", checks.len())?;
    } else {
        writeln!(out, "{} of {} checks failed:", failed.len(), checks.len())?;
        for c in failed {
            writeln!(out, "  {}: expected {}, got {}", c.name, c.expected, c.actual)?;
        }
    }
    Ok(checks.iter().all(Check::passed))
}

/// Runs the demo; returns the exit code (0 ok, 1 failure, 2 server not reachable).
pub async fn run(a: &DemoArgs, out: &mut impl Write, err: &mut impl Write) -> u8 {
    let own = match &a.url {
        Some(_) => None,
        None => match start(a).await {
            Ok(s) => Some(s),
            Err(e) => {
                let _ = writeln!(err, "tracedb demo: {}", tracedb_storage::redact(&format!("{e:#}")));
                return 1;
            }
        },
    };
    let url = match (&own, &a.url) {
        (Some(s), _) => format!("http://{}", s.addr),
        (None, Some(u)) => u.trim_end_matches('/').to_string(),
        (None, None) => unreachable!("either --url or the demo's own server"),
    };
    let s = scenario::generate(a.seed, tracedb_model::Ts::now().0 - 6 * 3_600_000_000, a.memtable);
    let loaded = match scenario::load(&url, &s).await {
        Ok(l) => l,
        Err(e @ SeedError::Unreachable(..)) => {
            let _ = writeln!(err, "tracedb demo: {e}");
            return 2;
        }
        Err(e) => {
            let _ = writeln!(err, "tracedb demo: {e}");
            return 1;
        }
    };
    let checks = match checks(&url, &s, &loaded).await {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "tracedb demo: {e}");
            return 1;
        }
    };
    let header = format!(
        "tracedb demo — M2 checks  (seed {}, {} traces, {} flushes)",
        a.seed,
        thousands(loaded.traces as u64),
        loaded.flushes
    );
    let ok = report(&header, &checks, out);
    if let Some(own) = own {
        own.stop().await;
    }
    match ok {
        Ok(true) => 0,
        _ => 1,
    }
}

struct Own {
    addr: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    _dir: Option<tempfile::TempDir>,
}

impl Own {
    /// Stops the server and closes the data folder before a temporary one is removed.
    async fn stop(self) {
        let _ = self.stop.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.task).await;
    }
}

async fn start(a: &DemoArgs) -> anyhow::Result<Own> {
    let tmp = match &a.data {
        Some(_) => None,
        None => Some(tempfile::Builder::new().prefix("tracedb-demo-").tempdir()?),
    };
    let cfg = ServeConfig {
        data_dir: a
            .data
            .clone()
            .unwrap_or_else(|| tmp.as_ref().unwrap().path().to_path_buf()),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        storage: a.storage.clone(),
        metastore: a.metastore.clone(),
        limits: Limits {
            memtable_bytes: a.memtable,
            ..Limits::default()
        },
        ui_dir: None,
    };
    let listener = tokio::net::TcpListener::bind(cfg.addr).await?;
    let (state, _) = tokio::task::spawn_blocking(move || crate::open(&cfg)).await??;
    let addr = listener.local_addr()?;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app(state))
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
            })
            .await;
    });
    Ok(Own {
        addr,
        stop,
        task,
        _dir: tmp,
    })
}

struct Api {
    http: reqwest::Client,
    url: String,
}

impl Api {
    async fn send(&self, req: reqwest::RequestBuilder, what: &str) -> Result<(Value, u64), String> {
        let started = Instant::now();
        let r = req.send().await.map_err(|e| format!("{what}: {e}"))?;
        let status = r.status();
        let v: Value = r.json().await.map_err(|e| format!("{what}: {e}"))?;
        if !status.is_success() {
            return Err(format!("{what}: {status}: {v}"));
        }
        Ok((v, started.elapsed().as_millis() as u64))
    }

    async fn query(&self, body: Value) -> Result<(Value, u64), String> {
        let what = format!("POST /api/query {body}");
        self.send(self.http.post(format!("{}/api/query", self.url)).json(&body), &what)
            .await
    }

    async fn get(&self, path: &str) -> Result<(Value, u64), String> {
        self.send(self.http.get(format!("{}{path}", self.url)), &format!("GET {path}"))
            .await
    }
}

fn roots(filter: &str, tree_filter: Option<&str>) -> Value {
    json!({
        "project": PROJECT, "time": {"last": "12h"}, "is_root": true,
        "filter": filter, "tree_filter": tree_filter, "limit": 200,
    })
}

fn matched(v: &Value) -> u64 {
    v["stats"]["matched"].as_u64().unwrap_or(0)
}

fn ids(v: &Value) -> std::collections::BTreeSet<String> {
    v["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["run_id"].as_str().map(str::to_string))
        .collect()
}

/// A count check that also requires exactly the planted runs.
fn count(name: &str, v: &Value, ms: u64, planted: &[uuid::Uuid]) -> Check {
    let n = matched(v);
    let want: std::collections::BTreeSet<String> = planted.iter().map(|u| u.to_string()).collect();
    let actual = if n == want.len() as u64 && ids(v) != want {
        format!("{n} (not the planted runs)")
    } else {
        n.to_string()
    };
    Check::new(name, &want.len().to_string(), &actual, Some(ms))
}

async fn checks(url: &str, s: &Scenario, loaded: &Loaded) -> Result<Vec<Check>, String> {
    let api = Api {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())?,
        url: url.into(),
    };
    let tag = scenario::BATCH_TAG;
    // Read before the queries, so a flush while they run can't make a run read across both look file-only.
    let (health, _) = api.get("/healthz").await?;
    let flushed = health["flushed_seq"].as_u64().unwrap_or(0);
    let (failing, ms1) = api
        .query(roots(&format!(r#"and(has(tags, "{tag}"), eq(status, "error"))"#), None))
        .await?;
    let (quiet, ms2) = api
        .query(roots(
            &format!(r#"and(has(tags, "{tag}"), eq(status, "ok"))"#),
            Some(r#"eq(status, "error")"#),
        ))
        .await?;

    // Top values as the field browser shows them, cross-checked against exact query counts.
    let (values, mut ms3) = api
        .get(&format!(
            "/api/fields/values?project={PROJECT}&time=12h&path=metadata.prompt_version"
        ))
        .await?;
    let mut top: Vec<String> = values["values"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| format!("{} {}", v["value"].as_str().unwrap_or("?"), v["count"]))
        .collect();
    if values["other_count"].as_u64().unwrap_or(0) > 0 {
        top.push(format!("other {}", values["other_count"]));
    }
    for (v, want) in [("v6", scenario::PROMPT_V6), ("v7", scenario::PROMPT_V7)] {
        let filter = format!(r#"eq(metadata.prompt_version, "{v}")"#);
        let body = json!({"project": PROJECT, "time": {"last": "12h"}, "filter": filter, "limit": 1});
        let (r, ms) = api.query(body).await?;
        ms3 += ms;
        if matched(&r) != want as u64 {
            top.push(format!("(query: {v} {})", matched(&r)));
        }
    }

    let (limited, ms4) = api
        .query(json!({
            "project": PROJECT, "time": {"last": "12h"}, "limit": 200,
            "filter": r#"and(eq(metadata.prompt_version, "v7"), eq(outputs.status_code, 429))"#,
        }))
        .await?;
    let across = [&failing, &quiet, &limited]
        .iter()
        .flat_map(|v| v["runs"].as_array().into_iter().flatten())
        .filter(|r| {
            let (first, last) = (
                r["first_seq"].as_u64().unwrap_or(0),
                r["last_seq"].as_u64().unwrap_or(0),
            );
            first <= flushed && last > flushed
        })
        .count();
    let across = match (loaded.in_files, across) {
        (false, _) => "no (starts never reached files; with --url, pass the server's --memtable)".to_string(),
        (true, 0) => "no".to_string(),
        (true, _) => "yes".to_string(),
    };

    let checks = vec![
        count("14 failing roots", &failing, ms1, &s.failing),
        count("23 quiet failures", &quiet, ms2, &s.quiet),
        Check::new(
            "prompt_version top values",
            &format!("v6 {} · v7 {}", scenario::PROMPT_V6, scenario::PROMPT_V7),
            &top.join(" · "),
            Some(ms3),
        ),
        Check::new(
            "v7 ∧ status_code = 429",
            &scenario::V7_429.to_string(),
            &matched(&limited).to_string(),
            Some(ms4),
        ),
        Check::new("checks across memtable and files", "yes", &across, None),
    ];
    Ok(checks)
}

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
