//! Feedback the SDK sends for a run (design §4.8, D-027): one catalog row per item, committed before the reply
//! and keyed by `run_id` whether or not that run has arrived, plus an in-memory `run_id → (key, score)` index
//! loaded at start. The join with runs happens at read time.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracedb_model::{RunId, TraceId, Ts};
use tracedb_storage::{FeedbackKey, FeedbackRow, Metastore, StorageError};

use crate::Storage;

/// One feedback item as stored and answered; fields tracedb doesn't know are kept in `extra`, verbatim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Feedback {
    pub id: String,
    pub run_id: RunId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<TraceId>,
    pub key: String,
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correction: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_source: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub created_at: Ts,
    pub modified_at: Ts,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum FeedbackError {
    #[error("{0}")]
    Bad(String),
    #[error("no feedback {0}")]
    NotFound(String),
    #[error("catalog: {0}")]
    Storage(String),
}

impl From<StorageError> for FeedbackError {
    fn from(e: StorageError) -> FeedbackError {
        FeedbackError::Storage(e.to_string())
    }
}

fn bad(m: impl Into<String>) -> FeedbackError {
    FeedbackError::Bad(m.into())
}

fn opt_str(obj: &mut Map<String, Value>, k: &str) -> Result<Option<String>, FeedbackError> {
    match obj.remove(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(bad(format!("{k} must be a string"))),
    }
}

fn opt_json(obj: &mut Map<String, Value>, k: &str) -> Option<Value> {
    obj.remove(k).filter(|v| !v.is_null())
}

fn uuid_field(obj: &mut Map<String, Value>, k: &str) -> Result<Option<uuid::Uuid>, FeedbackError> {
    opt_str(obj, k)?
        .map(|s| uuid::Uuid::try_parse(&s).map_err(|_| bad(format!("{k} must be a UUID, got {s:?}"))))
        .transpose()
}

/// A UUID id in its canonical form (the SDK's are lowercase), so a differently cased retry is the same row.
pub fn canonical_id(id: &str) -> String {
    uuid::Uuid::try_parse(id).map_or_else(|_| id.to_string(), |u| u.to_string())
}

impl Feedback {
    /// Validates an uploaded item (`POST /feedback` body or a `feedback.{id}` part): `run_id` and `key` are
    /// required, `score` is a number (a bool counts as 1 / 0) or null; a missing `id` is `default_id`, else
    /// generated; a missing `created_at` is `now`; `modified_at` is always `now`.
    pub fn from_upload(body: Value, default_id: Option<&str>, now: Ts) -> Result<Feedback, FeedbackError> {
        let Value::Object(mut obj) = body else {
            return Err(bad("feedback must be a JSON object"));
        };
        let id = match opt_str(&mut obj, "id")?.filter(|s| !s.is_empty()) {
            Some(id) => canonical_id(&id),
            None => default_id
                .map(canonical_id)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        };
        let run_id = uuid_field(&mut obj, "run_id")?.ok_or_else(|| bad("missing run_id"))?;
        let trace_id = uuid_field(&mut obj, "trace_id")?;
        let key = opt_str(&mut obj, "key")?
            .filter(|k| !k.is_empty())
            .ok_or_else(|| bad("missing key"))?;
        let score = match obj.remove("score") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(b)) => Some(if b { 1.0 } else { 0.0 }),
            Some(Value::Number(n)) => n.as_f64(),
            Some(v) => return Err(bad(format!("score must be a number, got {v}"))),
        };
        let created_at = match obj.remove("created_at") {
            Some(Value::String(s)) => Ts::parse(&s).unwrap_or(now),
            _ => now,
        };
        obj.remove("modified_at");
        Ok(Feedback {
            id,
            run_id,
            trace_id,
            key,
            score,
            value: opt_json(&mut obj, "value"),
            comment: opt_str(&mut obj, "comment")?,
            correction: opt_json(&mut obj, "correction"),
            feedback_source: opt_json(&mut obj, "feedback_source"),
            session_id: opt_str(&mut obj, "session_id")?,
            created_at,
            modified_at: now,
            extra: obj,
        })
    }

    /// `self` with the fields of `patch` replaced (`id` stays), validated again.
    pub fn patched(&self, patch: Value, now: Ts) -> Result<Feedback, FeedbackError> {
        let Value::Object(patch) = patch else {
            return Err(bad("feedback must be a JSON object"));
        };
        let Value::Object(mut obj) = serde_json::to_value(self).expect("serializable") else {
            unreachable!("a struct serializes to an object")
        };
        let keep_created = !matches!(patch.get("created_at"), Some(Value::String(s)) if Ts::parse(s).is_some());
        obj.extend(patch);
        obj.insert("id".into(), Value::String(self.id.clone()));
        let mut f = Feedback::from_upload(Value::Object(obj), None, now)?;
        if keep_created {
            f.created_at = self.created_at;
        }
        Ok(f)
    }

    fn row(&self) -> FeedbackRow {
        let json = |v: &Option<Value>| v.as_ref().map(Value::to_string);
        FeedbackRow {
            id: self.id.clone(),
            run_id: self.run_id.to_string(),
            trace_id: self.trace_id.map(|t| t.to_string()),
            key: self.key.clone(),
            score: self.score,
            value: json(&self.value),
            comment: self.comment.clone(),
            correction: json(&self.correction),
            source: json(&self.feedback_source),
            created_at: self.created_at.0,
            modified_at: self.modified_at.0,
            raw: serde_json::to_string(self).expect("serializable"),
        }
    }

    fn from_raw(raw: &str) -> Result<Feedback, FeedbackError> {
        serde_json::from_str(raw).map_err(|e| FeedbackError::Storage(format!("stored feedback is not readable: {e}")))
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    id: Box<str>,
    key: Box<str>,
    score: Option<f64>,
}

