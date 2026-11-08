//! Time series and latency distribution (design §4.5, research M3-R7): per group, one cell per `start_time`
//! bucket on a round step, or one count per fixed log-latency bin. Runs are kept per minute and bucketed at
//! the end, once the span (and so the step) is known.

use std::collections::HashMap;

use serde::Serialize;
use smithling_model::Ts;
use smithling_model::agg::{Bucket, Metric};
use smithling_model::filter::{CmpOp, Expr, Field, Path, TimeAt, Value, print};

use super::accum::{Acc, BIN_LOWER_MS, BINS, Cells, Keep};
use super::{Accs, KeyVal};

pub const MINUTE_US: i64 = 60_000_000;
/// Steps `bucket: auto` picks from, in minutes; the smallest giving at most [`AUTO_BUCKETS`] buckets.
pub const AUTO_STEPS_MIN: [i64; 14] = [1, 2, 5, 10, 15, 30, 60, 120, 180, 360, 720, 1440, 2880, 10080];
pub const AUTO_BUCKETS: i64 = 100;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Series {
    pub step_ms: i64,
    /// Buckets are offsets from each side's start (a comparison), not times.
    pub offset: bool,
    pub buckets: Vec<SeriesBucket>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SeriesBucket {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t: Option<Ts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_ms: Option<i64>,
    /// One per group, in `groups` order (the totals when not grouped); `null` where the group has no runs.
    pub per_group: Vec<Option<Cells>>,
    /// `and(gte(start_time, t0), lt(start_time, t1))`.
    #[serde(rename = "where")]
    pub where_: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Dist {
    /// Lower bound of each bin shown (a contiguous run of the fixed bins), in ms.
    pub bins_ms: Vec<i64>,
    /// Per group (in `groups` order, or the totals), finished runs per bin.
    pub per_group: Vec<Vec<u64>>,
    /// `and(gte(latency, a), lt(latency, b))` in seconds, as filters write latency.
    pub where_per_bin: Vec<String>,
}

fn step_of(b: Bucket) -> Option<i64> {
    Some(match b {
        Bucket::Auto => return None,
        Bucket::M1 => 1,
        Bucket::M5 => 5,
        Bucket::M15 => 15,
        Bucket::H1 => 60,
        Bucket::H6 => 360,
        Bucket::D1 => 1440,
    })
}

/// A given step past this many buckets is coarsened to the smallest auto step that fits.
pub const MAX_BUCKETS: i64 = 1000;

/// The step in minutes: the given one, or the smallest auto step giving at most `limit` buckets (`buckets(s)`).
fn pick_step(b: Bucket, buckets: impl Fn(i64) -> i64) -> i64 {
    let fits = |s: i64, limit: i64| buckets(s) <= limit;
    if let Some(s) = step_of(b).filter(|&s| fits(s, MAX_BUCKETS)) {
        return s;
    }
    let limit = if step_of(b).is_some() {
        MAX_BUCKETS
    } else {
        AUTO_BUCKETS
    };
    let floor = step_of(b).unwrap_or(1);
    AUTO_STEPS_MIN
        .into_iter()
        .filter(|&s| s >= floor)
        .chain((1..).map(|d| AUTO_STEPS_MIN[AUTO_STEPS_MIN.len() - 1] + d * 1440))
        .find(|&s| fits(s, limit))
        .expect("a step fits")
}

/// Step in minutes for a span of `span_min` minutes counted from its start.
pub(crate) fn step_min(b: Bucket, span_min: i64) -> i64 {
    pick_step(b, |s| span_min / s + 1)
}

/// Where one side's buckets start and how many there are, in minutes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Axis {
    pub step: i64,
    pub first: i64,
    pub n: i64,
    pub offset: bool,
}

/// A side's span in minutes: `[lo, hi]`, the window where given, else its runs' first and last start.
pub(crate) fn span(accs: &Accs, from: Option<i64>, to: Option<i64>) -> Option<(i64, i64)> {
    let lo = from.or(accs.first)?;
    let hi = to.or(accs.last)?.max(lo);
    Some((lo.div_euclid(MINUTE_US), hi.div_euclid(MINUTE_US)))
}

/// One axis per side. Alone, buckets are whole steps of UTC time; compared, each side counts from its own
/// start and both sides get the same number of buckets.
pub(crate) fn axes(b: Bucket, spans: &[Option<(i64, i64)>], offset: bool) -> Vec<Option<Axis>> {
    let longest = spans.iter().flatten().map(|(lo, hi)| hi - lo).max().unwrap_or(0);
    // Alone, the first bucket starts on a whole step at or before the first run.
    let step = match (offset, spans.iter().flatten().next()) {
        (false, Some(&(lo, hi))) => pick_step(b, |s| (hi - lo.div_euclid(s) * s) / s + 1),
        _ => step_min(b, longest),
    };
    let n = longest / step + 1;
    spans
        .iter()
        .map(|s| {
            let (lo, hi) = (*s)?;
            Some(if offset {
                Axis {
                    step,
                    first: lo,
                    n,
                    offset,
                }
            } else {
                let first = lo.div_euclid(step) * step;
                Axis {
                    step,
                    first,
                    n: (hi - first) / step + 1,
                    offset,
                }
            })
        })
        .collect()
}

fn time(minute: i64) -> (Ts, Value) {
    let ts = Ts(minute * MINUTE_US);
    (
        ts,
        Value::Time {
            raw: ts.to_string(),
            at: TimeAt::Abs(ts),
        },
    )
}

fn cond(path: Path, lo: Value, hi: Option<Value>) -> String {
    let gte = Expr::Cmp {
        op: CmpOp::Gte,
        path: path.clone(),
        value: lo,
    };
    print(&match hi {
        None => gte,
        Some(value) => Expr::And(vec![
            gte,
            Expr::Cmp {
                op: CmpOp::Lt,
                path,
                value,
            },
        ]),
    })
}

/// Raw key → the output groups (indexes) its runs are in.
pub(crate) type Members = HashMap<Vec<KeyVal>, Vec<usize>>;

pub(crate) fn series(
    slots: &HashMap<Vec<KeyVal>, HashMap<i64, Acc>>,
    members: &Members,
    groups: usize,
    axis: Axis,
    metrics: &[Metric],
    keep: Keep,
    scale: Option<f64>,
) -> Series {
    let n = axis.n.max(0) as usize;
    let mut accs: Vec<Vec<Option<Acc>>> = vec![vec![None; groups]; n];
    for (raw, per) in slots {
        let gs = members.get(raw).map_or(&[][..], Vec::as_slice);
        for (minute, acc) in per {
            let b = (minute - axis.first).div_euclid(axis.step);
            if b < 0 || b as usize >= n {
                continue;
            }
            for &g in gs {
                accs[b as usize][g].get_or_insert_with(|| Acc::new(keep)).merge(acc);
            }
        }
    }
    let buckets = accs
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let start = axis.first + i as i64 * axis.step;
            let (t0, v0) = time(start);
            let (_, v1) = time(start + axis.step);
            SeriesBucket {
                t: (!axis.offset).then_some(t0),
                offset_ms: axis.offset.then_some(i as i64 * axis.step * 60_000),
                per_group: row
                    .into_iter()
                    .map(|a| {
                        let c = a?.finish(metrics);
                        Some(scale.map_or(c.clone(), |k| c.scaled(k)))
                    })
                    .collect(),
                where_: cond(Path::Field(Field::StartTime), v0, Some(v1)),
            }
        })
        .collect();
    Series {
        step_ms: axis.step * 60_000,
        offset: axis.offset,
        buckets,
    }
}

