//! Idle memory (SC-05, research M3-R12): after 3,000 seeded traces (~480k runs) and a few queries, and again
//! after a restart, an idle server's physical footprint, the median of 11 samples over 30 s, is at most
//! 200 MB. Idle: no uploads for one flush age, the memtable flushed, the dedup window drained (both shortened
//! to 30 s here). Slow (~10 min), so `#[ignore]`; run it with `--release`.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const LIMIT_MB: f64 = 200.0;

fn serve(dir: &Path) -> (Child, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tracedb"))
        .args(["serve", "--data", dir.to_str().unwrap(), "--port", "0"])
        .args(["--flush-age", "30s", "--dedup-window", "30s"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line
        .split_whitespace()
        .find(|w| w.starts_with("http://"))
        .unwrap()
        .to_string();
    (child, url)
}

/// Physical footprint in MB: `vmmap` on macOS (what Activity Monitor shows), resident set elsewhere.
fn footprint_mb(pid: u32) -> f64 {
    if cfg!(target_os = "macos") {
        let out = Command::new("vmmap")
            .args(["--summary", &pid.to_string()])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text
            .lines()
            .find(|l| l.starts_with("Physical footprint:"))
            .unwrap_or_else(|| panic!("no footprint in vmmap output: {text}"));
        let v = line.split_whitespace().nth(2).unwrap();
        let (n, unit) = v.split_at(v.len() - 1);
        let n: f64 = n.parse().unwrap();
        match unit {
            "K" => n / 1024.0,
            "M" => n,
            "G" => n * 1024.0,
            _ => panic!("footprint {v}"),
        }
    } else {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let kb: f64 = status
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap()
            .parse()
            .unwrap();
        kb / 1024.0
    }
}

async fn health(url: &str) -> Value {
    reqwest::get(format!("{url}/healthz"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Waits for idle (the memtable flushed, the dedup window drained, a flush age since the last upload), then
/// samples the footprint every 3 s for 30 s; returns the median.
async fn idle_median(child: &Child, url: &str, last_upload: Instant) -> (f64, Vec<f64>) {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let h = health(url).await;
        let flushed = h["runs"] == 0 && h["frozen"] == false;
        if flushed && h["dedup_keys"] == 0 && last_upload.elapsed() >= Duration::from_secs(30) {
            break;
        }
        assert!(Instant::now() < deadline, "never went idle: {h}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let mut samples = Vec::new();
    for i in 0..11 {
        if i > 0 {
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        samples.push(footprint_mb(child.id()));
    }
    let mut sorted = samples.clone();
    sorted.sort_by(f64::total_cmp);
    (sorted[sorted.len() / 2], samples)
}

/// A list, a search, a grouped query and one run with its payload: what a developer does before leaving.
async fn browse(url: &str) {
    let http = reqwest::Client::new();
    let post = |path: &str, body: Value| http.post(format!("{url}{path}")).json(&body).send();
    let list: Value = post("/api/query", json!({"project": "seed", "limit": 50}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = list["runs"][0]["run_id"].as_str().unwrap().to_string();
    for (path, body) in [
        (
            "/api/query",
            json!({"project": "seed", "filter": "eq(status, \"error\")", "is_root": true, "limit": 200}),
        ),
        (
            "/api/query",
            json!({"project": "seed", "filter": "search(\"rate limit\")", "limit": 50}),
        ),
        (
            "/api/aggregate",
            json!({"project": "seed", "group_by": ["name"], "metrics": ["count", "p95(latency)"]}),
        ),
    ] {
        let r = post(path, body.clone()).await.unwrap();
        assert_eq!(r.status(), 200, "{path} {body}");
    }
    let r = reqwest::get(format!("{url}/api/runs/{id}?include=inputs,outputs,events"))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn idle_footprint_stays_under_200_mb() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, url) = serve(dir.path());
    let loaded = Instant::now();
    let seed = Command::new(env!("CARGO_BIN_EXE_tracedb"))
        .args(["seed", "--traces", "3000", "--no-delay", "--url", &url])
        .output()
        .unwrap();
    assert!(seed.status.success(), "{}", String::from_utf8_lossy(&seed.stderr));
    let last_upload = Instant::now();
    let h = health(&url).await;
    eprintln!(
        "loaded in {:.0?}: {} files, {} runs in files",
        loaded.elapsed(),
        h["segments"],
        h["segment_rows"]
    );
    browse(&url).await;
    let (after_load, samples) = idle_median(&child, &url, last_upload).await;
    let h = health(&url).await;
    eprintln!(
        "idle after the load: median {after_load:.1} MB {samples:?}; cache {} B",
        h["cache_bytes"]
    );
    child.kill().unwrap();
    child.wait().unwrap();

    let (mut child, url) = serve(dir.path());
    let started = Instant::now();
    browse(&url).await;
    let (after_restart, samples) = idle_median(&child, &url, started).await;
    let h = health(&url).await;
    eprintln!(
        "idle after a restart: median {after_restart:.1} MB {samples:?}; cache {} B",
        h["cache_bytes"]
    );
    child.kill().unwrap();
    child.wait().unwrap();

    assert!(after_load <= LIMIT_MB, "idle after the load: {after_load:.1} MB");
    assert!(after_restart <= LIMIT_MB, "idle after a restart: {after_restart:.1} MB");
}
