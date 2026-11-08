//! Live views (design §4.5): the same query, re-run over the traces that changed.
//!
//! Subscribe first, then `Hello`. The first change after a quiet spell is answered at once, later ones at
//! most every 100 ms; changes keep arriving while a query runs. Runs that changed, or were sent and whose
//! trace changed, and no longer match are sent as `Left`. Too many changed traces, falling behind the
//! writer, or a failed query (retried with backoff) sends `Resync`; the writer never waits.

use std::collections::HashSet;
use std::pin::Pin;
use std::time::Duration;

use indexmap::IndexMap;
use smithling_ingest::Change;
use smithling_model::{RunId, TraceId};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_stream::wrappers::ReceiverStream;

use crate::{Page, Query, QueryError, QuerySpec, RunRow};

pub const TICK: Duration = Duration::from_millis(100);
/// Most changed traces (and runs the tab may have loaded) per tick, and most sent run ids remembered.
pub const MAX_TRACKED: usize = 5_000;
const MAX_BACKOFF: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq)]
pub enum TailMsg {
    Hello {
        server_seq: u64,
    },
    /// Runs that match now, newest version each.
    Runs(Vec<RunRow>),
    /// Runs that may be on the tab but no longer match (ids it doesn't show can be ignored).
    Left(Vec<RunId>),
    Resync,
}

/// What a tab was sent: run id → (trace, version), least recently sent first.
#[derive(Default)]
pub struct Sent(IndexMap<RunId, (TraceId, u64)>);

impl Sent {
    /// Forgets and returns the runs that may be on the tab but no longer match: those sent whose trace
    /// changed, and `runs` (changed, and possibly loaded by the tab itself), minus `now`.
    pub fn left(&mut self, traces: &HashSet<TraceId>, runs: &HashSet<RunId>, now: &HashSet<RunId>) -> Vec<RunId> {
        let mut left: HashSet<RunId> = runs.difference(now).copied().collect();
        left.extend(
            self.0
                .iter()
                .filter(|(id, (t, _))| traces.contains(t) && !now.contains(*id))
                .map(|(id, _)| *id),
        );
        self.0.retain(|id, _| !left.contains(id));
        left.into_iter().collect()
    }

    /// Records a matching run; true if the tab hasn't seen this version.
    pub fn record(&mut self, run: RunId, trace: TraceId, seq: u64) -> bool {
        let old = self.0.shift_remove(&run);
        self.0.insert(run, (trace, seq));
        old != Some((trace, seq))
    }

    /// Keeps the [`MAX_TRACKED`] most recently sent ids.
    pub fn trim(&mut self) {
        let over = self.0.len().saturating_sub(MAX_TRACKED);
        self.0.drain(..over);
    }

    /// Runs at or before `since` are not sent: the tab loaded them after `Hello`.
    fn update(&mut self, traces: &HashSet<TraceId>, runs: &HashSet<RunId>, now: Vec<RunRow>, since: u64) -> Tick {
        let left = self.left(traces, runs, &now.iter().map(|r| r.header.run_id).collect());
        let runs = now
            .into_iter()
            .filter(|r| r.header.last_seq > since && self.record(r.header.run_id, r.header.trace_id, r.header.last_seq))
            .collect();
        self.trim();
        Tick { runs, left }
    }
}

struct Tick {
    runs: Vec<RunRow>,
    left: Vec<RunId>,
}

/// The traces and runs a re-query covers, and its result.
type Requery = (HashSet<TraceId>, HashSet<RunId>, Result<Page, QueryError>);
type Running = Pin<Box<dyn Future<Output = Requery> + Send>>;

enum Step {
    Change(Result<Change, RecvError>),
    Done(Box<Requery>),
    Due,
    Closed,
}

