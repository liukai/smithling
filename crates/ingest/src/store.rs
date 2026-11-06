//! The run store: every run, merged, in memory, plus the indexes the M0 reads need.
//! Mutated only by the writer; never persisted (it is rebuilt from the WAL on start).

use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use tracedb_model::{
    ChangeKind, Reason, Reject, RunEvent, RunHeader, RunId, RunState, RunView, Status, TraceId, TraceView, Ts, merge,
};

/// Rough per-run cost of the header and indexes, added to payload bytes for the memory budget.
pub const HEADER_BYTES: u64 = 512;

/// Position in the root list: newest `start_time` first, then `run_id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RootKey {
    pub start_time: Ts,
    pub run_id: RunId,
}

type IndexKey = (Reverse<Ts>, RunId);

impl From<IndexKey> for RootKey {
    fn from((Reverse(start_time), run_id): IndexKey) -> RootKey {
        RootKey { start_time, run_id }
    }
}

impl From<RootKey> for IndexKey {
    fn from(k: RootKey) -> IndexKey {
        (Reverse(k.start_time), k.run_id)
    }
}

/// One applied change, as published to the tail hub.
#[derive(Clone, Debug)]
pub struct Change {
    pub kind: ChangeKind,
    pub header: Arc<RunHeader>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Include {
    pub inputs: bool,
    pub outputs: bool,
    pub events: bool,
}

#[derive(Default)]
pub struct RunStore {
    runs: HashMap<RunId, RunState>,
    by_trace: HashMap<TraceId, Vec<RunId>>,
    roots: HashMap<String, BTreeSet<IndexKey>>,
    children: HashMap<RunId, u32>,
    bytes: u64,
    last_seq: u64,
}

impl RunStore {
    pub fn new() -> RunStore {
        RunStore::default()
    }

    pub fn apply(&mut self, ev: &RunEvent) -> Result<Change, Reject> {
        let id = ev.fields.id.ok_or_else(|| Reject::new(Reason::MissingField, "id"))?;
        let mut slot = self.runs.remove(&id);
        let before = slot
            .as_ref()
            .map(|s| (root_key(s), s.run.project.clone(), approx_bytes(s)));
        let kind = match merge(&mut slot, ev) {
            Ok(kind) => kind,
            Err(e) => {
                if let Some(s) = slot {
                    self.runs.insert(id, s);
                }
                return Err(e);
            }
        };
        let st = slot.expect("merge fills the slot on success");

        match before {
            None => {
                self.by_trace.entry(st.run.trace_id).or_default().push(id);
                if let Some(parent) = st.run.parent_run_id {
                    *self.children.entry(parent).or_default() += 1;
                }
            }
            Some((old_key, old_project, old_bytes)) => {
                if let Some(k) = old_key
                    && let Some(set) = self.roots.get_mut(&old_project)
                {
                    set.remove(&k);
                }
                self.bytes -= old_bytes;
            }
        }
        if let Some(k) = root_key(&st) {
            self.roots.entry(st.run.project.clone()).or_default().insert(k);
        }
        self.bytes += approx_bytes(&st);
        self.last_seq = self.last_seq.max(ev.event_seq);

        let header = Arc::new(RunHeader::from_state(&st, self.child_count(id)));
        self.runs.insert(id, st);
        Ok(Change { kind, header })
    }

    pub fn contains(&self, id: &RunId) -> bool {
        self.runs.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.runs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    fn child_count(&self, id: RunId) -> u32 {
        self.children.get(&id).copied().unwrap_or(0)
    }

    fn header(&self, st: &RunState) -> RunHeader {
        RunHeader::from_state(st, self.child_count(st.run.run_id))
    }

    pub fn run(&self, id: RunId, include: Include) -> Option<RunView> {
        let st = self.runs.get(&id)?;
        Some(RunView {
            header: self.header(st),
            inputs: include.inputs.then(|| st.run.inputs.clone()).flatten(),
            outputs: include.outputs.then(|| st.run.outputs.clone()).flatten(),
            events: include.events.then(|| st.run.events.clone()).flatten(),
        })
    }

    pub fn trace(&self, trace_id: TraceId) -> Option<TraceView> {
        let ids = self.by_trace.get(&trace_id)?;
        let mut runs: Vec<RunHeader> = ids
            .iter()
            .filter_map(|id| self.runs.get(id))
            .map(|s| self.header(s))
            .collect();
        runs.sort_by(|a, b| a.dotted_order.cmp(&b.dotted_order));
        Some(TraceView {
            root: runs.iter().find(|r| r.is_root).cloned(),
            run_count: runs.len(),
            has_running: runs.iter().any(|r| r.status == Status::Running),
            runs,
        })
    }

    /// Root runs newest first, strictly after `after`. Returns the page and the key to continue from, if any.
    pub fn roots(
        &self,
        project: Option<&str>,
        status: Option<Status>,
        after: Option<RootKey>,
        limit: usize,
    ) -> (Vec<RunHeader>, Option<RootKey>) {
        let sets: Vec<&BTreeSet<IndexKey>> = match project {
            Some(p) => self.roots.get(p).into_iter().collect(),
            None => self.roots.values().collect(),
        };
        let start = after.map(IndexKey::from);
        let mut keys: Vec<IndexKey> = Vec::new();
        for set in sets {
            let iter: Box<dyn Iterator<Item = &IndexKey>> = match &start {
                Some(s) => Box::new(set.range((std::ops::Bound::Excluded(*s), std::ops::Bound::Unbounded))),
                None => Box::new(set.iter()),
            };
            keys.extend(
                iter.filter(|(_, id)| {
                    status.is_none_or(|want| self.runs.get(id).is_some_and(|s| s.run.status == want))
                })
                .take(limit + 1)
                .copied(),
            );
        }
        keys.sort();
        let more = keys.len() > limit;
        keys.truncate(limit);
        let next = if more {
            keys.last().copied().map(RootKey::from)
        } else {
            None
        };
        let page = keys
            .iter()
            .filter_map(|(_, id)| self.runs.get(id))
            .map(|s| self.header(s))
            .collect();
        (page, next)
    }

    /// Projects with their number of listed root runs.
    pub fn projects(&self) -> Vec<(String, usize)> {
        let mut v: Vec<_> = self.roots.iter().map(|(p, s)| (p.clone(), s.len())).collect();
        v.sort();
        v
    }
}

fn root_key(st: &RunState) -> Option<IndexKey> {
    if !st.is_listed_root() {
        return None;
    }
    st.run.start_time.map(|t| (Reverse(t), st.run.run_id))
}

fn approx_bytes(st: &RunState) -> u64 {
    HEADER_BYTES + st.payload_bytes()
}
