//! Text index sidecar (data-model §2): the dictionary's columns and order, the postings format, lookups that
//! return exactly the rows holding every word on one side, and lookups that read only the footer, the
//! dictionary pages whose term zone holds the words, and one postings range per word.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use datafusion::arrow::array::AsArray;
use datafusion::arrow::datatypes::{DataType, UInt8Type, UInt32Type, UInt64Type};
use datafusion::parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use datafusion::parquet::basic::Compression;
use datafusion::parquet::file::reader::{FileReader, SerializedFileReader};
use datafusion::parquet::file::serialized_reader::ReadOptionsBuilder;
use futures_util::stream::BoxStream;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore, PutMultipartOptions,
    PutOptions, PutPayload, PutResult,
};
use proptest::prelude::*;
use serde_json::{Value, json};
use smithling_storage::fts::{self, Fts, TextField, TextIndex, TextRow};
use smithling_storage::segment::{self, SegmentMeta};
use smithling_storage::{StorageError, Store};

#[derive(Clone, Debug, Default)]
struct Reads {
    /// `(path, bytes)` per request: a ranged get, a suffix get or a whole `get_ranges` call.
    requests: Vec<(String, u64)>,
}

/// Counts every read request that reaches the object store.
#[derive(Debug)]
struct Counting {
    inner: InMemory,
    reads: Arc<Mutex<Reads>>,
}

impl fmt::Display for Counting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Counting({})", self.inner)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Counting {
    async fn put_opts(&self, l: &Path, p: PutPayload, o: PutOptions) -> object_store::Result<PutResult> {
        self.inner.put_opts(l, p, o).await
    }
    async fn put_multipart_opts(
        &self,
        l: &Path,
        o: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(l, o).await
    }
    async fn get_opts(&self, l: &Path, o: GetOptions) -> object_store::Result<GetResult> {
        let r = self.inner.get_opts(l, o).await?;
        let n = r.range.end - r.range.start;
        self.reads.lock().unwrap().requests.push((l.to_string(), n));
        Ok(r)
    }
    async fn get_ranges(&self, l: &Path, ranges: &[Range<u64>]) -> object_store::Result<Vec<Bytes>> {
        let n = ranges.iter().map(|r| r.end - r.start).sum();
        self.reads.lock().unwrap().requests.push((l.to_string(), n));
        self.inner.get_ranges(l, ranges).await
    }
    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }
    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(&self, from: &Path, to: &Path, o: CopyOptions) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, o).await
    }
}

fn counting() -> (Store, Arc<Mutex<Reads>>) {
    let reads = Arc::new(Mutex::new(Reads::default()));
    let store = Counting {
        inner: InMemory::new(),
        reads: reads.clone(),
    };
    (Store::with_object_store(Arc::new(store), "fts-test"), reads)
}

fn meta(fts_path: &str) -> SegmentMeta {
    SegmentMeta {
        id: "seg-test".into(),
        project: "p".into(),
        core_path: format!("{fts_path}.core.parquet"),
        payload_path: format!("{fts_path}.payload.parquet"),
        min_start_key: 0,
        max_start_key: 0,
        min_trace_id: Default::default(),
        max_trace_id: Default::default(),
        min_seq: 0,
        max_seq: 0,
        row_count: 0,
        needs_merge_rows: 0,
        core_bytes: 0,
        payload_bytes: 0,
        created_at: 0,
        fts_path: Some(fts_path.into()),
        fts_bytes: 0,
    }
}

#[derive(Clone, Debug)]
struct Row {
    inputs: Option<Value>,
    outputs: Option<Value>,
    error: Option<String>,
}

impl Row {
    fn text(&self) -> TextRow<'_> {
        TextRow {
            inputs: self.inputs.as_ref(),
            outputs: self.outputs.as_ref(),
            error: self.error.as_deref(),
        }
    }
}

async fn put(store: &Store, stem: &str, rows: &[Row]) -> fts::Sidecar {
    let text: Vec<TextRow<'_>> = rows.iter().map(Row::text).collect();
    let sidecar = Fts::new(store.clone()).write(&text).unwrap();
    store
        .put_new(&fts::dict_path(stem), sidecar.dict.clone())
        .await
        .unwrap();
    store
        .put_new(&fts::post_path(stem), sidecar.post.clone())
        .await
        .unwrap();
    sidecar
}

