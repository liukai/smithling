//! Restart at M2 scale (SC-06): 2M runs on disk, ready within 10 s, every run still there.
//! Slow, so `#[ignore]`; CI runs it with `--ignored`.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn spawn(data: &std::path::Path) -> (Child, String, Duration) {
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tracedb"))
        .args(["serve", "--data", data.to_str().unwrap(), "--port", "0"])
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
    (child, url, started.elapsed())
}

fn item(n: u64) -> Value {
    let id = format!("00000000-0000-4000-8000-{n:012}");
    json!({"id": id, "trace_id": id, "dotted_order": format!("20261105T020000000000Z{id}"),
           "session_name": "fill", "name": "r", "run_type": "chain", "start_time": "2026-11-05T02:00:00.000000",
           "end_time": "2026-11-05T02:00:01.000000", "inputs": {"q": "a small input"}, "outputs": {"a": "a small output"}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn restart_with_2m_runs_is_ready_within_10s() {
    const RUNS: u64 = 2_000_000;
    let dir = tempfile::tempdir().unwrap();
    let (mut child, url, _) = spawn(dir.path());
    let fill = Instant::now();
    let workers: Vec<_> = (0..8u64)
        .map(|w| {
            let url = url.clone();
            tokio::spawn(async move {
                let http = reqwest::Client::new();
                let mut b = w;
                while b * 100 < RUNS {
                    let post: Vec<Value> = (b * 100..(b + 1) * 100).map(|n| item(n + 1)).collect();
                    loop {
                        let r = http
                            .post(format!("{url}/runs/batch"))
                            .json(&json!({"post": post}))
                            .send()
                            .await
                            .unwrap();
                        match r.status().as_u16() {
                            202 => break,
                            // Busy while a flush catches up: retry, as the SDK does.
                            429 => tokio::time::sleep(Duration::from_millis(50)).await,
                            s => panic!("upload failed: {s}"),
                        }
                    }
                    b += 8;
                }
            })
        })
        .collect();
    for w in workers {
        w.await.unwrap();
    }
    let fill = fill.elapsed();
    let health = |url: String| async move {
        reqwest::get(format!("{url}/healthz"))
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    let before = health(url.clone()).await;
    let stored = |h: &Value| h["runs"].as_u64().unwrap() + h["segment_rows"].as_u64().unwrap();
    assert_eq!(stored(&before), RUNS, "{before}");
    child.kill().unwrap();
    child.wait().unwrap();
    let disk = du(dir.path());

    let (mut child, url, ready) = spawn(dir.path());
    let after = health(url.clone()).await;
    eprintln!(
        "filled {RUNS} runs in {fill:?} ({:.0} events/s); {} segments, {} MB on disk; restart ready in {ready:?} ({} runs replayed into memory)",
        RUNS as f64 / fill.as_secs_f64(),
        after["segments"],
        disk / 1024 / 1024,
        after["runs"]
    );
    assert_eq!(stored(&after), RUNS, "{after}");
    assert!(ready < Duration::from_secs(10), "restart took {ready:?}");
    // A run that's only on disk opens.
    let t: Value = reqwest::get(format!("{url}/api/traces/00000000-0000-4000-8000-000000000007"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(t["run_count"], 1);
    child.kill().unwrap();
    child.wait().unwrap();
}

fn du(p: &std::path::Path) -> u64 {
    std::fs::read_dir(p)
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| match e.file_type() {
                    Ok(t) if t.is_dir() => du(&e.path()),
                    _ => e.metadata().map_or(0, |m| m.len()),
                })
                .sum()
        })
        .unwrap_or(0)
}
