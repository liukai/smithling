//! Write-once object storage for data files: local disk or S3-compatible, chosen by URL (constitution V).

use std::ops::Range;
use std::path::Path as FsPath;
use std::sync::Arc;

use bytes::Bytes;
use datafusion::parquet::arrow::arrow_reader::ArrowReaderOptions;
use datafusion::parquet::arrow::async_reader::AsyncFileReader;
use datafusion::parquet::errors::{ParquetError, Result as PqResult};
use datafusion::parquet::file::metadata::{ParquetMetaData, ParquetMetaDataReader};
use futures_util::future::BoxFuture;
use futures_util::{FutureExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{GetOptions, GetRange, ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};
use url::Url;

use crate::StorageError;

/// Data files under one root: a local folder or `s3://bucket/prefix`.
#[derive(Clone)]
pub struct Store {
    /// Rooted at `/` (local) or the bucket (S3): the form DataFusion expects for its URLs.
    root: Arc<dyn ObjectStore>,
    prefix: Path,
    /// `file://` or `s3://bucket`, the key DataFusion looks the store up by.
    base: Url,
    /// Set for local stores: puts are written and fsynced here directly (object_store's local backend
    /// doesn't fsync, and a catalog entry must never point at a file a power cut could lose).
    local_dir: Option<std::path::PathBuf>,
}

impl Store {
    /// `file://<abs dir>`, a plain path, or `s3://bucket/prefix` (credentials and endpoint from `AWS_*`).
    pub fn open(spec: &str) -> Result<Store, StorageError> {
        if let Some(rest) = spec.strip_prefix("s3://") {
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            let s3 = AmazonS3Builder::from_env()
                .with_bucket_name(bucket)
                .with_allow_http(true)
                .build()
                .map_err(|e| StorageError::Config(format!("s3 storage: {e}")))?;
            return Ok(Store {
                root: Arc::new(s3),
                prefix: Path::from(prefix.trim_matches('/')),
                base: Url::parse(&format!("s3://{bucket}")).map_err(|e| StorageError::Config(e.to_string()))?,
                local_dir: None,
            });
        }
        let dir = spec.strip_prefix("file://").unwrap_or(spec);
        Store::local(FsPath::new(dir))
    }

    pub fn local(dir: &FsPath) -> Result<Store, StorageError> {
        std::fs::create_dir_all(dir)?;
        let abs = dir.canonicalize()?;
        Ok(Store {
            root: Arc::new(LocalFileSystem::new()),
            prefix: Path::from_absolute_path(&abs).map_err(|e| StorageError::Config(e.to_string()))?,
            base: Url::parse("file://").expect("valid"),
            local_dir: Some(abs),
        })
    }

    /// Any `ObjectStore` under `prefix`, for tests (an in-memory or request-counting store): DataFusion can't
    /// read through it, as [`Store::register`] has no real URL for it.
    pub fn with_object_store(root: Arc<dyn ObjectStore>, prefix: &str) -> Store {
        Store {
            root,
            prefix: Path::from(prefix.trim_matches('/')),
            base: Url::parse("tracedb-object-store://").expect("valid"),
            local_dir: None,
        }
    }

    fn full(&self, rel: &str) -> Path {
        rel.split('/')
            .filter(|p| !p.is_empty())
            .fold(self.prefix.clone(), |p, seg| p.join(seg))
    }

    /// Writes a new object; fails if the name is already taken (names are never reused). Durable when it
    /// returns: on local disk the file and its folder are fsynced.
    pub async fn put_new(&self, rel: &str, bytes: Bytes) -> Result<(), StorageError> {
        if let Some(dir) = &self.local_dir {
            let dir = dir.clone();
            let rel = rel.to_string();
            return tokio::task::spawn_blocking(move || put_local(&dir, &rel, &bytes))
                .await
                .map_err(|e| StorageError::Config(e.to_string()))?;
        }
        let opts = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        match self
            .root
            .put_opts(&self.full(rel), PutPayload::from(bytes.clone()), opts)
            .await
        {
            Ok(_) => Ok(()),
            // Some S3-compatible stores lack conditional puts; names are unique ULIDs, so a plain put is safe.
            Err(object_store::Error::NotImplemented { .. }) => {
                self.root.put(&self.full(rel), PutPayload::from(bytes)).await?;
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn get(&self, rel: &str) -> Result<Bytes, StorageError> {
        Ok(self.root.get(&self.full(rel)).await?.bytes().await?)
    }

    /// The last `n` bytes of `rel` (all of it if shorter), and its size.
    pub async fn get_suffix(&self, rel: &str, n: u64) -> Result<(Bytes, u64), StorageError> {
        let opts = GetOptions {
            range: Some(GetRange::Suffix(n)),
            ..Default::default()
        };
        let r = self.root.get_opts(&self.full(rel), opts).await?;
        let size = r.meta.size;
        Ok((r.bytes().await?, size))
    }

    /// Several ranges of `rel` in one request (the store may merge nearby ones).
    pub async fn get_ranges(&self, rel: &str, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StorageError> {
        Ok(self.root.get_ranges(&self.full(rel), ranges).await?)
    }

    /// Size in bytes; `NotFound` if `rel` doesn't exist.
    pub async fn size(&self, rel: &str) -> Result<u64, StorageError> {
        Ok(self.root.head(&self.full(rel)).await?.size)
    }

    /// A Parquet reader over `rel` that fetches only the byte ranges it reads.
    pub fn parquet(&self, rel: &str, size: u64) -> Ranged {
        Ranged {
            store: self.root.clone(),
            path: self.full(rel),
            size,
        }
    }

    pub async fn delete(&self, rel: &str) -> Result<(), StorageError> {
        match self.root.delete(&self.full(rel)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Every object under the root, as paths relative to it.
    pub async fn list(&self) -> Result<Vec<String>, StorageError> {
        let metas: Vec<_> = self.root.list(Some(&self.prefix)).try_collect().await?;
        let skip = self.prefix.parts().count();
        Ok(metas
            .into_iter()
            .map(|m| {
                m.location
                    .parts()
                    .skip(skip)
                    .map(|p| p.as_ref().to_string())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect())
    }

    /// The URL DataFusion reads `rel` from (after [`Store::register`]).
    pub fn url(&self, rel: &str) -> String {
        format!("{}/{}", self.base.as_str().trim_end_matches('/'), self.full(rel))
    }

    /// Makes the store readable by a DataFusion session.
    pub fn register(&self, ctx: &datafusion::prelude::SessionContext) {
        ctx.register_object_store(&self.base, self.root.clone());
    }

    pub fn describe(&self) -> String {
        format!("{}/{}", self.base.as_str().trim_end_matches('/'), self.prefix)
    }
}

/// Ranged reads of one object, for Parquet's async reader.
pub struct Ranged {
    store: Arc<dyn ObjectStore>,
    path: Path,
    size: u64,
}

impl AsyncFileReader for Ranged {
    fn get_bytes(&mut self, range: Range<u64>) -> BoxFuture<'_, PqResult<Bytes>> {
        async move {
            self.store
                .get_range(&self.path, range)
                .await
                .map_err(|e| ParquetError::External(Box::new(e)))
        }
        .boxed()
    }

    fn get_metadata<'a>(
        &'a mut self,
        options: Option<&'a ArrowReaderOptions>,
    ) -> BoxFuture<'a, PqResult<Arc<ParquetMetaData>>> {
        async move {
            let size = self.size;
            let meta = ParquetMetaDataReader::new()
                .with_arrow_reader_options(options)
                .load_and_finish(self, size)
                .await?;
            Ok(Arc::new(meta))
        }
        .boxed()
    }
}

/// Write to a temp file, fsync it, hard-link it to its final name (fails if taken), fsync the folder.
fn put_local(dir: &FsPath, rel: &str, bytes: &[u8]) -> Result<(), StorageError> {
    use std::io::Write;
    let dest = dir.join(rel);
    let parent = dest
        .parent()
        .ok_or_else(|| StorageError::Config(format!("bad object name {rel}")))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        dest.file_name().unwrap_or_default().to_string_lossy(),
        ulid::Ulid::new()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::hard_link(&tmp, &dest)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&tmp);
    result?;
    // The new name (and any new parent folders) must survive a power cut too.
    let mut d = parent;
    loop {
        std::fs::File::open(d)?.sync_all()?;
        if d == dir {
            break;
        }
        match d.parent() {
            Some(p) if p.starts_with(dir) => d = p,
            _ => break,
        }
    }
    Ok(())
}
