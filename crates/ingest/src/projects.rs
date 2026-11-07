//! Recent runs' projects, so the flusher can file a late update that doesn't name its project (as the SDK
//! sends them) under its run's project. A version filed under project `""` is read by every project's queries.
//!
//! Bounded: two generations of [`GENERATION`] runs; a run older than that is not found, and its update stays
//! under `""`. Not persisted: a start rebuilds it from the replayed runs and the newest files (D-024).

use std::collections::HashMap;

use tracedb_model::{RunId, RunState};

pub const GENERATION: usize = 1 << 20;

#[derive(Debug)]
pub struct Projects {
    names: Vec<String>,
    ids: HashMap<String, u32>,
    cur: HashMap<RunId, u32>,
    prev: HashMap<RunId, u32>,
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
            prev: HashMap::new(),
            generation: generation.max(1),
        }
    }

    /// Records the run's project unless one is known: the first version that names a project decides it
    /// (the merge rule keeps the first).
    pub fn note(&mut self, run: RunId, project: &str) {
        if self.cur.contains_key(&run) || self.prev.contains_key(&run) {
            return;
        }
        let id = self.intern(project);
        if self.cur.len() >= self.generation {
            self.prev = std::mem::take(&mut self.cur);
        }
        self.cur.insert(run, id);
    }

    /// Fills in older runs (read back from files) without displacing anything noted since.
    pub fn backfill<'a>(&mut self, older: impl IntoIterator<Item = (RunId, &'a str)>) {
        for (run, project) in older {
            if self.prev.len() >= self.generation {
                break;
            }
            if self.cur.contains_key(&run) || self.prev.contains_key(&run) {
                continue;
            }
            let id = self.intern(project);
            self.prev.insert(run, id);
        }
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
        let id = self.cur.get(run).or_else(|| self.prev.get(run))?;
        Some(&self.names[*id as usize])
    }

    pub fn len(&self) -> usize {
        self.cur.len() + self.prev.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
}
