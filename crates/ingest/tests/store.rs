mod common;

use common::*;
use smithling_ingest::store::{Include, RunStore};
use smithling_model::Status;

fn store_with_roots(n: u64) -> RunStore {
    let mut s = RunStore::new();
    for i in 0..n {
        let start = format!("2026-11-05T02:{:02}:00Z", i % 60);
        s.apply(&event(i + 1, &post(root(uuid(i as u128 + 1), &start, "r"))))
            .unwrap();
    }
    s
}

#[test]
fn roots_page_newest_first_without_repeats() {
    let s = store_with_roots(25);
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let (page, next) = s.roots(Some("p"), None, after, 10);
        seen.extend(page.iter().map(|h| (h.start_time.unwrap(), h.run_id)));
        match next {
            Some(k) => after = Some(k),
            None => break,
        }
    }
    assert_eq!(seen.len(), 25);
    let mut sorted = seen.clone();
    sorted.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    assert_eq!(seen, sorted, "newest first, ties by run id");
    sorted.dedup();
    assert_eq!(sorted.len(), 25, "no repeats");
}

#[test]
fn new_roots_do_not_disturb_older_pages() {
    let mut s = store_with_roots(10);
    let (first, next) = s.roots(None, None, None, 5);
    s.apply(&event(100, &post(root(uuid(999), "2026-11-05T03:00:00Z", "newest"))))
        .unwrap();
    let (second, _) = s.roots(None, None, next, 5);
    let ids: Vec<_> = first.iter().chain(&second).map(|h| h.run_id).collect();
    assert_eq!(ids.len(), 10);
    assert!(
        !ids.contains(&uuid(999)),
        "a newer run appears on a fresh first page, not mid-paging"
    );
}

#[test]
fn status_and_project_filters() {
    let mut s = store_with_roots(3);
    let mut done = root(uuid(1), "2026-11-05T02:00:00Z", "r");
    done.error = Some("boom".into());
    s.apply(&event(10, &patch(done))).unwrap();
    let (errs, _) = s.roots(Some("p"), Some(Status::Error), None, 10);
    assert_eq!(errs.iter().map(|h| h.run_id).collect::<Vec<_>>(), vec![uuid(1)]);
    assert!(s.roots(Some("other"), None, None, 10).0.is_empty());
    assert_eq!(s.projects(), vec![("p".to_string(), 3)]);
}

#[test]
fn trace_view_is_ordered_and_counts_children() {
    let mut s = RunStore::new();
    let t = uuid(1);
    s.apply(&event(1, &post(root(t, "2026-11-05T02:00:00Z", "root"))))
        .unwrap();
    s.apply(&event(
        2,
        &post(child(t, "2026-11-05T02:00:00Z", uuid(3), "2026-11-05T02:00:05Z")),
    ))
    .unwrap();
    s.apply(&event(
        3,
        &post(child(t, "2026-11-05T02:00:00Z", uuid(2), "2026-11-05T02:00:01Z")),
    ))
    .unwrap();
    let tv = s.trace(t).unwrap();
    assert_eq!(tv.run_count, 3);
    assert_eq!(
        tv.runs.iter().map(|h| h.run_id).collect::<Vec<_>>(),
        vec![t, uuid(2), uuid(3)],
        "siblings by start time"
    );
    assert_eq!(tv.root.unwrap().child_count, 2);
    assert!(tv.has_running);
}

#[test]
fn patch_first_root_is_hidden_until_its_start_arrives() {
    let mut s = RunStore::new();
    let mut early = root(uuid(1), "2026-11-05T02:00:00Z", "r");
    early.name = None;
    early.start_time = None;
    early.end_time = Some(smithling_model::Ts::parse("2026-11-05T02:10:00Z").unwrap());
    s.apply(&event(1, &patch(early))).unwrap();
    assert!(s.roots(None, None, None, 10).0.is_empty());
    s.apply(&event(2, &post(root(uuid(1), "2026-11-05T02:00:00Z", "r"))))
        .unwrap();
    let (page, _) = s.roots(None, None, None, 10);
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].status, Status::Ok);
}

#[test]
fn payloads_only_when_asked_and_bytes_tracked() {
    let mut s = RunStore::new();
    let mut f = root(uuid(1), "2026-11-05T02:00:00Z", "r");
    f.inputs = Some(serde_json::json!({"big": "x".repeat(1000)}));
    s.apply(&event(1, &post(f))).unwrap();
    assert!(s.run(uuid(1), Include::default()).unwrap().inputs.is_none());
    assert!(
        s.run(
            uuid(1),
            Include {
                inputs: true,
                ..Default::default()
            }
        )
        .unwrap()
        .inputs
        .is_some()
    );
    assert!(s.bytes() > 1000);
}
