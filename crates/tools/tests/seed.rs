//! `tracedb seed` shapes (spec Story 4) and determinism; then a real send through the HTTP path.

use tracedb_server::testkit as common;

use tracedb_tools::seed::{PROJECT, generate, send};

#[test]
fn shapes_match_the_spec() {
    let traces = generate(42, 100, 1_762_308_000_000_000);
    assert_eq!(traces.len(), 100);
    let mut steps = 0usize;
    let (mut errors, mut late) = (0usize, 0usize);
    for t in &traces {
        assert!((20..=300).contains(&t.steps.len()), "steps {}", t.steps.len());
        let depth = t.steps.iter().map(|s| s.depth).max().unwrap();
        assert!((3..=8).contains(&(depth + 1)), "depth {}", depth + 1);
        for s in &t.steps {
            assert!((1024..=50 * 1024).contains(&s.payload_bytes));
            errors += s.error as usize;
            if let Some((d, _)) = &s.late_patch {
                assert!((30..=120).contains(&d.as_secs()));
                late += 1;
            }
        }
        steps += t.steps.len();
    }
    let (e, l) = (errors as f64 / steps as f64, late as f64 / steps as f64);
    assert!((0.03..0.07).contains(&e), "error rate {e}");
    assert!((0.08..0.12).contains(&l), "late rate {l}");
}

#[test]
fn same_seed_same_traces() {
    let a = generate(42, 5, 0);
    let b = generate(42, 5, 0);
    let c = generate(43, 5, 0);
    let ids = |t: &[tracedb_tools::seed::GenTrace]| t.iter().map(|x| x.trace_id).collect::<Vec<_>>();
    assert_eq!(ids(&a), ids(&b));
    assert_eq!(
        a[0].steps.iter().map(|s| s.post.clone()).collect::<Vec<_>>(),
        b[0].steps.iter().map(|s| s.post.clone()).collect::<Vec<_>>()
    );
    assert_ne!(ids(&a), ids(&c));
}

#[tokio::test]
async fn sends_through_the_agent_path_and_every_item_is_accepted() {
    let server = common::start().await;
    let traces = generate(7, 10, tracedb_model::Ts::now().0);
    let (posts, patches) = send(&format!("http://{}", server.addr), &traces, true).await.unwrap();
    let steps: usize = traces.iter().map(|t| t.steps.len()).sum();
    assert_eq!(posts, steps);
    assert_eq!(server.state.ingest.health().runs, steps);
    let snap = server.state.ingest.snapshot();
    let (roots, _) = snap.roots(Some(PROJECT), None, None, 100);
    assert_eq!(roots.len(), 10);
    let late_done = traces
        .iter()
        .flat_map(|t| &t.steps)
        .filter(|s| s.late_patch.is_some())
        .count();
    assert_eq!(patches, late_done);
    for t in &traces {
        let tv = snap.trace(t.trace_id).unwrap();
        assert_eq!(tv.run_count, t.steps.len());
        assert!(!tv.has_running, "with --no-delay every late step has finished");
    }
}

#[tokio::test]
async fn unreachable_server_is_reported() {
    let err = send("http://127.0.0.1:9", &generate(1, 1, 0), true).await.unwrap_err();
    assert!(matches!(err, tracedb_tools::seed::SeedError::Unreachable(..)));
}

fn times(traces: &[tracedb_tools::seed::GenTrace]) -> Vec<i64> {
    let at = |v: &serde_json::Value| v.as_str().map(|s| tracedb_model::Ts::parse(s).unwrap().0);
    traces
        .iter()
        .flat_map(|t| &t.steps)
        .flat_map(|s| {
            [
                at(&s.post["start_time"]),
                at(&s.post["end_time"]),
                s.late_patch.as_ref().and_then(|(_, p)| at(&p["end_time"])),
            ]
        })
        .flatten()
        .collect()
}

#[test]
fn seeded_runs_never_start_or_end_in_the_future() {
    let now = tracedb_model::Ts::now().0;
    let traces = tracedb_tools::seed::generate_before(42, 50, now);
    let t = times(&traces);
    assert!(
        t.iter().all(|&x| x <= now),
        "max {} > now {now}",
        t.iter().max().unwrap()
    );
    // Recent: the last trace ends near now (within its 7 s slot plus the longest duration).
    assert!(t.iter().max().unwrap() > &(now - 50 * 7_000_000 - 3_600_000_000));
    // Deterministic for a given seed and time; ids differ when the time does.
    let again = tracedb_tools::seed::generate_before(42, 50, now);
    assert_eq!(traces[3].steps[5].post, again[3].steps[5].post);
    let later = tracedb_tools::seed::generate_before(42, 50, now + 1);
    assert_ne!(traces[0].trace_id, later[0].trace_id);
}

#[tokio::test]
async fn seeding_the_same_seed_twice_into_one_folder_is_accepted() {
    let server = common::start().await;
    let url = format!("http://{}", server.addr);
    let first = tracedb_tools::seed::generate_before(7, 3, tracedb_model::Ts::now().0);
    send(&url, &first, true).await.unwrap();
    let second = tracedb_tools::seed::generate_before(7, 3, tracedb_model::Ts::now().0);
    send(&url, &second, true).await.expect("no identity_conflict");
    let steps: usize = first.iter().chain(&second).map(|t| t.steps.len()).sum();
    assert_eq!(server.state.ingest.health().runs, steps);
}
