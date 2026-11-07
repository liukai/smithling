//! Start-up recovery (design §4.6): read the catalog, delete orphan files, check every file's footer, rebuild
//! the multi-version set and the flushed-ids filter, replay the WAL past `flushed_seq`, and rebuild the dedup
//! window from the catalog's saved keys plus the WAL.
//! An M1 folder (only `wal/`) gets a fresh catalog with `flushed_seq = 0`, so its whole log is replayed.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use tracedb_model::{Op, RunId, Ts};
use tracedb_storage::{DedupKeys, SegmentMeta, StorageError, fts, segment};

use crate::dedup::{self, Dedup, DedupKey};
use crate::flushed::FlushedIds;
use crate::memtable::Version;
use crate::store::RunStore;
use crate::wal::{ReplayStats, Wal};
use crate::{Damaged, IngestConfig, OpenError, OpenStats, Storage};

pub(crate) struct Recovered {
    pub wal: Wal,
    pub wal_stats: ReplayStats,
    pub active: RunStore,
    pub dedup: Dedup,
    /// Dedup keys of the replayed events, saved by the flush that takes them.
    pub pending: Vec<(DedupKey, Ts)>,
    pub mvset: HashSet<RunId>,
    /// Run ids in the readable files.
    pub flushed: FlushedIds,
    pub version: Version,
    /// Registered segments whose files are missing or unreadable; left out of `version`.
    pub damaged: Vec<Damaged>,
    pub stats: OpenStats,
}

