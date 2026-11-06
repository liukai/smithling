mod common;

use std::fs;

use common::*;
use proptest::prelude::*;
use tracedb_ingest::wal::{DEFAULT_ROLL_BYTES, Wal, WalError};
use tracedb_model::RunEvent;

fn replay(dir: &std::path::Path) -> Vec<RunEvent> {
    let mut out = Vec::new();
    Wal::open(dir, DEFAULT_ROLL_BYTES, |ev| out.push(ev)).unwrap();
    out
}

fn sample(n: u64, payload: usize) -> RunEvent {
    let mut f = root(uuid(n as u128 + 1), "2026-11-05T02:14:07Z", "r");
    f.inputs = Some(serde_json::json!("x".repeat(payload)));
    event(n, &post(f))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn replay_returns_exactly_the_records_synced_before_a_cut(
        sizes in proptest::collection::vec(0usize..2000, 1..30),
        cut_frac in 0.0f64..=1.0,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let wal_dir = dir.path().join("wal");
        let (mut wal, _) = Wal::open(&wal_dir, DEFAULT_ROLL_BYTES, |_| {}).unwrap();
        let events: Vec<RunEvent> = sizes.iter().enumerate().map(|(i, s)| sample(i as u64 + 1, *s)).collect();
        let file = fs::read_dir(&wal_dir).unwrap().next().unwrap().unwrap().path();
        let mut ends = Vec::new();
        for ev in &events {
            wal.append(ev).unwrap();
            wal.sync().unwrap();
            ends.push(fs::metadata(&file).unwrap().len());
        }
        drop(wal);

        let total = *ends.last().unwrap();
        let cut = 8 + ((total - 8) as f64 * cut_frac) as u64;
        fs::OpenOptions::new().write(true).open(&file).unwrap().set_len(cut).unwrap();

        let expected: Vec<&RunEvent> = events.iter().zip(&ends).filter(|(_, end)| **end <= cut).map(|(e, _)| e).collect();
        let got = replay(&wal_dir);
        prop_assert_eq!(got.iter().collect::<Vec<_>>(), expected);
        prop_assert_eq!(fs::metadata(&file).unwrap().len(), ends.iter().copied().filter(|e| *e <= cut).max().unwrap_or(8));
    }
}

#[test]
fn appends_continue_after_a_torn_tail_is_cut() {
    let dir = tempfile::tempdir().unwrap();
    let wal_dir = dir.path().join("wal");
    let (mut wal, _) = Wal::open(&wal_dir, DEFAULT_ROLL_BYTES, |_| {}).unwrap();
    wal.append(&sample(1, 10)).unwrap();
    wal.append(&sample(2, 10)).unwrap();
    wal.sync().unwrap();
    drop(wal);
    let file = fs::read_dir(&wal_dir).unwrap().next().unwrap().unwrap().path();
    let len = fs::metadata(&file).unwrap().len();
    fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .unwrap()
        .set_len(len - 3)
        .unwrap();

    let mut seen = Vec::new();
    let (mut wal, stats) = Wal::open(&wal_dir, DEFAULT_ROLL_BYTES, |ev| seen.push(ev.event_seq)).unwrap();
    assert_eq!(seen, vec![1]);
    assert!(stats.truncated_bytes > 0);
    wal.append(&sample(3, 10)).unwrap();
    wal.sync().unwrap();
    drop(wal);
    assert_eq!(
        replay(&wal_dir).iter().map(|e| e.event_seq).collect::<Vec<_>>(),
        vec![1, 3]
    );
}

#[test]
fn rolls_files_and_replays_across_them_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let wal_dir = dir.path().join("wal");
    let (mut wal, _) = Wal::open(&wal_dir, 4096, |_| {}).unwrap();
    for i in 1..=20 {
        wal.append(&sample(i, 1000)).unwrap();
    }
    wal.sync().unwrap();
    drop(wal);
    assert!(
        fs::read_dir(&wal_dir).unwrap().count() > 3,
        "small roll size makes several files"
    );
    let seqs: Vec<u64> = replay(&wal_dir).iter().map(|e| e.event_seq).collect();
    assert_eq!(seqs, (1..=20).collect::<Vec<_>>());
}

#[test]
fn corruption_in_an_earlier_file_is_an_error_not_a_guess() {
    let dir = tempfile::tempdir().unwrap();
    let wal_dir = dir.path().join("wal");
    let (mut wal, _) = Wal::open(&wal_dir, 4096, |_| {}).unwrap();
    for i in 1..=20 {
        wal.append(&sample(i, 1000)).unwrap();
    }
    wal.sync().unwrap();
    drop(wal);
    let mut files: Vec<_> = fs::read_dir(&wal_dir).unwrap().map(|e| e.unwrap().path()).collect();
    files.sort();
    let mut bytes = fs::read(&files[0]).unwrap();
    bytes[20] ^= 0xff;
    fs::write(&files[0], bytes).unwrap();
    let err = Wal::open(&wal_dir, 4096, |_| {}).err().expect("must refuse");
    assert!(matches!(err, WalError::Corrupt { .. }), "{err}");
}
