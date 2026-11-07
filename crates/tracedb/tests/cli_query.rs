//! `tracedb query` (contracts/cli.md): a table or JSON lines from a running server; exit 1 on a bad query,
//! 2 when the server isn't reachable.

mod common;

use common::*;
use serde_json::{Value, json};

async fn tracedb(args: Vec<String>) -> (i32, String, String) {
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_tracedb"))
            .arg("query")
            .args(args)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

fn args(server: &Server, rest: &[&str]) -> Vec<String> {
    let mut a = vec!["--url".to_string(), server.url("")];
    a.extend(rest.iter().map(|s| s.to_string()));
    a
}

#[tokio::test(flavor = "multi_thread")]
async fn prints_quiet_failures_as_a_table_or_json_lines() {
    let server = start().await;
    let done = |mut r: Value, error: Option<&str>| {
        r["end_time"] = json!("2026-11-05T02:30:00.000000");
        if let Some(e) = error {
            r["error"] = json!(e);
        }
        r
    };
    post_batch(
        &server,
        vec![
            done(ls_root(1, "p", 1), Some("task failed")),
            done(ls_root(2, "p", 2), None),
            done(ls_child(2, 2, 21, "p"), Some("HTTPError: 429")),
            done(ls_root(3, "p", 3), None),
            done(ls_root(4, "q", 4), None),
        ],
        vec![],
    )
    .await;

    let (code, out, err) = tracedb(args(
        &server,
        &[
            "--project",
            "p",
            "--root",
            "--tree-filter",
            r#"eq(status, "error")"#,
            r#"eq(status, "ok")"#,
        ],
    ))
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(out.lines().next().unwrap().starts_with("START"), "{out}");
    assert!(
        out.contains(&uid(2)) && !out.contains(&uid(1)) && !out.contains(&uid(3)),
        "{out}"
    );
    assert!(out.contains("ok") && out.trim_end().ends_with("1 run"), "{out}");

    let (code, out, _) = tracedb(args(&server, &["--project", "p", "--root", "--json", "--limit", "2"])).await;
    assert_eq!(code, 0);
    let lines: Vec<Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().all(|r| r["project"] == "p" && r["is_root"] == true));
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_queries_and_missing_servers_exit_non_zero() {
    let server = start().await;
    let (code, _, err) = tracedb(args(&server, &["--tree-filter", r#"eq(staus, "error")"#])).await;
    assert_eq!(code, 1);
    assert!(err.contains("--tree-filter") && err.contains("did you mean"), "{err}");
    let lines: Vec<&str> = err.lines().collect();
    assert_eq!(
        lines[1..],
        [r#"  eq(staus, "error")"#, "     ^^^^^"],
        "points at the problem"
    );

    let (code, _, err) = tracedb(args(&server, &["--time", "yesterday"])).await;
    assert_eq!(code, 1);
    assert!(err.contains("--time"), "{err}");
    let (code, _, _) = tracedb(args(&server, &["--limit", "0"])).await;
    assert_eq!(code, 1, "usage errors exit 1");

    let (code, _, err) = tracedb(vec!["--url".into(), "http://127.0.0.1:1".into()]).await;
    assert_eq!(code, 2, "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn prints_an_aggregate_table_with_other_total_and_no_price() {
    let server = start().await;
    let llm = |n: u64, model: &str, secs: u32| {
        let mut r = ls_child(1, 1, n, "p");
        r["name"] = json!("llm_call");
        r["run_type"] = json!("llm");
        r["extra"] = json!({"metadata": {"ls_model_name": model}});
        r["outputs"] = json!({"usage_metadata": {"input_tokens": 1_000_000, "output_tokens": 0}});
        r["end_time"] = json!(format!("2026-11-05T02:01:{secs:02}.000000"));
        r
    };
    let mut fetch = ls_child(1, 1, 30, "p");
    fetch["name"] = json!("fetch_url");
    fetch["error"] = json!("HTTPError: 429");
    fetch["end_time"] = json!("2026-11-05T02:01:31.000000");
    post_batch(
        &server,
        vec![
            ls_root(1, "p", 1),
            llm(10, "gpt-4o-mini", 3),
            llm(11, "gpt-4o-mini", 5),
            llm(12, "local-llama", 9),
            fetch,
        ],
        vec![],
    )
    .await;
    let metrics = "count,error_rate,p95(latency),sum(cost)";
    let (code, out, err) = tracedb(args(
        &server,
        &[
            "--project",
            "p",
            "--group-by",
            "name",
            "--metrics",
            metrics,
            "--top",
            "1",
        ],
    ))
    .await;
    assert_eq!(code, 0, "{err}");
    let lines: Vec<&str> = out.lines().collect();
    let words = |l: &str| l.split_whitespace().map(str::to_string).collect::<Vec<_>>();
    assert_eq!(words(lines[0]), ["name", "runs", "error_rate", "p95", "cost"], "{out}");
    assert_eq!(words(lines[1]), ["llm_call", "3", "0.0%", "8.0", "s", "$0.30"], "{out}");
    assert_eq!(words(lines[2])[..3], ["(other)", "2", "100.0%"], "{out}");
    assert!(
        lines[3].starts_with("total") && lines[3].ends_with("1 run had no price"),
        "{out}"
    );
    assert_eq!(words(lines[3])[..5], ["total", "5", "25.0%", "30.0", "s"], "{out}");

    let (code, out, _) = tracedb(args(&server, &["--project", "p", "--group-by", "name", "--json"])).await;
    assert_eq!(code, 0);
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(v["totals"]["runs"], 5);

    let (code, _, err) = tracedb(args(&server, &["--group-by", "colour"])).await;
    assert_eq!(code, 1);
    assert!(err.contains("--group-by/--metrics") && err.contains("colour"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn search_adds_a_phrase_to_the_filter() {
    let server = start().await;
    let mut a = ls_child(1, 1, 11, "p");
    a["name"] = json!("fetch_url");
    a["tags"] = json!(["batch=a"]);
    a["error"] = json!("HTTPError: 429 Too Many Requests");
    let mut b = ls_child(1, 1, 12, "p");
    b["name"] = json!("fetch_url");
    b["outputs"] = json!({"body": "429 \"Too\" Many Requests"});
    let mut c = ls_child(1, 1, 13, "p");
    c["tags"] = json!(["batch=a"]);
    c["outputs"] = json!({"body": "Too Many 429 Requests"});
    post_batch(&server, vec![ls_root(1, "p", 1), a, b, c], vec![]).await;

    let (code, out, err) = tracedb(args(&server, &["--search", "429 Too Many Requests", "--json"])).await;
    assert_eq!(code, 0, "{err}");
    let lines: Vec<Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let mut ids: Vec<&str> = lines.iter().map(|r| r["run_id"].as_str().unwrap()).collect();
    ids.sort();
    assert_eq!(ids, [uid(11), uid(12)]);
    assert!(lines.iter().all(|r| r["match"]["excerpt"].is_string()));

    let (code, out, err) = tracedb(args(
        &server,
        &["--search", "429 \"too\" many", r#"has(tags, "batch=a")"#],
    ))
    .await;
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(&uid(11)) && !out.contains(&uid(12)) && !out.contains(&uid(13)),
        "{out}"
    );
    assert!(out.trim_end().ends_with("1 run"), "{out}");

    // Errors point at the flag they came from.
    let (code, _, err) = tracedb(args(&server, &["--search", "x", r#"eq(staus, "error")"#])).await;
    assert_eq!(code, 1);
    assert!(err.contains("EXPR") && err.contains("did you mean"), "{err}");
    assert_eq!(err.lines().nth(2), Some("     ^^^^^"), "{err}");
    let (code, _, err) = tracedb(args(&server, &["--search", "!!", r#"eq(status, "error")"#])).await;
    assert_eq!(code, 1);
    assert!(err.contains("--search") && err.contains("at least one word"), "{err}");
    let (code, _, err) = tracedb(args(&server, &["--search", "x", r#"eq(name, "a"), eq(name, "b")"#])).await;
    assert_eq!(code, 1, "EXPR is checked on its own, not as part of the and: {err}");
    assert!(err.contains("EXPR"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compares_batch_a_with_batch_b_in_b_columns() {
    const MB: u64 = 1024 * 1024;
    let server = start_with(|c| c.limits.memtable_bytes = MB).await;
    let s = tracedb::scenario::generate(7, tracedb_model::Ts::now().0, MB);
    tracedb::scenario::load(&format!("http://{}", server.addr), &s)
        .await
        .unwrap();
    let tag = |t: &str| format!("has(tags, {t:?})");
    let (a, b) = (tag(tracedb::scenario::BATCH_TAG), tag(tracedb::scenario::BATCH_B_TAG));
    let base = [
        "--project",
        tracedb::scenario::PROJECT,
        "--group-by",
        "name",
        "--metrics",
        "error_rate,p95(latency)",
    ];
    let mut rest = base.to_vec();
    rest.extend(["--compare", b.as_str(), a.as_str()]);
    let (code, out, err) = tracedb(args(&server, &rest)).await;
    assert_eq!(code, 0, "{err}");
    let words = |l: &str| l.split_whitespace().map(str::to_string).collect::<Vec<_>>();
    let lines: Vec<&str> = out.lines().collect();
    let head = "name runs error_rate p95 │ b: runs error_rate p95";
    assert_eq!(words(lines[0]), words(head), "{out}");
    let fetch = lines.iter().find(|l| l.starts_with("fetch_url")).unwrap();
    assert_eq!(
        words(fetch),
        ["fetch_url", "300", "14.0%", "31.0", "s", "│", "300", "2.0%", "9.0", "s"],
        "{out}"
    );
    assert!(lines.iter().any(|l| l.starts_with("total") && l.contains('│')), "{out}");

    let mut rest = base.to_vec();
    rest.extend(["--time", "24h", "--shift", "1d"]);
    let (code, out, err) = tracedb(args(&server, &rest)).await;
    assert_eq!(code, 0, "{err}");
    assert!(out.lines().next().unwrap().contains("│ b:"), "{out}");
    let mut rest = base.to_vec();
    rest.extend(["--shift", "1d", "--compare", b.as_str()]);
    let (code, _, err) = tracedb(args(&server, &rest)).await;
    assert!(code == 1 && err.contains("cannot be used with"), "{err}");
}
