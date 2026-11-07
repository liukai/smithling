//! Crash safety against the real binary (SC-04). Slow, so `#[ignore]`; CI runs it with `--ignored`.
//!
//! Each round: start `tracedb serve` with a tiny memtable (so data is flushed to files all the time), upload
//! from several clients (starts, then updates), kill -9 at a random moment, restart, re-send every batch
//! that never got an answer (as an SDK retry would), then check every confirmed item is present, merged,
//! and stored exactly once. Five rounds kill at a random moment of the upload; the other five make the server
//! abort itself at a chosen point of a flush (SC-05): between the two file puts, between the puts and the
//! catalog commit, after the commit before the WAL trim, in the middle of the trim, and after it.

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

/// With `TRACEDB_TEST_S3_URL` and `TRACEDB_TEST_PG_URL` set (CI's cloud job), each round gets its own S3 prefix
/// and Postgres database; otherwise the defaults (files and SQLite in the data folder).
async fn storage_flags(round: u64) -> Vec<String> {
    let (Ok(s3), Ok(pg)) = (
        std::env::var("TRACEDB_TEST_S3_URL"),
        std::env::var("TRACEDB_TEST_PG_URL"),
    ) else {
        assert!(
            std::env::var_os("TRACEDB_TEST_REQUIRE_CLOUD").is_none(),
            "TRACEDB_TEST_REQUIRE_CLOUD is set but TRACEDB_TEST_S3_URL / TRACEDB_TEST_PG_URL aren't"
        );
        return Vec::new();
    };
    use sqlx::Connection;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let name = format!("tracedb_crash_{nanos}_r{round}");
    let mut conn = sqlx::PgConnection::connect(&pg)
        .await
        .expect("connect to TRACEDB_TEST_PG_URL");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&mut conn)
        .await
        .unwrap();
    let mut metastore = url::Url::parse(&pg).expect("a postgres:// URL");
    metastore.set_path(&name);
    vec![
        "--storage".into(),
        format!("{}/{name}", s3.trim_end_matches('/')),
        "--metastore".into(),
        metastore.to_string(),
    ]
}

fn spawn(data: &std::path::Path, storage: &[String], crash_at: Option<&str>) -> (Child, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tracedb"));
    // Small WAL files, so flushes have whole files to trim; retries are re-sent right after the restart, well
    // inside the dedup window (rebuilt from the catalog's saved keys).
    cmd.args([
        "serve",
        "--data",
        data.to_str().unwrap(),
        "--port",
        "0",
        "--memtable",
        "256KB",
    ])
    .args(["--dedup-window", "20s"])
    .args(storage)
    .env("TRACEDB_TEST_WAL_ROLL", "65536");
    match crash_at {
        Some(p) => cmd.env("TRACEDB_TEST_CRASH_AT", p),
        None => cmd.env_remove("TRACEDB_TEST_CRASH_AT"),
    };
    // On cloud storage, show the server's errors (a bad endpoint or password otherwise looks like a hang).
    let stderr = if storage.is_empty() {
        Stdio::null()
    } else {
        Stdio::inherit()
    };
    let mut child = cmd.stdout(Stdio::piped()).stderr(stderr).spawn().unwrap();
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
        let crash_at = match round {
            5 => Some("during_put"),
            6 => Some("after_put"),
            7 => Some("after_commit"),
            8 => Some("during_trim"),
            9 => Some("after_trim"),
            _ => None,
        };
        let dir = tempfile::tempdir().unwrap();
        let storage = storage_flags(round).await;
        let (mut child, url) = spawn(dir.path(), &storage, crash_at);
        let log = Arc::new(Mutex::new(Log::default()));
        let clients: Vec<_> = (0..4u64)
            .map(|c| tokio::spawn(client(url.clone(), c * 1_000_000 + 1, log.clone())))
            .collect();

        let wait = random_ms(round);
        if crash_at.is_some() {
            // The server aborts itself at the crash point of a flush.
            let deadline = std::time::Instant::now() + Duration::from_secs(120);
            let status = loop {
                if let Some(s) = child.try_wait().unwrap() {
                    break s;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "round {round}: crash point never reached"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(
                status.signal(),
                Some(6),
                "round {round}: died other than by the crash point: {status}"
            );
        } else {
            tokio::time::sleep(Duration::from_millis(wait)).await;
            child.kill().unwrap();
            child.wait().unwrap();
        }
        for c in clients {
            c.await.unwrap();
        }

        let (mut child, url) = spawn(dir.path(), &storage, None);
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
        let health: Value = reqwest::get(format!("{url}/healthz"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let segments = health["segments"].as_u64().unwrap_or(0);
        for n in &log.confirmed_start {
            let r = runs
                .get(&uid(*n))
                .unwrap_or_else(|| panic!("round {round}: confirmed run {n} missing after kill -9 at {wait} ms"));
            if log.confirmed_end.contains(n) {
                assert_eq!(r["status"], "ok", "round {round}: confirmed end of {n} not merged");
            }
        }
        let mut ok = 0;
        for r in runs.values() {
            let expected = if r["status"] == "ok" { 2 } else { 1 };
            ok += (expected - 1) as u64;
            assert_eq!(
                r["event_count"], expected,
                "round {round}: {} stored more than once",
                r["run_id"]
            );
        }
        // Reads merge versions, so also count them: a run has at most a start and an end version; more means
        // recovery stored flushed events again.
        let versions = health["runs"].as_u64().unwrap() + health["segment_rows"].as_u64().unwrap();
        assert!(
            versions <= runs.len() as u64 + ok,
            "round {round}: {versions} stored versions for {} runs ({ok} ended)",
            runs.len()
        );
        eprintln!(
            "round {round}{}: {}; {} starts + {} ends confirmed, {} batches re-sent, {} segments; all present",
            storage.get(1).map_or(String::new(), |s| format!(" on {s}")),
            crash_at.map_or(format!("killed at {wait} ms"), |p| format!("crashed {p}")),
            log.confirmed_start.len(),
            log.confirmed_end.len(),
            log.unanswered.len(),
            segments
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }
    eprintln!("10/10 rounds: 0 missing, 0 duplicated");
}