/// The dictionary's rows, in file order.
fn dictionary(dict: &Bytes) -> Vec<(String, u8, u32, u64, u32)> {
    let mut out = Vec::new();
    for b in ParquetRecordBatchReaderBuilder::try_new(dict.clone())
        .unwrap()
        .build()
        .unwrap()
    {
        let b = b.unwrap();
        let term = b.column_by_name("term").unwrap().as_string::<i32>();
        let field = b.column_by_name("field").unwrap().as_primitive::<UInt8Type>();
        let df = b.column_by_name("df").unwrap().as_primitive::<UInt32Type>();
        let off = b.column_by_name("post_off").unwrap().as_primitive::<UInt64Type>();
        let len = b.column_by_name("post_len").unwrap().as_primitive::<UInt32Type>();
        for i in 0..b.num_rows() {
            out.push((
                term.value(i).to_string(),
                field.value(i),
                df.value(i),
                off.value(i),
                len.value(i),
            ));
        }
    }
    out
}

/// Naive answer: rows whose `field` has every word.
fn naive(rows: &[Row], words: &[&str], field: TextField) -> Vec<u32> {
    let leaves = |v: &Value| {
        fn walk(v: &Value, out: &mut BTreeSet<String>) {
            match v {
                Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
                Value::Object(o) => o.values().for_each(|x| walk(x, out)),
                s => out.extend(smithling_model::text::scalar_tokens(s)),
            }
        }
        let mut out = BTreeSet::new();
        walk(v, &mut out);
        out
    };
    rows.iter()
        .enumerate()
        .filter(|(_, r)| {
            let have = match field {
                TextField::Inputs => r.inputs.as_ref().map(leaves).unwrap_or_default(),
                TextField::Outputs => r.outputs.as_ref().map(leaves).unwrap_or_default(),
                TextField::Error => r
                    .error
                    .as_deref()
                    .map(|e| smithling_model::text::tokens(e).into_iter().collect())
                    .unwrap_or_default(),
            };
            words.iter().all(|w| have.contains(&w.to_lowercase()))
        })
        .map(|(i, _)| i as u32)
        .collect()
}

