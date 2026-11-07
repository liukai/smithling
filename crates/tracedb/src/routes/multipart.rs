//! `POST /runs/multipart`: the LangSmith SDK's multipart upload, plain or zstd (contracts/http-api.md).
//!
//! The body is parsed as it arrives. Run parts are assembled per `(op, id)` and go through the same mapping and
//! writer as `/runs/batch`; other parts are drained without buffering and rejected by name.

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio_stream::StreamExt;
use tracedb_model::{Op, Source};
use zstd::stream::raw::{DParameter, Decoder, InBuffer, Operation, OutBuffer};

use crate::langsmith_map::Dialect;
use crate::routes::errors::{accept_error, error};
use crate::routes::ingest::accept_items;
use crate::{AppState, MAX_BODY_BYTES};

const FIELDS: [&str; 6] = ["inputs", "outputs", "events", "extra", "error", "serialized"];
const MAX_PART_HEADERS: usize = 16 * 1024;
const MAX_PADDING: usize = 256;
/// 16 MB: enough for every zstd level up to 19 (the SDK uses 1); bigger windows are refused, not allocated.
const ZSTD_WINDOW_LOG_MAX: u32 = 24;

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Base(Op, String),
    Field(Op, String, String),
    Attachment,
    Feedback,
    Unknown,
}

fn kind(name: &str) -> Kind {
    let mut it = name.splitn(3, '.');
    let (a, b, c) = (it.next(), it.next().filter(|s| !s.is_empty()), it.next());
    let op = match a {
        Some("post") => Some(Op::Post),
        Some("patch") => Some(Op::Patch),
        _ => None,
    };
    match (op, a, b, c) {
        (Some(op), _, Some(id), None) => Kind::Base(op, id.to_string()),
        (Some(op), _, Some(id), Some(f)) if FIELDS.contains(&f) => Kind::Field(op, id.to_string(), f.to_string()),
        (None, Some("attachment"), Some(_), Some(n)) if !n.is_empty() => Kind::Attachment,
        (None, Some("feedback"), Some(_), None) => Kind::Feedback,
        _ => Kind::Unknown,
    }
}

fn keep(name: &str) -> bool {
    matches!(kind(name), Kind::Base(..) | Kind::Field(..))
}

pub struct Part {
    pub name: String,
    /// `None` when the part was drained.
    pub data: Option<Vec<u8>>,
}

enum St {
    Preamble,
    AfterBoundary,
    Headers,
    Body {
        name: String,
        data: Option<Vec<u8>>,
        left: Option<usize>,
    },
    Done,
}

