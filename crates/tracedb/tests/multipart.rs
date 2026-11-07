//! `POST /runs/multipart`: the SDK's multipart upload (plain or zstd) stores runs exactly as `/runs/batch` does,
//! and bad parts are rejected by name (contracts/http-api.md).

mod common;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use common::{Server, uid};
use proptest::prelude::*;
use serde_json::{Map, Value, json};
use tracedb_ingest::Include;
use tracedb_model::RunView;

const FIELDS: [&str; 6] = ["inputs", "outputs", "events", "extra", "error", "serialized"];
const BOUNDARY: &str = "4a7c1e0b9f11d2e3a4b5c6d7e8f90a1b";

struct Part {
    name: String,
    content_type: String,
    data: Vec<u8>,
    /// Inline parts carry `Content-Length`; streamed ones (big attachments) don't.
    with_length: bool,
}

fn json_part(name: impl Into<String>, v: &Value) -> Part {
    Part {
        name: name.into(),
        content_type: "application/json".into(),
        data: serde_json::to_vec(v).unwrap(),
        with_length: true,
    }
}

fn encode(parts: &[Part]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        out.extend_from_slice(format!("Content-Disposition: form-data; name=\"{}\"\r\n", p.name).as_bytes());
        out.extend_from_slice(format!("Content-Type: {}\r\n", p.content_type).as_bytes());
        if p.with_length {
            out.extend_from_slice(format!("Content-Length: {}\r\n", p.data.len()).as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&p.data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}

/// The SDK's split of one item: the base without its large fields, then one part per field present.
fn item_parts(op: &str, item: &Value) -> Vec<Part> {
    let id = item["id"].as_str().unwrap();
    let mut base = item.as_object().unwrap().clone();
    let mut parts = Vec::new();
    for f in FIELDS {
        if let Some(v) = base.remove(f) {
            parts.push(json_part(format!("{op}.{id}.{f}"), &v));
        }
    }
    parts.insert(0, json_part(format!("{op}.{id}"), &Value::Object(base)));
    parts
}

fn batch_parts(post: &[Value], patch: &[Value]) -> Vec<Part> {
    let posts = post.iter().flat_map(|v| item_parts("post", v));
    posts.chain(patch.iter().flat_map(|v| item_parts("patch", v))).collect()
}

async fn send(server: &Server, body: Vec<u8>, zstd: bool) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(server.url("/runs/multipart"))
        .header("content-type", format!("multipart/form-data; boundary={BOUNDARY}"));
    let body = if zstd {
        let c = zstd::encode_all(&body[..], 3).unwrap();
        req = req
            .header("content-encoding", "zstd")
            .header("x-pre-compressed-size", body.len())
            .header("x-post-compressed-size", c.len());
        c
    } else {
        body
    };
    req.body(body).send().await.unwrap()
}

async fn send_ok(server: &Server, parts: &[Part], zstd: bool) -> Value {
    let r = send(server, encode(parts), zstd).await;
    assert_eq!(r.status(), 202);
    r.json().await.unwrap()
}

const ALL: Include = Include {
    inputs: true,
    outputs: true,
    events: true,
};

fn stored(server: &Server, id: &str) -> Option<RunView> {
    server.state.ingest.snapshot().run(id.parse().unwrap(), ALL)
}

fn reasons(reply: &Value) -> Vec<(String, String)> {
    reply["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["part"].as_str().unwrap_or("").to_string(),
                r["reason"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn full_root(n: u64) -> (Value, Value) {
    let mut post = common::ls_root(n, "p", 14);
    post["inputs"] = json!({"url": "https://example.com"});
    post["extra"] = json!({"metadata": {"prompt_version": "v7"}});
    post["events"] = json!([{"name": "start", "time": "2026-11-05T02:14:00.000000"}]);
    post["serialized"] = json!({"name": "fetch_url"});
    let id = post["id"].clone();
    let patch = json!({
        "id": id, "trace_id": id, "dotted_order": post["dotted_order"],
        "end_time": "2026-11-05T02:14:03.000000", "outputs": {"status_code": 429},
        "error": "HTTPError: 429 Too Many Requests",
    });
    (post, patch)
}

#[tokio::test]
async fn split_fields_are_stored_as_the_json_batch_stores_them() {
    let batch = common::start().await;
    let (post, patch) = full_root(1);
    common::post_batch(&batch, vec![post.clone()], vec![patch.clone()]).await;
    let id = post["id"].as_str().unwrap();
    for zstd in [false, true] {
        let mp = common::start().await;
        let r = send_ok(
            &mp,
            &batch_parts(std::slice::from_ref(&post), std::slice::from_ref(&patch)),
            zstd,
        )
        .await;
        assert_eq!(r, json!({"accepted": 2, "rejected": []}), "zstd={zstd}");
        assert_eq!(stored(&mp, id), stored(&batch, id), "zstd={zstd}");
    }
}

#[tokio::test]
async fn part_order_does_not_matter() {
    let (a, b) = (common::start().await, common::start().await);
    let (post, patch) = full_root(1);
    let parts = batch_parts(std::slice::from_ref(&post), std::slice::from_ref(&patch));
    send_ok(&a, &parts, false).await;
    let mut reversed = parts;
    reversed.reverse();
    let r = send_ok(&b, &reversed, false).await;
    assert_eq!(r["accepted"], 2);
    let id = post["id"].as_str().unwrap();
    assert!(stored(&b, id).unwrap().header.end_time.is_some());
    assert_eq!(stored(&a, id), stored(&b, id));
}

#[tokio::test]
async fn a_field_part_wins_over_the_same_key_in_its_base_in_any_order() {
    let server = common::start().await;
    let mut base = common::ls_root(1, "p", 14);
    base["inputs"] = json!({"from": "base"});
    let field = json_part(format!("post.{}.inputs", uid(1)), &json!({"from": "part"}));
    let r = send_ok(&server, &[field, json_part(format!("post.{}", uid(1)), &base)], false).await;
    assert_eq!(r["accepted"], 1);
    assert_eq!(stored(&server, &uid(1)).unwrap().inputs, Some(json!({"from": "part"})));
}

#[tokio::test]
async fn attachments_are_rejected_by_name_and_the_run_is_stored() {
    let server = common::start().await;
    let (post, _) = full_root(1);
    let id = post["id"].as_str().unwrap().to_string();
    let mut parts = item_parts("post", &post);
    parts.push(Part {
        name: format!("attachment.{id}.screenshot.png"),
        content_type: "image/png".into(),
        data: vec![0x89; 4096],
        with_length: true,
    });
    let streamed = vec![b'-'; 70_000];
    parts.push(Part {
        name: format!("attachment.{id}.trace_log"),
        content_type: format!("text/plain; length={}", streamed.len()),
        data: streamed,
        with_length: false,
    });
    let r = send_ok(&server, &parts, false).await;
    assert_eq!(r["accepted"], 1);
    assert_eq!(
        reasons(&r),
        vec![
            (
                format!("attachment.{id}.screenshot.png"),
                "attachment_not_stored".into()
            ),
            (format!("attachment.{id}.trace_log"), "attachment_not_stored".into()),
        ]
    );
    let run = stored(&server, &id).unwrap();
    assert_eq!(run.inputs, Some(json!({"url": "https://example.com"})));
}

#[tokio::test]
async fn unknown_and_feedback_parts_are_rejected_by_name_and_the_rest_stored() {
    let server = common::start().await;
    let (post, _) = full_root(1);
    let id = post["id"].as_str().unwrap().to_string();
    let mut parts = item_parts("post", &post);
    parts.push(json_part(format!("post.{id}.bogus"), &json!({})));
    parts.push(json_part("something", &json!({})));
    parts.push(json_part(
        "feedback.5b0c0000-0000-4000-8000-000000000001",
        &json!({"run_id": id, "key": "correctness", "score": 0}),
    ));
    let r = send_ok(&server, &parts, false).await;
    assert_eq!(r["accepted"], 1);
    assert_eq!(
        reasons(&r),
        vec![
            (format!("post.{id}.bogus"), "unknown_part".into()),
            ("something".into(), "unknown_part".into()),
            (
                "feedback.5b0c0000-0000-4000-8000-000000000001".into(),
                "feedback_not_stored".into()
            ),
        ]
    );
    assert!(stored(&server, &id).is_some());
}

#[tokio::test]
async fn item_rejections_carry_the_run_id() {
    let server = common::start().await;
    let (post, _) = full_root(1);
    let mut bad = common::ls_root(2, "p", 14);
    bad["tags"] = json!("not-a-list");
    let r = send_ok(&server, &batch_parts(&[post, bad], &[]), false).await;
    assert_eq!(r["accepted"], 1);
    let rej = &r["rejected"][0];
    assert_eq!(
        (rej["id"].as_str(), rej["reason"].as_str()),
        (Some(uid(2).as_str()), Some("invalid_field"))
    );
}

#[tokio::test]
async fn zstd_body_in_several_frames_and_tiny_chunks_is_accepted() {
    let server = common::start().await;
    let (post, patch) = full_root(1);
    let body = encode(&batch_parts(std::slice::from_ref(&post), &[patch]));
    let (head, tail) = body.split_at(body.len() / 2);
    let mut z = zstd::encode_all(head, 1).unwrap();
    z.extend(zstd::encode_all(tail, 19).unwrap());
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> = z.chunks(7).map(|c| Ok(c.to_vec())).collect();
    let r = reqwest::Client::new()
        .post(server.url("/runs/multipart"))
        .header("content-type", format!("multipart/form-data; boundary=\"{BOUNDARY}\""))
        .header("content-encoding", "zstd")
        .body(reqwest::Body::wrap_stream(futures_util::stream::iter(chunks)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    assert_eq!(r.json::<Value>().await.unwrap()["accepted"], 2);
    assert!(
        stored(&server, post["id"].as_str().unwrap())
            .unwrap()
            .header
            .end_time
            .is_some()
    );
}

#[tokio::test]
async fn malformed_bodies_are_400_and_store_nothing() {
    let server = common::start().await;
    let (post, _) = full_root(1);
    let good = encode(&item_parts("post", &post));
    let http = reqwest::Client::new();
    let url = server.url("/runs/multipart");
    let mp = format!("multipart/form-data; boundary={BOUNDARY}");
    let mut bad_json = item_parts("post", &post);
    bad_json[1].data = b"{not json".to_vec();
    bad_json[1].with_length = false;
    let mut long_part = item_parts("post", &post);
    long_part[0].data.extend_from_slice(b"  ");
    let header_len = format!("Content-Length: {}\r\n", long_part[0].data.len());
    let long_part = String::from_utf8(encode(&long_part)).unwrap().replacen(
        &header_len,
        &format!("Content-Length: {}\r\n", long_part[0].data.len() - 2),
        1,
    );
    type Case<'a> = (&'a str, Option<String>, Option<&'a str>, Vec<u8>);
    let cases: Vec<Case> = vec![
        ("no content type", None, None, good.clone()),
        ("not multipart", Some("application/json".into()), None, good.clone()),
        ("no boundary", Some("multipart/form-data".into()), None, good.clone()),
        ("truncated", Some(mp.clone()), None, good[..good.len() - 30].to_vec()),
        ("bad json", Some(mp.clone()), None, encode(&bad_json)),
        (
            "base not an object",
            Some(mp.clone()),
            None,
            encode(&[json_part(format!("post.{}", uid(1)), &json!([1]))]),
        ),
        (
            "part longer than its length",
            Some(mp.clone()),
            None,
            long_part.into_bytes(),
        ),
        (
            "no name",
            Some(mp.clone()),
            None,
            format!("--{BOUNDARY}\r\nContent-Type: application/json\r\n\r\n{{}}\r\n--{BOUNDARY}--\r\n").into_bytes(),
        ),
        (
            "bad zstd",
            Some(mp.clone()),
            Some("zstd"),
            b"definitely not zstd".to_vec(),
        ),
        ("truncated zstd", Some(mp.clone()), Some("zstd"), {
            let z = zstd::encode_all(&good[..], 3).unwrap();
            z[..z.len() - 4].to_vec()
        }),
        ("gzip", Some(mp.clone()), Some("gzip"), good.clone()),
        ("zstd then garbage", Some(mp.clone()), Some("zstd"), {
            let mut z = zstd::encode_all(&good[..], 3).unwrap();
            z.extend_from_slice(b"trailing garbage");
            z
        }),
        (
            "LF line endings",
            Some(mp.clone()),
            None,
            String::from_utf8(good.clone())
                .unwrap()
                .replace("\r\n", "\n")
                .into_bytes(),
        ),
    ];
    for (what, ct, enc, body) in cases {
        let mut req = http.post(&url).body(body);
        if let Some(ct) = ct {
            req = req.header("content-type", ct);
        }
        if let Some(enc) = enc {
            req = req.header("content-encoding", enc);
        }
        let r = req.send().await.unwrap();
        assert_eq!(r.status(), 400, "{what}");
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["error"]["code"], "bad_multipart", "{what}: {body}");
    }
    assert_eq!(server.state.ingest.health().runs, 0);
}

#[tokio::test]
async fn decoded_body_over_the_limit_is_413() {
    let server = common::start().await;
    let (post, _) = full_root(1);
    let mut parts = item_parts("post", &post);
    parts.push(Part {
        name: format!("attachment.{}.big", uid(1)),
        content_type: "application/octet-stream".into(),
        data: vec![0; tracedb::MAX_BODY_BYTES + 1],
        with_length: true,
    });
    let body = encode(&parts);
    assert!(
        zstd::encode_all(&body[..], 3).unwrap().len() < 1024 * 1024,
        "small on the wire"
    );
    assert_eq!(send(&server, body.clone(), true).await.status(), 413, "zstd");
    assert_eq!(send(&server, body, false).await.status(), 413, "plain");
    assert_eq!(server.state.ingest.health().runs, 0);
}

#[tokio::test]
async fn busy_is_429_with_retry_after_never_404_or_415() {
    let server = common::start_with(|c| c.queue_events = 4).await;
    let mut handles = Vec::new();
    for i in 0..48u64 {
        let url = server.url("/runs/multipart");
        let posts: Vec<Value> = (0..4).map(|j| common::ls_root(i * 10 + j + 1, "p", 14)).collect();
        let body = encode(&batch_parts(&posts, &[]));
        handles.push(tokio::spawn(async move {
            let r = reqwest::Client::new()
                .post(url)
                .header("content-type", format!("multipart/form-data; boundary={BOUNDARY}"))
                .body(body)
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.headers().get("retry-after").is_some())
        }));
    }
    let mut busy = 0;
    for h in handles {
        match h.await.unwrap() {
            (202, _) => {}
            (429, retry_after) => {
                assert!(retry_after, "429 without Retry-After");
                busy += 1;
            }
            (s, _) => panic!("unexpected {s}"),
        }
    }
    assert!(
        busy > 0,
        "with room for one batch in flight, concurrent uploads see 429"
    );
}

#[tokio::test]
async fn info_turns_on_multipart_and_zstd() {
    let server = common::start().await;
    let info: Value = reqwest::get(server.url("/info")).await.unwrap().json().await.unwrap();
    assert_eq!(info["version"], "", "empty version keeps feedback on POST /feedback");
    assert_eq!(info["batch_ingest_config"]["use_multipart_endpoint"], true);
    assert_eq!(info["instance_flags"]["zstd_compression_enabled"], true);
}

struct Pair {
    rt: tokio::runtime::Runtime,
    batch: Server,
    multipart: Server,
}

fn pair() -> &'static Pair {
    static PAIR: OnceLock<Pair> = OnceLock::new();
    PAIR.get_or_init(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (batch, multipart) = rt.block_on(async { (common::start().await, common::start().await) });
        Pair { rt, batch, multipart }
    })
}

#[derive(Debug, Clone)]
struct Gen {
    input: String,
    output: Option<String>,
    error: Option<String>,
    meta: Option<String>,
    events: usize,
    serialized: bool,
    patch: bool,
}

fn gen_run() -> impl Strategy<Value = Gen> {
    (
        "[a-z ]{0,12}",
        proptest::option::of("[a-z]{0,8}"),
        proptest::option::of("[A-Za-z:]{1,16}"),
        proptest::option::of("[a-z0-9]{1,6}"),
        0usize..3,
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(input, output, error, meta, events, serialized, patch)| Gen {
            input,
            output,
            error,
            meta,
            events,
            serialized,
            patch,
        })
}

fn build(n: u64, g: &Gen) -> (Value, Option<Value>) {
    let mut post = common::ls_root(n, "p", (n % 600) as u32);
    let o = post.as_object_mut().unwrap();
    o.insert("inputs".into(), json!({"q": g.input}));
    // As the SDK does: a post carries empty outputs that its patch replaces, so post/patch order matters.
    o.insert("outputs".into(), json!({}));
    if let Some(m) = &g.meta {
        o.insert("extra".into(), json!({"metadata": {"k": m}}));
    }
    if g.events > 0 {
        let ev: Vec<Value> = (0..g.events).map(|i| json!({"name": format!("e{i}")})).collect();
        o.insert("events".into(), Value::Array(ev));
    }
    if g.serialized {
        o.insert("serialized".into(), json!({"name": "tool"}));
    }
    let mut end = Map::new();
    end.insert("id".into(), post["id"].clone());
    end.insert("trace_id".into(), post["trace_id"].clone());
    end.insert("dotted_order".into(), post["dotted_order"].clone());
    end.insert("end_time".into(), json!("2026-11-05T23:59:00.000000"));
    if let Some(out) = &g.output {
        end.insert("outputs".into(), json!({"a": out}));
    }
    if let Some(e) = &g.error {
        end.insert("error".into(), json!(e));
    }
    if g.patch {
        (post, Some(Value::Object(end)))
    } else {
        for (k, v) in end {
            post[k] = v;
        }
        (post, None)
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn multipart_round_trips_to_the_same_runs_as_json(
        runs in proptest::collection::vec(gen_run(), 1..6),
        order in any::<proptest::sample::Index>(),
        swaps in proptest::collection::vec((any::<proptest::sample::Index>(), any::<proptest::sample::Index>()), 0..12),
        zstd in any::<bool>(),
    ) {
        static NEXT: AtomicU64 = AtomicU64::new(1_000_000);
        let p = pair();
        let base = NEXT.fetch_add(runs.len() as u64, Ordering::SeqCst);
        let (mut post, mut patch) = (Vec::new(), Vec::new());
        for (i, g) in runs.iter().enumerate() {
            let (a, b) = build(base + i as u64, g);
            post.push(a);
            patch.extend(b);
        }
        let mut parts = batch_parts(&post, &patch);
        let len = parts.len();
        parts.rotate_left(order.index(len));
        for (i, j) in &swaps {
            parts.swap(i.index(len), j.index(len));
        }
        let (want, got) = p.rt.block_on(async {
            let want = common::post_batch(&p.batch, post.clone(), patch.clone()).await;
            (want, send_ok(&p.multipart, &parts, zstd).await)
        });
        prop_assert_eq!(&got, &want);
        for item in &post {
            let id = item["id"].as_str().unwrap();
            let (mut a, mut b) = (stored(&p.batch, id).unwrap(), stored(&p.multipart, id).unwrap());
            prop_assert_eq!(a.header.event_count, b.header.event_count);
            for v in [&mut a, &mut b] {
                (v.header.first_seq, v.header.last_seq) = (0, 0);
            }
            prop_assert_eq!(a, b);
        }
    }
}
