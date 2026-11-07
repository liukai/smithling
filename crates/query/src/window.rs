//! Start-time windows for a list ordered by start time: read the newest (or oldest) rows first and stop once
//! the page is full. Windows are disjoint ranges of `start_key`, so a run's versions (which all share its
//! `dotted_order`) always fall in the same window, and every run in a later window sorts after every run in
//! an earlier one.

use tracedb_storage::segment::SegmentMeta;

/// Estimated rows in the first window; each next one is four times larger.
pub const FIRST_WINDOW_ROWS: usize = 20_000;

pub struct Windows {
    /// (min key, max key, rows) per file; rows are assumed spread evenly over the range.
    files: Vec<(i64, i64, f64)>,
    /// Memtable start keys, ascending.
    mem: Vec<i64>,
    lo: i64,
    hi: i64,
    ascending: bool,
    /// Edge of the next window (its upper bound when descending, lower when ascending); `None` when done.
    next: Option<i64>,
    target: f64,
}

impl Windows {
    pub fn new<'a>(
        files: impl IntoIterator<Item = &'a SegmentMeta>,
        mem: Vec<i64>,
        from: Option<i64>,
        to: Option<i64>,
        ascending: bool,
        first: usize,
    ) -> Windows {
        debug_assert!(mem.is_sorted());
        let files: Vec<(i64, i64, f64)> = files
            .into_iter()
            .map(|s| (s.min_start_key, s.max_start_key, s.row_count as f64))
            .collect();
        let lo = files.iter().map(|f| f.0).chain(mem.first().copied()).min();
        let hi = files.iter().map(|f| f.1).chain(mem.last().copied()).max();
        let (lo, hi) = match (lo, hi) {
            (Some(l), Some(h)) => (from.map_or(l, |f| f.max(l)), to.map_or(h, |t| t.min(h))),
            _ => (0, -1),
        };
        let next = (lo <= hi).then_some(if ascending { lo } else { hi });
        Windows {
            files,
            mem,
            lo,
            hi,
            ascending,
            next,
            target: first.max(1) as f64,
        }
    }

    /// Estimated rows with a start key in `[a, b]`.
    fn rows(&self, a: i64, b: i64) -> f64 {
        let files: f64 = self
            .files
            .iter()
            .map(|&(min, max, n)| {
                let (l, h) = (a.max(min), b.min(max));
                if l > h {
                    0.0
                } else {
                    n * ((h as i128 - l as i128 + 1) as f64 / (max as i128 - min as i128 + 1) as f64)
                }
            })
            .sum();
        let mem = self.mem.partition_point(|k| *k <= b) - self.mem.partition_point(|k| *k < a);
        files + mem as f64
    }

    /// Every window has been handed out.
    pub fn done(&self) -> bool {
        self.next.is_none()
    }

    /// The next window `[a, b]`, inclusive, in reading order.
    pub fn next_window(&mut self) -> Option<(i64, i64)> {
        let edge = self.next?;
        let w = if self.ascending {
            // Smallest b with about `target` rows in [edge, b].
            let (mut l, mut h) = (edge, self.hi);
            while l < h {
                let m = l + ((h as i128 - l as i128) / 2) as i64;
                if self.rows(edge, m) >= self.target {
                    h = m
                } else {
                    l = m + 1
                }
            }
            self.next = (l < self.hi).then(|| l + 1);
            (edge, l)
        } else {
            // Largest a with about `target` rows in [a, edge].
            let (mut l, mut h) = (self.lo, edge);
            while l < h {
                let m = h - ((h as i128 - l as i128) / 2) as i64;
                if self.rows(m, edge) >= self.target {
                    l = m
                } else {
                    h = m - 1
                }
            }
            self.next = (l > self.lo).then(|| l - 1);
            (l, edge)
        };
        self.target *= 4.0;
        Some(w)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn seg(min: i64, max: i64, rows: u64) -> SegmentMeta {
        SegmentMeta {
            id: String::new(),
            project: String::new(),
            core_path: String::new(),
            payload_path: String::new(),
            min_start_key: min,
            max_start_key: max,
            min_trace_id: Default::default(),
            max_trace_id: Default::default(),
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

    fn all(mut w: Windows) -> Vec<(i64, i64)> {
        std::iter::from_fn(|| w.next_window()).collect()
    }

    proptest! {
        /// Windows are non-empty, disjoint, in reading order, and cover exactly the data range within the scope.
        #[test]
        fn windows_tile_the_range(
            files in prop::collection::vec((-50i64..50, 0i64..40, 1u64..500), 0..6),
            mut mem in prop::collection::vec(-60i64..60, 0..30),
            from in prop::option::of(-70i64..70),
            to in prop::option::of(-70i64..70),
            ascending: bool,
            first in 1usize..300,
        ) {
            mem.sort();
            let segs: Vec<SegmentMeta> = files.iter().map(|&(min, len, n)| seg(min, min + len, n)).collect();
            let lo = segs.iter().map(|s| s.min_start_key).chain(mem.first().copied()).min();
            let hi = segs.iter().map(|s| s.max_start_key).chain(mem.last().copied()).max();
            let mut ws = all(Windows::new(&segs, mem, from, to, ascending, first));
            if !ascending {
                ws.reverse();
            }
            let want = match (lo, hi) {
                (Some(l), Some(h)) => {
                    let (l, h) = (from.map_or(l, |f| f.max(l)), to.map_or(h, |t| t.min(h)));
                    (l <= h).then_some((l, h))
                }
                _ => None,
            };
            match want {
                None => prop_assert!(ws.is_empty()),
                Some((l, h)) => {
                    prop_assert_eq!(ws.first().unwrap().0, l);
                    prop_assert_eq!(ws.last().unwrap().1, h);
                    for w in &ws {
                        prop_assert!(w.0 <= w.1);
                    }
                    for p in ws.windows(2) {
                        prop_assert_eq!(p[0].1 + 1, p[1].0);
                    }
                }
            }
        }
    }

    #[test]
    fn first_window_holds_about_the_target_and_later_ones_grow() {
        // 10 files of 1,000 rows over keys 0..10,000, newest first.
        let segs: Vec<SegmentMeta> = (0..10).map(|i| seg(i * 1000, i * 1000 + 999, 1000)).collect();
        let ws = all(Windows::new(&segs, vec![], None, None, false, 500));
        assert_eq!(ws[0], (9500, 9999));
        assert_eq!(ws[1], (7500, 9499));
        assert_eq!(ws.last().unwrap().0, 0);
        let up = all(Windows::new(&segs, vec![], Some(2000), None, true, 500));
        assert_eq!(up[0], (2000, 2499));
    }

    #[test]
    fn memtable_rows_count_by_key() {
        let mem: Vec<i64> = (0..100).map(|k| 1_000 + k).collect();
        let ws = all(Windows::new(&[], mem, None, None, false, 10));
        assert_eq!(ws[0], (1090, 1099));
        let one = all(Windows::new(&[], vec![5; 50], None, None, false, 10));
        assert_eq!(one, vec![(5, 5)], "many rows at one key stay in one window");
    }
}
