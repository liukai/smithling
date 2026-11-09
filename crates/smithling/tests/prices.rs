//! `serve --prices` and `<data>/prices.toml` (design §8): read at start; an invalid file stops start naming the line.

use std::net::SocketAddr;

use smithling_ingest::Limits;
use smithling_server::{ServeConfig, open};

fn cfg(dir: &std::path::Path, prices: Option<std::path::PathBuf>) -> ServeConfig {
    ServeConfig {
        data_dir: dir.into(),
        addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        storage: None,
        metastore: None,
        limits: Limits::default(),
        ui_dir: None,
        prices,
        compact: Default::default(),
    }
}

const GPT_4O_AT_5: &str = "version = 1\n[models.\"gpt-4o\"]\ninput = 5.0\noutput = 15.0\n";

#[test]
fn embedded_prices_without_a_user_file() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _) = open(&cfg(dir.path(), None)).unwrap();
    assert_eq!(state.prices.get("gpt-4o").unwrap().input_pico, 2_500_000);
}

#[test]
fn data_folder_prices_replace_entries() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("prices.toml"), GPT_4O_AT_5).unwrap();
    let (state, _) = open(&cfg(dir.path(), None)).unwrap();
    assert_eq!(state.prices.get("gpt-4o").unwrap().input_pico, 5_000_000);
    assert_eq!(state.prices.get("gpt-4o-mini").unwrap().input_pico, 150_000);
}

#[test]
fn prices_flag_wins_over_the_data_folder() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("prices.toml"), "not toml [").unwrap();
    let other = dir.path().join("mine.toml");
    std::fs::write(&other, GPT_4O_AT_5).unwrap();
    let (state, _) = open(&cfg(dir.path(), Some(other))).unwrap();
    assert_eq!(state.prices.get("gpt-4o").unwrap().input_pico, 5_000_000);
    let missing = open(&cfg(dir.path(), Some(dir.path().join("nope.toml"))));
    assert!(missing.is_err(), "a named --prices file must exist");
}

#[test]
fn serve_with_an_invalid_price_list_fails_naming_the_line() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "version = 1\n[models.\"x\"]\ninput = \"cheap\"\noutput = 1\n").unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_smithling"))
        .args(["serve", "--port", "0", "--data"])
        .arg(dir.path().join("data"))
        .arg("--prices")
        .arg(&bad)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            panic!("serve kept running with an invalid price list");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("bad.toml line 3"), "{err}");
}