/// A push parser for `multipart/form-data`: feed bytes in any chunking, then `finish`.
pub struct Parser {
    /// `\r\n--<boundary>`; the body is read as if it started with `\r\n`, so the first boundary needs no special case.
    delim: Vec<u8>,
    buf: Vec<u8>,
    st: St,
    parts: Vec<Part>,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The `name` parameter of `form-data; name="…"; filename="…"`; quoted values may hold `;`.
fn disposition_name(v: &str) -> Option<String> {
    let mut rest = v.split_once(';')?.1;
    loop {
        let (k, after) = rest.split_once('=')?;
        let after = after.trim_start();
        let (val, next) = match after.strip_prefix('"') {
            Some(q) => {
                let end = q.find('"')?;
                (&q[..end], q[end + 1..].split_once(';').map_or("", |(_, n)| n))
            }
            None => after.split_once(';').unwrap_or((after, "")),
        };
        if k.trim().eq_ignore_ascii_case("name") {
            return Some(val.trim().to_string());
        }
        rest = next;
    }
}

impl Parser {
    pub fn new(boundary: &str) -> Parser {
        Parser {
            delim: format!("\r\n--{boundary}").into_bytes(),
            buf: b"\r\n".to_vec(),
            st: St::Preamble,
            parts: Vec::new(),
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), String> {
        self.buf.extend_from_slice(chunk);
        let mut pos = 0;
        let r = self.step(&mut pos);
        self.buf.drain(..pos);
        r
    }

    pub fn finish(self) -> Result<Vec<Part>, String> {
        match self.st {
            St::Done => Ok(self.parts),
            _ => Err("body ends before the closing boundary".into()),
        }
    }

    fn step(&mut self, pos: &mut usize) -> Result<(), String> {
        let d = self.delim.len();
        loop {
            let rest = &self.buf[*pos..];
            match &mut self.st {
                St::Done => {
                    *pos = self.buf.len();
                    return Ok(());
                }
                St::Preamble => match find(rest, &self.delim) {
                    Some(i) => {
                        *pos += i + d;
                        self.st = St::AfterBoundary;
                    }
                    None => {
                        *pos += rest.len().saturating_sub(d - 1);
                        return Ok(());
                    }
                },
                St::AfterBoundary => {
                    if rest.len() < 2 {
                        return Ok(());
                    }
                    if rest.starts_with(b"--") {
                        self.st = St::Done;
                        continue;
                    }
                    let pad = rest.iter().take_while(|b| matches!(b, b' ' | b'\t')).count();
                    if pad > MAX_PADDING {
                        return Err("malformed boundary line".into());
                    }
                    if rest.len() < pad + 2 {
                        return Ok(());
                    }
                    if &rest[pad..pad + 2] != b"\r\n" {
                        return Err("malformed boundary line".into());
                    }
                    *pos += pad + 2;
                    self.st = St::Headers;
                }
                St::Headers => {
                    let end = if rest.starts_with(b"\r\n") {
                        Some(2)
                    } else {
                        find(rest, b"\r\n\r\n").map(|i| i + 4)
                    };
                    let Some(end) = end else {
                        if rest.len() > MAX_PART_HEADERS {
                            return Err("part headers too large".into());
                        }
                        return Ok(());
                    };
                    let mut hs = [httparse::EMPTY_HEADER; 16];
                    let hs = match httparse::parse_headers(&rest[..end], &mut hs) {
                        Ok(httparse::Status::Complete((_, hs))) => hs,
                        _ => return Err("malformed part headers".into()),
                    };
                    let mut name = None;
                    let mut left = None;
                    for h in hs.iter() {
                        let v = std::str::from_utf8(h.value).map_err(|_| "part header is not UTF-8")?;
                        if h.name.eq_ignore_ascii_case("content-disposition") {
                            name = disposition_name(v);
                        } else if h.name.eq_ignore_ascii_case("content-length") {
                            left = Some(v.trim().parse::<usize>().map_err(|_| "bad part Content-Length")?);
                        }
                    }
                    let name = name.ok_or("part without a name")?;
                    *pos += end;
                    let data = keep(&name).then(Vec::new);
                    self.st = St::Body { name, data, left };
                }
                St::Body { name, data, left } => {
                    let took = match left {
                        Some(n) => {
                            let k = (*n).min(rest.len());
                            if let Some(data) = data {
                                data.extend_from_slice(&rest[..k]);
                            }
                            *n -= k;
                            *pos += k;
                            if *n > 0 {
                                return Ok(());
                            }
                            let rest = &self.buf[*pos..];
                            if rest.len() < d {
                                return Ok(());
                            }
                            if rest[..d] != self.delim[..] {
                                return Err(format!("part {name} is longer than its Content-Length"));
                            }
                            d
                        }
                        None => match find(rest, &self.delim) {
                            Some(i) => {
                                if let Some(data) = data {
                                    data.extend_from_slice(&rest[..i]);
                                }
                                i + d
                            }
                            None => {
                                let k = rest.len().saturating_sub(d - 1);
                                if let Some(data) = data {
                                    data.extend_from_slice(&rest[..k]);
                                }
                                *pos += k;
                                return Ok(());
                            }
                        },
                    };
                    *pos += took;
                    let (name, data) = (std::mem::take(name), data.take());
                    self.parts.push(Part { name, data });
                    self.st = St::AfterBoundary;
                }
            }
        }
    }
}

enum Fail {
    Bad(String),
    TooLarge,
}

fn bad(m: impl Into<String>) -> Fail {
    Fail::Bad(m.into())
}

fn boundary(headers: &HeaderMap) -> Result<String, Fail> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| bad("missing Content-Type"))?;
    let mut it = ct.split(';');
    if !it
        .next()
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("multipart/form-data"))
    {
        return Err(bad("Content-Type is not multipart/form-data"));
    }
    it.find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| v.trim().trim_matches('"').to_string())
    })
    .filter(|b| !b.is_empty() && b.len() <= 70)
    .ok_or_else(|| bad("missing boundary"))
}

/// Decodes `Content-Encoding: zstd` as the body arrives, so the decoded size is capped before it is held.
struct Unzstd {
    dec: Decoder<'static>,
    out: Vec<u8>,
    frame_done: bool,
}

