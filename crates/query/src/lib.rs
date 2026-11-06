//! Query service: answers every read over memtables ∪ data files (design §4.3–4.4).
//!
//! DataFusion scans the files with pruning and a pushed-down filter that may only keep extra rows; the exact
//! filter is evaluated on merged runs by [`tracedb_model::filter::eval`], the same evaluator used for
//! memtable rows. So results never depend on where a run's pieces are.

pub mod cursor;
pub mod fields;
pub mod pushdown;
pub mod scan;
pub mod tail;
pub mod window;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use datafusion::prelude::{SessionConfig, SessionContext};
use serde::Serialize;
use tracedb_ingest::{Include, Ingest};
use tracedb_model::filter::{EvalStats, Expr, Field, Path, Row, TimeAt, eval};
use tracedb_model::{RunHeader, RunId, RunView, Status, TraceId, TraceView, Ts};
use tracedb_storage::StorageError;
use tracedb_storage::segment::start_key;
use xxhash_rust::xxh64::xxh64;

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
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PageStats {
    pub files_total: usize,
    pub files_scanned: usize,
    pub rows_read: usize,
    pub rows_merged: usize,
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
            ingest,
            ctx,
            first_window: window::FIRST_WINDOW_ROWS,
        }
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
            ..Scope::default()
        }
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
        Ok(runs
            .into_iter()
            .filter(|m| scope.project.as_ref().is_none_or(|p| m.state.run.project == *p))
            .filter(|m| is_root.is_none_or(|r| m.state.run.is_root == r))
            .filter(|m| {
                filter.is_none_or(|f| {
                    eval(
                        f,
                        &Row {
                            run: &m.state.run,
                            attrs: &m.attrs,
                        },
                        now,
                        eval_stats,
                    )
                })
            })
            .collect())
    }

    /// Runs one query: scope → scan → merge → filter → root/tree conditions → order → page.
    pub async fn query(&self, q: &QuerySpec) -> Result<Page, QueryError> {
        self.select(q, q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT))
            .await
    }

    async fn select(&self, q: &QuerySpec, limit: usize) -> Result<Page, QueryError> {
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
        let pinned = scan::pin(&self.ingest, &scope);
        // By start time, read in windows in page order and stop once the page and one more run are found.
        let early = q.order == OrderBy::StartTime && !q.count && q.traces.is_none();
        let mut windows = early.then(|| {
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
        let mut found: Vec<(Key, Merged)> = Vec::new();
        let mut complete = true;
        loop {
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
            let runs = self
                .matching(
                    &pinned,
                    &wscope,
                    q.filter.as_ref(),
                    q.is_root,
                    now,
                    &mut scan_stats,
                    &mut eval_stats,
                )
                .await?;
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

            // Trace-level conditions, evaluated over the candidates' whole traces (no time bound: a root may
            // have started before the window, a step after it), in page order, a chunk of candidates at a time.
            if q.trace_filter.is_none() && q.tree_filter.is_none() {
                found.extend(keyed);
            } else {
                let mut rest = keyed.into_iter().peekable();
                while rest.peek().is_some() && !(early && found.len() >= want) {
                    let chunk: Vec<(Key, Merged)> = rest.by_ref().take(scan::IN_CHUNK).collect();
                    let ok = self
                        .trace_conditions(q, &scope, &chunk, now, &mut scan_stats, &mut eval_stats)
                        .await?;
                    found.extend(chunk.into_iter().filter(|(_, m)| ok.contains(&m.state.run.trace_id)));
                }
                if rest.peek().is_some() {
                    complete = false;
                }
            }
            let Some(w) = windows.as_ref() else { break };
            if found.len() >= want {
                complete &= w.done();
                break;
            }
        }
        let matched = found.len();
        let more = found.len() > limit;
        found.truncate(limit);
        let next_cursor = if more {
            found.last().map(|(k, _)| cursor::encode(*k, hash))
        } else {
            None
        };
        let keyed = found;

        let mut warnings: Vec<QueryWarning> = eval_stats
            .mixed_types
            .into_iter()
            .map(|(path, counts)| QueryWarning::MixedTypes {
                path,
                counts: counts.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            })
            .collect();
        warnings.extend(
            eval_stats
                .not_indexed
                .into_iter()
                .map(|(path, runs)| QueryWarning::NotIndexed { path, runs }),
        );
        let rows: Vec<RunRow> = keyed
            .into_iter()
            .map(|(_, m)| {
                if m.state.run.warnings.contains(&tracedb_model::Warning::IdentityConflict) {
                    warnings.push(QueryWarning::IdentityConflict {
                        run_id: m.state.run.run_id,
                    });
                }
                RunRow {
                    columns: columns(&m, &q.columns),
                    header: RunHeader::from_state(&m.state, 0),
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
                matched,
                complete,
                ms: started.elapsed().as_millis() as u64,
            },
        })
    }

    /// Traces of `chunk` whose root matches `trace_filter` and that hold a run matching `tree_filter`.
    async fn trace_conditions(
        &self,
        q: &QuerySpec,
        scope: &Scope,
        chunk: &[(Key, Merged)],
        now: Ts,
        stats: &mut ScanStats,
        eval_stats: &mut EvalStats,
    ) -> Result<HashSet<TraceId>, QueryError> {
        let mut ok: HashSet<TraceId> = chunk.iter().map(|(_, m)| m.state.run.trace_id).collect();
        for (f, is_root) in [(&q.trace_filter, Some(true)), (&q.tree_filter, None)] {
            let Some(f) = f else { continue };
            if ok.is_empty() {
                break;
            }
            let tscope = Scope {
                project: scope.project.clone(),
                traces: Some(std::sync::Arc::new(ok.iter().copied().collect())),
                ..Scope::default()
            };
            let pinned = scan::pin(&self.ingest, &tscope);
            ok = self
                .matching(&pinned, &tscope, Some(f), is_root, now, stats, eval_stats)
                .await?
                .into_iter()
                .map(|m| m.state.run.trace_id)
                .collect();
        }
        Ok(ok)
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
        let scope = Scope {
            run_id: Some(id),
            ..Scope::default()
        };
        let mut stats = ScanStats::default();
        let Some(m) = scan::collect(&self.ctx, &self.ingest, &scope, None, &mut stats)
            .await?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let children = self.trace(m.state.run.trace_id).await?.map_or(0, |t| {
            t.runs.iter().filter(|r| r.parent_run_id == Some(id)).count() as u32
        });
        let header = RunHeader::from_state(&m.state, children);
        if !(include.inputs || include.outputs || include.events) {
            return Ok(Some(RunView {
                header,
                inputs: None,
                outputs: None,
                events: None,
            }));
        }
        let p = scan::payload(&self.ctx, &self.ingest, id, &scope).await?;
        Ok(Some(RunView {
            header,
            inputs: if include.inputs { p.inputs } else { None },
            outputs: if include.outputs { p.outputs } else { None },
            events: if include.events { p.events } else { None },
        }))
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

/// Values for the Table view's extra columns.
fn columns(m: &Merged, paths: &[Path]) -> BTreeMap<String, serde_json::Value> {
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
