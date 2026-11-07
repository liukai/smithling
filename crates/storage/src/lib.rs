//! Storage: everything that touches files or SQL lives here (design §3, boundary rules).
//!
//! - [`object::Store`]: write-once data files on local disk or S3-compatible storage.
//! - [`metastore::Metastore`]: the catalog of segments and field stats (SQLite or Postgres).
//! - [`segment`]: the Parquet core + payload format and its decoder.
//!
//! Reads go through DataFusion; its Parquet metadata cache plays the role of design §5's `Cache`.

pub mod metastore;
pub mod object;
pub mod segment;

pub use metastore::{DedupKeys, Metastore, redact};
pub use object::Store;
pub use segment::{FieldStat, Payload, SegmentMeta, StoredRow, Version, Written};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("storage configuration: {0}")]
    Config(String),
    #[error("object store: {0}")]
    Object(#[from] object_store::Error),
    #[error("metastore: {0}")]
    Metastore(String),
    #[error("parquet: {0}")]
    Parquet(#[from] datafusion::parquet::errors::ParquetError),
    #[error("arrow: {0}")]
    Arrow(#[from] datafusion::arrow::error::ArrowError),
    #[error("bad data file: {0}")]
    Format(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl StorageError {
    /// A write-once name was already taken.
    pub fn already_exists(&self) -> bool {
        match self {
            StorageError::Io(e) => e.kind() == std::io::ErrorKind::AlreadyExists,
            StorageError::Object(object_store::Error::AlreadyExists { .. }) => true,
            _ => false,
        }
    }
}

impl From<sqlx::Error> for StorageError {
    fn from(e: sqlx::Error) -> StorageError {
        StorageError::Metastore(redact(&e.to_string()))
    }
}
