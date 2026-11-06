//! `tracedb demo` (contracts/cli.md): the overnight-eval scenario on its own server, a pass/fail table, exit 1
//! naming expected vs actual, 2 when `--url` isn't reachable.

use tracedb::demo::{Check, report};

fn demo(args: &[&str]) -> (i32, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tracedb"))
        .arg("demo")
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn passes_every_m2_check_on_its_own_server() {
    let (code, out, err) = demo(&["--seed", "7", "--memtable", "1MB", "--checks", "m2"]);
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.starts_with("tracedb demo — M2 checks  (seed 7, "), "{out}");
    for row in [
        "✓ 14 failing roots",
        "✓ 23 quiet failures",
        "✓ prompt_version top values",
        "v6 928 · v7 912",
        "✓ v7 ∧ status_code = 429",
        "✓ checks across memtable and files",
    ] {
        assert!(out.contains(row), "missing {row:?} in\n{out}");
    }
    assert!(out.trim_end().ends_with("all 5 checks passed"), "{out}");
}

#[test]
fn a_failed_check_names_expected_and_actual() {
    let checks = [
        Check::new("14 failing roots", "14", "14", Some(31)),
        Check::new("23 quiet failures", "23", "22", Some(58)),
    ];
    let mut out = Vec::new();
    let ok = report("tracedb demo — M2 checks  (seed 7)", &checks, &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(!ok);
    assert!(out.contains("✗ 23 quiet failures"), "{out}");
    assert!(out.contains("23 quiet failures: expected 23, got 22"), "{out}");
    assert!(out.contains("1 of 2 checks failed"), "{out}");
}

#[test]
fn unreachable_url_exits_2() {
    let (code, _, err) = demo(&["--url", "http://127.0.0.1:9"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("not reachable"), "{err}");
}

#[test]
fn unknown_check_set_is_a_usage_error() {
    let (code, _, err) = demo(&["--checks", "m9"]);
    assert_eq!(code, 1, "{err}");
}