impl Query {
    /// Keeps `q` live. The stream ends when the ingest side closes; dropping it stops the work.
    pub fn tail(&self, mut q: QuerySpec) -> ReceiverStream<TailMsg> {
        let mut rx = self.ingest.subscribe();
        let server_seq = self.last_seq();
        let only = q.traces.take();
        // Without trace or tree conditions an unchanged run matches now only if it did when the tab loaded
        // (filters relative to now aside).
        let tree = q.trace_filter.is_some() || q.tree_filter.is_some();
        let mut loaded = server_seq;
        let mut since = if tree { 0 } else { loaded };
        q.cursor = None;
        let (tx, out) = mpsc::channel(16);
        let this = self.clone();
        tokio::spawn(async move {
            if tx.send(TailMsg::Hello { server_seq }).await.is_err() {
                return;
            }
            let (mut traces, mut runs, mut sent) = (HashSet::new(), HashSet::new(), Sent::default());
            let mut running: Option<Running> = None;
            let (mut next_run, mut backoff) = (Instant::now(), TICK);
            loop {
                if running.is_none() && !traces.is_empty() && Instant::now() >= next_run {
                    next_run = Instant::now() + TICK;
                    let (t, r) = (std::mem::take(&mut traces), std::mem::take(&mut runs));
                    let (this, mut q) = (this.clone(), q.clone());
                    q.traces = Some(t.clone());
                    running = Some(Box::pin(async move {
                        let res = this.select(&q, usize::MAX, true).await;
                        (t, r, res)
                    }));
                }
                let step = tokio::select! {
                    msg = rx.recv() => Step::Change(msg),
                    done = async {
                        match running.as_mut() {
                            Some(f) => f.await,
                            None => std::future::pending().await,
                        }
                    } => Step::Done(Box::new(done)),
                    _ = tokio::time::sleep_until(next_run), if running.is_none() && !traces.is_empty() => Step::Due,
                    _ = tx.closed() => Step::Closed,
                };
                let mut msgs = Vec::new();
                match step {
                    Step::Change(Ok(c)) => {
                        let t = c.header.trace_id;
                        if only.as_ref().is_none_or(|o| o.contains(&t)) {
                            traces.insert(t);
                            // A run new since the tab loaded can only be on it if it was sent.
                            if !c.is_new() || c.header.last_seq <= loaded {
                                runs.insert(c.header.run_id);
                            }
                        }
                        if traces.len() > MAX_TRACKED || runs.len() > MAX_TRACKED {
                            msgs.push(TailMsg::Resync);
                        }
                    }
                    Step::Change(Err(RecvError::Lagged(_))) => msgs.push(TailMsg::Resync),
                    Step::Change(Err(RecvError::Closed)) | Step::Closed => return,
                    Step::Due => {}
                    Step::Done(done) => {
                        let (t, r, res) = *done;
                        running = None;
                        match res {
                            Ok(page) => {
                                backoff = TICK;
                                let tick = sent.update(&t, &r, page.runs, since);
                                if !tick.runs.is_empty() {
                                    msgs.push(TailMsg::Runs(tick.runs));
                                }
                                if !tick.left.is_empty() {
                                    msgs.push(TailMsg::Left(tick.left));
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "live view query failed");
                                next_run = Instant::now() + backoff;
                                backoff = (backoff * 2).min(MAX_BACKOFF);
                                msgs.push(TailMsg::Resync);
                            }
                        }
                    }
                }
                if msgs.last() == Some(&TailMsg::Resync) {
                    (traces, runs, running) = (HashSet::new(), HashSet::new(), None);
                    loaded = this.last_seq();
                    if !tree {
                        since = loaded;
                    }
                }
                for m in msgs {
                    if tx.send(m).await.is_err() {
                        return;
                    }
                }
            }
        });
        ReceiverStream::new(out)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use proptest::prelude::*;

    use super::*;

    fn id(n: u8) -> uuid::Uuid {
        uuid::Uuid::from_u128(n as u128 + 1)
    }

    proptest! {
        /// A run is sent iff the tab hasn't seen this version; it leaves iff it changed, or was sent and its
        /// trace changed, and it no longer matches.
        #[test]
        fn sent_tracks_what_the_tab_shows(ticks in prop::collection::vec((
            prop::collection::hash_set(0u8..4, 0..4),
            prop::collection::hash_set(0u8..16, 0..6),
            prop::collection::hash_map(0u8..16, 1u64..4, 0..10),
        ), 1..20)) {
            let (mut sent, mut shown) = (Sent::default(), HashMap::<u8, u64>::new());
            let trace = |r: u8| id(100 + r % 4);
            for (traces, changed, runs) in ticks {
                let in_t = |r: &u8| traces.contains(&(r % 4));
                let changed: HashSet<u8> = changed.into_iter().filter(in_t).collect();
                let now: HashMap<u8, u64> = runs.into_iter().filter(|(r, _)| in_t(r)).collect();
                let mut left = sent.left(
                    &traces.iter().map(|t| id(100 + t)).collect(),
                    &changed.iter().map(|r| id(*r)).collect(),
                    &now.keys().map(|r| id(*r)).collect(),
                );
                left.sort();
                let mut expect: Vec<RunId> = shown.keys().filter(|r| in_t(r)).chain(&changed)
                    .filter(|r| !now.contains_key(r))
                    .map(|r| id(*r)).collect::<HashSet<_>>().into_iter().collect();
                expect.sort();
                prop_assert_eq!(left, expect);
                shown.retain(|r, _| !in_t(r) || now.contains_key(r));
                for (r, s) in &now {
                    prop_assert_eq!(sent.record(id(*r), trace(*r), *s), shown.insert(*r, *s) != Some(*s));
                }
                sent.trim();
            }
        }
    }

    #[test]
    fn remembers_only_the_most_recently_sent_ids() {
        let mut sent = Sent::default();
        let ids: Vec<RunId> = (0..MAX_TRACKED as u128 + 10)
            .map(|i| uuid::Uuid::from_u128(i + 1))
            .collect();
        for r in &ids {
            assert!(sent.record(*r, id(0), 1));
        }
        assert!(sent.record(ids[0], id(0), 2), "a newer version is sent");
        sent.trim();
        assert_eq!(sent.0.len(), MAX_TRACKED);
        assert!(!sent.record(ids[0], id(0), 2), "the recently updated run is kept");
        assert!(sent.record(ids[1], id(0), 1), "the least recently sent was forgotten");
    }
}
