//! Query service: answers reads from a run-store snapshot. Depends only on the read-only
//! [`Snapshot`] trait, never on ingest internals, so M2 can put DataFusion behind the same API.

pub mod cursor;

use std::sync::Arc;

use serde::Serialize;
use tracedb_ingest::{Include, Snapshot};
use tracedb_model::{RootsPage, RunId, RunView, Status, TraceId, TraceView};
use xxhash_rust::xxh64::xxh64;

pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 200;

#[derive(Clone, Debug, Default)]
pub struct RootsParams {
    pub project: Option<String>,
    pub status: Option<Status>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectInfo {
    pub name: String,
    pub runs: usize,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("cursor is malformed or belongs to a different filter")]
    BadCursor,
}

#[derive(Clone)]
pub struct Query {
    snap: Arc<dyn Snapshot>,
}

impl Query {
    pub fn new(snap: Arc<dyn Snapshot>) -> Query {
        Query { snap }
    }

    /// Root runs newest first; `limit` is clamped to 1..=200.
    pub fn roots(&self, p: &RootsParams) -> Result<RootsPage, QueryError> {
        let filter_hash = filter_hash(p.project.as_deref(), p.status);
        let after = match &p.cursor {
            Some(c) => Some(cursor::decode(c, filter_hash).ok_or(QueryError::BadCursor)?),
            None => None,
        };
        let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let (runs, next) = self.snap.roots(p.project.as_deref(), p.status, after, limit);
        Ok(RootsPage {
            runs,
            next_cursor: next.map(|k| cursor::encode(k, filter_hash)),
        })
    }

    pub fn trace(&self, id: TraceId) -> Option<TraceView> {
        self.snap.trace(id)
    }

    pub fn run(&self, id: RunId, include: Include) -> Option<RunView> {
        self.snap.run(id, include)
    }

    pub fn projects(&self) -> Vec<ProjectInfo> {
        self.snap
            .projects()
            .into_iter()
            .map(|(name, runs)| ProjectInfo { name, runs })
            .collect()
    }

    pub fn last_seq(&self) -> u64 {
        self.snap.last_seq()
    }
}

fn filter_hash(project: Option<&str>, status: Option<Status>) -> u64 {
    let s = format!("{project:?}|{status:?}");
    xxh64(s.as_bytes(), 0)
}
