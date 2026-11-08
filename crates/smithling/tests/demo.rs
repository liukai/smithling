//! `smithling demo` (contracts/cli.md): the overnight-eval scenario on its own server, a pass/fail table, exit 1
//! naming expected vs actual, 2 when `--url` isn't reachable.

use smithling_server::testkit as common;

use serde_json::json;
use smithling_tools::demo::{Check, report};
use smithling_tools::scenario;

fn demo(args: &[&str]) -> (i32, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_smithling"))
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

const M2_ROWS: [&str; 6] = [
    "✓ 14 failing roots",
    "✓ 23 quiet failures",
    "✓ prompt_version top values",
    "v6 928 · v7 912",
    "✓ v7 ∧ status_code = 429",
    "✓ checks across memtable and files",
];

const M3_ROWS: [&str; 5] = [
    "31 text hits, all fetch_url",
    "fetch_url error rate / p95 (batch a)",
    "batch a cost",
    "batch a vs b",
    "feedback filter and average",
];

fn row<'a>(out: &'a str, name: &str) -> &'a str {
    out.lines()
        .find(|l| l.contains(name))
        .unwrap_or_else(|| panic!("no {name:?} row in\n{out}"))
}

#[test]
fn passes_every_m2_and_m3_check_on_its_own_server() {
    let (code, out, err) = demo(&["--seed", "7", "--memtable", "1MB"]);
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.starts_with("smithling demo — M2 + M3 checks  (seed 7, "), "{out}");
    for r in M2_ROWS {
        assert!(out.contains(r), "missing {r:?} in\n{out}");
    }
    for (name, actual) in [
        ("31 text hits, all fetch_url", "31"),
        ("fetch_url error rate / p95 (batch a)", "14.0% / 31 s"),
        ("batch a cost", "$4.12 (12 no price)"),
        ("batch a vs b", "14.0% → 2.0%, 31 s → 9 s, side by side + overlaid"),
        (
            "feedback filter and average",
            "2 runs · avg 0.00 (1 sent before its run)",
        ),
    ] {
        let line = row(&out, name);
        assert!(line.trim_start().starts_with(&format!("✓ {name}")), "{line}");
        assert!(line.contains(actual), "{name}: want {actual:?} in {line:?}");
    }
    for name in ["31 text hits", "error rate / p95", "batch a vs b", "feedback filter"] {
        assert!(
            row(&out, name).trim_end().ends_with(" ms"),
            "{name} has a timing:\n{out}"
        );
    }
    assert!(out.trim_end().ends_with("all 10 checks passed"), "{out}");
}

#[test]
fn checks_m2_runs_only_the_m2_rows() {
    let (code, out, err) = demo(&["--seed", "7", "--memtable", "1MB", "--checks", "m2"]);
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.starts_with("smithling demo — M2 checks  (seed 7, "), "{out}");
    for r in M2_ROWS {
        assert!(out.contains(r), "missing {r:?} in\n{out}");
    }
    for name in M3_ROWS {
        assert!(!out.contains(name), "unexpected {name:?} in\n{out}");
    }
    assert!(out.trim_end().ends_with("all 5 checks passed"), "{out}");
}

/// A planted number changed under the demo (one more batch run with the phrase, not a fetch_url): exit 1 naming
/// expected vs actual.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_planted_number_exits_1_naming_expected_and_actual() {
    const MB: u64 = 1024 * 1024;
    let server = common::start_with(|c| c.limits.memtable_bytes = MB).await;
    let mut extra = common::ls_root(1, scenario::PROJECT, 0);
    extra["name"] = json!("web_probe");
    extra["tags"] = json!([scenario::BATCH_B_TAG]);
    extra["error"] = json!(format!("HTTPError: {}", scenario::PHRASE));
    common::post_batch(&server, vec![extra], vec![]).await;
    let url = server.url("");
    let (code, out, err) = tokio::task::spawn_blocking(move || {
        demo(&["--url", &url, "--seed", "7", "--memtable", "1MB", "--checks", "m3"])
    })
    .await
    .unwrap();
    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(out.starts_with("smithling demo — M3 checks  (seed 7, "), "{out}");
    assert!(row(&out, "31 text hits").contains("✗"), "{out}");
    assert!(
        out.contains("31 text hits, all fetch_url: expected 31, got 32 (not all fetch_url)"),
        "{out}"
    );
    assert!(out.contains("1 of 5 checks failed"), "{out}");
}

/// A `smithling serve` child on a temporary folder and a free port, killed on drop.
struct Serve {
    child: std::process::Child,
    url: String,
    _dir: tempfile::TempDir,
}

impl Serve {
    fn start(extra: &[&str]) -> Serve {
        use std::io::{BufRead, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_smithling"))
            .args(["serve", "--data", dir.path().to_str().unwrap(), "--port", "0"])
            .args(extra)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut serve = Serve {
            child,
            url: String::new(),
            _dir: dir,
        };
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        serve.url = line
            .split_whitespace()
            .find(|w| w.starts_with("http://"))
            .unwrap_or_else(|| panic!("no url in {line:?}"))
            .to_string();
        serve
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `serve` with its default settings (a 64 MB heap memtable): `demo --url` sizes its background load from the
/// limit in `/healthz`, so every check passes without `--memtable` on either side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passes_every_check_against_a_default_serve() {
    let serve = Serve::start(&[]);
    let url = serve.url.clone();
    let health: serde_json::Value = reqwest::get(format!("{url}/healthz"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["memtable_limit_bytes"], 64 * 1024 * 1024, "{health}");
    let (code, out, err) = tokio::task::spawn_blocking(move || demo(&["--url", &url, "--seed", "7"]))
        .await
        .unwrap();
    assert_eq!(code, 0, "stdout:\n{out}\nstderr:\n{err}");
    for r in M2_ROWS {
        assert!(out.contains(r), "missing {r:?} in\n{out}");
    }
    assert!(out.trim_end().ends_with("all 10 checks passed"), "{out}");
}

/// A server memtable over 256 MB would make the demo build and send gigabytes: refused before any upload.
#[test]
fn refuses_a_server_memtable_over_256mb() {
    let serve = Serve::start(&["--memtable", "512MB"]);
    let (code, out, err) = demo(&["--url", &serve.url, "--seed", "7"]);
    assert_eq!(code, 1, "stdout:\n{out}\nstderr:\n{err}");
    assert!(err.contains("memtable limit is 512 MB"), "{err}");
    assert!(out.is_empty(), "{out}");
}

#[test]
fn a_failed_check_names_expected_and_actual() {
    let checks = [
        Check::new("14 failing roots", "14", "14", Some(31)),
        Check::new("23 quiet failures", "23", "22", Some(58)),
    ];
    let mut out = Vec::new();
    let ok = report("smithling demo — M2 checks  (seed 7)", &checks, &mut out).unwrap();
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
    for bad in ["m9", "m2,m9", ""] {
        let (code, _, err) = demo(&["--checks", bad]);
        assert_eq!(code, 1, "{bad:?}: {err}");
    }
}