/// Count and mean score of one key.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct KeyStat {
    pub key: String,
    pub count: usize,
    /// Mean of the scores given; null when every item has only a value.
    pub avg: Option<f64>,
}

/// `run_id → [(key, score)]` for every stored item.
#[derive(Debug, Default)]
pub struct FeedbackIndex {
    by_run: HashMap<RunId, Vec<Entry>>,
    by_id: HashMap<Box<str>, RunId>,
}

impl FeedbackIndex {
    fn insert(&mut self, id: &str, run: RunId, key: &str, score: Option<f64>) {
        if let Some(old) = self.by_id.insert(id.into(), run)
            && let Some(v) = self.by_run.get_mut(&old)
        {
            v.retain(|e| &*e.id != id);
            if v.is_empty() {
                self.by_run.remove(&old);
            }
        }
        self.by_run.entry(run).or_default().push(Entry {
            id: id.into(),
            key: key.into(),
            score,
        });
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

/// Read-only feedback for queries: `feedback.<key>` on a run is the mean of its scores for that key.
pub trait FeedbackLookup: Send + Sync {
    /// Mean score of `key` on `run`; `None` when the run has no score for it.
    fn mean(&self, run: &RunId, key: &str) -> Option<f64>;
    /// `(key, score)` of every item on `run`.
    fn scores(&self, run: &RunId) -> Vec<(String, Option<f64>)>;
    /// Every run some feedback points at, found or not.
    fn run_ids(&self) -> Vec<RunId>;
    /// How many items point at `run`.
    fn items(&self, run: &RunId) -> usize;
    /// Per key, over the items whose run passes `keep`, in key order.
    fn keys(&self, keep: &dyn Fn(&RunId) -> bool) -> Vec<KeyStat>;
}

impl FeedbackLookup for FeedbackIndex {
    fn mean(&self, run: &RunId, key: &str) -> Option<f64> {
        let (sum, n) = self
            .by_run
            .get(run)?
            .iter()
            .filter(|e| &*e.key == key)
            .filter_map(|e| e.score)
            .fold((0.0, 0u32), |(s, n), x| (s + x, n + 1));
        (n > 0).then(|| sum / n as f64)
    }

    fn scores(&self, run: &RunId) -> Vec<(String, Option<f64>)> {
        self.by_run
            .get(run)
            .map(|v| v.iter().map(|e| (e.key.to_string(), e.score)).collect())
            .unwrap_or_default()
    }

    fn run_ids(&self) -> Vec<RunId> {
        self.by_run.keys().copied().collect()
    }

    fn items(&self, run: &RunId) -> usize {
        self.by_run.get(run).map_or(0, Vec::len)
    }

    fn keys(&self, keep: &dyn Fn(&RunId) -> bool) -> Vec<KeyStat> {
        let mut acc: BTreeMap<&str, (usize, f64, usize)> = BTreeMap::new();
        for entries in self.by_run.iter().filter(|(r, _)| keep(r)).map(|(_, v)| v) {
            for e in entries {
                let a = acc.entry(&e.key).or_default();
                a.0 += 1;
                if let Some(s) = e.score {
                    a.1 += s;
                    a.2 += 1;
                }
            }
        }
        acc.into_iter()
            .map(|(key, (count, sum, n))| KeyStat {
                key: key.to_string(),
                count,
                avg: (n > 0).then(|| sum / n as f64),
            })
            .collect()
    }
}

/// The shared index, read under its lock.
#[derive(Clone, Default)]
pub struct Feedbacks(Arc<RwLock<FeedbackIndex>>);

impl FeedbackLookup for Feedbacks {
    fn mean(&self, run: &RunId, key: &str) -> Option<f64> {
        self.0.read().unwrap().mean(run, key)
    }

    fn scores(&self, run: &RunId) -> Vec<(String, Option<f64>)> {
        self.0.read().unwrap().scores(run)
    }

    fn run_ids(&self) -> Vec<RunId> {
        self.0.read().unwrap().run_ids()
    }

    fn items(&self, run: &RunId) -> usize {
        self.0.read().unwrap().items(run)
    }

    fn keys(&self, keep: &dyn Fn(&RunId) -> bool) -> Vec<KeyStat> {
        self.0.read().unwrap().keys(keep)
    }
}

impl Feedbacks {
    pub fn len(&self) -> usize {
        self.0.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.read().unwrap().is_empty()
    }
}

/// Catalog-backed store: writes are serialized so the index follows the commit order.
pub(crate) struct FeedbackStore {
    storage: Arc<Storage>,
    index: Feedbacks,
    write: Arc<tokio::sync::Mutex<()>>,
}

/// Commits `items`, then adds them to the index; the caller holds the write lock.
async fn commit(meta: &Metastore, index: &Feedbacks, items: &[Feedback]) -> Result<(), FeedbackError> {
    let rows: Vec<FeedbackRow> = items.iter().map(Feedback::row).collect();
    meta.put_feedback(&rows).await?;
    let mut ix = index.0.write().unwrap();
    for f in items {
        ix.insert(&f.id, f.run_id, &f.key, f.score);
    }
    Ok(())
}

async fn get(meta: &Metastore, id: &str) -> Result<Option<Feedback>, FeedbackError> {
    let raw = meta.feedback_raw(id).await?;
    raw.as_deref().map(Feedback::from_raw).transpose()
}

impl FeedbackStore {
    pub fn load(storage: Arc<Storage>) -> Result<FeedbackStore, StorageError> {
        let meta = storage.meta.clone();
        let keys: Vec<FeedbackKey> = storage.block(async move { meta.feedback_keys().await })?;
        let mut index = FeedbackIndex::default();
        for k in keys {
            match uuid::Uuid::try_parse(&k.run_id) {
                Ok(run) => index.insert(&k.id, run, &k.key, k.score),
                Err(_) => tracing::warn!(id = %k.id, run_id = %k.run_id, "feedback row with a bad run_id skipped"),
            }
        }
        Ok(FeedbackStore {
            storage,
            index: Feedbacks(Arc::new(RwLock::new(index))),
            write: Arc::default(),
        })
    }

    pub fn index(&self) -> Feedbacks {
        self.index.clone()
    }

    /// Stores every item in one transaction (a retried id replaces its row). The commit and the index update run
    /// as one task on the storage runtime, so a client that goes away can't leave them apart.
    pub async fn put(&self, items: Vec<Feedback>) -> Result<Vec<Feedback>, FeedbackError> {
        if items.is_empty() {
            return Ok(items);
        }
        let (meta, index, write) = (self.storage.meta.clone(), self.index.clone(), self.write.clone());
        self.storage
            .run(async move {
                let _w = write.lock_owned().await;
                commit(&meta, &index, &items).await?;
                Ok(items)
            })
            .await
    }

    /// Replaces the fields given on an existing item.
    pub async fn patch(&self, id: &str, patch: Value) -> Result<Feedback, FeedbackError> {
        let (meta, index, write) = (self.storage.meta.clone(), self.index.clone(), self.write.clone());
        let id = canonical_id(id);
        self.storage
            .run(async move {
                let _w = write.lock_owned().await;
                let old = get(&meta, &id).await?.ok_or(FeedbackError::NotFound(id))?;
                let new = old.patched(patch, Ts::now())?;
                commit(&meta, &index, std::slice::from_ref(&new)).await?;
                Ok(new)
            })
            .await
    }

    /// Every stored item for these runs, by run then oldest first.
    pub async fn for_runs(&self, runs: Vec<RunId>) -> Result<Vec<Feedback>, FeedbackError> {
        let meta = self.storage.meta.clone();
        let ids: Vec<String> = runs.iter().map(RunId::to_string).collect();
        let raws = self
            .storage
            .run(async move { meta.feedback_for_runs(&ids).await })
            .await?;
        raws.iter().map(|r| Feedback::from_raw(r)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RUN: &str = "00000000-0000-4000-8000-000000000001";

    #[test]
    fn upload_keeps_unknown_fields_and_fills_times() {
        let now = Ts(1_000);
        let f = Feedback::from_upload(
            json!({"id": "f", "run_id": RUN, "key": "k", "score": 1, "extend_trace_retention": true,
                   "created_at": "2026-11-06T08:10:00Z", "modified_at": "1999-01-01T00:00:00Z"}),
            None,
            now,
        )
        .unwrap();
        assert_eq!(f.score, Some(1.0));
        assert_eq!(f.created_at, Ts::parse("2026-11-06T08:10:00Z").unwrap());
        assert_eq!(f.modified_at, now);
        assert_eq!(
            f.extra,
            json!({"extend_trace_retention": true}).as_object().unwrap().clone()
        );
        let back: Feedback = serde_json::from_value(serde_json::to_value(&f).unwrap()).unwrap();
        assert_eq!(back, f, "round trip through raw");
    }

    #[test]
    fn the_index_mean_is_per_run_and_key_and_a_replaced_id_moves() {
        let (a, b) = (RunId::from_u128(1), RunId::from_u128(2));
        let mut ix = FeedbackIndex::default();
        ix.insert("1", a, "k", Some(0.0));
        ix.insert("2", a, "k", Some(1.0));
        ix.insert("3", a, "k", None);
        ix.insert("4", a, "other", Some(0.25));
        assert_eq!(ix.mean(&a, "k"), Some(0.5));
        assert_eq!(ix.mean(&a, "nope"), None);
        ix.insert("2", b, "k", Some(1.0));
        assert_eq!(ix.mean(&a, "k"), Some(0.0));
        assert_eq!(ix.mean(&b, "k"), Some(1.0));
        assert_eq!(ix.len(), 4);
        assert_eq!(
            ix.keys(&|r| *r == a),
            vec![
                KeyStat {
                    key: "k".into(),
                    count: 2,
                    avg: Some(0.0)
                },
                KeyStat {
                    key: "other".into(),
                    count: 1,
                    avg: Some(0.25)
                },
            ]
        );
        ix.insert("4", b, "other", None);
        ix.insert("1", b, "k", None);
        ix.insert("3", b, "k", None);
        assert!(ix.scores(&a).is_empty());
        assert_eq!(ix.run_ids(), vec![b]);
    }
}
