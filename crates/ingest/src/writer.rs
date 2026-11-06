//! The single writer: dedup → number → WAL → fsync → apply → publish → confirm.
//!
//! Runs on its own thread because WAL I/O is blocking. Requests that queue up while one fsync runs are
//! committed together by the next one (group commit), so there is no fixed wait.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tracedb_model::{RunEvent, RunId, Ts};

use crate::budget::Budget;
use crate::dedup::{Dedup, DedupKey};
use crate::hub::Hub;
use crate::store::RunStore;
use crate::wal::Wal;
use crate::{AcceptError, AcceptResult, Incoming, Rejected};

/// Stop gathering more requests into one commit once this many payload bytes are queued.
const GROUP_BYTES: usize = 1024 * 1024;

pub(crate) struct Request {
    pub items: Vec<Incoming>,
    pub bytes: usize,
    pub reply: oneshot::Sender<Result<AcceptResult, AcceptError>>,
    pub _permit: OwnedSemaphorePermit,
}

pub(crate) struct Shared {
    pub store: Arc<RwLock<RunStore>>,
    pub hub: Hub,
    pub budget: Budget,
    pub health: Arc<Mutex<Option<String>>>,
}

pub(crate) fn run(rx: Receiver<Request>, mut wal: Wal, mut dedup: Dedup, mut seq: u64, sh: Shared) {
    while let Ok(first) = rx.recv() {
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
        commit(group, &mut wal, &mut dedup, &mut seq, &sh);
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
    let mut new_in_group: HashSet<RunId> = HashSet::new();
    let mut keys_in_group: HashSet<DedupKey> = HashSet::new();
    let mut outcomes = Vec::with_capacity(group.len());
    let mut write_err = None;

    for req in &group {
        let store = sh.store.read().unwrap();
        let adds_new_run = req.items.iter().any(|it| {
            it.fields
                .id
                .is_some_and(|id| !store.contains(&id) && !new_in_group.contains(&id))
        });
        if adds_new_run && sh.budget.is_full(store.bytes(), store.len() + new_in_group.len()) {
            outcomes.push(Outcome {
                result: Err(AcceptError::Capacity),
                events: vec![],
                keys: vec![],
            });
            continue;
        }
        let mut out = Outcome {
            result: Ok(AcceptResult::default()),
            events: vec![],
            keys: vec![],
        };
        for it in &req.items {
            let id = it.fields.id.expect("validated before queueing");
            let key = (id, it.op, it.body_hash);
            if dedup.contains(&key) || !keys_in_group.insert(key) {
                if let Ok(r) = &mut out.result {
                    r.accepted += 1;
                }
                continue;
            }
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
            if !store.contains(&id) {
                new_in_group.insert(id);
            }
            out.keys.push(key);
            out.events.push(ev);
        }
        drop(store);
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
    {
        let mut store = sh.store.write().unwrap();
        for out in &mut outcomes {
            let Ok(result) = &mut out.result else { continue };
            for ev in &out.events {
                match store.apply(ev) {
                    Ok(change) => {
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
