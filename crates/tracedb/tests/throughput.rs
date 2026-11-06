//! Sustained ingest (SC-07): at least 5,000 events/s for 60 s from 8 clients. Slow, so `#[ignore]`.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn sustains_5000_events_per_second_for_60s() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tracedb"))
        .args([
            "serve",
            "--data",
            dir.path().to_str().unwrap(),
            "--port",
            "0",
            "--max-runs",
            "100000000",
            "--max-memory",
            "16GB",
        ])
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

    let accepted = Arc::new(AtomicU64::new(0));
    let until = Instant::now() + Duration::from_secs(60);
    let clients: Vec<_> = (0..8u64)
        .map(|c| {
            let (url, accepted) = (url.clone(), accepted.clone());
            tokio::spawn(async move {
                let http = reqwest::Client::new();
                let mut n = c * 1_000_000_000;
                while Instant::now() < until {
                    let post: Vec<_> = (0..100)
                        .map(|_| {
                            n += 1;
                            let id = format!("00000000-0000-4000-8000-{n:012}");
                            json!({"id": id, "trace_id": id, "dotted_order": format!("20261105T020000000000Z{id}"),
                                   "session_name": "tput", "name": "r", "run_type": "chain",
                                   "start_time": "2026-11-05T02:00:00.000000", "inputs": {"q": "x".repeat(200)}})
                        })
                        .collect();
                    let r = http
                        .post(format!("{url}/runs/batch"))
                        .json(&json!({"post": post}))
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(r.status(), 202);
                    accepted.fetch_add(100, Ordering::Relaxed);
                }
            })
        })
        .collect();
    let started = Instant::now();
    for c in clients {
        c.await.unwrap();
    }
    let rate = accepted.load(Ordering::Relaxed) as f64 / started.elapsed().as_secs_f64();
    eprintln!("sustained {rate:.0} events/s over {:?}", started.elapsed());
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(rate >= 5000.0, "{rate:.0} events/s");
}
