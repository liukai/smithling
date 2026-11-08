//! Query service: answers every read over memtables ∪ data files (design §4.3–4.4).
//!
//! DataFusion scans the files with pruning and a pushed-down filter that may only keep extra rows; the exact
//! filter is evaluated on merged runs by [`tracedb_model::filter::eval`], the same evaluator used for
//! memtable rows. So results never depend on where a run's pieces are.

pub mod aggregate;
pub mod cursor;
pub mod fields;
pub mod pushdown;
pub mod scan;
mod search;
pub mod tail;
pub mod window;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use datafusion::prelude::{SessionConfig, SessionContext};
use serde::Serialize;
use tracedb_ingest::{FeedbackLookup, Feedbacks, Include, Ingest};
use tracedb_model::filter::{EvalStats, Expr, Field, Path, Row, Side, TimeAt, contains_search, eval_with};
use tracedb_model::text::{Match, Text};
use tracedb_model::{RunHeader, RunId, RunView, Status, TraceId, TraceView, Ts};
use tracedb_storage::StorageError;
use tracedb_storage::fts::Fts;
use tracedb_storage::segment::start_key;
use xxhash_rust::xxh64::xxh64;

pub use aggregate::{AggResult, AggStats, Cells, Group};
pub use cursor::Key;
pub use fields::{FieldInfo, Fields, TypeCount, ValueCount, Values};
pub use scan::{Merged, ScanStats, Scope};
pub use tail::TailMsg;

pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("cursor is malformed or belongs to a different query")]
    BadCursor,
    #[error("query engine: {0}")]
    Engine(#[from] datafusion::error::DataFusionError),
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderBy {
    #[default]
    StartTime,
    Latency,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TimeRange {
    pub from: Option<TimeAt>,
    pub to: Option<TimeAt>,
}

/// One query (data-model §9). Every read in the UI, API, CLI, live views and the demo is one of these.
#[derive(Clone, Debug, Default)]
pub struct QuerySpec {
    pub project: Option<String>,
    pub time: TimeRange,
    /// Conditions on the run itself.
    pub filter: Option<Expr>,
    /// Conditions on the root of its trace.
    pub trace_filter: Option<Expr>,
    /// Conditions on any run in its trace (including itself).
    pub tree_filter: Option<Expr>,
    pub is_root: Option<bool>,
    pub order: OrderBy,
    pub ascending: bool,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    /// Extra values to return per run (paths), for the Table view.
    pub columns: Vec<Path>,
    /// Restrict to these traces (live views re-query only what changed).
    pub traces: Option<HashSet<TraceId>>,
    /// Count every match in `stats.matched`. Otherwise a list by start time stops reading once the page is
    /// full, and `matched` counts only the runs read.
    pub count: bool,
    /// Only these runs: set for an aggregate whose filter searches text, to the runs it matched (`filter` is
    /// then cleared).
    pub runs: Option<std::sync::Arc<HashSet<RunId>>>,
}

impl QuerySpec {
    /// Hash of everything that changes results (not cursor, columns, limit or count).
    pub fn hash(&self) -> u64 {
        let p = |e: &Option<Expr>| e.as_ref().map(tracedb_model::filter::print);
        let s = format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{}",
            self.project,
            self.time.from,
            self.time.to,
            p(&self.filter),
            p(&self.trace_filter),
            p(&self.tree_filter),
            self.traces
                .as_ref()
                .map(|t| t.iter().collect::<std::collections::BTreeSet<_>>()),
            self.is_root,
            self.order,
            self.ascending
        );
        xxh64(s.as_bytes(), 0)
    }
}

/// A run in a result page, plus the requested column values.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunRow {
    #[serde(flatten)]
    pub header: RunHeader,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub columns: BTreeMap<String, serde_json::Value>,
    /// Where the filter's search matched (page rows of a query with a `search` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#match: Option<Match>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QueryWarning {
    MixedTypes {
        path: String,
        counts: BTreeMap<String, u64>,
    },
    NotIndexed {
        path: String,
        runs: u64,
    },
    IdentityConflict {
        run_id: RunId,
    },
    /// Damaged or missing data files in scope that weren't read (M3-FR-18).
    SkippedFiles {
        ids: Vec<String>,
    },
    /// Files in scope a search read whole because they have no text index (M2 files until backfilled).
    #[serde(rename = "not_indexed")]
    NotIndexedFiles {
        files: Vec<String>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PageStats {
    pub files_total: usize,
    pub files_scanned: usize,
    pub rows_read: usize,
    pub rows_merged: usize,
    /// Files searched for other versions of multi-version runs (not in `files_scanned`).
    pub files_version_lookup: usize,
    /// Rows that search read (in `rows_read`).
    pub merge_rows: usize,
    /// Files a text search narrowed with their index.
    pub index_files: usize,
    /// Files a text search read whole (no index, or only very common words).
    pub scan_files: usize,
    /// Runs checked against their merged text.
    pub candidates: usize,
    /// Payload rows read for that check.
    pub payload_rows: usize,
    /// Files whose index footer was already in memory.
    pub cache_hits: usize,
    pub matched: usize,
    /// Every run in scope was read, so `matched` is the total.
    pub complete: bool,
    pub ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Page {
    pub runs: Vec<RunRow>,
    pub next_cursor: Option<String>,
    pub warnings: Vec<QueryWarning>,
    pub stats: PageStats,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectInfo {
    pub name: String,
    pub runs: usize,
}

#[derive(Clone)]
pub struct Query {
    ingest: Ingest,
    ctx: SessionContext,
    first_window: usize,
    version_rows: std::sync::Arc<tokio::sync::Mutex<fields::VersionRows>>,
    value_cap: usize,
    fts: std::sync::Arc<Fts>,
    /// Search through the text index (tests turn it off to compare with reading every file).
    index: bool,
    /// `feedback.<key>` values: the run's mean score for the key.
    feedback: Feedbacks,
}

fn order_key(m: &Merged, order: OrderBy) -> Key {
    let value = match order {
        OrderBy::StartTime => start_key(&m.state.run.dotted_order),
        // Runs without a latency (still running) sort as the smallest.
        OrderBy::Latency => m.state.run.latency_ms.map_or(-1, |v| v as i64),
    };
    Key {
        value,
        run_id: m.state.run.run_id,
    }
}

impl Query {
    pub fn new(ingest: Ingest) -> Query {
        let cfg = SessionConfig::new()
            .set_bool("datafusion.execution.parquet.pushdown_filters", true)
            .set_bool("datafusion.execution.parquet.reorder_filters", true);
        let ctx = SessionContext::new_with_config(cfg);
        ingest.storage().store.register(&ctx);
        Query {
            fts: std::sync::Arc::new(Fts::with_cache(
                ingest.storage().store.clone(),
                ingest.storage().cache.clone(),
            )),
            index: true,
            feedback: ingest.feedback(),
            ingest,
            ctx,
            first_window: window::FIRST_WINDOW_ROWS,
            version_rows: Default::default(),
            value_cap: aggregate::accum::VALUE_CAP,
        }
    }

    /// Searches by reading every file's text, never its index.
    pub fn without_text_index(mut self) -> Query {
        self.index = false;
        self
    }

    /// Estimated rows in the first start-time window (tests use small ones to cross many windows).
    pub fn with_first_window(mut self, rows: usize) -> Query {
        self.first_window = rows;
        self
    }

    pub fn ingest(&self) -> &Ingest {
        &self.ingest
    }

    fn scope(&self, q: &QuerySpec, now: Ts) -> Scope {
        Scope {
            project: q.project.clone(),
            from: q.time.from.map(|t| t.resolve(now).0),
            to: q.time.to.map(|t| t.resolve(now).0),
            traces: q
                .traces
                .as_ref()
                .map(|t| std::sync::Arc::new(t.iter().copied().collect())),
            runs: q.runs.clone(),
            ..Scope::default()
        }
    }

    /// `f` on a merged run, `feedback.<key>` read from the feedback index.
    fn admits(&self, f: &Expr, m: &Merged, now: Ts, stats: &mut EvalStats) -> bool {
        let run = &m.state.run;
        let fb = |k: &str| self.feedback.mean(&run.run_id, k);
        eval_with(f, &Row { run, attrs: &m.attrs }, &fb, now, stats)
    }

    /// All merged runs in `scope` matching `filter` (no ordering, no limit), read from `pinned`.
    #[allow(clippy::too_many_arguments)]
    async fn matching(
        &self,
        pinned: &scan::Pinned,
        scope: &Scope,
        filter: Option<&Expr>,
        is_root: Option<bool>,
        now: Ts,
        stats: &mut ScanStats,
        eval_stats: &mut EvalStats,
    ) -> Result<Vec<Merged>, QueryError> {
        use datafusion::logical_expr::{col, lit};
        let mut pushed = filter.and_then(|f| pushdown::compile(f, now)).map(|p| p.expr);
        if let Some(r) = is_root {
            let root = col("is_root").eq(lit(r));
            pushed = Some(match pushed {
                Some(p) => p.and(root),
                None => root,
            });
        }
        let runs = scan::collect_pinned(&self.ctx, &self.ingest, pinned, scope, pushed, stats).await?;
        let kept = |m: &Merged| {
            scope.project.as_ref().is_none_or(|p| m.state.run.project == *p)
                && is_root.is_none_or(|r| m.state.run.is_root == r)
        };
        let ok = |m: &Merged, f: Option<&Expr>, eval_stats: &mut EvalStats| {
            f.is_none_or(|f| self.admits(f, m, now, eval_stats))
        };
        let Some(f) = filter.filter(|f| contains_search(f)) else {
            return Ok(runs
                .into_iter()
                .filter(|m| kept(m) && ok(m, filter, eval_stats))
                .collect());
        };
        // Without the index: the other conditions first, then the text of the runs left.
        let pre = search::before_text(f);
        let ids: HashSet<RunId> = runs
            .iter()
            .filter(|m| kept(m) && ok(m, pre.as_ref(), eval_stats))
            .map(|m| m.state.run.run_id)
            .collect();
        stats.candidates += ids.len();
        stats.scan_files = stats.scan_files.max(stats.files_scanned);
        let inner = scope.narrowed();
        let need = search::Need::of(f);
        Ok(
            search::collect_text(self, pinned, scope, &ids, None, need, &mut |_| true, stats)
                .await?
                .into_iter()
                .filter(|m| inner.matches_key(start_key(&m.state.run.dotted_order)))
                .filter(|m| kept(m) && ok(m, Some(f), eval_stats))
                .collect(),
        )
    }

    /// The runs `ids` that match `filter` and `scope`, merged with their text (`held`: see
    /// [`search::collect_text`]).
    #[allow(clippy::too_many_arguments)]
    async fn matching_ids(
        &self,
        pinned: &scan::Pinned,
        scope: &Scope,
        ids: &HashSet<RunId>,
        held: Option<&HashSet<usize>>,
        filter: &Expr,
        is_root: Option<bool>,
        now: Ts,
        stats: &mut ScanStats,
        eval_stats: &mut EvalStats,
    ) -> Result<Vec<Merged>, QueryError> {
        stats.candidates += ids.len();
        let pre = search::before_text(filter);
        let ok = |m: &Merged, f: &Expr, eval_stats: &mut EvalStats| self.admits(f, m, now, eval_stats);
        let mut keep = |m: &Merged| {
            scope.matches_key(start_key(&m.state.run.dotted_order))
                && scope.project.as_ref().is_none_or(|p| m.state.run.project == *p)
                && is_root.is_none_or(|r| m.state.run.is_root == r)
                && pre.as_ref().is_none_or(|p| ok(m, p, eval_stats))
        };
        let need = search::Need::of(filter);
        let runs = search::collect_text(self, pinned, scope, ids, held, need, &mut keep, stats).await?;
        Ok(runs.into_iter().filter(|m| ok(m, filter, eval_stats)).collect())
    }

    /// Runs one query: scope → scan → merge → filter → root/tree conditions → order → page.
    pub async fn query(&self, q: &QuerySpec) -> Result<Page, QueryError> {
        self.select(q, q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT), true)
            .await
    }

    /// The runs matching `q`'s scope, `filter` and `is_root` (not its trace conditions), if it searches text.
    pub(crate) async fn search_set(&self, q: &QuerySpec) -> Result<Option<HashSet<RunId>>, QueryError> {
        if !q.filter.as_ref().is_some_and(contains_search) {
            return Ok(None);
        }
        let only = QuerySpec {
            trace_filter: None,
            tree_filter: None,
            cursor: None,
            columns: vec![],
            count: true,
            order: OrderBy::StartTime,
            ..q.clone()
        };
        let page = self.select(&only, usize::MAX, false).await?;
        Ok(Some(page.runs.into_iter().map(|r| r.header.run_id).collect()))
    }

    /// `specs` with each text search resolved to the runs it matches, for aggregates (whose file scan can't
    /// read text).
    pub(crate) async fn with_searches<T>(&self, specs: Vec<(QuerySpec, T)>) -> Result<Vec<(QuerySpec, T)>, QueryError> {
        let mut out = Vec::with_capacity(specs.len());
        for (mut q, t) in specs {
            if let Some(runs) = self.search_set(&q).await? {
                q.runs = Some(std::sync::Arc::new(runs));
                q.filter = None;
            }
            out.push((q, t));
        }
        Ok(out)
    }

    /// One page; with `excerpts`, page rows of a query that searches carry where it matched.
    pub(crate) async fn select(&self, q: &QuerySpec, limit: usize, excerpts: bool) -> Result<Page, QueryError> {
        let started = Instant::now();
        let now = Ts::now();
        let hash = q.hash();
        let after = match &q.cursor {
            Some(c) => Some(cursor::decode(c, hash).ok_or(QueryError::BadCursor)?),
            None => None,
        };
        let scope = self.scope(q, now);
        // Later pages only list runs at or before the cursor (ties are settled below); their versions still
        // come from the whole scope.
        let (mut from, mut to) = (scope.from, scope.to);
        if let (Some(a), OrderBy::StartTime) = (after, q.order) {
            if q.ascending {
                from = Some(from.map_or(a.value, |f| f.max(a.value)));
            } else {
                to = Some(to.map_or(a.value, |t| t.min(a.value)));
            }
        }
        let (mut scan_stats, mut eval_stats) = (ScanStats::default(), EvalStats::default());
        let searches = q
            .filter
            .as_ref()
            .filter(|f| contains_search(f))
            .map(search::all)
            .unwrap_or_default();
        let pinned = self.pin_kept(&scope, &searches).await?;
        // By start time, read in windows in page order and stop once the page and one more run are found.
        let early = q.order == OrderBy::StartTime && !q.count && q.traces.is_none();
        let want = limit.saturating_add(1);
        let page_order = |a: &Key, b: &Key| {
            if q.ascending {
                a.cmp(b)
            } else {
                b.value.cmp(&a.value).then(a.run_id.cmp(&b.run_id))
            }
        };
        let after_cursor = |k: &Key| {
            after.is_none_or(|a| {
                if q.ascending {
                    *k > a
                } else {
                    k.value < a.value || (k.value == a.value && k.run_id > a.run_id)
                }
            })
        };
        // A search the index answers: its candidates, in page order, read a chunk at a time.
        let indexed = q.filter.as_ref().filter(|_| self.index).and_then(search::indexed);
        let mut queue: Option<std::vec::IntoIter<search::Cand>> = None;
        if let Some((words, side)) = indexed {
            let cscope = Scope {
                from,
                to,
                ..scope.clone()
            };
            let key = |c: &search::Cand| Key {
                value: c.key,
                run_id: c.run_id,
            };
            let pre = q.filter.as_ref().and_then(search::before_text);
            let mut cands: Vec<search::Cand> =
                search::candidates(self, &pinned, &cscope, words, side, pre.as_ref(), now, &mut scan_stats)
                    .await?
                    .into_iter()
                    .filter(|c| q.order != OrderBy::StartTime || after_cursor(&key(c)))
                    .collect();
            cands.sort_by(|a, b| page_order(&key(a), &key(b)));
            let mut seen = HashSet::new();
            cands.retain(|c| seen.insert(c.run_id));
            queue = Some(cands.into_iter());
        }
        let mut windows = (early && queue.is_none()).then(|| {
            window::Windows::new(
                pinned
                    .version
                    .segments
                    .iter()
                    .map(|s| &**s)
                    .filter(|s| scope.file_in_scope(s)),
                pinned.keys(),
                from,
                to,
                q.ascending,
                self.first_window,
            )
        });
        let traces = self.searched_traces(q, &scope, &mut scan_stats).await?;
        let shown = q.filter.as_ref().filter(|_| excerpts).and_then(search::shown);
        let mut matches: HashMap<RunId, Match> = HashMap::new();
        // A first page checks the text of only as many candidates as it shows (plus one for the next page),
        // then twice as many each time some fail (late materialization).
        let mut chunk = if early { want } else { scan::IN_CHUNK };
        let mut found: Vec<(Key, Merged)> = Vec::new();
        let mut complete = true;
        loop {
            let runs = match (queue.as_mut(), q.filter.as_ref()) {
                (Some(rest), Some(f)) => {
                    let next: Vec<search::Cand> = rest.by_ref().take(chunk).collect();
                    if next.is_empty() {
                        break;
                    }
                    chunk = (chunk * 2).min(scan::IN_CHUNK);
                    let ids: HashSet<RunId> = next.iter().map(|c| c.run_id).collect();
                    let held: HashSet<usize> = next.iter().filter_map(|c| c.file).collect();
                    self.matching_ids(
                        &pinned,
                        &scope,
                        &ids,
                        Some(&held),
                        f,
                        q.is_root,
                        now,
                        &mut scan_stats,
                        &mut eval_stats,
                    )
                    .await?
                }
                _ => {
                    let wscope = match windows.as_mut() {
                        None => Scope {
                            window: ((from, to) != (scope.from, scope.to))
                                .then(|| (from.unwrap_or(i64::MIN), to.unwrap_or(i64::MAX))),
                            ..scope.clone()
                        },
                        Some(w) => match w.next_window() {
                            Some(w) => Scope {
                                window: Some(w),
                                ..scope.clone()
                            },
                            None => break,
                        },
                    };
                    self.matching(
                        &pinned,
                        &wscope,
                        q.filter.as_ref(),
                        q.is_root,
                        now,
                        &mut scan_stats,
                        &mut eval_stats,
                    )
                    .await?
                }
            };
            let mut keyed: Vec<(Key, Merged)> = runs
                .into_iter()
                .filter(|m| {
                    q.traces
                        .as_ref()
                        .is_none_or(|only| only.contains(&m.state.run.trace_id))
                })
                .map(|m| (order_key(&m, q.order), m))
                .filter(|(k, _)| after_cursor(k))
                .collect();
            keyed.sort_by(|a, b| page_order(&a.0, &b.0));
            // The text was only needed to decide: keep the excerpts of runs that can still be on the page.
            let mut add = |found: &mut Vec<(Key, Merged)>, (k, mut m): (Key, Merged)| {
                if let Some((words, side)) = shown
                    && found.len() < want
                    && let Some(x) = tracedb_model::text::excerpt(&Text::of(&m.state.run), words, side)
                {
                    matches.insert(m.state.run.run_id, x);
                }
                m.state.run.inputs = None;
                m.state.run.outputs = None;
                found.push((k, m));
            };

            // Trace-level conditions, evaluated over the candidates' whole traces (no time bound: a root may
            // have started before the window, a step after it), in page order, a chunk of candidates at a time.
            if q.trace_filter.is_none() && q.tree_filter.is_none() {
                keyed.into_iter().for_each(|r| add(&mut found, r));
            } else {
                let mut rest = keyed.into_iter().peekable();
                while rest.peek().is_some() && !(early && found.len() >= want) {
                    let chunk: Vec<(Key, Merged)> = rest.by_ref().take(scan::IN_CHUNK).collect();
                    let ok = self
                        .trace_conditions(q, &scope, &chunk, &traces, now, &mut scan_stats, &mut eval_stats)
                        .await?;
                    for r in chunk.into_iter().filter(|(_, m)| ok.contains(&m.state.run.trace_id)) {
                        add(&mut found, r);
                    }
                }
                if rest.peek().is_some() {
                    complete = false;
                }
            }
            if let Some(rest) = queue.as_ref() {
                if early && found.len() >= want {
                    complete &= rest.len() == 0;
                    break;
                }
                continue;
            }
            let Some(w) = windows.as_ref() else { break };
            if found.len() >= want {
                complete &= w.done();
                break;
            }
        }
        // Candidate chunks follow each version's start key; a merged run's may differ.
        found.sort_by(|a, b| page_order(&a.0, &b.0));
        let matched = found.len();
        let more = found.len() > limit;
        found.truncate(limit);
        let next_cursor = if more {
            found.last().map(|(k, _)| cursor::encode(*k, hash))
        } else {
            None
        };
        let keyed = found;

        let mut warnings = eval_warnings(eval_stats);
        let skipped = self.skipped(&scope);
        if !skipped.is_empty() {
            warnings.push(QueryWarning::SkippedFiles { ids: skipped });
        }
        if !scan_stats.not_indexed.is_empty() {
            warnings.push(QueryWarning::NotIndexedFiles {
                files: std::mem::take(&mut scan_stats.not_indexed).into_iter().collect(),
            });
        }
        let rows: Vec<RunRow> = keyed
            .into_iter()
            .map(|(_, m)| {
                if m.state.run.warnings.contains(&tracedb_model::Warning::IdentityConflict) {
                    warnings.push(QueryWarning::IdentityConflict {
                        run_id: m.state.run.run_id,
                    });
                }
                RunRow {
                    columns: columns(&m, &q.columns, &self.feedback),
                    r#match: matches.remove(&m.state.run.run_id),
                    header: RunHeader::from_state(&m.shown(), 0),
                }
            })
            .collect();
        Ok(Page {
            runs: rows,
            next_cursor,
            warnings,
            stats: PageStats {
                files_total: scan_stats.files_total,
                files_scanned: scan_stats.files_scanned,
                rows_read: scan_stats.rows_read,
                rows_merged: scan_stats.rows_merged,
                files_version_lookup: scan_stats.files_version_lookup,
                merge_rows: scan_stats.merge_rows,
                index_files: scan_stats.index_files,
                scan_files: scan_stats.scan_files,
                candidates: scan_stats.candidates,
                payload_rows: scan_stats.payload_rows,
                cache_hits: scan_stats.cache_hits,
                matched,
                complete,
                ms: started.elapsed().as_millis() as u64,
            },
        })
    }

    /// [`scan::pin_text`] with the kept merged file rows of multi-version runs (D-023) brought up to it under
    /// one lock, so their versions aren't fetched again. If another request holds the lock (the start-up warm
    /// up, an update), the pin goes without them: same results, read the M2 way.
    async fn pin_kept(&self, scope: &Scope, searches: &[(&[String], Side)]) -> Result<scan::Pinned, QueryError> {
        let Ok(mut cache) = self.version_rows.try_lock() else {
            return Ok(scan::pin_text(&self.ingest, scope, searches));
        };
        let (mut pinned, frozen, multi) = scan::pin_for_kept(&self.ingest, scope, searches);
        // A file dropped since (found damaged) is still in the kept rows: they're read again without it, in the
        // background.
        if cache.segments.len() > pinned.version.segments.len() {
            *cache = Default::default();
            drop(cache);
            self.warm_aggregates();
            return Ok(pinned);
        }
        match cache
            .update(self, &pinned.version.segments, frozen.as_ref(), &multi)
            .await
        {
            Ok(()) if cache.segments.len() == pinned.version.segments.len() => pinned.kept = Some(cache.rows.clone()),
            Ok(()) => {}
            Err(e) => tracing::warn!(error = %e, "kept rows not brought up to date; reading versions instead"),
        }
        Ok(pinned)
    }

    /// For `trace_filter` and `tree_filter`: the runs the text index says may match, by trace, if that slot's
    /// search can use it. Not bounded in time, as trace conditions aren't.
    async fn searched_traces(
        &self,
        q: &QuerySpec,
        scope: &Scope,
        stats: &mut ScanStats,
    ) -> Result<[Option<HashMap<TraceId, Vec<search::Cand>>>; 2], QueryError> {
        let mut out = [None, None];
        for (i, f) in [&q.trace_filter, &q.tree_filter].into_iter().enumerate() {
            let Some(f) = f.as_ref().filter(|_| self.index) else {
                continue;
            };
            let Some((words, side)) = search::indexed(f) else {
                continue;
            };
            let tscope = Scope {
                project: scope.project.clone(),
                traces: scope.traces.clone(),
                ..Scope::default()
            };
            let pinned = scan::pin_text(&self.ingest, &tscope, &[(words, side)]);
            let mut by_trace: HashMap<TraceId, Vec<search::Cand>> = HashMap::new();
            let pre = search::before_text(f);
            for c in search::candidates(self, &pinned, &tscope, words, side, pre.as_ref(), Ts::now(), stats).await? {
                by_trace.entry(c.trace_id).or_default().push(c);
            }
            out[i] = Some(by_trace);
        }
        Ok(out)
    }

    /// Traces of `chunk` whose root matches `trace_filter` and that hold a run matching `tree_filter`.
    #[allow(clippy::too_many_arguments)]
    async fn trace_conditions(
        &self,
        q: &QuerySpec,
        scope: &Scope,
        chunk: &[(Key, Merged)],
        searched: &[Option<HashMap<TraceId, Vec<search::Cand>>>; 2],
        now: Ts,
        stats: &mut ScanStats,
        eval_stats: &mut EvalStats,
    ) -> Result<HashSet<TraceId>, QueryError> {
        let mut ok: HashSet<TraceId> = chunk.iter().map(|(_, m)| m.state.run.trace_id).collect();
        for ((f, is_root), cands) in [(&q.trace_filter, Some(true)), (&q.tree_filter, None)]
            .into_iter()
            .zip(searched)
        {
            let Some(f) = f else { continue };
            if let Some(by_trace) = cands {
                ok.retain(|t| by_trace.contains_key(t));
            }
            if ok.is_empty() {
                break;
            }
            let tscope = Scope {
                project: scope.project.clone(),
                traces: Some(std::sync::Arc::new(ok.iter().copied().collect())),
                ..Scope::default()
            };
            let pinned = if contains_search(f) {
                scan::pin_text(&self.ingest, &tscope, &search::all(f))
            } else {
                scan::pin(&self.ingest, &tscope)
            };
            let runs = match cands {
                // Only the index's candidates in these traces are checked.
                Some(by_trace) => {
                    let ids: HashSet<RunId> = ok.iter().flat_map(|t| &by_trace[t]).map(|c| c.run_id).collect();
                    self.matching_ids(&pinned, &tscope, &ids, None, f, is_root, now, stats, eval_stats)
                        .await?
                }
                None => {
                    self.matching(&pinned, &tscope, Some(f), is_root, now, stats, eval_stats)
                        .await?
                }
            };
            ok = runs.into_iter().map(|m| m.state.run.trace_id).collect();
        }
        Ok(ok)
    }

    /// Damaged data files (found at start or while serving) that `scope` would have read.
    pub fn skipped(&self, scope: &Scope) -> Vec<String> {
        self.ingest
            .damaged()
            .into_iter()
            .filter(|d| scope.file_in_scope(&d.segment))
            .map(|d| d.id)
            .collect()
    }

    /// [`Query::skipped`] for one trace.
    pub fn skipped_for_trace(&self, id: TraceId) -> Vec<String> {
        self.skipped(&Scope {
            trace_id: Some(id),
            ..Scope::default()
        })
    }

    /// [`Query::skipped`] for a project and time range (field catalog).
    pub fn skipped_in(&self, project: Option<&str>, time: TimeRange) -> Vec<String> {
        let now = Ts::now();
        self.skipped(&Scope {
            project: project.map(str::to_string),
            from: time.from.map(|t| t.resolve(now).0),
            to: time.to.map(|t| t.resolve(now).0),
            ..Scope::default()
        })
    }

    /// The whole tree of one trace, headers only, in `dotted_order`.
    pub async fn trace(&self, id: TraceId) -> Result<Option<TraceView>, QueryError> {
        let scope = Scope {
            trace_id: Some(id),
            ..Scope::default()
        };
        let mut stats = ScanStats::default();
        let runs = scan::collect(&self.ctx, &self.ingest, &scope, None, &mut stats).await?;
        if runs.is_empty() {
            return Ok(None);
        }
        let mut children: HashMap<RunId, u32> = HashMap::new();
        for m in &runs {
            if let Some(p) = m.state.run.parent_run_id {
                *children.entry(p).or_default() += 1;
            }
        }
        let mut headers: Vec<RunHeader> = runs
            .iter()
            .map(|m| RunHeader::from_state(&m.state, children.get(&m.state.run.run_id).copied().unwrap_or(0)))
            .collect();
        headers.sort_by(|a, b| a.dotted_order.cmp(&b.dotted_order));
        Ok(Some(TraceView {
            root: headers.iter().find(|r| r.is_root).cloned(),
            run_count: headers.len(),
            has_running: headers.iter().any(|r| r.status == Status::Running),
            runs: headers,
        }))
    }

    /// One run, with the payload fields asked for.
    pub async fn run(&self, id: RunId, include: Include) -> Result<Option<RunView>, QueryError> {
        Ok(self.run_with_stats(id, include).await?.0)
    }

    /// [`Query::run`], with what it read: the run's versions (one scan of every file), the rows of its children
    /// (to count them), and payload only from the files holding a version that set a field asked for.
    pub async fn run_with_stats(
        &self,
        id: RunId,
        include: Include,
    ) -> Result<(Option<RunView>, ScanStats), QueryError> {
        let mut stats = ScanStats::default();
        let Some(found) = scan::run_versions(&self.ctx, &self.ingest, id, &mut stats).await? else {
            return Ok((None, stats));
        };
        let m = found.merged();
        let children = scan::child_count(&self.ctx, &self.ingest, m.state.run.trace_id, id, &mut stats).await?;
        let header = RunHeader::from_state(&m.state, children);
        let p = if include.inputs || include.outputs || include.events {
            let fields = (include.inputs, include.outputs, include.events);
            found.payload(&self.ingest, id, fields, &mut stats).await?
        } else {
            Default::default()
        };
        let view = RunView {
            header,
            inputs: if include.inputs { p.inputs } else { None },
            outputs: if include.outputs { p.outputs } else { None },
            events: if include.events { p.events } else { None },
        };
        Ok((Some(view), stats))
    }

    /// Projects and how many runs each holds (memtables + files). Counted from the catalog without reading
    /// data, so a run whose versions sit in several files counts once per file (D-019).
    pub async fn projects(&self) -> Result<Vec<ProjectInfo>, QueryError> {
        let mem = self.ingest.mem();
        let mem = mem.read().unwrap();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for s in mem.version.segments.iter().filter(|s| !s.project.is_empty()) {
            *counts.entry(s.project.clone()).or_default() += s.row_count as usize;
        }
        for store in std::iter::once(&mem.active).chain(mem.frozen.as_deref()) {
            for (p, n) in store.run_counts() {
                *counts.entry(p).or_default() += n;
            }
        }
        Ok(counts
            .into_iter()
            .map(|(name, runs)| ProjectInfo { name, runs })
            .collect())
    }

    pub fn last_seq(&self) -> u64 {
        self.ingest.snapshot().last_seq()
    }
}

fn eval_warnings(s: EvalStats) -> Vec<QueryWarning> {
    let mut warnings: Vec<QueryWarning> = s
        .mixed_types
        .into_iter()
        .map(|(path, counts)| QueryWarning::MixedTypes {
            path,
            counts: counts.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        })
        .collect();
    warnings.extend(
        s.not_indexed
            .into_iter()
            .map(|(path, runs)| QueryWarning::NotIndexed { path, runs }),
    );
    warnings
}

/// Values for the Table view's extra columns.
fn columns(m: &Merged, paths: &[Path], feedback: &Feedbacks) -> BTreeMap<String, serde_json::Value> {
    use serde_json::Value as J;
    use tracedb_model::attrs::Scalar;
    let r = &m.state.run;
    paths
        .iter()
        .filter_map(|p| {
            let v = match p {
                Path::Attr { .. } => match m.attrs.get(&p.text())? {
                    Scalar::Str(s) => J::from(s.clone()),
                    Scalar::Num(n) => serde_json::Number::from_f64(*n).map(J::Number)?,
                    Scalar::Bool(b) => J::Bool(*b),
                    Scalar::Unindexed => J::Null,
                },
                Path::Feedback(k) => serde_json::Number::from_f64(feedback.mean(&r.run_id, k)?).map(J::Number)?,
                Path::Field(f) => match f {
                    Field::Status => J::from(tracedb_model::filter::status_name(r.status)),
                    Field::Name => J::from(r.name.clone()?),
                    Field::Model => J::from(r.model.clone()?),
                    Field::Latency => J::from(r.latency_ms? as f64 / 1000.0),
                    Field::TotalTokens => J::from(r.total_tokens?),
                    Field::Tags => J::from(r.tags.clone()),
                    _ => return None,
                },
            };
            Some((p.text(), v))
        })
        .collect()
}
