//! The 2M-run bench (`tracedb bench`, SC-03 – SC-06) as a test. It runs the binary, so the allocator and runtime
//! are the shipped ones. Slow, so `#[ignore]`; run with `--release -- --ignored --nocapture`.

use std::process::Command;

#[test]
#[ignore]
fn bench_at_2m_runs_every_row_passes() {
    let status = Command::new(env!("CARGO_BIN_EXE_tracedb"))
        .args(["bench", "--runs", "2000000", "--check"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0), "a row missed its budget (table above)");
}
