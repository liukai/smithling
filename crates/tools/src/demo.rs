//! `smithling demo`: loads the overnight-eval scenario and checks the M2 and M3 numbers through the real read API
//! (contracts/cli.md). Without `--url` it runs its own server on a temporary folder (or `--data`).

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use smithling_ingest::Limits;

use crate::scenario::{self, Loaded, PROJECT, Scenario};
use crate::seed::SeedError;
use smithling_server::{ServeConfig, app, parse_size};

/// The demo's own server's memtable: small, so a few MB of background load forces the flushes it needs.
const DEMO_MEMTABLE: u64 = 4 * MB;
/// With `--compact`: small enough that a day of demo data lands in many small files, as weeks of use would.
const DEMO_COMPACT_MEMTABLE: u64 = 256 * 1024;
/// With `--url`, the largest server memtable the demo sizes its background load for (it builds up to 3×).
const MAX_URL_MEMTABLE: u64 = 256 * MB;
const MB: u64 = 1024 * 1024;

#[derive(clap::Args, Debug)]
pub struct DemoArgs {
    #[arg(long, default_value_t = 7)]
    pub seed: u64,
    /// Load into this running smithling instead of starting one.
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
    /// The demo server's memtable size (default 4MB); the background volume is up to three of these. With `--url`
    /// the server's own limit from `/healthz` is used; this only for a server that doesn't report one.
    #[arg(long, value_parser = parse_size)]
    pub memtable: Option<u64>,
    /// Which checks to run: `m2`, `m3` or both.
    #[arg(long, default_value = "m2,m3", value_parser = parse_checks)]
    pub checks: CheckSet,
    /// Run the checks, compact everything (`POST /api/compact`, every window), run them again: every answer must
    /// be the same; prints the file count before and after (M4).
    #[arg(long)]
    pub compact: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckSet {
    pub m2: bool,
    pub m3: bool,
}

impl CheckSet {
    fn title(self) -> &'static str {
        match (self.m2, self.m3) {
            (true, true) => "M2 + M3",
            (false, true) => "M3",
            _ => "M2",
        }
    }
}

fn parse_checks(s: &str) -> Result<CheckSet, String> {
    let mut set = CheckSet { m2: false, m3: false };
    for part in s.split(',').map(str::trim) {
        match part {
            "m2" => set.m2 = true,
            "m3" => set.m3 = true,
            _ => return Err(format!("unknown check set {part:?} (expected m2, m3 or m2,m3)")),
        }
    }
    Ok(set)
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
        let line = format!("  {mark} {:<38} {:<width$} {ms}", c.name, c.actual);
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
                let _ = writeln!(err, "smithling demo: {}", smithling_storage::redact(&format!("{e:#}")));
                return 1;
            }
        },
    };
    let url = match (&own, &a.url) {
        (Some(s), _) => format!("http://{}", s.addr),
        (None, Some(u)) => u.trim_end_matches('/').to_string(),
        (None, None) => unreachable!("either --url or the demo's own server"),
    };
    let memtable = match &own {
        Some(_) => a.memtable.unwrap_or(if a.compact {
            DEMO_COMPACT_MEMTABLE
        } else {
            DEMO_MEMTABLE
        }),
        None => match scenario::memtable_limit(&url).await {
            Ok(limit) => limit.or(a.memtable).unwrap_or(DEMO_MEMTABLE),
            Err(e) => {
                let _ = writeln!(err, "smithling demo: {e}");
                return 2;
            }
        },
    };
    if own.is_none() && memtable > MAX_URL_MEMTABLE {
        let _ = writeln!(
            err,
            "smithling demo: the server's memtable limit is {} MB; the demo would build and send up to three of them, so \
             it stops above {} MB (run `serve` with a smaller --memtable)",
            memtable / MB,
            MAX_URL_MEMTABLE / MB
        );
        return 1;
    }
    let s = scenario::generate(a.seed, smithling_model::Ts::now().0, memtable);
    let loaded = match scenario::load(&url, &s).await {
        Ok(l) => l,
        Err(e @ SeedError::Unreachable(..)) => {
            let _ = writeln!(err, "smithling demo: {e}");
            return 2;
        }
        Err(e) => {
            let _ = writeln!(err, "smithling demo: {e}");
            return 1;
        }
    };
    let mut checks = match checks(&url, a.checks, &s, &loaded).await {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "smithling demo: {e}");
            return 1;
        }
    };
    let mut compacted = None;
    if a.compact {
        let again = match compact(&url).await {
            Ok(c) => {
                compacted = Some(c);
                checks_again(&url, a.checks, &s, &loaded).await
            }
            Err(e) => Err(format!("compaction failed: {e}")),
        };
        match again {
            Ok(after) => checks.push(same_answers(&checks, &after)),
            Err(e) => {
                let _ = writeln!(err, "smithling demo: {e}");
                return 1;
            }
        }
    }
    let header = format!(
        "smithling demo — {} checks  (seed {}, {} traces, {} flushes)",
        a.checks.title(),
        a.seed,
        thousands(loaded.traces as u64),
        loaded.flushes
    );
    let ok = report(&header, &checks, out);
    if let Some(c) = compacted {
        let _ = writeln!(out, "{c}");
    }
    if let Some(own) = own {
        own.stop().await;
    }
    match ok {
        Ok(true) => 0,
        _ => 1,
    }
}