fn words(rows: &[&str]) -> Vec<String> {
    rows.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn dictionary_has_one_sorted_row_per_term_and_side() {
    let (store, _) = counting();
    let rows = vec![
        Row {
            inputs: Some(json!({"url": "https://api.example.com/v1", "retries": 3, "stream": true})),
            outputs: None,
            error: None,
        },
        Row {
            inputs: Some(json!({"messages": [{"role": "user", "content": "Too many requests?"}]})),
            outputs: Some(json!({"body": "429 Too Many Requests", "status_code": 429})),
            error: Some("HTTPError: 429 Too Many Requests".into()),
        },
        Row {
            inputs: None,
            outputs: Some(json!("many many MANY")),
            error: None,
        },
    ];
    let sidecar = put(&store, "p/d/seg-1.fts", &rows).await;
    assert_eq!(sidecar.bytes(), (sidecar.dict.len() + sidecar.post.len()) as u64);

    let reader = ParquetRecordBatchReaderBuilder::try_new(sidecar.dict.clone()).unwrap();
    let schema = reader.schema().clone();
    let cols: Vec<(&str, &DataType)> = schema
        .fields()
        .iter()
        .map(|f| (f.name().as_str(), f.data_type()))
        .collect();
    assert_eq!(
        cols,
        vec![
            ("term", &DataType::Utf8),
            ("field", &DataType::UInt8),
            ("df", &DataType::UInt32),
            ("post_off", &DataType::UInt64),
            ("post_len", &DataType::UInt32),
        ]
    );
    let dict = dictionary(&sidecar.dict);
    let keys: Vec<(String, u8)> = dict.iter().map(|r| (r.0.clone(), r.1)).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(keys, sorted, "sorted by (term, field), one row each");
    let get = |t: &str, f: u8| dict.iter().find(|r| r.0 == t && r.1 == f).map(|r| r.2);
    // Leaves of inputs / outputs, numbers and booleans as text; error as a whole.
    assert_eq!(get("https", 0), Some(1));
    for w in smithling_model::text::tokens("https://api.example.com/v1") {
        assert_eq!(get(&w, 0), Some(1), "{w}: the tokenizer's words");
    }
    assert_eq!(get("3", 0), Some(1));
    assert_eq!(get("true", 0), Some(1));
    assert_eq!(get("too", 0), Some(1));
    assert_eq!(get("429", 1), Some(1));
    assert_eq!(get("many", 1), Some(2), "df counts rows, not occurrences");
    assert_eq!(get("httperror", 2), Some(1));
    assert_eq!(get("429", 2), Some(1));
    for key in ["url", "retries", "messages", "role", "content", "body", "status_code"] {
        assert!(dict.iter().all(|r| r.0 != key), "keys aren't indexed: {key}");
    }

    let file = SerializedFileReader::new_with_options(
        sidecar.dict.clone(),
        ReadOptionsBuilder::new().with_page_index().build(),
    )
    .unwrap();
    let md = file.metadata();
    assert!(
        md.column_index().is_some() && md.offset_index().is_some(),
        "page index on"
    );
    for c in md.row_group(0).columns() {
        assert!(matches!(c.compression(), Compression::ZSTD(_)), "zstd");
    }
}

#[test]
fn a_list_is_n_then_blocks_of_first_value_and_bit_packed_deltas() {
    // n = 3; block: first 5, width 0 (deltas 1, 1 are stored as gaps 0, 0).
    assert_eq!(fts::encode_list(&[5, 6, 7]), vec![3, 5, 0]);
    // first 1, gaps 2 and 0 at width 2: 0b00_10 in the low bits.
    assert_eq!(fts::encode_list(&[1, 4, 5]), vec![3, 1, 2, 0b0000_0010]);
    // A second block starts at row 128 with its own first value.
    let rows: Vec<u32> = (0..129).map(|i| i * 2).collect();
    let bytes = fts::encode_list(&rows);
    assert_eq!(&bytes[..2], &[129, 1], "n = 129 as a varint");
    assert_eq!(fts::decode_list(&bytes).unwrap(), rows);
    assert!(
        fts::decode_list(&[5, 1]).is_err(),
        "a short list is an error, not a panic"
    );
}

proptest! {
    #[test]
    fn postings_round_trip(set in proptest::collection::btree_set(any::<u32>(), 0..700)) {
        let rows: Vec<u32> = set.into_iter().collect();
        prop_assert_eq!(fts::decode_list(&fts::encode_list(&rows)).unwrap(), rows);
    }
}

/// Rows over a big vocabulary, so the dictionary has many pages and postings span several chunks.
fn big(n: usize) -> Vec<Row> {
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    (0..n)
        .map(|i| {
            let ws: Vec<String> = (0..12).map(|_| format!("w{}", next() % 20_000)).collect();
            let planted = match (i % 3 == 0, i % 4 == 0) {
                (true, true) => " alpha beta",
                (true, false) => " alpha",
                (false, true) => " beta",
                _ => "",
            };
            Row {
                inputs: Some(json!({"q": format!("{}{planted}", ws[..6].join(" ")), "n": i % 7})),
                outputs: Some(json!({"text": format!("{} common", ws[6..].join(" "))})),
                error: (i % 5 == 0).then(|| {
                    let timeout = if i % 10 == 0 { " timeout" } else { "" };
                    format!("Error {} {}{timeout}", ws[0], ws[11])
                }),
            }
        })
        .collect()
}

/// Every probe's answer equals the naive one; the ones marked `true` must be non-empty.
async fn check_lookups(index: &Fts, seg: &SegmentMeta, rows: &[Row]) {
    let ws = |r: usize, f: &str, k: usize| -> Vec<String> {
        let v = match f {
            "in" => rows[r].inputs.as_ref().unwrap()["q"].as_str().unwrap(),
            _ => rows[r].outputs.as_ref().unwrap()["text"].as_str().unwrap(),
        };
        v.split(' ').take(k).map(str::to_string).collect()
    };
    let (row12_in, row7_out) = (ws(12, "in", 3), ws(7, "out", 2));
    let mut probes: Vec<(Vec<&str>, TextField, bool)> = vec![
        (vec!["alpha"], TextField::Inputs, true),
        (vec!["alpha", "beta"], TextField::Inputs, true),
        (vec!["ALPHA", "Beta"], TextField::Inputs, true),
        (vec!["common"], TextField::Outputs, true),
        (vec!["error", "timeout"], TextField::Error, true),
        (vec!["3"], TextField::Inputs, true),
        (vec!["common"], TextField::Inputs, false),
        (vec!["alpha"], TextField::Outputs, false),
        (vec!["nope"], TextField::Inputs, false),
        (vec!["alpha", "nope"], TextField::Inputs, false),
    ];
    probes.push((row12_in.iter().map(String::as_str).collect(), TextField::Inputs, true));
    let mut with_alpha: Vec<&str> = row12_in.iter().map(String::as_str).collect();
    with_alpha.push("alpha");
    probes.push((with_alpha, TextField::Inputs, true));
    let mut out: Vec<&str> = row7_out.iter().map(String::as_str).collect();
    out.push("common");
    probes.push((out, TextField::Outputs, true));
    for (ws, f, some) in probes {
        let want = naive(rows, &ws, f);
        assert_eq!(
            !want.is_empty(),
            some,
            "probe {ws:?} {f:?} is meant to be {}",
            if some { "non-empty" } else { "empty" }
        );
        let got = index.lookup(seg, &words(&ws), f).await.unwrap();
        assert_eq!(got, want, "{ws:?} {f:?}");
    }
    let mut found = 0;
    for i in (0..20_000).step_by(97) {
        let w = format!("w{i}");
        for f in [TextField::Inputs, TextField::Outputs, TextField::Error] {
            let want = naive(rows, &[&w], f);
            found += !want.is_empty() as usize;
            let got = index.lookup(seg, std::slice::from_ref(&w), f).await.unwrap();
            assert_eq!(got, want, "{w} {f:?}");
        }
    }
    assert!(found > 100, "most single words exist: {found}");
}

#[tokio::test]
async fn lookups_return_exactly_the_rows_with_every_word_on_that_side() {
    let (store, _) = counting();
    let rows = big(4_000);
    let sidecar = put(&store, "p/d/seg-2.fts", &rows).await;
    let index = Fts::new(store);
    let seg = meta("p/d/seg-2.fts");

    let dict = dictionary(&sidecar.dict);
    assert!(dict.len() > 20_000, "a dictionary of many pages: {}", dict.len());
    for (_, _, _, off, len) in &dict {
        let (first, last) = (off / fts::CHUNK, (off + *len as u64 - 1) / fts::CHUNK);
        assert!(
            first == last || *len as u64 > fts::CHUNK,
            "list at {off}+{len} straddles a chunk"
        );
    }
    let common = dict.iter().find(|r| r.0 == "common" && r.1 == 1).unwrap();
    assert!(common.4 as u64 > 64, "a long list");
    check_lookups(&index, &seg, &rows).await;
    let none = index.lookup(&seg, &[], TextField::Inputs).await;
    assert!(
        matches!(none, Err(StorageError::NotIndexed(_))),
        "no words: nothing to look up"
    );
}

#[tokio::test]
async fn lookups_work_across_dictionary_row_groups() {
    let (store, _) = counting();
    let rows = big(4_000);
    let text: Vec<TextRow<'_>> = rows.iter().map(Row::text).collect();
    let sidecar = fts::build_in_groups(text, 1_000).unwrap();
    let groups = ParquetRecordBatchReaderBuilder::try_new(sidecar.dict.clone())
        .unwrap()
        .metadata()
        .num_row_groups();
    assert!(groups > 20, "{groups} row groups");
    store.put_new("p/d/seg-g.fts.dict.parquet", sidecar.dict).await.unwrap();
    store.put_new("p/d/seg-g.fts.post", sidecar.post).await.unwrap();
    check_lookups(&Fts::new(store), &meta("p/d/seg-g.fts"), &rows).await;
}

#[tokio::test]
async fn a_lookup_reads_the_footer_one_dictionary_page_and_one_postings_range() {
    let (store, reads) = counting();
    let rows = big(4_000);
    let sidecar = put(&store, "p/d/seg-3.fts", &rows).await;
    let index = Fts::new(store);
    let seg = meta("p/d/seg-3.fts");
    let dict_bytes = sidecar.dict.len() as u64;
    assert!(dict_bytes > 100_000, "dictionary {dict_bytes} B");

    let mut by_word = HashMap::new();
    for (i, w) in ["w123", "w9001", "w19876"].into_iter().enumerate() {
        reads.lock().unwrap().requests.clear();
        let got = index.lookup(&seg, &[w.to_string()], TextField::Inputs).await.unwrap();
        assert_eq!(got, naive(&rows, &[w], TextField::Inputs));
        let r = reads.lock().unwrap().clone();
        let on = |suffix: &str| -> Vec<u64> {
            r.requests
                .iter()
                .filter(|(p, _)| p.ends_with(suffix))
                .map(|(_, n)| *n)
                .collect()
        };
        let (mut dict, post) = (on(".fts.dict.parquet"), on(".fts.post"));
        if i == 0 {
            assert_eq!(dict.len(), 2, "{w}: footer, then the page: {r:?}");
            assert!(dict.remove(0) <= 64 * 1024, "{w}: footer read");
        }
        assert_eq!(dict.len(), 1, "{w}: the footer is read once, then kept: {r:?}");
        assert!(dict[0] <= 5 * 8 * 1024, "{w}: one page per column, {} B", dict[0]);
        assert_eq!(post.len(), 1, "{w}: one postings range: {r:?}");
        by_word.insert(w, dict[0]);
    }
    eprintln!("dictionary {dict_bytes} B; page reads {by_word:?}");
}

#[tokio::test]
async fn a_word_past_every_zone_reads_only_the_footer() {
    let (store, reads) = counting();
    let rows = big(500);
    put(&store, "p/d/seg-4.fts", &rows).await;
    let index = Fts::new(store);
    reads.lock().unwrap().requests.clear();
    let got = index
        .lookup(&meta("p/d/seg-4.fts"), &words(&["zzzz"]), TextField::Inputs)
        .await
        .unwrap();
    assert!(got.is_empty());
    assert_eq!(reads.lock().unwrap().requests.len(), 1, "after the last zone");
}

#[tokio::test]
async fn long_words_are_indexed_by_their_first_64_bytes() {
    let (store, _) = counting();
    let long = format!("{}tail", "é".repeat(40));
    let other = format!("{}other", "é".repeat(40));
    let rows = vec![
        Row {
            inputs: Some(json!({ "blob": long })),
            outputs: None,
            error: None,
        },
        Row {
            inputs: Some(json!({ "blob": "short" })),
            outputs: None,
            error: None,
        },
    ];
    let sidecar = put(&store, "p/d/seg-5.fts", &rows).await;
    assert!(
        dictionary(&sidecar.dict)
            .iter()
            .all(|r| r.0.len() <= fts::MAX_TERM_BYTES)
    );
    let index = Fts::new(store);
    let seg = meta("p/d/seg-5.fts");
    assert_eq!(index.lookup(&seg, &[long], TextField::Inputs).await.unwrap(), vec![0]);
    // Same prefix: a candidate (the superset rule), for the query's text check to reject.
    assert_eq!(index.lookup(&seg, &[other], TextField::Inputs).await.unwrap(), vec![0]);
}

#[tokio::test]
async fn a_segment_with_no_text_has_an_empty_sidecar() {
    let (store, _) = counting();
    let rows = vec![
        Row {
            inputs: None,
            outputs: Some(json!({"k": null, "a": []})),
            error: None,
        };
        3
    ];
    let sidecar = put(&store, "p/d/seg-6.fts", &rows).await;
    assert!(dictionary(&sidecar.dict).is_empty());
    let got = Fts::new(store)
        .lookup(&meta("p/d/seg-6.fts"), &words(&["k"]), TextField::Outputs)
        .await
        .unwrap();
    assert!(got.is_empty());
}

#[tokio::test]
async fn a_lookup_without_a_sidecar_is_an_error() {
    let (store, _) = counting();
    let mut seg = meta("p/d/seg-7.fts");
    seg.fts_path = None;
    let r = Fts::new(store.clone())
        .lookup(&seg, &words(&["x"]), TextField::Inputs)
        .await;
    assert!(matches!(r, Err(StorageError::NotIndexed(_))), "{r:?}");
    assert_eq!(
        r.err().as_ref().and_then(segment::damage),
        None,
        "not damage: the caller scans"
    );
    let missing = Fts::new(store)
        .lookup(&meta("p/d/seg-7.fts"), &words(&["x"]), TextField::Inputs)
        .await;
    assert_eq!(missing.err().as_ref().and_then(segment::damage), Some("missing"));
}

/// The flush path: `segment::write` builds the sidecar from the rows in payload order, so a hit's row number
/// is the version's `payload_row`.
#[tokio::test]
async fn segment_files_carry_a_sidecar_whose_rows_are_payload_rows() {
    use smithling_model::attrs::promote;
    use smithling_model::{Op, RunEvent, RunFields, Source, Ts, merge};
    let versions: Vec<segment::Version> = (0..50u128)
        .map(|n| {
            let id = uuid::Uuid::from_u128(0x4000_8000_0000_0000_0000 | (n * 7919 % 50));
            let ev = RunEvent {
                op: Op::Post,
                event_seq: n as u64 + 1,
                received_at: Ts(1_000),
                source: Source::Native,
                body_hash: 0,
                fields: RunFields {
                    id: Some(id),
                    trace_id: Some(id),
                    dotted_order: Some(format!("20261105T020000000000Z{id}")),
                    project: Some("p".into()),
                    name: Some("r".into()),
                    start_time: Some(Ts(1_000)),
                    inputs: Some(json!({"q": format!("needle{n}")})),
                    error: (n % 2 == 0).then(|| format!("Failed needle{n}")),
                    ..Default::default()
                },
            };
            let mut slot = None;
            merge(&mut slot, &ev).unwrap();
            let state = slot.unwrap();
            segment::Version {
                attrs: promote(&state.run),
                state,
                needs_merge: false,
            }
        })
        .collect();
    let w = segment::write("p", versions, Ts::parse("2026-11-05T02:00:00Z").unwrap()).unwrap();
    let stem = w.meta.fts_path.clone().unwrap();
    assert_eq!(stem, w.meta.core_path.replace(".core.parquet", ".fts"));
    assert_eq!(w.meta.fts_bytes, w.fts.bytes());
    let (store, _) = counting();
    store.put_new(&fts::dict_path(&stem), w.fts.dict.clone()).await.unwrap();
    store.put_new(&fts::post_path(&stem), w.fts.post.clone()).await.unwrap();

    let core: Vec<_> = ParquetRecordBatchReaderBuilder::try_new(w.core.clone())
        .unwrap()
        .build()
        .unwrap()
        .flat_map(|b| segment::decode_core(&b.unwrap()).unwrap())
        .collect();
    assert_eq!(core.len(), 50);
    let index = Fts::new(store);
    for n in [0u128, 7, 31, 49] {
        let id = uuid::Uuid::from_u128(0x4000_8000_0000_0000_0000 | (n * 7919 % 50));
        let want = core.iter().find(|r| r.state.run.run_id == id).unwrap().payload_row;
        let got = index
            .lookup(&w.meta, &[format!("needle{n}")], TextField::Inputs)
            .await
            .unwrap();
        assert_eq!(got, vec![want], "needle{n}");
        let err = index
            .lookup(&w.meta, &[format!("needle{n}")], TextField::Error)
            .await
            .unwrap();
        assert_eq!(err, if n % 2 == 0 { vec![want] } else { vec![] }, "error of needle{n}");
    }
}
