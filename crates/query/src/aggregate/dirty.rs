//! Dirty side: multi-version runs (file rows merged once and kept across requests, D-023) plus memtable
//! versions, merged with the M1 rule, then every condition checked on the merged run.

use std::collections::{HashMap, HashSet};

use tracedb_model::attrs::Attrs;
use tracedb_model::filter::EvalStats;
use tracedb_model::{RunId, RunState};

use super::{Accs, AggStats, Plan, keys_of};
use crate::scan::{Pinned, merge_group};

pub(crate) struct Pin {
    pub pinned: Pinned,
    /// The multi-version set when `pinned` was taken.
    pub multi: HashSet<RunId>,
}

pub(crate) fn walk(
    pin: &Pin,
    files: &HashMap<RunId, (RunState, Attrs)>,
    plan: &Plan,
    accs: &mut Accs,
    stats: &mut AggStats,
    eval_stats: &mut EvalStats,
) {
    let mut mem: HashMap<RunId, Vec<(&RunState, &Attrs)>> = HashMap::new();
    for (_, st, attrs) in &pin.pinned.rows {
        mem.entry(st.run.run_id).or_default().push((st, attrs));
    }
    let mut add = |st: &RunState, attrs: &Attrs| {
        if plan.admits(&st.run, attrs, eval_stats) {
            stats.dirty_runs += 1;
            accs.add(keys_of(plan.keys, &st.run, attrs), &plan.facts(&st.run));
        }
    };
    let merged = |vs: Vec<(&RunState, &Attrs)>, file: Option<&(RunState, Attrs)>| {
        let owned = vs
            .into_iter()
            .map(|(s, a)| (s.clone(), a.clone()))
            .chain(file.cloned())
            .collect();
        merge_group(owned, true)
    };
    // Every kept id is in the set (it only grows); runs only in memory are walked below.
    for (id, file) in files {
        match mem.remove(id) {
            None => add(&file.0, &file.1),
            Some(vs) => {
                let m = merged(vs, Some(file));
                add(&m.state, &m.attrs);
            }
        }
    }
    for (_, vs) in mem {
        match vs.as_slice() {
            [(st, attrs)] => add(st, attrs),
            _ => {
                let m = merged(vs, None);
                add(&m.state, &m.attrs);
            }
        }
    }
}