/// The checks again, after compaction.
async fn checks_again(url: &str, set: CheckSet, s: &Scenario, loaded: &Loaded) -> Result<Vec<Check>, String> {
    checks(url, set, s, loaded).await
}

/// Whether every check gave the same answer after compaction as before. "Across memtable and files" says where
/// the data was when the checks ran (the last flush may land while compaction runs), not what they answered.
fn same_answers(before: &[Check], after: &[Check]) -> Check {
    let pairs: Vec<_> = before
        .iter()
        .zip(after)
        .filter(|(b, _)| b.name != "checks across memtable and files")
        .collect();
    let changed: Vec<String> = pairs
        .iter()
        .filter(|(b, a)| b.actual != a.actual)
        .map(|(b, a)| format!("{}: {} → {}", b.name, b.actual, a.actual))
        .collect();
    let n = pairs.len();
    let actual = if changed.is_empty() {
        format!("all {n} the same")
    } else {
        format!("{} of {n} changed ({})", changed.len(), changed.join("; "))
    };
    Check::new(
        "same answers after compaction",
        &format!("all {n} the same"),
        &actual,
        None,
    )
}

/// Compacts everything now and describes it: files before → after, and how many runs' rows were merged.
async fn compact(url: &str) -> Result<String, String> {
    let v: Value = reqwest::Client::new()
        .post(format!("{url}/api/compact"))
        .json(&serde_json::json!({"force": true}))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let files = |k: &str| v[k]["l0"].as_u64().unwrap_or(0) + v[k]["l1"].as_u64().unwrap_or(0);
    let n = |k: &str| v[k].as_u64().unwrap_or(0);
    let (before, after) = (files("files_before"), files("files_after"));
    Ok(format!(
        "compaction: {before} data files → {after} ({}% of before; {} small files merged into {}; {} stored versions \
         → {} rows)",
        (after * 100).checked_div(before).unwrap_or(0),
        n("inputs"),
        n("outputs"),
        thousands(n("rows_in")),
        thousands(n("rows_out"))
    ))
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
        None => Some(tempfile::Builder::new().prefix("smithling-demo-").tempdir()?),
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
            memtable_bytes: a.memtable.unwrap_or(if a.compact {
                DEMO_COMPACT_MEMTABLE
            } else {
                DEMO_MEMTABLE
            }),
            ..Limits::default()
        },
        ui_dir: None,
        prices: None,
        compact: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind(cfg.addr).await?;
    let (state, _) = tokio::task::spawn_blocking(move || smithling_server::open(&cfg)).await??;
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

    async fn aggregate(&self, body: Value) -> Result<(Value, u64), String> {
        let what = format!("POST /api/aggregate {body}");
        self.send(self.http.post(format!("{}/api/aggregate", self.url)).json(&body), &what)
            .await
    }

    async fn get(&self, path: &str) -> Result<(Value, u64), String> {
        self.send(self.http.get(format!("{}{path}", self.url)), &format!("GET {path}"))
            .await
    }
}

