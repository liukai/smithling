//! What readers see of ingest, behind one lock: the active memtable, the frozen one being flushed, and the
//! registered segments. Taking all three under one read lock means a query never sees a half-done freeze
//! or flush (design §4.3: a pinned `Version`).

use std::sync::Arc;

use smithling_storage::{FieldStat, SegmentMeta};

use crate::store::RunStore;

#[derive(Default)]
pub struct Mem {
    pub active: RunStore,
    pub frozen: Option<Arc<RunStore>>,
    pub version: Arc<Version>,
}

/// The immutable part of the read state: files and their field stats. Readers clone the `Arc` and keep
/// using it for the whole query, so a later flush (or, in M4, compaction) can't change it under them
/// (R-7: no file a pinned version references is deleted while it's held).
#[derive(Clone, Debug, Default)]
pub struct Version {
    pub id: u64,
    pub segments: Vec<Arc<SegmentMeta>>,
    pub field_stats: Vec<Arc<FieldStat>>,
    /// Every event with `seq ≤ flushed_seq` is in `segments`.
    pub flushed_seq: u64,
    /// Shared by every version since the last compaction swap: while any is held (a query's pin), the
    /// multi-version set keeps the runs that swap merged (M4).
    pub era: Era,
}

/// A token whose count says whether any version of an era is still alive.
pub type Era = Arc<()>;

impl Version {
    pub fn with_flush(&self, segments: Vec<SegmentMeta>, stats: Vec<FieldStat>, flushed_seq: u64) -> Version {
        let mut v = self.clone();
        v.id += 1;
        v.segments.extend(segments.into_iter().map(Arc::new));
        v.field_stats.extend(stats.into_iter().map(Arc::new));
        v.flushed_seq = flushed_seq;
        v
    }

    /// The same files, segment `id` with a text index.
    pub fn with_fts(&self, id: &str, fts_path: &str, fts_bytes: u64) -> Version {
        let mut v = self.clone();
        v.id += 1;
        for s in v.segments.iter_mut().filter(|s| s.id == id) {
            let mut meta = (**s).clone();
            meta.fts_path = Some(fts_path.to_string());
            meta.fts_bytes = fts_bytes;
            *s = Arc::new(meta);
        }
        v
    }

    /// A compaction's swap: `removed` files out (and their stats), its outputs in.
    pub fn with_compaction(
        &self,
        removed: &std::collections::HashSet<String>,
        added: Vec<SegmentMeta>,
        stats: Vec<FieldStat>,
    ) -> Version {
        let mut v = self.clone();
        v.id += 1;
        v.era = Era::default();
        v.segments.retain(|s| !removed.contains(&s.id));
        v.field_stats.retain(|f| !removed.contains(&f.segment_id));
        v.segments.extend(added.into_iter().map(Arc::new));
        v.field_stats.extend(stats.into_iter().map(Arc::new));
        v
    }

    /// The same files without segment `id` (found damaged).
    pub fn without(&self, id: &str) -> Version {
        let mut v = self.clone();
        v.id += 1;
        v.segments.retain(|s| s.id != id);
        v.field_stats.retain(|f| f.segment_id != id);
        v
    }
}

/// A consistent copy of what to read: the active memtable is read under the caller's lock; frozen and
/// the version are shared.
pub struct Pinned {
    pub frozen: Option<Arc<RunStore>>,
    pub version: Arc<Version>,
}
