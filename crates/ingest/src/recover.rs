//! Start-up recovery (design §4.6): read the catalog, delete orphan files, rebuild the multi-version set,
//! replay the WAL past `flushed_seq`, and rebuild the dedup window. An M1 folder (only `wal/`) gets a fresh
//! catalog with `flushed_seq = 0`, so its whole log is replayed.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use tracedb_model::{Op, RunId, Ts};
use tracedb_storage::{SegmentMeta, StorageError};

use crate::dedup::Dedup;
use crate::memtable::Version;
use crate::store::RunStore;
use crate::wal::{ReplayStats, Wal};
use crate::{IngestConfig, OpenError, OpenStats, Storage};

pub(crate) struct Recovered {
    pub wal: Wal,
    pub wal_stats: ReplayStats,
    pub active: RunStore,
    pub dedup: Dedup,
    pub mvset: HashSet<RunId>,
    pub version: Version,
    /// Registered segments whose files are missing or unreadable; left out of `version`.
    pub corrupt: Vec<String>,
    pub stats: OpenStats,
}

pub(crate) fn recover(cfg: &IngestConfig, storage: &Storage) -> Result<Recovered, OpenError> {
    let started = Instant::now();
    let (store, meta) = (storage.store.clone(), storage.meta.clone());
    let (flushed_seq, segments, field_stats, objects) = storage.block(async move {
        Ok::<_, StorageError>((
            meta.flushed_seq().await?,
            meta.active_segments().await?,
            meta.field_stats().await?,
            store.list().await?,
        ))
    })?;

    // Files written by a flush that crashed before its catalog commit were never registered.
    let present: HashSet<&str> = objects.iter().map(String::as_str).collect();
    let registered: HashSet<&str> = segments
        .iter()
        .flat_map(|s| [s.core_path.as_str(), s.payload_path.as_str()])
        .collect();
    let orphans: Vec<String> = objects
        .iter()
        .filter(|p| (p.ends_with(".parquet") || p.contains(".parquet.tmp-")) && !registered.contains(p.as_str()))
        .cloned()
        .collect();
    // A crashed flush's events are still in the WAL (it is trimmed only after the commit), so its files are safe
    // to delete. If the WAL doesn't reach back to `flushed_seq`, the catalog doesn't match these files (lost or
    // wrong `--metastore`): deleting them could lose data, so refuse instead.
    let wal_covers = crate::wal::first_seq(&cfg.data_dir.join("wal"))
        .map_err(crate::wal::WalError::from)?
        .is_some_and(|f| f <= flushed_seq + 1);
    if !orphans.is_empty() && !wal_covers {
        return Err(StorageError::Config(format!(
            "{} data files are not in the catalog and the WAL doesn't cover them (e.g. {}); is --metastore the \
             catalog for this --storage? Not deleting them",
            orphans.len(),
            orphans[0]
        ))
        .into());
    }
    let orphans_removed = orphans.len();
    if !orphans.is_empty() {
        let store = storage.store.clone();
        storage.block(async move {
            for o in orphans {
                tracing::warn!(file = %o, "removing an unregistered data file (left by a crash mid-flush)");
                store.delete(&o).await?;
            }
            Ok::<_, StorageError>(())
        })?;
    }

    let mut corrupt = Vec::new();
    let mut readable: Vec<SegmentMeta> = Vec::with_capacity(segments.len());
    let mut mvset: HashSet<RunId> = HashSet::new();
    for s in segments {
        if !present.contains(s.core_path.as_str()) || !present.contains(s.payload_path.as_str()) {
            tracing::error!(segment = %s.id, "a registered data file is missing; its runs are not served");
            corrupt.push(s.id);
            continue;
        }
        if s.needs_merge_rows > 0 {
            let (store, path) = (storage.store.clone(), s.core_path.clone());
            let bytes = storage.block(async move { store.get(&path).await })?;
            match tracedb_storage::segment::needs_merge_ids(bytes) {
                Ok(ids) => mvset.extend(ids),
                Err(e @ (StorageError::Parquet(_) | StorageError::Arrow(_) | StorageError::Format(_))) => {
                    tracing::error!(segment = %s.id, error = %e, "a data file is unreadable; its runs are not served");
                    corrupt.push(s.id);
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
        readable.push(s);
    }
    let corrupt_ids: HashSet<&str> = corrupt.iter().map(String::as_str).collect();
    let field_stats: Vec<_> = field_stats
        .into_iter()
        .filter(|f| !corrupt_ids.contains(f.segment_id.as_str()))
        .map(Arc::new)
        .collect();

    let mut active = RunStore::new();
    let mut dedup = Dedup::new(cfg.limits.dedup_window_us);
    let now = Ts::now();
    let (mut replay_rejected, mut skipped) = (0, 0);
    // Runs with a version already flushed (still in the WAL for the dedup window): a later start for one of
    // them is a second version too.
    let mut flushed_ids: HashSet<RunId> = HashSet::new();
    let (wal, wal_stats) = Wal::open(&cfg.data_dir.join("wal"), cfg.wal_roll_bytes, |ev| {
        if let Some(id) = ev.fields.id
            && dedup.in_window(ev.received_at, now)
        {
            dedup.insert((id, ev.op, ev.body_hash), ev.received_at);
        }
        if ev.event_seq <= flushed_seq {
            skipped += 1;
            if let Some(id) = ev.fields.id {
                flushed_ids.insert(id);
            }
            return;
        }
        match active.apply(&ev) {
            Ok(c) if c.created && (ev.op == Op::Patch || flushed_ids.contains(&c.header.run_id)) => {
                mvset.insert(c.header.run_id);
            }
            Ok(_) => {}
            Err(_) => replay_rejected += 1,
        }
    })?;

    let stats = OpenStats {
        replayed_events: wal_stats.records - skipped,
        skipped_flushed: skipped,
        replay_rejected,
        wal_files: wal_stats.files,
        truncated_bytes: wal_stats.truncated_bytes,
        runs: active.len(),
        segments: readable.len(),
        orphans_removed,
        multi_version_runs: mvset.len(),
        flushed_seq,
        replay_ms: started.elapsed().as_millis() as u64,
    };
    let version = Version {
        id: 0,
        segments: readable.into_iter().map(Arc::new).collect(),
        field_stats,
        flushed_seq,
    };
    Ok(Recovered {
        wal,
        wal_stats,
        active,
        dedup,
        mvset,
        version,
        corrupt,
        stats,
    })
}
