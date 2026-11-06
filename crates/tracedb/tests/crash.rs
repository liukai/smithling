//! Crash safety against the real binary (SC-04). Slow, so `#[ignore]`; CI runs it with `--ignored`.
//!
//! Each round: start `tracedb serve`, upload from several clients (starts, then updates), kill -9
//! at a random moment, restart, re-send every batch that never got an answer (as an SDK retry would),
//! then check every confirmed item is present, merged, and stored exactly once.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

fn uid(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

fn start_item(n: u64) -> Value {
    let id = uid(n);
    json!({"id": id, "trace_id": id, "dotted_order": format!("20261105T020000000000Z{id}"),
           "session_name": "crash", "name": format!("run-{n}"), "run_type": "chain",
           "start_time": "2026-11-05T02:00:00.000000"})
}

fn end_item(n: u64) -> Value {
    let id = uid(n);
    json!({"id": id, "trace_id": id, "dotted_order": format!("20261105T020000000000Z{id}"),
           "end_time": "2026-11-05T02:05:00.000000", "outputs": {"n": n}})
}

fn spawn(data: &std::path::Path) -> (Child, String) {
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
        .expect("listening line")
        .to_string();
    (child, url)
}

fn random_ms(round: u64) -> u64 {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() as u64;
    500 + (nanos ^ round.wrapping_mul(0x9E37_79B9)) % 4500
}

#[derive(Default)]
struct Log {
    confirmed_start: HashSet<u64>,
    confirmed_end: HashSet<u64>,
    unanswered: Vec<Value>,
}

async fn client(url: String, first: u64, log: Arc<Mutex<Log>>) {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let mut n = first;
    loop {
        let starts: Vec<u64> = (n..n + 10).collect();
        let ends: Vec<u64> = starts.iter().copied().filter(|i| i % 2 == 0).collect();
        n += 10;
        for (body, ids, is_end) in [
            (
                json!({"post": starts.iter().map(|i| start_item(*i)).collect::<Vec<_>>()}),
                starts.clone(),
                false,
            ),
            (
                json!({"patch": ends.iter().map(|i| end_item(*i)).collect::<Vec<_>>()}),
                ends.clone(),
                true,
            ),
        ] {
            match http.post(format!("{url}/runs/batch")).json(&body).send().await {
                Ok(r) if r.status() == 202 => {
                    let mut l = log.lock().unwrap();
                    let set = if is_end {
                        &mut l.confirmed_end
                    } else {
                        &mut l.confirmed_start
                    };
                    set.extend(ids);
                }
                _ => {
                    log.lock().unwrap().unanswered.push(body);
                    return;
                }
            }
        }
    }
}

async fn all_runs(url: &str) -> HashMap<String, Value> {
    let mut out = HashMap::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut q = format!("{url}/api/roots?project=crash&limit=200");
        if let Some(c) = &cursor {
            q.push_str(&format!("&cursor={c}"));
        }
        let page: Value = reqwest::get(q).await.unwrap().json().await.unwrap();
        for r in page["runs"].as_array().unwrap() {
            out.insert(r["run_id"].as_str().unwrap().to_string(), r.clone());
        }
        match page["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => return out,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn kill_9_loses_nothing_confirmed() {
    for round in 0..10u64 {
        let dir = tempfile::tempdir().unwrap();
        let (mut child, url) = spawn(dir.path());
        let log = Arc::new(Mutex::new(Log::default()));
        let clients: Vec<_> = (0..4u64)
            .map(|c| tokio::spawn(client(url.clone(), c * 1_000_000 + 1, log.clone())))
            .collect();

        let wait = random_ms(round);
        tokio::time::sleep(Duration::from_millis(wait)).await;
        child.kill().unwrap();
        child.wait().unwrap();
        for c in clients {
            c.await.unwrap();
        }

        let (mut child, url) = spawn(dir.path());
        let log = Arc::try_unwrap(log).ok().unwrap().into_inner().unwrap();
        let http = reqwest::Client::new();
        for body in &log.unanswered {
            assert_eq!(
                http.post(format!("{url}/runs/batch"))
                    .json(body)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                202
            );
        }

        let runs = all_runs(&url).await;
        for n in &log.confirmed_start {
            let r = runs
                .get(&uid(*n))
                .unwrap_or_else(|| panic!("round {round}: confirmed run {n} missing after kill -9 at {wait} ms"));
            if log.confirmed_end.contains(n) {
                assert_eq!(r["status"], "ok", "round {round}: confirmed end of {n} not merged");
            }
        }
        for r in runs.values() {
            let expected = if r["status"] == "ok" { 2 } else { 1 };
            assert_eq!(
                r["event_count"], expected,
                "round {round}: {} stored more than once",
                r["run_id"]
            );
        }
        eprintln!(
            "round {round}: killed at {wait} ms; {} starts + {} ends confirmed, {} batches re-sent; all present",
            log.confirmed_start.len(),
            log.confirmed_end.len(),
            log.unanswered.len()
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
