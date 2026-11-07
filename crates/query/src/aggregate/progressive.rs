//! Progressive answers (design §4.4, research M3-R6): an estimate from files spread evenly over the time range
//! (about a tenth of the rows, or what's read in the budget) plus the same share of dirty runs, counts and
//! sums scaled up; then the exact result, reusing the sample. Dropping the stream cancels the work.
//!
//! The estimate doesn't wait for the merged file rows of multi-version runs (D-023) when they aren't ready
//! (the first query after a start): it goes without them, scaling the rest up to cover them.

use std::pin::Pin as StdPin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::future::try_join_all;
use tokio::sync::mpsc;
use tracedb_model::Ts;
use tracedb_model::agg::AggSpec;
use tracedb_model::cost::Prices;
use tracedb_model::filter::EvalStats;
use tracedb_storage::segment::SegmentMeta;

use super::{AggResult, Pick, Side, Snap, Writeup, query_id, side_specs};
use crate::{Query, QueryError, QuerySpec};

/// Set to stop a computation at its next batch; counts the rows read so far.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<Inner>);

#[derive(Debug, Default)]
struct Inner {
    cancelled: AtomicBool,
    rows: AtomicU64,
}

impl Cancel {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Relaxed)
    }

    /// File rows and dirty runs read so far, counted per batch.
    pub fn rows_read(&self) -> u64 {
        self.0.rows.load(Ordering::Relaxed)
    }

    pub(crate) fn read(&self, n: u64) {
        self.0.rows.fetch_add(n, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub enum AggMsg {
    /// `partial: true`, every cell `estimated`.
    Estimate(AggResult),
    Result(AggResult),
    Error {
        query_id: String,
        error: QueryError,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct StreamOpts {
    /// Below this many rows in scope only the exact result is sent.
    pub min_rows: u64,
    /// Share of the data files the estimate reads.
    pub sample: f64,
    /// The sample starts no new file after this.
    pub budget: Duration,
    /// How long the estimate waits for the merged file rows of multi-version runs.
    pub fill_wait: Duration,
}

impl Default for StreamOpts {
    fn default() -> StreamOpts {
        StreamOpts {
            min_rows: 200_000,
            sample: 0.10,
            budget: Duration::from_millis(300),
            fill_wait: Duration::from_millis(100),
        }
    }
}

static RUNNING: AtomicUsize = AtomicUsize::new(0);
static CANCELLED: AtomicU64 = AtomicU64::new(0);

/// Stream computations running now, and how many have stopped because their stream was dropped.
pub fn stream_counts() -> (usize, u64) {
    (RUNNING.load(Ordering::Relaxed), CANCELLED.load(Ordering::Relaxed))
}

/// Counts a stream computation as running until dropped (also on a panic).
struct Running(Cancel);

impl Running {
    fn start(c: Cancel) -> Running {
        RUNNING.fetch_add(1, Ordering::Relaxed);
        Running(c)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.0.is_cancelled() {
            CANCELLED.fetch_add(1, Ordering::Relaxed);
        }
        RUNNING.fetch_sub(1, Ordering::Relaxed);
    }
}

/// An estimate (when slow), then the result or an error. Dropping it cancels the computation.
pub struct AggStream {
    rx: mpsc::Receiver<AggMsg>,
    cancel: Cancel,
}

impl AggStream {
    pub fn cancel_token(&self) -> Cancel {
        self.cancel.clone()
    }
}

impl futures_util::Stream for AggStream {
    type Item = AggMsg;

    fn poll_next(self: StdPin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<AggMsg>> {
        self.get_mut().rx.poll_recv(cx)
    }
}

impl Drop for AggStream {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// About `share` of `files` (sorted by start time), spread evenly by rows.
pub(crate) fn pick_files<'a>(files: &[&'a SegmentMeta], share: f64) -> Vec<&'a SegmentMeta> {
    if share >= 1.0 || files.is_empty() {
        return files.to_vec();
    }
    let rows = |s: &SegmentMeta| s.row_count.max(1);
    let total: u64 = files.iter().map(|s| rows(s)).sum();
    let m = ((files.len() as f64 * share).round() as usize).clamp(1, files.len());
    let point = |j: usize| ((j as f64 + 0.5) * total as f64 / m as f64) as u64;
    let (mut out, mut end, mut j) = (Vec::new(), 0, 0);
    for f in files {
        end += rows(f);
        if j < m && point(j) < end {
            out.push(*f);
            while j < m && point(j) < end {
                j += 1;
            }
        }
    }
    out
}

fn join_error(e: tokio::task::JoinError) -> QueryError {
    QueryError::Engine(datafusion::error::DataFusionError::Execution(format!(
        "reading multi-version runs: {e}"
    )))
}

impl Query {
    /// `GET /api/aggregate/stream`: an estimate when the scope holds `opts.min_rows` or more, then the exact
    /// result (as [`Query::aggregate`]).
    pub fn aggregate_stream(&self, q: QuerySpec, a: AggSpec, prices: Arc<Prices>, opts: StreamOpts) -> AggStream {
        let (tx, rx) = mpsc::channel(2);
        let cancel = Cancel::default();
        let (this, token) = (self.clone(), cancel.clone());
        let running = Running::start(token.clone());
        tokio::spawn(async move {
            let _running = running;
            let now = Ts::now();
            let id = query_id(&q, now);
            if let Err(error) = this.progressive(&q, &a, &prices, opts, now, &id, &token, &tx).await
                && !token.is_cancelled()
            {
                let _ = tx.send(AggMsg::Error { query_id: id, error }).await;
            }
        });
        AggStream { rx, cancel }
    }

    #[allow(clippy::too_many_arguments)]
    async fn progressive(
        &self,
        q: &QuerySpec,
        a: &AggSpec,
        prices: &Prices,
        opts: StreamOpts,
        now: Ts,
        id: &str,
        cancel: &Cancel,
        tx: &mpsc::Sender<AggMsg>,
    ) -> Result<(), QueryError> {
        let started = Instant::now();
        let specs = self.with_searches(side_specs(q, a, now)).await?;
        let tscope = self.pin_scope(q);
        let cold = self.snap_cold(&tscope);
        let in_scope = self
            .sides(&cold, &specs, a, prices, now, None)
            .iter()
            .map(|s| s.files.iter().map(|f| f.row_count).sum::<u64>())
            .max()
            .unwrap_or(0)
            + cold.pin.pinned.rows.len() as u64;
        if in_scope < opts.min_rows {
            if let Some(r) = self.aggregate_with(q, a, prices, now, id.to_string(), cancel).await? {
                let _ = tx.send(AggMsg::Result(r)).await;
            }
            return Ok(());
        }
        let mut fill = {
            let (this, tscope) = (self.clone(), tscope.clone());
            tokio::spawn(async move { this.snap(&tscope).await })
        };
        let warm: Option<Snap> = match tokio::time::timeout(opts.fill_wait, &mut fill).await {
            Ok(joined) => Some(joined.map_err(join_error)??),
            Err(_) => None,
        };
        let snap = warm.as_ref().unwrap_or(&cold);
        let mut eval_stats = EvalStats::default();
        let traces = self.trace_set(q, snap, now, &mut eval_stats).await?;
        if cancel.is_cancelled() {
            return Ok(());
        }
        let sides = self.sides(snap, &specs, a, prices, now, traces);
        let deadline = Instant::now() + opts.budget;
        let samples = try_join_all(sides.iter().map(|side| async {
            let files = side.files();
            let picked = pick_files(&files, opts.sample);
            let mut p = self
                .clean_pass(snap, side, &picked, a.view, cancel, Some(deadline))
                .await?;
            let read: Vec<&SegmentMeta> = picked.iter().filter(|f| p.done.contains(&f.id)).copied().collect();
            let (total, got) = (Side::clean_rows(&files), Side::clean_rows(&read));
            let share = if total == 0 { 1.0 } else { got as f64 / total as f64 };
            let (pick, rest) = Pick::split(share);
            self.dirty_pass(snap, side, pick, &mut p, cancel);
            let scale = if warm.is_some() {
                (got > 0).then(|| total as f64 / got as f64)
            } else {
                (got > 0).then(|| (total as f64 + dirty_estimate(snap, &files)) / got as f64)
            }
            .or((total == 0).then_some(1.0));
            Ok::<_, QueryError>((p, rest, scale.map(|k| (k, (share * 100.0) as f32))))
        }))
        .await?;
        if cancel.is_cancelled() {
            return Ok(());
        }
        if samples.iter().all(|s| s.2.is_some()) {
            let estimate = self.write_up(
                &sides,
                &samples.iter().map(|s| &s.0).collect::<Vec<_>>(),
                a,
                eval_stats.clone(),
                Writeup {
                    query_id: id.to_string(),
                    started,
                    estimate: samples.iter().map(|s| s.2).collect(),
                },
            );
            if tx.send(AggMsg::Estimate(estimate)).await.is_err() {
                return Ok(());
            }
        }
        let exact = if warm.is_some() {
            let rest = try_join_all(sides.iter().zip(&samples).map(|(side, (p, rest, _))| async {
                let files: Vec<&SegmentMeta> = side.files().into_iter().filter(|f| !p.done.contains(&f.id)).collect();
                self.pass(snap, side, &files, *rest, a.view, cancel, None).await
            }))
            .await?;
            if cancel.is_cancelled() {
                return Ok(());
            }
            let parts: Vec<_> = samples
                .into_iter()
                .zip(rest)
                .map(|((mut p, ..), r)| {
                    p.merge(r);
                    p
                })
                .collect();
            self.write_up(
                &sides,
                &parts.iter().collect::<Vec<_>>(),
                a,
                eval_stats,
                Writeup {
                    query_id: id.to_string(),
                    started,
                    estimate: vec![None; specs.len()],
                },
            )
        } else {
            drop(sides);
            let snap = fill.await.map_err(join_error)??;
            match self
                .exact_over(&snap, q, &specs, a, prices, now, id.to_string(), started, cancel)
                .await?
            {
                Some(r) => r,
                None => return Ok(()),
            }
        };
        let _ = tx.send(AggMsg::Result(exact)).await;
        Ok(())
    }
}

/// Multi-version runs in `files`' share of the set, for an estimate without their merged rows: the set's size
/// times the share of its file rows in scope.
fn dirty_estimate(snap: &Snap, files: &[&SegmentMeta]) -> f64 {
    let all: u64 = snap
        .pin
        .pinned
        .version
        .segments
        .iter()
        .map(|s| s.needs_merge_rows)
        .sum();
    let mine: u64 = files.iter().map(|s| s.needs_merge_rows).sum();
    if all == 0 {
        return 0.0;
    }
    snap.pin.multi.len() as f64 * mine as f64 / all as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(i: usize, rows: u64) -> SegmentMeta {
        SegmentMeta {
            id: format!("f{i:03}"),
            project: String::new(),
            core_path: String::new(),
            payload_path: String::new(),
            min_start_key: i as i64,
            max_start_key: i as i64,
            min_trace_id: uuid::Uuid::nil(),
            max_trace_id: uuid::Uuid::nil(),
            min_seq: 0,
            max_seq: 0,
            row_count: rows,
            needs_merge_rows: 0,
            core_bytes: 0,
            payload_bytes: 0,
            created_at: 0,
            fts_path: None,
            fts_bytes: 0,
        }
    }

    #[test]
    fn files_are_picked_evenly_by_rows() {
        let segs: Vec<SegmentMeta> = (0..40).map(|i| seg(i, 1000)).collect();
        let files: Vec<&SegmentMeta> = segs.iter().collect();
        let ids = |p: Vec<&SegmentMeta>| {
            p.iter()
                .map(|s| s.id[1..].parse::<usize>().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(pick_files(&files, 0.10)), vec![5, 15, 25, 35]);
        assert_eq!(pick_files(&files, 1.0).len(), 40);
        assert_eq!(pick_files(&files, 0.001).len(), 1);
        assert!(pick_files(&[], 0.1).is_empty());
        // One big file holds most rows: it's picked, and the share read follows the rows.
        let mut segs: Vec<SegmentMeta> = (0..10).map(|i| seg(i, 100)).collect();
        segs[3].row_count = 9100;
        let files: Vec<&SegmentMeta> = segs.iter().collect();
        assert_eq!(ids(pick_files(&files, 0.10)), vec![3]);
    }
}
