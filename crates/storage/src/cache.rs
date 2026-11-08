//! A bounded memory cache of data-file bytes (design §5, D-028): Parquet footers with their page indexes,
//! text index dictionary pages and postings chunks, and payload pages. Files are write-once, so an entry never
//! goes stale. Least recently used out first, weighed by bytes; an entry over an eighth of the capacity isn't
//! kept. DataFusion's own metadata cache still serves its scans. A disk tier can sit behind the same calls.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use datafusion::parquet::arrow::arrow_reader::ArrowReaderOptions;
use datafusion::parquet::arrow::async_reader::AsyncFileReader;
use datafusion::parquet::errors::{ParquetError, Result as PqResult};
use datafusion::parquet::file::metadata::{PageIndexPolicy, ParquetMetaData, ParquetMetaDataReader};
use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use moka::policy::EvictionPolicy;

use crate::{StorageError, Store};

pub const DEFAULT_BYTES: u64 = 32 * 1024 * 1024;
/// First read of a footer: enough for the footer and page index of a sidecar dictionary in one request.
const FOOTER_READ: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// A file's parsed footer and page index.
    Footer(Arc<str>),
    /// Bytes `start..end` of a file.
    Range(Arc<str>, u64, u64),
}

impl Key {
    pub fn footer(path: &str) -> Key {
        Key::Footer(path.into())
    }

    pub fn range(path: &str, r: Range<u64>) -> Key {
        Key::Range(path.into(), r.start, r.end)
    }

    fn kind(&self) -> &'static str {
        match self {
            Key::Footer(_) => "footer",
            Key::Range(p, ..) if p.ends_with(".dict.parquet") => "dict_page",
            Key::Range(p, ..) if p.ends_with(".post") => "postings",
            Key::Range(p, ..) if p.ends_with(".payload.parquet") => "payload_page",
            Key::Range(..) => "range",
        }
    }

    fn bytes(&self) -> u64 {
        let (Key::Footer(p) | Key::Range(p, ..)) = self;
        (p.len() + size_of::<Key>()) as u64
    }
}

#[derive(Clone, Debug)]
pub enum Cached {
    Bytes(Bytes),
    Meta(Arc<ParquetMetaData>),
}

impl Cached {
    fn bytes(&self) -> u64 {
        match self {
            Cached::Bytes(b) => b.len() as u64,
            Cached::Meta(m) => m.memory_size() as u64,
        }
    }
}

