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
