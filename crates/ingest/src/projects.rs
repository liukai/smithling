//! Recent runs' projects, so the flusher can file a late update that doesn't name its project (as the SDK
//! sends them) under its run's project. A version filed under project `""` is read by every project's queries.
//!
//! Bounded: the current generation (a hash map) and the one before (a sorted array, 20 B a run), each of up
//! to [`GENERATION`] runs, ≈20 MB in all; a run older than that is not found, and its update stays under `""`.
//! Not persisted: a start rebuilds it from the replayed runs and the newest files (D-024).

use std::collections::HashMap;

use tracedb_model::{RunId, RunState};

/// 7/8 of 2^19: a full generation's table has 2^19 buckets (D-028; M2 kept two of 1M, up to 88 MB).
pub const GENERATION: usize = 458_752;

#[derive(Debug)]
pub struct Projects {
    names: Vec<String>,
    ids: HashMap<String, u32>,
    cur: HashMap<RunId, u32>,
    /// The generation before, sorted by run.
    prev: Vec<(RunId, u32)>,
    generation: usize,
}

impl Default for Projects {
    fn default() -> Projects {
        Projects::new(GENERATION)
    }
}

impl Projects {
    pub fn new(generation: usize) -> Projects {
        Projects {
            names: Vec::new(),
            ids: HashMap::new(),
            cur: HashMap::new(),
            prev: Vec::new(),
            generation: generation.max(1),
        }
    }

    fn known(&self, run: &RunId) -> bool {
        self.cur.contains_key(run) || self.prev.binary_search_by_key(run, |e| e.0).is_ok()
    }

    /// Records the run's project unless one is known: the first version that names a project decides it
    /// (the merge rule keeps the first).
    pub fn note(&mut self, run: RunId, project: &str) {
        if self.known(&run) {
            return;
        }
        let id = self.intern(project);
        if self.cur.len() >= self.generation {
            let mut prev: Vec<(RunId, u32)> = std::mem::take(&mut self.cur).into_iter().collect();
            prev.sort_unstable_by_key(|e| e.0);
            self.prev = prev;
        }
        self.cur.insert(run, id);
    }

    /// Fills in older runs (read back from files) without displacing anything noted since.
    pub fn backfill<'a>(&mut self, older: impl IntoIterator<Item = (RunId, &'a str)>) {
        let mut add: Vec<(RunId, u32)> = Vec::new();
        for (run, project) in older {
            if self.prev.len() + add.len() >= self.generation {
                break;
            }
            if !self.known(&run) {
                let id = self.intern(project);
                add.push((run, id));
            }
        }
        // Within one batch the first wins, as across batches.
        add.sort_by_key(|e| e.0);
        add.dedup_by_key(|e| e.0);
        // Exactly: doubling would leave up to a generation of spare slots.
        self.prev.reserve_exact(add.len());
        self.prev.extend(add);
        self.prev.sort_by_key(|e| e.0);
    }

    fn intern(&mut self, project: &str) -> u32 {
        match self.ids.get(project) {
            Some(&id) => id,
            None => {
                let id = self.names.len() as u32;
                self.names.push(project.to_string());
                self.ids.insert(project.to_string(), id);
                id
            }
        }
    }

    /// The project a version is filed under: its own, else its run's if known, else `""`.
    pub fn filed<'a>(&'a self, st: &'a RunState) -> &'a str {
        if st.project_set {
            &st.run.project
        } else {
            self.get(&st.run.run_id).unwrap_or_default()
        }
    }

    pub fn get(&self, run: &RunId) -> Option<&str> {
        let id = match self.cur.get(run) {
            Some(id) => *id,
            None => self.prev[self.prev.binary_search_by_key(run, |e| e.0).ok()?].1,
        };
        Some(&self.names[id as usize])
    }

    pub fn len(&self) -> usize {
        self.cur.len() + self.prev.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Memory held by both generations' tables.
    pub fn heap_bytes(&self) -> usize {
        self.cur.capacity() * 8 / 7 * (size_of::<(RunId, u32)>() + 1) + self.prev.capacity() * size_of::<(RunId, u32)>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> RunId {
        uuid::Uuid::from_u128(n)
    }

    #[test]
    fn first_project_wins_and_old_runs_age_out() {
        let mut p = Projects::new(2);
        p.note(id(1), "a");
        p.note(id(1), "b");
        assert_eq!(p.get(&id(1)), Some("a"));
        p.note(id(2), "b");
        p.note(id(3), "b");
        assert_eq!(p.get(&id(1)), Some("a"), "previous generation still answers");
        p.note(id(4), "a");
        p.note(id(5), "a");
        assert_eq!(p.get(&id(1)), None);
        assert_eq!(p.get(&id(5)), Some("a"));
        assert!(p.len() <= 4);
    }

    #[test]
    fn backfilled_runs_are_found_until_two_generations_pass() {
        let mut p = Projects::new(3);
        p.note(id(10), "live");
        p.backfill([(id(1), "a"), (id(2), "b"), (id(10), "old"), (id(3), "c"), (id(4), "d")]);
        assert_eq!(p.get(&id(10)), Some("live"), "a run noted since keeps its project");
        assert_eq!((p.get(&id(1)), p.get(&id(3))), (Some("a"), Some("c")));
        assert_eq!(p.get(&id(4)), None, "past a generation");
        for n in 20..26 {
            p.note(id(n), "new");
        }
        assert_eq!(p.get(&id(1)), None);
        assert_eq!(p.get(&id(25)), Some("new"));
    }

    #[test]
    fn two_full_generations_fit_in_about_20_mb() {
        let mut p = Projects::default();
        for n in 0..2 * GENERATION as u128 {
            p.note(id(n), if n % 2 == 0 { "a" } else { "b" });
        }
        assert!(p.len() >= GENERATION);
        assert!(p.heap_bytes() <= 21 << 20, "{} B", p.heap_bytes());
    }
}
