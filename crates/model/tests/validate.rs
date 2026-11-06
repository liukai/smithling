mod common;

use common::*;
use serde_json::json;
use tracedb_model::validate::{parse_ts, parse_uuid};
use tracedb_model::{Op, Reason, RunFields, Ts, validate};

fn reason(op: Op, mut f: RunFields) -> Reason {
    validate(op, &mut f).unwrap_err().reason
}

fn ok_post() -> RunFields {
    post_fields(child_ids(), "fetch_url")
}

#[test]
fn valid_post_and_patch_pass() {
    assert!(validate(Op::Post, &mut ok_post()).is_ok());
    assert!(validate(Op::Patch, &mut child_ids()).is_ok());
    assert!(validate(Op::Post, &mut post_fields(root_ids(), "root")).is_ok());
}

#[test]
fn missing_required_fields() {
    for clear in ["id", "trace_id", "dotted_order", "name", "run_type", "start_time"] {
        let mut f = ok_post();
        match clear {
            "id" => f.id = None,
            "trace_id" => f.trace_id = None,
            "dotted_order" => f.dotted_order = None,
            "name" => f.name = None,
            "run_type" => f.run_type = None,
            _ => f.start_time = None,
        }
        assert_eq!(reason(Op::Post, f), Reason::MissingField, "post without {clear}");
    }
    let mut empty_name = ok_post();
    empty_name.name = Some(String::new());
    assert_eq!(reason(Op::Post, empty_name), Reason::MissingField);

    for clear in ["id", "trace_id", "dotted_order"] {
        let mut f = child_ids();
        match clear {
            "id" => f.id = None,
            "trace_id" => f.trace_id = None,
            _ => f.dotted_order = None,
        }
        assert_eq!(reason(Op::Patch, f), Reason::MissingField, "patch without {clear}");
    }
    let mut patch_without_name = child_ids();
    patch_without_name.name = None;
    assert!(
        validate(Op::Patch, &mut patch_without_name).is_ok(),
        "patches need only identity fields"
    );
}

#[test]
fn dotted_order_checks() {
    let bad = |d: String| {
        let mut f = ok_post();
        f.dotted_order = Some(d);
        reason(Op::Post, f)
    };
    assert_eq!(bad("nonsense".into()), Reason::InvalidDottedOrder);
    assert_eq!(
        bad(format!("2026-11-05T02Z{TRACE}.20261105T021407123456Z{RUN}")),
        Reason::InvalidDottedOrder
    );
    assert_eq!(
        bad(format!(
            "20261105T021400000000Z{TRACE}.20261105T021407123456Znot-a-uuid-at-all-but-36-chars-long"
        )),
        Reason::InvalidDottedOrder
    );
    assert_eq!(
        bad(format!("20261105T021400000000Z{RUN}.20261105T021407123456Z{RUN}")),
        Reason::InvalidDottedOrder,
        "first != trace_id"
    );
    assert_eq!(
        bad(format!("20261105T021400000000Z{TRACE}.20261105T021407123456Z{TRACE}")),
        Reason::InvalidDottedOrder,
        "last != id"
    );

    let mut wrong_parent = ok_post();
    wrong_parent.parent_run_id = Some(run_id());
    assert_eq!(
        reason(Op::Post, wrong_parent),
        Reason::InvalidDottedOrder,
        "second-to-last != parent_run_id"
    );

    let mut root_with_parent = post_fields(root_ids(), "root");
    root_with_parent.parent_run_id = Some(run_id());
    assert_eq!(reason(Op::Post, root_with_parent), Reason::InvalidDottedOrder);
}

#[test]
fn normalization() {
    let mut f = ok_post();
    f.parent_run_id = None;
    f.project = Some(String::new());
    let order = validate(Op::Post, &mut f).unwrap();
    assert_eq!(f.parent_run_id, Some(trace_id()), "parent filled in from dotted_order");
    assert_eq!(f.project, None, "empty project counts as absent");
    assert_eq!(order.depth(), 1);
}

#[test]
fn size_limits() {
    let big = |n: usize| "x".repeat(n);
    let check = |f: RunFields, ok: bool, what: &str| {
        let mut f = f;
        let r = validate(Op::Post, &mut f);
        if ok {
            assert!(r.is_ok(), "{what} should pass: {r:?}");
        } else {
            assert_eq!(r.unwrap_err().reason, Reason::TooLarge, "{what}");
        }
    };

    let with = |edit: &dyn Fn(&mut RunFields)| {
        let mut f = ok_post();
        edit(&mut f);
        f
    };
    check(with(&|f| f.name = Some(big(512))), true, "name 512 chars");
    check(with(&|f| f.name = Some(big(513))), false, "name 513 chars");
    check(with(&|f| f.project = Some(big(128))), true, "project 128 chars");
    check(with(&|f| f.project = Some(big(129))), false, "project 129 chars");
    check(with(&|f| f.address = Some(big(512))), true, "address 512 chars");
    check(with(&|f| f.address = Some(big(513))), false, "address 513 chars");
    check(with(&|f| f.tags = Some(vec!["t".into(); 100])), true, "100 tags");
    check(with(&|f| f.tags = Some(vec!["t".into(); 101])), false, "101 tags");
    check(with(&|f| f.tags = Some(vec![big(256)])), true, "tag 256 chars");
    check(with(&|f| f.tags = Some(vec![big(257)])), false, "tag 257 chars");
    check(with(&|f| f.error = Some(big(1024 * 1024))), true, "error 1 MB");
    check(
        with(&|f| f.error = Some(big(1024 * 1024 + 1))),
        false,
        "error over 1 MB",
    );
    check(
        with(&|f| f.metadata = Some(json!({"k": big(1024 * 1024)}).as_object().unwrap().clone())),
        false,
        "metadata over 1 MB",
    );
    let mb20 = 20 * 1024 * 1024;
    check(
        with(&|f| f.inputs = Some(json!(big(mb20 - 2)))),
        true,
        "inputs exactly 20 MB",
    );
    check(with(&|f| f.inputs = Some(json!(big(mb20)))), false, "inputs over 20 MB");
    check(
        with(&|f| f.outputs = Some(json!(big(mb20)))),
        false,
        "outputs over 20 MB",
    );
    check(
        with(&|f| f.events = Some(vec![json!(big(mb20))])),
        false,
        "events over 20 MB",
    );
}

#[test]
fn uuid_and_timestamp_parsing() {
    assert!(parse_uuid("id", &json!(RUN)).is_ok());
    assert_eq!(
        parse_uuid("id", &json!("nope")).unwrap_err().reason,
        Reason::InvalidUuid
    );
    assert_eq!(parse_uuid("id", &json!(42)).unwrap_err().reason, Reason::InvalidUuid);

    let want = Ts::parse("2026-11-05T02:14:07.123456Z").unwrap();
    for s in [
        "2026-11-05T02:14:07.123456Z",
        "2026-11-05T02:14:07.123456+00:00",
        "2026-11-05T02:14:07.123456",
        "2026-11-05T04:14:07.123456+02:00",
    ] {
        assert_eq!(parse_ts("start_time", &json!(s)).unwrap(), want, "{s}");
    }
    assert!(parse_ts("t", &json!("2026-11-05T02:14:07")).is_ok(), "no fraction");
    assert_eq!(
        parse_ts("t", &json!("yesterday")).unwrap_err().reason,
        Reason::InvalidTimestamp
    );
    assert_eq!(want.to_string(), "2026-11-05T02:14:07.123456Z");
}
