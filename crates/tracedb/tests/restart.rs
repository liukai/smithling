//! Restart time at the M1 capacity limit (500k runs): ready within 30 s (design §11.4).
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
async fn restart_at_the_capacity_limit_is_ready_within_30s() {
    const RUNS: u64 = 500_000;
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
                    let r = http
                        .post(format!("{url}/runs/batch"))
                        .json(&json!({"post": post}))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(r.status(), 202);
                    b += 8;
                }
            })
        })
        .collect();
    for w in workers {
        w.await.unwrap();
    }
    let fill = fill.elapsed();
    let health: Value = reqwest::get(format!("{url}/healthz"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["runs"], RUNS);
    child.kill().unwrap();
    child.wait().unwrap();

    let (mut child, url, ready) = spawn(dir.path());
    let health: Value = reqwest::get(format!("{url}/healthz"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    eprintln!(
        "filled {RUNS} runs in {fill:?} ({:.0} events/s); restart ready in {ready:?}; memory {} MB",
        RUNS as f64 / fill.as_secs_f64(),
        health["memory_bytes"].as_u64().unwrap() / 1024 / 1024
    );
    assert_eq!(health["runs"], RUNS);
    assert!(ready < Duration::from_secs(30), "restart took {ready:?}");
    child.kill().unwrap();
    child.wait().unwrap();
}
