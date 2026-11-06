//! The single writer: dedup → identity check → number → WAL → fsync → apply → publish → confirm.
//!
//! Runs on its own thread because WAL I/O is blocking. Requests that queue up while one fsync runs are
//! committed together by the next one (group commit), so there is no fixed wait.
//!
//! M2 adds two things around each group (design §4.1): when the active memtable is full (or old) it is
//! frozen and handed to the flusher, and when the flusher can't keep up the group is answered "busy".

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tracedb_model::{Op, RunEvent, RunFields, RunId, Ts, check_identity};

use crate::dedup::{Dedup, DedupKey};
use crate::flush::FlushJob;
use crate::hub::Hub;
use crate::memtable::Mem;
use crate::wal::Wal;
use crate::{AcceptError, AcceptResult, Counters, Incoming, Limits, Rejected};

/// Stop gathering more requests into one commit once this many payload bytes are queued.
const GROUP_BYTES: usize = 1024 * 1024;
/// How often an idle writer checks whether the active memtable is old enough to flush.
const TICK: Duration = Duration::from_millis(500);

pub(crate) struct Request {
    pub items: Vec<Incoming>,
    pub bytes: usize,
    pub reply: oneshot::Sender<Result<AcceptResult, AcceptError>>,
    pub _permit: OwnedSemaphorePermit,
}

pub(crate) struct Shared {
    pub mem: Arc<RwLock<Mem>>,
    pub hub: Hub,
    pub health: Arc<Mutex<Option<String>>>,
    pub mvset: Arc<RwLock<HashSet<RunId>>>,
    pub counters: Arc<Counters>,
    pub limits: Limits,
    pub flush_tx: Sender<FlushJob>,
}

