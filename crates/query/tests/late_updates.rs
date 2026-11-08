//! Late updates without a project (as the SDK sends them) are filed under their run's project, so they don't
//! pull work into every other project's queries; results stay those of applying every event in order.

mod common;

use std::collections::{BTreeSet, HashMap};

use common::*;
use tracedb_ingest::{Ingest, IngestConfig, Limits};
use tracedb_model::filter::parse_filter;
use tracedb_model::{Op, RunFields, RunId};
use tracedb_query::{Query, QuerySpec};

const MEMTABLE: u64 = 256 * 1024;
/// Every 5th ends late: more late runs than one `IN (…)` list holds.
const BIG: u64 = 12_000;
const SMALL: u64 = 30;

fn reopen(dir: &std::path::Path) -> Ingest {
    let mut cfg = IngestConfig::new(dir);
    cfg.limits = Limits {
        memtable_bytes: MEMTABLE,
        dedup_window_us: 0,
        ..Limits::default()
    };
    Ingest::open(cfg).unwrap().0
}

/// Rows in files filed under no project.
fn unknown_rows(ingest: &Ingest) -> u64 {
    let mem = ingest.mem();
    let mem = mem.read().unwrap();
    mem.version
        .segments
        .iter()
        .filter(|s| s.project.is_empty())
        .map(|s| s.row_count)
        .sum()
}

/// Fills (and so flushes) the memtable with runs of a project outside the queried time range.
async fn flush_with_filler(ingest: &Ingest, from: u64) {
    let filler = (0..600)
        .map(|i| (Op::Post, ended(&root(from + i, "filler", 80_000 + i), None)))
        .collect();
    send(ingest, filler).await;
    settle(ingest, MEMTABLE).await;
}

fn spec(project: &str, filter: &str, is_root: Option<bool>) -> QuerySpec {
    QuerySpec {
        project: Some(project.into()),
        filter: Some(parse_filter(filter).unwrap()),
        is_root,
        limit: Some(200),
        count: true,
        ..QuerySpec::default()
    }
}

/// Every page's run ids, and the first page.
async fn ids(q: &Query, s: &QuerySpec) -> (BTreeSet<RunId>, tracedb_query::Page) {
    let first = q.query(s).await.unwrap();
    let mut out: BTreeSet<RunId> = first.runs.iter().map(|r| r.header.run_id).collect();
    let mut cursor = first.next_cursor.clone();
    while let Some(c) = cursor {
        let page = q
            .query(&QuerySpec {
                cursor: Some(c),
                ..s.clone()
            })
            .await
            .unwrap();
        out.extend(page.runs.iter().map(|r| r.header.run_id));
        cursor = page.next_cursor;
    }
    (out, first)
}

fn every(runs: &[RunFields], keep: impl Fn(usize) -> bool) -> BTreeSet<RunId> {
    (0..runs.len())
        .filter(|&i| keep(i))
        .map(|i| runs[i].id.unwrap())
        .collect()
}