impl Unzstd {
    fn new() -> Result<Unzstd, Fail> {
        let mut dec = Decoder::new().map_err(|e| bad(format!("zstd: {e}")))?;
        dec.set_parameter(DParameter::WindowLogMax(ZSTD_WINDOW_LOG_MAX))
            .map_err(|e| bad(format!("zstd: {e}")))?;
        Ok(Unzstd {
            dec,
            out: vec![0; 128 * 1024],
            frame_done: true,
        })
    }

    fn feed(&mut self, chunk: &[u8], mut sink: impl FnMut(&[u8]) -> Result<(), Fail>) -> Result<(), Fail> {
        let mut input = InBuffer::around(chunk);
        loop {
            let before = input.pos();
            let mut output = OutBuffer::around(&mut self.out[..]);
            let hint = self
                .dec
                .run(&mut input, &mut output)
                .map_err(|e| bad(format!("zstd: {e}")))?;
            let (n, full) = (output.pos(), output.pos() == output.capacity());
            if n > 0 || input.pos() > before {
                self.frame_done = hint == 0;
            }
            sink(&self.out[..n])?;
            if input.pos() == chunk.len() && !full {
                return Ok(());
            }
        }
    }
}

async fn parse(headers: &HeaderMap, body: Body) -> Result<Vec<Part>, Fail> {
    let mut parser = Parser::new(&boundary(headers)?);
    let mut unzstd = match headers
        .get(header::CONTENT_ENCODING)
        .map(|v| v.to_str().unwrap_or("?").trim())
    {
        None | Some("identity") | Some("") => None,
        Some(e) if e.eq_ignore_ascii_case("zstd") => Some(Unzstd::new()?),
        Some(e) => return Err(bad(format!("unsupported Content-Encoding {e:?}; use zstd or none"))),
    };
    let (mut raw, mut decoded) = (0usize, 0usize);
    let mut push = |bytes: &[u8]| {
        decoded += bytes.len();
        if decoded > MAX_BODY_BYTES {
            return Err(Fail::TooLarge);
        }
        parser.feed(bytes).map_err(Fail::Bad)
    };
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| bad(format!("reading the body: {e}")))?;
        raw += chunk.len();
        if raw > MAX_BODY_BYTES {
            return Err(Fail::TooLarge);
        }
        match &mut unzstd {
            Some(z) => z.feed(&chunk, &mut push)?,
            None => push(&chunk)?,
        }
    }
    if let Some(z) = &mut unzstd {
        z.feed(&[], &mut push)?;
        if !z.frame_done {
            return Err(bad("zstd: truncated frame"));
        }
    }
    parser.finish().map_err(Fail::Bad)
}

struct Assembled {
    items: Vec<(Op, Value)>,
    rejected: Vec<Value>,
}

fn assemble(parts: Vec<Part>) -> Result<Assembled, Fail> {
    // (base, fields): fields win over the same key in the base, whatever the part order.
    type BaseAndFields = (Map<String, Value>, Map<String, Value>);
    let mut runs: IndexMap<(Op, String), BaseAndFields> = IndexMap::new();
    let mut rejected = Vec::new();
    for Part { name, data } in parts {
        let reason = match kind(&name) {
            k @ (Kind::Base(..) | Kind::Field(..)) => {
                let v: Value = serde_json::from_slice(data.as_deref().unwrap_or_default())
                    .map_err(|e| bad(format!("part {name}: bad JSON: {e}")))?;
                match k {
                    Kind::Base(op, id) => {
                        let Value::Object(obj) = v else {
                            return Err(bad(format!("part {name}: not a JSON object")));
                        };
                        runs.entry((op, id)).or_default().0.extend(obj);
                    }
                    Kind::Field(op, id, f) => {
                        runs.entry((op, id)).or_default().1.insert(f, v);
                    }
                    _ => unreachable!(),
                }
                continue;
            }
            Kind::Attachment => "attachment_not_stored",
            // Stored from unit 9 (T101); the SDK only sends these when `/info` has a version.
            Kind::Feedback => "feedback_not_stored",
            Kind::Unknown => "unknown_part",
        };
        rejected.push(json!({"part": name, "reason": reason}));
    }
    let mut items: Vec<(Op, Value)> = runs
        .into_iter()
        .map(|((op, id), (mut obj, fields))| {
            obj.extend(fields);
            obj.entry("id").or_insert(Value::String(id));
            (op, Value::Object(obj))
        })
        .collect();
    // As in a JSON batch: every post, then every patch.
    items.sort_by_key(|(op, _)| *op == Op::Patch);
    Ok(Assembled { items, rejected })
}

