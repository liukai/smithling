//! Sustained ingest while queries run (SC-04): at least 20,000 events/s for 60 s from 8 clients while a query
//! loop lists, searches and groups the same project, and flushes write files and their text index; p99 upload
//! latency ≤ 1 s (a `429` retry counts toward its batch's latency). Slow, so `#[ignore]`; run it with `--release`.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const WORDS: [&str; 12] = [
    "fetch", "page", "summary", "retry", "model", "answer", "timeout", "parse", "plan", "tool", "search", "result",
];

fn text(n: u64, words: usize) -> String {
    (0..words as u64)
        .map(|i| WORDS[((n.wrapping_mul(2_654_435_761) >> 7).wrapping_add(i * 5) % WORDS.len() as u64) as usize])
        .collect::<Vec<_>>()
        .join(" ")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore]
async fn sustains_20000_events_per_second_while_querying() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tracedb"))
        .args(["serve", "--data", dir.path().to_str().unwrap(), "--port", "0"])
        // Small enough that the 60 s run flushes many times.
        .args(["--memtable", "16MB"])
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

    let latencies = Arc::new(Mutex::new(Vec::<Duration>::new()));
    let until = Instant::now() + Duration::from_secs(60);
    let clients: Vec<_> = (0..8u64)
        .map(|c| {
            let (url, latencies) = (url.clone(), latencies.clone());
            tokio::spawn(async move {
                let http = reqwest::Client::new();
                let mut n = c * 1_000_000_000;
                let mut mine = Vec::new();
                while Instant::now() < until {
                    let post: Vec<_> = (0..100)
                        .map(|_| {
                            n += 1;
                            let id = format!("00000000-0000-4000-8000-{n:012}");
                            json!({"id": id, "trace_id": id, "dotted_order": format!("20261105T020000000000Z{id}"),
                                   "session_name": "tput", "name": WORDS[(n % 4) as usize], "run_type": "chain",
                                   "start_time": "2026-11-05T02:00:00.000000",
                                   "inputs": {"q": text(n, 30)}, "outputs": {"a": text(n + 1, 10)}})
                        })
                        .collect();
                    let body = json!({"post": post});
                    let sent = Instant::now();
                    loop {
                        let r = http.post(format!("{url}/runs/batch")).json(&body).send().await.unwrap();
                        match r.status().as_u16() {
                            202 => break,
                            429 => tokio::time::sleep(Duration::from_millis(50)).await,
                            s => panic!("upload failed: {s}"),
                        }
                    }
                    mine.push(sent.elapsed());
                }
                latencies.lock().unwrap().extend(mine);
            })
        })
        .collect();

    // One reader at a time, back to back: a list, a text search and a grouped query over what's loaded so far.
    let queries = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (url, queries, stop) = (url.clone(), queries.clone(), stop.clone());
        tokio::spawn(async move {
            let http = reqwest::Client::new();
            let reqs = [
                ("/api/query", json!({"project": "tput", "limit": 50})),
                (
                    "/api/query",
                    json!({"project": "tput", "filter": "search(\"timeout retry\")", "limit": 50}),
                ),
                (
                    "/api/aggregate",
                    json!({"project": "tput", "group_by": ["name"], "metrics": ["count"]}),
                ),
            ];
            let mut i = 0;
            while !stop.load(Ordering::Relaxed) {
                let (path, body) = &reqs[i % reqs.len()];
                let r = http.post(format!("{url}{path}")).json(body).send().await.unwrap();
                assert_eq!(r.status(), 200, "{path} {body}: {}", r.text().await.unwrap());
                queries.fetch_add(1, Ordering::Relaxed);
                i += 1;
            }
        })
    };
    let started = Instant::now();
    for c in clients {
        c.await.unwrap();
    }
    let elapsed = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    reader.await.unwrap();
    let health: Value = reqwest::get(format!("{url}/healthz"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    child.kill().unwrap();
    child.wait().unwrap();

    let mut lat = Arc::try_unwrap(latencies).unwrap().into_inner().unwrap();
    lat.sort();
    let rate = (lat.len() * 100) as f64 / elapsed.as_secs_f64();
    let p99 = lat[(lat.len() * 99 / 100).min(lat.len() - 1)];
    let queries = queries.load(Ordering::Relaxed);
    eprintln!(
        "sustained {rate:.0} events/s over {elapsed:?} with {queries} queries; {} batches, p50 {:?}, p99 {p99:?}, \
         max {:?}; {} flushes, {} segments ({} without a text index)",
        lat.len(),
        lat[lat.len() / 2],
        lat[lat.len() - 1],
        health["flushes"],
        health["segments"],
        health["unindexed_segments"]
    );
    assert!(
        health["flushes"].as_u64().unwrap() >= 2,
        "flushes ran during the test: {health}"
    );
    assert_eq!(health["unindexed_segments"], 0, "every flush wrote its text index");
    assert!(queries >= 30, "the query loop ran: {queries} queries");
    assert!(rate >= 20_000.0, "{rate:.0} events/s");
    assert!(p99 <= Duration::from_secs(1), "p99 upload latency {p99:?}");
}