async fn check(q: &Query, errors: &HashMap<&str, BTreeSet<RunId>>, pass: &str) -> tracedb_query::Page {
    let (got, page) = ids(q, &spec("small", r#"eq(status, "error")"#, Some(true))).await;
    assert_eq!(got, errors["small"], "{pass}: small failing roots");
    let (got, big) = ids(q, &spec("big", r#"eq(status, "error")"#, None)).await;
    assert_eq!(got, errors["big"], "{pass}: big errors");
    // Merged from the kept file rows of multi-version runs (D-023), or by reading their versions again.
    assert!(
        big.stats.merge_rows >= 2 * (BIG as usize / 5) || big.stats.rows_merged >= BIG as usize / 5,
        "{pass}: {:?}",
        big.stats
    );
    let (got, _) = ids(q, &spec("big", r#"eq(status, "running")"#, None)).await;
    assert!(got.is_empty(), "{pass}: every late update merged");
    page
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_updates_stay_in_their_project() {
    let (dir, ingest) = open(MEMTABLE);
    // Two projects over the same hours: every 5th "big" run ends late, without a project; "small" has 5 roots
    // failing at once, 3 failing late, and one whose (failing) end arrives before its start.
    let big: Vec<RunFields> = (0..BIG).map(|i| root(1 + i, "big", i)).collect();
    let small: Vec<RunFields> = (0..SMALL).map(|i| root(20_001 + i, "small", i * 300 + 1)).collect();
    send(&ingest, vec![(Op::Patch, end_update(&small[8], Some("early")))]).await;
    flush_with_filler(&ingest, 40_000).await;
    assert_eq!(
        unknown_rows(&ingest),
        1,
        "an update before its start has no project yet"
    );

    let mut posts = Vec::new();
    for (i, f) in big.iter().enumerate() {
        posts.push((Op::Post, if i % 5 == 0 { f.clone() } else { ended(f, None) }));
    }
    for (i, f) in small.iter().enumerate() {
        posts.push((
            Op::Post,
            match i {
                0..5 => ended(f, Some("boom")),
                5..9 => f.clone(),
                _ => ended(f, None),
            },
        ));
    }
    send(&ingest, posts).await;
    flush_with_filler(&ingest, 50_000).await;

    let late_big: Vec<(Op, RunFields)> = big
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 5 == 0)
        .map(|(i, f)| (Op::Patch, end_update(f, (i % 10 == 0).then_some("late boom"))))
        .collect();
    let late_small: Vec<(Op, RunFields)> = small[5..8]
        .iter()
        .map(|f| (Op::Patch, end_update(f, Some("late"))))
        .collect();
    let mut late: Vec<(Op, RunFields)> = late_small.into_iter().chain(late_big).collect();
    let tail = late.split_off(late.len() - 150);
    send(&ingest, late).await;
    settle(&ingest, MEMTABLE).await;
    send(&ingest, tail).await;
    // Some updates are still in memory: they're pinned by the same rule as files.
    assert!(ingest.health().runs > 50, "{} runs in memory", ingest.health().runs);
    let (got, page) = ids(
        &Query::new(ingest.clone()),
        &spec("small", r#"eq(status, "error")"#, Some(true)),
    )
    .await;
    assert_eq!(got, every(&small, |i| i < 9), "memtable: small failing roots");
    assert!(page.stats.merge_rows <= 8, "memtable: {:?}", page.stats);
    flush_with_filler(&ingest, 60_000).await;
    assert_eq!(unknown_rows(&ingest), 1, "every late update found its project");

    let mut errors = HashMap::from([
        ("small", every(&small, |i| i < 9)),
        ("big", every(&big, |i| i % 10 == 0)),
    ]);
    let q = Query::new(ingest.clone());
    let page = check(&q, &errors, "files").await;
    // "small" holds 31 versions in files; nothing of "big" may be read or merged for it.
    assert!(page.stats.rows_read <= 40, "read {:?}", page.stats);
    assert!(page.stats.merge_rows <= 8, "{:?}", page.stats);
    assert!(
        page.stats.files_version_lookup > 0 || page.stats.rows_merged > 0,
        "{:?}",
        page.stats
    );

    // After a restart the map is rebuilt from the newest files (in the background).
    drop(q);
    drop(ingest);
    let ingest = reopen(dir.path());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while ingest.projects().read().unwrap().get(&big[1].id.unwrap()).is_none() {
        assert!(std::time::Instant::now() < deadline, "projects not learned from files");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let again: Vec<(Op, RunFields)> = big
        .iter()
        .enumerate()
        .filter(|(i, _)| i % 50 == 1)
        .map(|(_, f)| (Op::Patch, end_update(f, Some("after restart"))))
        .collect();
    send(&ingest, again).await;
    flush_with_filler(&ingest, 70_000).await;
    assert_eq!(unknown_rows(&ingest), 1);
    errors.get_mut("big").unwrap().extend(every(&big, |i| i % 50 == 1));
    let q = Query::new(ingest.clone());
    let page = check(&q, &errors, "restart").await;
    assert!(page.stats.rows_read <= 40, "read {:?}", page.stats);
}