pub(crate) fn run(rx: Receiver<Request>, mut wal: Wal, mut dedup: Dedup, mut seq: u64, sh: Shared) {
    let mut since_freeze = 0u64;
    loop {
        let first = match rx.recv_timeout(TICK) {
            Ok(r) => Some(r),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        maybe_freeze(&mut wal, seq, &mut since_freeze, &sh);
        let Some(first) = first else { continue };
        let mut group = vec![first];
        let mut bytes = group[0].bytes;
        while bytes < GROUP_BYTES {
            match rx.try_recv() {
                Ok(r) => {
                    bytes += r.bytes;
                    group.push(r);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        if overloaded(&sh) {
            for r in group {
                let _ = r.reply.send(Err(AcceptError::Busy));
            }
            continue;
        }
        let before = wal.appended_bytes();
        commit(group, &mut wal, &mut dedup, &mut seq, &sh);
        let added = wal.appended_bytes() - before;
        since_freeze += added;
        sh.counters.unflushed_wal.fetch_add(added, Ordering::Relaxed);
        maybe_freeze(&mut wal, seq, &mut since_freeze, &sh);
    }
}

/// Uploads wait ("busy, retry shortly") when memory or the unflushed log is over its limit (R-8).
fn overloaded(sh: &Shared) -> bool {
    let mem = sh.mem.read().unwrap();
    (mem.frozen.is_some() && mem.active.bytes() >= sh.limits.memtable_bytes)
        || sh.counters.unflushed_wal.load(Ordering::Relaxed) >= sh.limits.max_unflushed_wal
}

/// Freezes the active memtable when it is full or old, if no flush is in progress (at most one frozen).
fn maybe_freeze(wal: &mut Wal, seq: u64, since_freeze: &mut u64, sh: &Shared) {
    let now = Ts::now();
    {
        let mem = sh.mem.read().unwrap();
        let full = mem.active.bytes() >= sh.limits.memtable_bytes;
        let old = mem
            .active
            .oldest()
            .is_some_and(|o| now.0 - o.0 >= sh.limits.flush_age_us);
        if mem.frozen.is_some() || mem.active.is_empty() || !(full || old) {
            return;
        }
    }
    // Start a new log file first, so every earlier file holds only events of the memtable being frozen.
    if let Err(e) = wal.roll_at(seq + 1) {
        tracing::error!(error = %e, "wal roll failed; refusing further uploads");
        *sh.health.lock().unwrap() = Some(e.to_string());
        return;
    }
    let frozen = {
        let mut mem = sh.mem.write().unwrap();
        let frozen = Arc::new(std::mem::take(&mut mem.active));
        mem.frozen = Some(frozen.clone());
        frozen
    };
    let job = FlushJob {
        frozen,
        max_seq: seq,
        wal_bytes: std::mem::take(since_freeze),
    };
    if sh.flush_tx.send(job).is_err() {
        tracing::error!("flusher is gone; data stays in the WAL until restart");
    }
}

struct Outcome {
    result: Result<AcceptResult, AcceptError>,
    events: Vec<RunEvent>,
    keys: Vec<DedupKey>,
}

fn commit(group: Vec<Request>, wal: &mut Wal, dedup: &mut Dedup, seq: &mut u64, sh: &Shared) {
    if let Some(err) = sh.health.lock().unwrap().clone() {
        for r in group {
            let _ = r.reply.send(Err(AcceptError::Storage(err.clone())));
        }
        return;
    }

    let now = Ts::now();
    dedup.prune(now);
    let mut keys_in_group: HashSet<DedupKey> = HashSet::new();
    // First fields seen in this group for runs not yet in a memtable, for identity checks within the group.
    let mut idents: HashMap<RunId, RunFields> = HashMap::new();
    let mut outcomes = Vec::with_capacity(group.len());
    let mut write_err = None;

    for req in &group {
        let mem = sh.mem.read().unwrap();
        let mut out = Outcome {
            result: Ok(AcceptResult::default()),
            events: vec![],
            keys: vec![],
        };
        for it in &req.items {
            let id = it.fields.id.expect("validated before queueing");
            let key = (id, it.op, it.body_hash);
            if dedup.contains(&key) || keys_in_group.contains(&key) {
                if let Ok(r) = &mut out.result {
                    r.accepted += 1;
                }
                continue;
            }
            // Identity conflicts are rejected before the WAL, so a rejected item leaves no trace and an
            // identical retry is rejected again (M2-FR-23). Runs only on disk can't be checked here (D-009).
            let known = mem
                .active
                .get(&id)
                .or_else(|| mem.frozen.as_ref().and_then(|f| f.get(&id)));
            let conflict = match known {
                Some(st) => check_identity(st, &probe(it, now)).err(),
                None => idents.get(&id).and_then(|first| ident_conflict(first, &it.fields)),
            };
            if let Some(rej) = conflict {
                if let Ok(r) = &mut out.result {
                    r.rejected.push(Rejected {
                        id: Some(id),
                        reason: rej.reason,
                        detail: rej.detail,
                    });
                }
                continue;
            }
            keys_in_group.insert(key);
            idents.entry(id).or_insert_with(|| it.fields.clone());
            *seq += 1;
            let ev = RunEvent {
                op: it.op,
                event_seq: *seq,
                received_at: now,
                source: it.source,
                body_hash: it.body_hash,
                fields: it.fields.clone(),
            };
            if write_err.is_none()
                && let Err(e) = wal.append(&ev)
            {
                write_err = Some(e.to_string());
            }
            out.keys.push(key);
            out.events.push(ev);
        }
        drop(mem);
        outcomes.push(out);
    }

    if write_err.is_none()
        && let Err(e) = wal.sync()
    {
        write_err = Some(e.to_string());
    }
    if let Some(err) = write_err {
        tracing::error!(error = %err, "wal write failed; refusing further uploads");
        *sh.health.lock().unwrap() = Some(err.clone());
        for r in group {
            let _ = r.reply.send(Err(AcceptError::Storage(err.clone())));
        }
        return;
    }

    let mut changes = Vec::new();
    let mut multi = Vec::new();
    {
        let mut mem = sh.mem.write().unwrap();
        let mem = &mut *mem;
        for out in &mut outcomes {
            let Ok(result) = &mut out.result else { continue };
            for ev in &out.events {
                match mem.active.apply(ev) {
                    Ok(change) => {
                        let id = change.header.run_id;
                        // A new state started by an update, or a run whose earlier part is frozen, has (or
                        // may have) another version elsewhere: reads must merge it (design §4.3).
                        if change.created
                            && (ev.op == Op::Patch || mem.frozen.as_ref().is_some_and(|f| f.contains(&id)))
                        {
                            multi.push(id);
                        }
                        result.accepted += 1;
                        changes.push(change);
                    }
                    Err(rej) => result.rejected.push(Rejected {
                        id: ev.fields.id,
                        reason: rej.reason,
                        detail: rej.detail,
                    }),
                }
            }
        }
    }
    if !multi.is_empty() {
        sh.mvset.write().unwrap().extend(multi);
    }
    for out in &outcomes {
        for k in &out.keys {
            dedup.insert(*k, now);
        }
    }
    for c in changes {
        sh.hub.publish(c);
    }
    for (req, out) in group.into_iter().zip(outcomes) {
        let _ = req.reply.send(out.result);
    }
}

fn probe(it: &Incoming, now: Ts) -> RunEvent {
    RunEvent {
        op: it.op,
        event_seq: 0,
        received_at: now,
        source: it.source,
        body_hash: it.body_hash,
        fields: it.fields.clone(),
    }
}

/// Identity conflict between two items for the same new run in one group.
fn ident_conflict(first: &RunFields, f: &RunFields) -> Option<tracedb_model::Reject> {
    use tracedb_model::{Reason, Reject};
    let differs = |a: &Option<String>, b: &Option<String>| matches!((a, b), (Some(x), Some(y)) if x != y);
    let field = if f.trace_id.is_some() && first.trace_id.is_some() && f.trace_id != first.trace_id {
        "trace_id"
    } else if differs(&first.dotted_order, &f.dotted_order) {
        "dotted_order"
    } else if f.parent_run_id.is_some() && first.parent_run_id.is_some() && f.parent_run_id != first.parent_run_id {
        "parent_run_id"
    } else if differs(&first.address, &f.address) {
        "address"
    } else if differs(&first.project, &f.project) {
        "project"
    } else {
        return None;
    };
    Some(Reject::new(
        Reason::IdentityConflict,
        format!("{field} differs from the stored run"),
    ))
}
