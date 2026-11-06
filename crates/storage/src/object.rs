//! Write-once object storage for data files: local disk or S3-compatible, chosen by URL (constitution V).

use std::path::Path as FsPath;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload};
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
        })
    }

    fn full(&self, rel: &str) -> Path {
        rel.split('/')
            .filter(|p| !p.is_empty())
            .fold(self.prefix.clone(), |p, seg| p.join(seg))
    }

    /// Writes a new object; fails if the name is already taken (names are never reused).
    pub async fn put_new(&self, rel: &str, bytes: Bytes) -> Result<(), StorageError> {
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