pub struct Cache {
    inner: moka::sync::Cache<Key, Cached>,
    capacity: u64,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Default for Cache {
    fn default() -> Cache {
        Cache::new(DEFAULT_BYTES)
    }
}

impl Cache {
    pub fn new(capacity: u64) -> Cache {
        let inner = moka::sync::Cache::builder()
            .max_capacity(capacity)
            .weigher(|k: &Key, v: &Cached| (k.bytes() + v.bytes()).min(u32::MAX as u64) as u32)
            .eviction_policy(EvictionPolicy::lru())
            .build();
        Cache {
            inner,
            capacity,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// The cached value, else `load`'s (kept if it fits); true if it was cached.
    pub async fn get_or_load<F, Fut, E>(&self, key: Key, load: F) -> Result<(Cached, bool), E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Cached, E>>,
    {
        if let Some(v) = self.inner.get(&key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok((v, true));
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let v = load().await?;
        if (key.bytes() + v.bytes()) * 8 <= self.capacity {
            self.inner.insert(key, v.clone());
        }
        Ok((v, false))
    }

    /// Bytes held (keys and values).
    pub fn bytes(&self) -> u64 {
        self.inner.run_pending_tasks();
        self.inner.weighted_size()
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Entries by kind: `footer`, `dict_page`, `postings`, `payload_page`, `range`.
    pub fn kinds(&self) -> BTreeMap<&'static str, usize> {
        self.inner.run_pending_tasks();
        let mut out = BTreeMap::new();
        for (k, _) in self.inner.iter() {
            *out.entry(k.kind()).or_default() += 1;
        }
        out
    }

    /// `path`'s footer and page index, usually in one suffix read; true if it was cached.
    pub async fn footer(&self, store: &Store, path: &str) -> Result<(Arc<ParquetMetaData>, bool), StorageError> {
        let (v, hit) = self
            .get_or_load(Key::footer(path), || async {
                let (mut tail, size) = store.get_suffix(path, FOOTER_READ).await?;
                loop {
                    let mut r = ParquetMetaDataReader::new().with_page_index_policy(PageIndexPolicy::Optional);
                    match r.try_parse_sized(&tail, size) {
                        Ok(()) => return Ok(Cached::Meta(Arc::new(r.finish()?))),
                        Err(ParquetError::NeedMoreData(n)) if (n as u64) > tail.len() as u64 => {
                            tail = store.get_suffix(path, n as u64).await?.0;
                        }
                        Err(e) => return Err(StorageError::from(e)),
                    }
                }
            })
            .await?;
        match v {
            Cached::Meta(m) => Ok((m, hit)),
            Cached::Bytes(_) => Err(StorageError::Format(format!("{path}: a footer was cached as bytes"))),
        }
    }

    /// `ranges` of `path`: the cached ones from memory, the others in one request.
    pub async fn ranges(&self, store: &Store, path: &str, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StorageError> {
        let mut out: Vec<Option<Bytes>> = ranges
            .iter()
            .map(|r| match self.inner.get(&Key::range(path, r.clone())) {
                Some(Cached::Bytes(b)) => Some(b),
                _ => None,
            })
            .collect();
        let missing: Vec<Range<u64>> = ranges
            .iter()
            .zip(&out)
            .filter(|(_, got)| got.is_none())
            .map(|(r, _)| r.clone())
            .collect();
        self.hits
            .fetch_add((ranges.len() - missing.len()) as u64, Ordering::Relaxed);
        if !missing.is_empty() {
            self.misses.fetch_add(missing.len() as u64, Ordering::Relaxed);
            let mut read = store.get_ranges(path, &missing).await?.into_iter();
            for (r, slot) in ranges.iter().zip(out.iter_mut()) {
                if slot.is_none() {
                    let b = read
                        .next()
                        .ok_or_else(|| StorageError::Format(format!("{path}: short read")))?;
                    let key = Key::range(path, r.clone());
                    if (key.bytes() + b.len() as u64) * 8 <= self.capacity {
                        self.inner.insert(key, Cached::Bytes(b.clone()));
                    }
                    *slot = Some(b);
                }
            }
        }
        Ok(out.into_iter().map(|b| b.unwrap_or_default()).collect())
    }
}

/// Ranged reads of one Parquet file whose footer is already parsed, through the cache.
pub struct CachedReader {
    pub store: Store,
    pub cache: Arc<Cache>,
    pub path: String,
    pub meta: Arc<ParquetMetaData>,
}

impl AsyncFileReader for CachedReader {
    fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, PqResult<Bytes>> {
        async move {
            let mut r = self.get_byte_ranges(vec![range]).await?;
            Ok(r.pop().unwrap_or_default())
        }
        .boxed()
    }

    fn get_byte_ranges(&mut self, ranges: Vec<Range<u64>>) -> BoxFuture<'_, PqResult<Vec<Bytes>>> {
        async move {
            // The object store's own error, so `segment::parquet_damage` tells a failed read from damage.
            self.cache
                .ranges(&self.store, &self.path, &ranges)
                .await
                .map_err(|e| match e {
                    StorageError::Object(e) => ParquetError::External(Box::new(e)),
                    e => ParquetError::External(Box::new(e)),
                })
        }
        .boxed()
    }

    fn get_metadata<'a>(
        &'a mut self,
        _options: Option<&'a ArrowReaderOptions>,
    ) -> BoxFuture<'a, PqResult<Arc<ParquetMetaData>>> {
        let meta = self.meta.clone();
        async move { Ok(meta) }.boxed()
    }
}