/// Per group, finished runs per fixed bin.
pub(crate) fn bin_counts(
    bins: &HashMap<Vec<KeyVal>, Box<[u64; BINS]>>,
    members: &Members,
    groups: usize,
) -> Vec<[u64; BINS]> {
    let mut out = vec![[0u64; BINS]; groups];
    for (raw, counts) in bins {
        for &g in members.get(raw).into_iter().flatten() {
            for (o, c) in out[g].iter_mut().zip(counts.iter()) {
                *o += c;
            }
        }
    }
    out
}

/// The bins from the first to the last that any side has a run in.
pub(crate) fn bin_range(sides: &[&[[u64; BINS]]]) -> Option<(usize, usize)> {
    let used = |b: usize| sides.iter().any(|s| s.iter().any(|g| g[b] > 0));
    let lo = (0..BINS).find(|&b| used(b))?;
    let hi = (0..BINS).rev().find(|&b| used(b))?;
    Some((lo, hi))
}

pub(crate) fn dist(counts: &[[u64; BINS]], range: Option<(usize, usize)>, scale: Option<f64>) -> Dist {
    let Some((lo, hi)) = range else {
        return Dist {
            bins_ms: vec![],
            per_group: vec![vec![]; counts.len()],
            where_per_bin: vec![],
        };
    };
    let secs = |ms: i64| Value::Num(ms as f64 / 1000.0);
    Dist {
        bins_ms: BIN_LOWER_MS[lo..=hi].to_vec(),
        per_group: counts
            .iter()
            .map(|g| {
                g[lo..=hi]
                    .iter()
                    .map(|&c| scale.map_or(c, |k| (c as f64 * k).round() as u64))
                    .collect()
            })
            .collect(),
        where_per_bin: (lo..=hi)
            .map(|b| {
                cond(
                    Path::Field(Field::Latency),
                    secs(BIN_LOWER_MS[b]),
                    BIN_LOWER_MS.get(b + 1).map(|&u| secs(u)),
                )
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_steps_give_about_a_hundred_round_buckets() {
        for (span, want) in [
            (0, 1),
            (99, 1),
            (100, 2),
            (12 * 60, 10),
            (23 * 60, 15),
            (30 * 60, 30),
            (7 * 1440, 120),
        ] {
            let s = step_min(Bucket::Auto, span);
            assert_eq!(s, want, "span {span} min");
            let n = span / s + 1;
            assert!(n <= AUTO_BUCKETS && (span < 100 || n > AUTO_BUCKETS / 3), "{span}: {n}");
        }
        assert_eq!(step_min(Bucket::M5, 4_000), 5);
        assert_eq!(
            step_min(Bucket::M1, 30 * 1440),
            60,
            "a given step past MAX_BUCKETS is coarsened"
        );
        assert_eq!(step_min(Bucket::Auto, 400 * 1440) % 1440, 0);
        // 995 minutes take 100 ten-minute buckets, but 101 once the first starts on a whole step.
        let a = axes(Bucket::Auto, &[Some((655, 1650))], false)[0].unwrap();
        assert_eq!((a.step, a.first), (15, 645));
        assert!(a.n <= AUTO_BUCKETS, "{a:?}");
    }

    #[test]
    fn alone_buckets_are_whole_steps_compared_they_start_at_each_side() {
        let a = axes(Bucket::M15, &[Some((10 * 60 + 7, 12 * 60))], false)[0].unwrap();
        assert_eq!((a.first, a.n), (10 * 60, 9));
        let ab = axes(
            Bucket::Auto,
            &[Some((0, 12 * 60)), Some((1000, 1000 + 10 * 60)), None],
            true,
        );
        let (x, y) = (ab[0].unwrap(), ab[1].unwrap());
        assert_eq!((x.step, x.first, x.n), (10, 0, 73));
        assert_eq!((y.step, y.first, y.n), (10, 1000, 73));
        assert!(ab[2].is_none());
    }
}