pub(crate) fn recover(cfg: &IngestConfig, storage: &Storage) -> Result<Recovered, OpenError> {
    let started = Instant::now();
    let (store, meta) = (storage.store.clone(), storage.meta.clone());
    let (flushed_seq, segments, field_stats, saved_keys, objects) = storage.block(async move {
        Ok::<_, StorageError>((
            meta.flushed_seq().await?,
            meta.active_segments().await?,
            meta.field_stats().await?,
            meta.dedup_keys().await?,
            store.list().await?,
        ))
    })?;

    // Files written by a flush that crashed before its catalog commit were never registered.
    let present: HashSet<&str> = objects.iter().map(String::as_str).collect();
    let mut registered: HashSet<String> = HashSet::new();
    for s in &segments {
        registered.extend([s.core_path.clone(), s.payload_path.clone()]);
        if let Some(stem) = &s.fts_path {
            registered.extend([fts::dict_path(stem), fts::post_path(stem)]);
        }
    }
    let data_file = |p: &str| {
        [".parquet", ".post"]
            .iter()
            .any(|ext| p.ends_with(ext) || p.contains(&format!("{ext}.tmp-")))
    };
    let orphans: Vec<String> = objects
        .iter()
        .filter(|p| data_file(p) && !registered.contains(p.as_str()))
        .cloned()
        .collect();
    // A text index sidecar holds nothing the data files don't, so an unregistered one (a crash mid-flush or
    // mid-backfill) is always safe to delete.
    let sidecar = |p: &str| p.rsplit('/').next().is_some_and(|name| name.contains(".fts"));
    // A crashed flush's events are still in the WAL (it is trimmed only after the commit), so its files are safe
    // to delete. If the WAL doesn't reach back to `flushed_seq`, the catalog doesn't match these files (lost or
    // wrong `--metastore`): deleting them could lose data, so refuse instead.
    let wal_covers = crate::wal::first_seq(&cfg.data_dir.join("wal"))
        .map_err(crate::wal::WalError::from)?
        .is_some_and(|f| f <= flushed_seq + 1);
    if orphans.iter().any(|p| !sidecar(p)) && !wal_covers {
        return Err(StorageError::Config(format!(
            "{} data files are not in the catalog and the WAL doesn't cover them (e.g. {}); is --metastore the \
             catalog for this --storage? Not deleting them",
            orphans.iter().filter(|p| !sidecar(p)).count(),
            orphans.iter().find(|p| !sidecar(p)).expect("one")
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

    let found_at = Ts::now();
    let mut damaged = Vec::new();
    let mut damage = |s: SegmentMeta, reason: &str| {
        tracing::error!(segment = %s.id, reason, "a registered data file can't be read; its runs are not served");
        damaged.push(Damaged {
            id: s.id.clone(),
            reason: reason.to_string(),
            since: found_at,
            segment: Arc::new(s),
        });
    };
    let (present_segs, missing): (Vec<SegmentMeta>, Vec<SegmentMeta>) = segments
        .into_iter()
        .partition(|s| present.contains(s.core_path.as_str()) && present.contains(s.payload_path.as_str()));
    for s in missing {
        damage(s, "missing");
    }
    let mut flushed = FlushedIds::new(2 * present_segs.iter().map(|s| s.row_count as usize).sum::<usize>());
    let mut readable: Vec<SegmentMeta> = Vec::with_capacity(present_segs.len());
    let mut mvset: HashSet<RunId> = HashSet::new();
    let (store, segs) = (storage.store.clone(), present_segs.clone());
    let inspected = storage.block(async move { segment::inspect_all(store, segs).await });
    for (s, r) in present_segs.into_iter().zip(inspected) {
        match r {
            Ok(ids) => {
                mvset.extend(ids.needs_merge);
                for id in &ids.run_ids {
                    flushed.insert(id);
                }
                readable.push(s);
            }
            Err(e) => match segment::damage(&e) {
                Some(reason) => damage(s, reason),
                None => return Err(e.into()),
            },
        }
    }
    let damaged_ids: HashSet<&str> = damaged.iter().map(|d| d.id.as_str()).collect();
    let field_stats: Vec<_> = field_stats
        .into_iter()
        .filter(|f| !damaged_ids.contains(f.segment_id.as_str()))
        .map(Arc::new)
        .collect();

    let mut active = RunStore::new();
    let mut dedup = Dedup::new(cfg.limits.dedup_window_us);
    let now = Ts::now();
    let (mut replay_rejected, mut skipped) = (0, 0);
    // Keys inside the window, inserted oldest first after replay (pruning pops from the front).
    let mut recent: Vec<(DedupKey, Ts)> = Vec::new();
    let mut latest_saved: Vec<(DedupKey, Ts)> = Vec::new();
    for set in saved_keys {
        let Some(keys) = dedup::unpack(&set.keys) else {
            tracing::warn!(flushed_seq = set.flushed_seq, "unreadable saved dedup keys; skipped");
            continue;
        };
        for &(key, at) in &keys {
            if dedup.in_window(at, now) {
                recent.push((key, at));
            }
        }
        if set.flushed_seq == flushed_seq {
            latest_saved = keys;
        }
    }
    // Flushed records still in the WAL: a crash between the catalog commit and the trim, or a folder from
    // before D-022 (whose keys were never saved).
    let mut flushed_in_wal: Vec<(DedupKey, Ts)> = Vec::new();
    let mut pending: Vec<(DedupKey, Ts)> = Vec::new();
    let wal_dir = cfg.data_dir.join("wal");
    let (wal, wal_stats) = Wal::open(&wal_dir, cfg.wal_roll_bytes, |ev| {
        let key = ev.fields.id.map(|id| (id, ev.op, ev.body_hash));
        if let Some(key) = key
            && dedup.in_window(ev.received_at, now)
        {
            recent.push((key, ev.received_at));
        }
        if ev.event_seq <= flushed_seq {
            skipped += 1;
            if let Some(key) = key {
                flushed_in_wal.push((key, ev.received_at));
            }
            return;
        }
        if let Some(key) = key {
            pending.push((key, ev.received_at));
        }
        match active.apply(&ev) {
            // A new state for a run already in a file is a second version (D-025).
            Ok(c) if c.created && (ev.op == Op::Patch || flushed.contains(&c.header.run_id)) => {
                mvset.insert(c.header.run_id);
            }
            Ok(_) => {}
            Err(_) => replay_rejected += 1,
        }
    })?;
    recent.sort_by_key(|k| k.1);
    for (key, at) in recent {
        dedup.insert(key, at);
    }
    if skipped > 0 {
        // Save their keys with the latest flush's, then the records can go.
        let mut keys = latest_saved;
        let mut have: HashSet<DedupKey> = keys.iter().map(|k| k.0).collect();
        keys.extend(flushed_in_wal.into_iter().filter(|k| have.insert(k.0)));
        let set = DedupKeys {
            flushed_seq,
            newest_at: keys.iter().map(|k| k.1.0).max().unwrap_or(0),
            keys: dedup::pack(&keys),
        };
        let meta = storage.meta.clone();
        storage.block(async move { meta.save_dedup_keys(&set).await })?;
        match crate::wal::trim(&wal_dir, flushed_seq) {
            Ok(n) => tracing::info!(files = n, "trimmed flushed wal files on start"),
            Err(e) => tracing::warn!(error = %e, "wal trim failed; will retry after the next flush"),
        }
    }

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
        pending,
        mvset,
        flushed,
        version,
        damaged,
        stats,
    })
}
