//! `tracedb bench` as a gate (M3 T104, contracts/cli.md): `--check` exits 1 naming the query, target and value;
//! `--json` writes the table; a setup error exits 2. Small `--runs` and the hidden `--samples`, `--idle-secs`
//! and `--flush-age-secs` / `--dedup-window-secs` keep it quick.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use tracedb_tools::bench;

fn bench(dir: &Path, extra: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tracedb"));
    cmd.args([
        "bench",
        "--runs",
        "2000",
        "--samples",
        "5",
        "--idle-secs",
        "1",
        "--flush-age-secs",
        "1",
        "--dedup-window-secs",
        "1",
    ])
    .arg("--data")
    .arg(dir.join("data"))
    .args(extra)
    .env("RUST_LOG", "warn");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn json_has_every_row_and_the_calibration() {
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("bench.json");
    let o = bench(dir.path(), &["--json", json.to_str().unwrap()], &[]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    let v: Value = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
    assert_eq!(v["runs"], 2000);
    assert!(v["raw_bytes"].as_u64().unwrap() > 2000 * 1000, "{v}");
    let r = v["r"].as_f64().unwrap();
    assert!((1.0..=3.0).contains(&r), "{r}");
    let rows = v["rows"].as_array().unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r["query"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "trace tree",
            "run + payload",
            "filtered list",
            "text search: rare phrase",
            "text search: common word",
            "text search: search + filter",
            "grouped",
            "first estimate",
            "suggestions"
        ]
    );
    for row in rows {
        if row["query"].as_str().unwrap().starts_with("text search") {
            assert_eq!(row["target_ms"], 500.0, "{row}");
            assert_eq!(row["n"], 5, "{row}");
        }
        for k in ["p50_ms", "p99_ms", "target_ms", "budget_ms"] {
            assert!(row[k].as_f64().is_some(), "{k} in {row}");
        }
        assert!(row["pass"].is_boolean(), "{row}");
        let (t, b) = (row["target_ms"].as_f64().unwrap(), row["budget_ms"].as_f64().unwrap());
        assert!((b - t * r).abs() < 1e-6, "{row}");
    }
    assert!(v["disk"]["pct_of_raw"].as_f64().is_some(), "{v}");
    // The server shares the bench process, so its own footprint can't be told apart: `null`, the process's RSS
    // (an upper bound, `null` where `ps` isn't allowed) beside it; SC-05 is waived for M3, so the row never fails.
    assert!(v["idle_mb"].is_null(), "{v}");
    assert_eq!(v["idle"]["waived"], "M4", "{v}");
    assert!(v["idle"]["pass"].is_null(), "{v}");
    let strict = v["load"]["pass"] == true && v["disk"]["pass"] == true && rows.iter().all(|r| r["pass"] == true);
    assert_eq!(v["pass"], strict, "{v}");
    assert!(v["load"]["events_per_s"].as_f64().unwrap() > 0.0, "{v}");
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("calibration r = "), "{out}");
    assert!(out.contains("trace tree") && out.contains("suggestions"), "{out}");
    let idle = out
        .lines()
        .find(|l| l.contains("idle memory"))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(idle.contains("WAIVED (M4)"), "{idle}");
    let target_col = |name: &str| {
        let l = out
            .lines()
            .find(|l| l.trim_start().starts_with(name))
            .unwrap_or_else(|| panic!("{out}"));
        l[..l.find(" target ").unwrap_or_else(|| panic!("{l}"))].chars().count()
    };
    assert_eq!(target_col("load"), target_col("disk"), "{out}");
    assert_eq!(target_col("load"), target_col("idle memory"), "{out}");
    assert!(!out.contains("FAIL idle memory"), "{out}");
}

#[test]
fn check_fails_on_a_slipped_target_naming_query_target_and_value() {
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("bench.json");
    let o = bench(
        dir.path(),
        &["--check", "--json", json.to_str().unwrap()],
        &[("TRACEDB_BENCH_SLEEP_MS", "200")],
    );
    let all = text(&o);
    assert_eq!(o.status.code(), Some(1), "{all}");
    let line = all
        .lines()
        .find(|l| l.starts_with("FAIL trace tree"))
        .unwrap_or_else(|| panic!("no failing trace tree line: {all}"));
    assert!(line.contains("target 50 ms"), "{line}");
    let p50: f64 = line
        .split("p50 ")
        .nth(1)
        .and_then(|t| t.split(' ').next())
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("no measured p50 in {line}"));
    assert!(p50 >= 200.0, "{line}");
    assert!(!all.contains("FAIL idle memory"), "{all}");
    let v: Value = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
    assert_eq!(v["rows"][0]["pass"], false, "{v}");
    assert!(v["rows"][0]["p50_ms"].as_f64().unwrap() >= 200.0, "{v}");
}

#[test]
fn reused_data_skips_the_load() {
    let dir = tempfile::tempdir().unwrap();
    let first = bench(dir.path(), &[], &[]);
    assert_eq!(first.status.code(), Some(0), "{}", text(&first));
    let again = bench(dir.path(), &[], &[]);
    assert_eq!(again.status.code(), Some(0), "{}", text(&again));
    assert!(
        String::from_utf8_lossy(&again.stdout).contains("reused"),
        "{}",
        text(&again)
    );
}

#[test]
fn setup_error_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    // A folder that has data but no bench marker isn't reused or loaded over.
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    std::fs::write(dir.path().join("data/other"), "x").unwrap();
    let o = bench(dir.path(), &["--check"], &[]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    assert!(text(&o).contains("tracedb bench:"), "{}", text(&o));
}

#[test]
fn a_p99_over_5x_its_p50_fails_whatever_the_budget() {
    assert!(bench::verdict(10.0, 50.0, 1000.0).is_ok());
    let e = bench::verdict(10.0, 51.0, 1000.0).unwrap_err();
    assert!(e.contains("p99 51.0 ms > 5× p50 10.0 ms"), "{e}");
    let e = bench::verdict(60.0, 61.0, 50.0).unwrap_err();
    assert!(e.contains("p50 60.0 ms > budget 50 ms"), "{e}");
}

#[test]
fn calibration_ratio_is_clamped_to_1_and_3() {
    assert_eq!(bench::ratio(50.0, 100.0), 1.0);
    assert_eq!(bench::ratio(150.0, 100.0), 1.5);
    assert_eq!(bench::ratio(900.0, 100.0), 3.0);
}

#[test]
fn reference_workload_decodes_the_shipped_file() {
    let rows = bench::reference_workload().unwrap();
    assert!(rows >= 20 * 10_000, "{rows}");
}

/// Writes `tests/fixtures/bench-reference.parquet` from the bench generator; run by hand with `--ignored` when
/// the fixture changes, then measure `REFERENCE_MS` on the reference laptop (`tracedb bench --calibrate-only`).
#[test]
#[ignore]
fn write_reference_fixture() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/bench-reference.parquet");
    bench::write_reference(&path).unwrap();
    eprintln!(
        "wrote {} ({} bytes)",
        path.display(),
        std::fs::metadata(&path).unwrap().len()
    );
}
