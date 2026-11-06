//! `tracedb seed` shapes (spec Story 4) and determinism; then a real send through the HTTP path.

mod common;

use tracedb::seed::{PROJECT, generate, send};

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
    let ids = |t: &[tracedb::seed::GenTrace]| t.iter().map(|x| x.trace_id).collect::<Vec<_>>();
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
    assert!(matches!(err, tracedb::seed::SeedError::Unreachable(..)));
}