/// Scoped by the batch tag in `filter`, not by time: the batches end 30 h and 6 h ago.
fn roots(filter: &str, tree_filter: Option<&str>) -> Value {
    json!({
        "project": PROJECT, "is_root": true,
        "filter": filter, "tree_filter": tree_filter, "limit": 200, "count": true,
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

async fn checks(url: &str, set: CheckSet, s: &Scenario, loaded: &Loaded) -> Result<Vec<Check>, String> {
    let api = Api {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())?,
        url: url.into(),
    };
    let mut out = Vec::new();
    if set.m2 {
        out.extend(m2_checks(&api, s, loaded).await?);
    }
    if set.m3 {
        out.extend(m3_checks(&api, s).await?);
    }
    Ok(out)
}

async fn m2_checks(api: &Api, s: &Scenario, loaded: &Loaded) -> Result<Vec<Check>, String> {
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

    // Top values as the field browser shows them (only batch a has prompt_version), cross-checked against exact
    // query counts.
    let (values, mut ms3) = api
        .get(&format!(
            "/api/fields/values?project={PROJECT}&path=metadata.prompt_version"
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
        let filter = format!(r#"and(has(tags, "{tag}"), eq(metadata.prompt_version, "{v}"))"#);
        let body = json!({"project": PROJECT, "filter": filter, "limit": 1, "count": true});
        let (r, ms) = api.query(body).await?;
        ms3 += ms;
        if matched(&r) != want as u64 {
            top.push(format!("(query: {v} {})", matched(&r)));
        }
    }

    let (limited, ms4) = api
        .query(json!({
            "project": PROJECT, "limit": 200, "count": true,
            "filter": format!(r#"and(has(tags, "{tag}"), eq(metadata.prompt_version, "v7"), eq(outputs.status_code, 429))"#),
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
        (false, _) => {
            "no (starts never reached files; a server that doesn't report memtable_limit_bytes needs --memtable)"
                .to_string()
        }
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

fn tagged(tag: &str) -> String {
    format!(r#"has(tags, "{tag}")"#)
}

fn pct(rate: f64) -> String {
    format!("{:.1}%", rate * 100.0)
}

fn secs(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{} s", ms / 1000)
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

/// Cents when the amount is whole cents, else every digit, so a changed planted cost never rounds back.
fn dollars(pico: i64) -> String {
    if pico % 10_000_000_000 == 0 {
        format!("${:.2}", pico as f64 / 1e12)
    } else {
        format!("${:.12}", pico as f64 / 1e12)
    }
}

fn group<'a>(groups: &'a Value, key: &str) -> Option<&'a Value> {
    groups.as_array()?.iter().find(|g| g["key"][0] == key)
}

fn rate(cells: &Value) -> String {
    cells["error_rate"].as_f64().map_or("?".into(), pct)
}

fn p95(cells: &Value) -> String {
    cells["p95"].as_u64().map_or("?".into(), secs)
}

/// " (e errors of n)" when a group's counts aren't the planted ones, which a rounded rate could hide.
fn counts(cells: &Value, runs: usize, errors: usize) -> String {
    let (n, e) = (cells["runs"].as_u64(), cells["errors"].as_u64());
    if (n, e) == (Some(runs as u64), Some(errors as u64)) {
        String::new()
    } else {
        format!(" ({} errors of {})", e.unwrap_or(0), n.unwrap_or(0))
    }
}

/// Runs of the group `key` summed over a series' buckets.
fn series_runs(side: &Value, key: &str) -> u64 {
    let Some(i) = side["groups"]
        .as_array()
        .and_then(|g| g.iter().position(|g| g["key"][0] == key))
    else {
        return 0;
    };
    side["series"]["buckets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["per_group"][i]["runs"].as_u64())
        .sum()
}

fn cost(totals: &Value) -> String {
    let c = totals["cost_pico"].as_i64().map_or("no cost".into(), dollars);
    format!("{c} ({} no price)", totals["no_price_runs"])
}

const BY_NAME: &str = "by name | count, error_rate, p95(latency), sum(cost)";

async fn m3_checks(api: &Api, s: &Scenario) -> Result<Vec<Check>, String> {
    let (a, b) = (tagged(scenario::BATCH_TAG), tagged(scenario::BATCH_B_TAG));
    let fetch = "fetch_url";
    let runs = scenario::FETCH_URL_RUNS;
    let planted_rate = |errors: usize| pct(errors as f64 / runs as f64);
    let (a_rate, b_rate) = (
        planted_rate(scenario::BATCH_A_FETCH_ERRORS),
        planted_rate(scenario::BATCH_B_FETCH_ERRORS),
    );
    let (a_p95, b_p95) = (
        secs(scenario::BATCH_A_FETCH_P95_MS),
        secs(scenario::BATCH_B_FETCH_P95_MS),
    );

    let (hits, ms_search) = api
        .query(json!({
            "project": PROJECT, "limit": 200, "count": true,
            "filter": format!("and(or({a}, {b}), search({:?}))", scenario::PHRASE),
        }))
        .await?;
    let page = hits["runs"].as_array().cloned().unwrap_or_default();
    let mut search = matched(&hits).to_string();
    if page.iter().any(|r| r["name"] != fetch) {
        search.push_str(" (not all fetch_url)");
    } else if page.iter().any(|r| {
        !r["match"]["excerpt"]
            .as_str()
            .is_some_and(|e| e.contains(scenario::PHRASE))
    }) {
        search.push_str(" (an excerpt without the phrase)");
    }

    let (by_name, ms_agg) = api
        .aggregate(json!({"project": PROJECT, "filter": a, "stats": BY_NAME}))
        .await?;
    let a_fetch = match group(&by_name["groups"], fetch) {
        Some(g) => {
            let c = &g["cells"];
            format!(
                "{} / {}{}",
                rate(c),
                p95(c),
                counts(c, runs, scenario::BATCH_A_FETCH_ERRORS)
            )
        }
        None => "no fetch_url group".into(),
    };

    let compare =
        |view: &str| json!({"project": PROJECT, "filter": a, "stats": BY_NAME, "view": view, "compare": {"filter": b}});
    let (table, ms_table) = api.aggregate(compare("table")).await?;
    let (series, ms_series) = api.aggregate(compare("series")).await?;
    let side_b = &table["compare"]["side_b"];
    let pair = match (group(&table["groups"], fetch), group(&side_b["groups"], fetch)) {
        (Some(x), Some(y)) => {
            let (x, y) = (&x["cells"], &y["cells"]);
            format!(
                "{} → {}, {} → {}{}{}",
                rate(x),
                rate(y),
                p95(x),
                p95(y),
                counts(x, runs, scenario::BATCH_A_FETCH_ERRORS),
                counts(y, runs, scenario::BATCH_B_FETCH_ERRORS)
            )
        }
        _ => "no fetch_url group on both sides".into(),
    };
    let keys = |g: &Value| -> Vec<Value> { g.as_array().into_iter().flatten().map(|g| g["key"].clone()).collect() };
    let (ka, kb) = (keys(&table["groups"]), keys(&side_b["groups"]));
    let side = !ka.is_empty() && kb.starts_with(&ka);
    // Overlaid: both sides on one offset axis, each side's buckets holding all of its fetch_url runs.
    let series_b = &series["compare"]["side_b"];
    let (sa, sb) = (&series["series"], &series_b["series"]);
    let buckets = |s: &Value| s["buckets"].as_array().map_or(0, Vec::len);
    let overlaid = sa["offset"] == true
        && sb["offset"] == true
        && buckets(sa) > 0
        && buckets(sa) == buckets(sb)
        && series_runs(&series, fetch) == runs as u64
        && series_runs(series_b, fetch) == runs as u64;
    let mut vs = format!(
        "{pair}, {} + {}",
        if side { "side by side" } else { "not side by side" },
        if overlaid { "overlaid" } else { "not overlaid" }
    );
    if side_b["totals"]["cost_pico"].as_i64() != Some(scenario::BATCH_B_COST_PICO) {
        vs.push_str(&format!(" (b {})", cost(&side_b["totals"])));
    }

    // Feedback: the filter finds the scored runs, the average is over them, and the score sent before its run
    // is stored with it and no longer an orphan.
    let key = scenario::FEEDBACK_KEY;
    let planted: std::collections::BTreeSet<String> = s
        .feedback
        .iter()
        .filter_map(|f| f.body["run_id"].as_str().map(str::to_string))
        .collect();
    let scores: Vec<f64> = s.feedback.iter().filter_map(|f| f.body["score"].as_f64()).collect();
    let want_avg = scores.iter().sum::<f64>() / scores.len().max(1) as f64;
    let early: Vec<&scenario::Feedback> = s.feedback.iter().filter(|f| f.before_run).collect();
    let (scored, ms_filter) = api
        .query(json!({
            "project": PROJECT, "limit": 200, "count": true,
            "filter": format!("and({a}, lt(feedback.{key}, 0.5))"),
        }))
        .await?;
    let (avg, ms_avg) = api
        .aggregate(json!({"project": PROJECT, "filter": a, "stats": format!("count, avg(feedback.{key})")}))
        .await?;
    let (orphans, mut ms_joined) = api.get("/api/feedback?orphaned=true").await?;
    let orphaned = |id: &Value| {
        orphans["feedback"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|r| &r["id"] == id)
    };
    let mut joined = 0;
    for f in &early {
        let run = f.body["run_id"].as_str().unwrap_or_default();
        let (rows, ms) = api.get(&format!("/api/feedback?run_id={run}")).await?;
        ms_joined += ms;
        let found = rows["feedback"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|r| r["id"] == f.body["id"]);
        joined += (found && !orphaned(&f.body["id"])) as usize;
    }
    let n = matched(&scored);
    let mut runs_found = format!("{n} runs");
    if n == planted.len() as u64 && ids(&scored) != planted {
        runs_found.push_str(" (not the scored runs)");
    }
    let mut average = avg["totals"]["feedback_avg"]
        .as_f64()
        .map_or("avg none".into(), |v| format!("avg {v:.2}"));
    if avg["totals"]["feedback_n"].as_u64() != Some(planted.len() as u64) {
        average.push_str(&format!(" over {}", avg["totals"]["feedback_n"]));
    }

    Ok(vec![
        Check::new(
            "31 text hits, all fetch_url",
            &scenario::PHRASE_RUNS.to_string(),
            &search,
            Some(ms_search),
        ),
        Check::new(
            "fetch_url error rate / p95 (batch a)",
            &format!("{a_rate} / {a_p95}"),
            &a_fetch,
            Some(ms_agg),
        ),
        Check::new(
            "batch a cost",
            &format!(
                "{} ({} no price)",
                dollars(scenario::BATCH_A_COST_PICO),
                scenario::BATCH_A_NO_PRICE_RUNS
            ),
            &cost(&by_name["totals"]),
            None,
        ),
        Check::new(
            "batch a vs b",
            &format!("{a_rate} → {b_rate}, {a_p95} → {b_p95}, side by side + overlaid"),
            &vs,
            Some(ms_table + ms_series),
        ),
        Check::new(
            "feedback filter and average",
            &format!(
                "{} runs · avg {want_avg:.2} ({} sent before its run)",
                planted.len(),
                early.len()
            ),
            &format!("{runs_found} · {average} ({joined} sent before its run)"),
            Some(ms_filter + ms_avg + ms_joined),
        ),
    ])
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