pub async fn runs_multipart(State(st): State<AppState>, headers: HeaderMap, body: Body) -> Response {
    let assembled = match parse(&headers, body).await.and_then(assemble) {
        Ok(a) => a,
        Err(Fail::Bad(m)) => {
            tracing::debug!(error = %m, "POST /runs/multipart rejected");
            return error(StatusCode::BAD_REQUEST, "bad_multipart", m);
        }
        Err(Fail::TooLarge) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                format!("decoded body over {MAX_BODY_BYTES} bytes"),
            );
        }
    };
    tracing::debug!(
        encoding = ?headers.get(header::CONTENT_ENCODING),
        items = assembled.items.len(),
        rejected_parts = assembled.rejected.len(),
        "POST /runs/multipart"
    );
    let items = assembled.items.iter().map(|(op, v)| (*op, v));
    match accept_items(&st, items, Source::LsBatch, Dialect::LangSmith).await {
        Ok(r) => {
            let mut rejected: Vec<Value> = r.rejected.iter().map(|x| json!(x)).collect();
            rejected.extend(assembled.rejected);
            (
                StatusCode::ACCEPTED,
                Json(json!({"accepted": r.accepted, "rejected": rejected})),
            )
                .into_response()
        }
        Err(e) => accept_error(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_names() {
        let id = "7c1e0000-0000-4000-8000-0000000000a4";
        assert_eq!(kind(&format!("post.{id}")), Kind::Base(Op::Post, id.into()));
        assert_eq!(
            kind(&format!("patch.{id}.outputs")),
            Kind::Field(Op::Patch, id.into(), "outputs".into())
        );
        assert_eq!(kind(&format!("post.{id}.bogus")), Kind::Unknown);
        assert_eq!(kind(&format!("post.{id}.inputs.x")), Kind::Unknown);
        assert_eq!(kind("post."), Kind::Unknown);
        assert_eq!(kind(&format!("attachment.{id}.screenshot.png")), Kind::Attachment);
        assert_eq!(kind(&format!("attachment.{id}")), Kind::Unknown);
        assert_eq!(kind(&format!("feedback.{id}")), Kind::Feedback);
        assert_eq!(kind("put.x"), Kind::Unknown);
    }

    #[test]
    fn disposition_names() {
        assert_eq!(
            disposition_name(r#"form-data; name="post.1""#).as_deref(),
            Some("post.1")
        );
        assert_eq!(
            disposition_name(r#"form-data; filename="a;name=b"; name="x;y""#).as_deref(),
            Some("x;y")
        );
        assert_eq!(
            disposition_name("form-data; name=plain; filename=f").as_deref(),
            Some("plain")
        );
        assert_eq!(disposition_name(r#"form-data; filename="f""#), None);
        assert_eq!(disposition_name("form-data"), None);
    }

    #[test]
    fn padding_and_header_floods_are_errors_not_buffers() {
        let mut p = Parser::new("B");
        p.feed(b"--B").unwrap();
        let spaces = vec![b' '; 1024];
        assert!(p.feed(&spaces).is_err(), "padding is capped");

        let mut p = Parser::new("B");
        p.feed(b"--B\r\nX-A: 1\r\n").unwrap();
        let long = format!("X-B: {}\r\n", "y".repeat(MAX_PART_HEADERS));
        assert!(p.feed(long.as_bytes()).is_err(), "header block is capped");

        let many: String = (0..20).map(|i| format!("X-{i}: v\r\n")).collect();
        let mut p = Parser::new("B");
        let body = format!("--B\r\nContent-Disposition: form-data; name=\"a\"\r\n{many}\r\n{{}}\r\n--B--");
        assert!(p.feed(body.as_bytes()).is_err(), "too many part headers");
    }

    #[test]
    fn any_chunking_gives_the_same_parts() {
        let body = b"preamble\r\n--XB\r\nContent-Disposition: form-data; name=\"a\"\r\nContent-Length: 3\r\n\r\n{}x\r\n--XB \r\nContent-Disposition: form-data; name=\"post.1\"\r\n\r\n[1,\r\n--X]\r\n--XB--\r\nepilogue";
        for size in 1..body.len() {
            let mut p = Parser::new("XB");
            for c in body.chunks(size) {
                p.feed(c).unwrap();
            }
            let parts = p.finish().unwrap();
            let got: Vec<_> = parts.iter().map(|p| (p.name.as_str(), p.data.as_deref())).collect();
            assert_eq!(
                got,
                vec![("a", None), ("post.1", Some(&b"[1,\r\n--X]"[..]))],
                "chunk {size}"
            );
        }
    }
}
