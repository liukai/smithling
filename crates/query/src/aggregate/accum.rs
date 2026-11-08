//! Mergeable per-group accumulators and the cells they finish into (data-model §7, research M3-R5). Pure: no I/O.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Serialize;
use smithling_model::Status;
use smithling_model::agg::{Metric, Pct, SumOf};
use smithling_model::cost::Cost;

/// Latency values kept per query before percentiles fall back to the histogram.
pub const VALUE_CAP: usize = 4_000_000;
/// Log-latency bins: `[0, 10 ms)`, then 8 per doubling from 10 ms, the last open-ended from 1 h.
pub const BINS: usize = 150;

/// Lower bound of each bin, in ms.
pub static BIN_LOWER_MS: LazyLock<[i64; BINS]> = LazyLock::new(|| {
    let mut b = [0i64; BINS];
    for (i, v) in b.iter_mut().enumerate().skip(1) {
        *v = (10.0 * 2f64.powf((i - 1) as f64 / 8.0)).ceil() as i64;
    }
    b
});

pub fn bin_of(ms: i64) -> usize {
    BIN_LOWER_MS.partition_point(|&l| l <= ms).saturating_sub(1)
}

/// What one run adds to a group.
#[derive(Clone, Debug)]
pub struct Facts {
    pub status: Status,
    pub latency_ms: Option<i64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// `None` for runs that aren't model calls.
    pub cost: Option<Cost>,
    /// The run's mean score per feedback key.
    pub feedback: Vec<(String, f64)>,
    /// `start_time`, microseconds (read only for time series).
    pub start_us: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Sum {
    total: u64,
    n: u64,
}

impl Sum {
    fn add(&mut self, v: Option<u64>) {
        if let Some(v) = v {
            self.total = self.total.saturating_add(v);
            self.n += 1;
        }
    }
    fn merge(&mut self, o: &Sum) {
        self.total = self.total.saturating_add(o.total);
        self.n += o.n;
    }
    fn get(self) -> Option<u64> {
        (self.n > 0).then_some(self.total)
    }
}

/// How a group keeps latencies: not at all (no percentile asked for), every value, or bin counts only (past
/// the value cap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    Off,
    Values,
    Hist,
}

#[derive(Clone, Debug, PartialEq)]
enum Lat {
    Off,
    Values(Vec<i64>),
    Hist(Box<[u32; BINS]>),
}

impl Lat {
    fn hist_of(values: &[i64]) -> Box<[u32; BINS]> {
        let mut h = Box::new([0; BINS]);
        for &ms in values {
            h[bin_of(ms)] += 1;
        }
        h
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Acc {
    pub runs: u64,
    pub finished: u64,
    pub errors: u64,
    pub running: u64,
    sum_in: Sum,
    sum_out: Sum,
    sum_total: Sum,
    cost_pico: i64,
    priced: u64,
    no_price_runs: u64,
    n_latency: u64,
    lat: Lat,
    feedback: BTreeMap<String, (f64, u64)>,
}

impl Acc {
    pub fn new(keep: Keep) -> Acc {
        Acc {
            runs: 0,
            finished: 0,
            errors: 0,
            running: 0,
            sum_in: Sum::default(),
            sum_out: Sum::default(),
            sum_total: Sum::default(),
            cost_pico: 0,
            priced: 0,
            no_price_runs: 0,
            n_latency: 0,
            lat: match keep {
                Keep::Off => Lat::Off,
                Keep::Values => Lat::Values(Vec::new()),
                Keep::Hist => Lat::Hist(Box::new([0; BINS])),
            },
            feedback: BTreeMap::new(),
        }
    }

    /// Adds one run; returns whether a latency value was kept.
    pub fn add(&mut self, f: &Facts) -> bool {
        self.runs += 1;
        match f.status {
            Status::Running => self.running += 1,
            Status::Error => {
                self.errors += 1;
                self.finished += 1;
            }
            Status::Ok => self.finished += 1,
        }
        self.sum_in.add(f.input_tokens);
        self.sum_out.add(f.output_tokens);
        self.sum_total.add(f.total_tokens);
        match f.cost {
            Some(Cost::Reported(c) | Cost::Computed(c)) => {
                self.cost_pico = self.cost_pico.saturating_add(c);
                self.priced += 1;
            }
            Some(Cost::NoPrice) => self.no_price_runs += 1,
            None => {}
        }
        for (k, v) in &f.feedback {
            let e = self.feedback.entry(k.clone()).or_default();
            e.0 += v;
            e.1 += 1;
        }
        let Some(ms) = f.latency_ms.filter(|_| f.status != Status::Running) else {
            return false;
        };
        self.n_latency += 1;
        match &mut self.lat {
            Lat::Values(v) => {
                v.push(ms);
                true
            }
            Lat::Hist(h) => {
                h[bin_of(ms)] += 1;
                false
            }
            Lat::Off => false,
        }
    }

    pub fn merge(&mut self, o: &Acc) {
        self.runs += o.runs;
        self.finished += o.finished;
        self.errors += o.errors;
        self.running += o.running;
        self.sum_in.merge(&o.sum_in);
        self.sum_out.merge(&o.sum_out);
        self.sum_total.merge(&o.sum_total);
        self.cost_pico = self.cost_pico.saturating_add(o.cost_pico);
        self.priced += o.priced;
        self.no_price_runs += o.no_price_runs;
        self.n_latency += o.n_latency;
        match (&mut self.lat, &o.lat) {
            (Lat::Off, _) | (_, Lat::Off) => self.lat = Lat::Off,
            (Lat::Values(a), Lat::Values(b)) => a.extend_from_slice(b),
            (mine, theirs) => {
                let mut h = match std::mem::replace(mine, Lat::Off) {
                    Lat::Values(v) => Lat::hist_of(&v),
                    Lat::Hist(h) => h,
                    Lat::Off => unreachable!("handled above"),
                };
                let add = match theirs {
                    Lat::Values(v) => Lat::hist_of(v),
                    Lat::Hist(h) => h.clone(),
                    Lat::Off => unreachable!("handled above"),
                };
                for (a, b) in h.iter_mut().zip(add.iter()) {
                    *a += b;
                }
                *mine = Lat::Hist(h);
            }
        }
        for (k, (s, n)) in &o.feedback {
            let e = self.feedback.entry(k.clone()).or_default();
            e.0 += s;
            e.1 += n;
        }
    }

    /// Keeps bin counts instead of values (past the value cap).
    pub fn drop_values(&mut self) {
        if let Lat::Values(v) = &self.lat {
            self.lat = Lat::Hist(Lat::hist_of(v));
        }
    }

    pub fn finish(mut self, metrics: &[Metric]) -> Cells {
        let has = |m: &Metric| metrics.contains(m);
        let mut c = Cells {
            runs: self.runs,
            finished: self.finished,
            running: self.running,
            errors: self.errors,
            n_latency: self.n_latency,
            no_price_runs: self.no_price_runs,
            ..Cells::default()
        };
        if has(&Metric::ErrorRate) && self.finished > 0 {
            c.error_rate = Some(self.errors as f64 / self.finished as f64);
        }
        for p in Pct::ALL.into_iter().filter(|p| has(&Metric::Latency(*p))) {
            let v = self.percentile(p);
            match p {
                Pct::P50 => c.p50 = v,
                Pct::P95 => c.p95 = v,
                Pct::P99 => c.p99 = v,
            }
            c.approximate |= matches!(self.lat, Lat::Hist(_)) && self.n_latency > 0;
        }
        for s in SumOf::ALL.into_iter().filter(|s| has(&Metric::Sum(*s))) {
            match s {
                SumOf::InputTokens => c.input_tokens = self.sum_in.get(),
                SumOf::OutputTokens => c.output_tokens = self.sum_out.get(),
                SumOf::TotalTokens => c.total_tokens = self.sum_total.get(),
                SumOf::Cost => c.cost_pico = (self.priced > 0).then_some(self.cost_pico),
            }
        }
        if let Some(Metric::AvgFeedback(k)) = metrics.iter().find(|m| matches!(m, Metric::AvgFeedback(_)))
            && let Some((sum, n)) = self.feedback.get(k)
        {
            c.feedback_n = *n;
            c.feedback_avg = Some(sum / *n as f64);
        }
        c
    }

    /// Nearest rank: the ⌈p·N⌉-th smallest latency of the finished runs.
    fn percentile(&mut self, p: Pct) -> Option<i64> {
        let n = self.n_latency as usize;
        if n == 0 {
            return None;
        }
        let per = match p {
            Pct::P50 => 50,
            Pct::P95 => 95,
            Pct::P99 => 99,
        };
        let rank = (n * per).div_ceil(100).max(1);
        match &mut self.lat {
            Lat::Off => None,
            Lat::Values(v) => Some(*v.select_nth_unstable(rank - 1).1),
            Lat::Hist(h) => {
                let mut seen = 0usize;
                let bin = h.iter().position(|&c| {
                    seen += c as usize;
                    seen >= rank
                })?;
                let lo = BIN_LOWER_MS[bin];
                Some(match BIN_LOWER_MS.get(bin + 1) {
                    Some(&hi) if bin > 0 => ((lo as f64) * (hi as f64)).sqrt().round() as i64,
                    Some(&hi) => hi / 2,
                    None => lo,
                })
            }
        }
    }
}

/// One group's figures. Missing values are `null`, never 0; metrics not asked for are `null` too.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Cells {
    pub runs: u64,
    pub finished: u64,
    pub running: u64,
    pub errors: u64,
    pub error_rate: Option<f64>,
    /// Milliseconds, nearest rank.
    pub p50: Option<i64>,
    pub p95: Option<i64>,
    pub p99: Option<i64>,
    pub n_latency: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    /// Picodollars; `null` when no run had a price.
    pub cost_pico: Option<i64>,
    pub no_price_runs: u64,
    pub feedback_avg: Option<f64>,
    pub feedback_n: u64,
    pub estimated: bool,
    /// Percentiles came from the histogram (past the value cap).
    pub approximate: bool,
}

impl Cells {
    /// An estimate from a sample of `1 / k` of the rows: counts and sums times `k`; rates, percentiles and
    /// averages as sampled.
    pub fn scaled(self, k: f64) -> Cells {
        let n = |v: u64| (v as f64 * k).round() as u64;
        Cells {
            runs: n(self.runs),
            finished: n(self.finished),
            running: n(self.running),
            errors: n(self.errors),
            n_latency: n(self.n_latency),
            input_tokens: self.input_tokens.map(n),
            output_tokens: self.output_tokens.map(n),
            total_tokens: self.total_tokens.map(n),
            cost_pico: self.cost_pico.map(|c| (c as f64 * k).round() as i64),
            no_price_runs: n(self.no_price_runs),
            feedback_n: n(self.feedback_n),
            estimated: true,
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(status: Status, latency_ms: Option<i64>) -> Facts {
        Facts {
            status,
            latency_ms,
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cost: None,
            feedback: vec![],
            start_us: None,
        }
    }

    fn all_metrics() -> Vec<Metric> {
        let mut m = vec![Metric::Count, Metric::ErrorRate, Metric::AvgFeedback("k".into())];
        m.extend(Pct::ALL.map(Metric::Latency));
        m.extend(SumOf::ALL.map(Metric::Sum));
        m
    }

    #[test]
    fn nearest_rank_over_finished_runs() {
        let mut a = Acc::new(Keep::Values);
        for ms in 1..=20 {
            a.add(&run(Status::Ok, Some(ms * 1000)));
        }
        // A running run's latency (none yet) never counts, nor does it enter N.
        a.add(&run(Status::Running, None));
        let c = a.finish(&all_metrics());
        assert_eq!(c.n_latency, 20);
        // ⌈0.5·20⌉ = 10, ⌈0.95·20⌉ = 19, ⌈0.99·20⌉ = 20.
        assert_eq!((c.p50, c.p95, c.p99), (Some(10_000), Some(19_000), Some(20_000)));
        assert!(!c.approximate);

        let mut one = Acc::new(Keep::Values);
        one.add(&run(Status::Error, Some(7)));
        let c = one.finish(&all_metrics());
        assert_eq!((c.p50, c.p95, c.p99), (Some(7), Some(7), Some(7)));
    }

    #[test]
    fn error_rate_is_errors_over_finished_with_running_apart() {
        let mut a = Acc::new(Keep::Values);
        for _ in 0..42 {
            a.add(&run(Status::Error, Some(31_000)));
        }
        for _ in 0..258 {
            a.add(&run(Status::Ok, Some(1_000)));
        }
        for _ in 0..5 {
            a.add(&run(Status::Running, None));
        }
        let c = a.finish(&[Metric::ErrorRate]);
        assert_eq!((c.runs, c.finished, c.errors, c.running), (305, 300, 42, 5));
        assert_eq!(c.error_rate, Some(0.14));
        assert_eq!(c.p95, None, "not asked for");
    }

    #[test]
    fn missing_values_are_null_never_zero() {
        let mut a = Acc::new(Keep::Values);
        a.add(&run(Status::Running, None));
        let c = a.finish(&all_metrics());
        assert_eq!(c.error_rate, None, "no finished run");
        assert_eq!((c.p50, c.p95, c.p99), (None, None, None));
        assert_eq!((c.input_tokens, c.output_tokens, c.total_tokens), (None, None, None));
        assert_eq!(c.cost_pico, None);
        assert_eq!(c.feedback_avg, None);
        let c = Acc::new(Keep::Values).finish(&all_metrics());
        assert_eq!((c.runs, c.error_rate, c.p95), (0, None, None));
    }

    #[test]
    fn cost_is_null_when_no_run_had_a_price() {
        let mut a = Acc::new(Keep::Values);
        let mut f = run(Status::Ok, Some(1));
        f.cost = Some(Cost::NoPrice);
        a.add(&f);
        a.add(&f);
        let c = a.clone().finish(&[Metric::Sum(SumOf::Cost)]);
        assert_eq!((c.cost_pico, c.no_price_runs), (None, 2));
        f.cost = Some(Cost::Reported(5));
        a.add(&f);
        f.cost = Some(Cost::Computed(7));
        a.add(&f);
        f.cost = Some(Cost::Computed(0));
        a.add(&f);
        let c = a.finish(&[Metric::Sum(SumOf::Cost)]);
        assert_eq!((c.cost_pico, c.no_price_runs), (Some(12), 2));
    }

    #[test]
    fn past_the_cap_percentiles_come_from_the_histogram() {
        let mut a = Acc::new(Keep::Values);
        let mut b = Acc::new(Keep::Values);
        for ms in 1..=1000 {
            a.add(&run(Status::Ok, Some(ms * 37)));
            b.add(&run(Status::Ok, Some(ms * 37)));
        }
        b.drop_values();
        let exact = a.finish(&all_metrics());
        let approx = b.finish(&all_metrics());
        assert!(approx.approximate && !exact.approximate);
        assert_eq!(approx.n_latency, exact.n_latency);
        for (e, x) in [
            (exact.p50, approx.p50),
            (exact.p95, approx.p95),
            (exact.p99, approx.p99),
        ] {
            let (e, x) = (e.unwrap() as f64, x.unwrap() as f64);
            assert!((x - e).abs() / e < 0.05, "{x} vs {e}");
        }
        // Dropped values stay dropped when merged.
        let mut m = Acc::new(Keep::Values);
        let mut d = Acc::new(Keep::Hist);
        d.add(&run(Status::Ok, Some(5)));
        m.merge(&d);
        assert!(m.finish(&[Metric::Latency(Pct::P50)]).approximate);
    }

    #[test]
    fn without_percentiles_no_latencies_are_kept() {
        let mut a = Acc::new(Keep::Off);
        a.add(&run(Status::Ok, Some(5)));
        let mut b = Acc::new(Keep::Off);
        b.merge(&a);
        assert_eq!(b.lat, Lat::Off);
        let c = b.finish(&[Metric::Count]);
        assert_eq!((c.runs, c.n_latency, c.p50, c.approximate), (1, 1, None, false));
    }

    #[test]
    fn an_estimate_scales_counts_and_sums_not_rates_or_percentiles() {
        let mut a = Acc::new(Keep::Values);
        for i in 0..10 {
            let mut f = run(if i < 3 { Status::Error } else { Status::Ok }, Some(100 * (i + 1)));
            f.input_tokens = Some(7);
            f.cost = Some(Cost::Computed(5));
            a.add(&f);
        }
        let mut m = all_metrics();
        m.retain(|m| *m != Metric::AvgFeedback("k".into()));
        let c = a.finish(&m);
        let e = c.clone().scaled(4.0);
        assert!(e.estimated && !c.estimated);
        assert_eq!((e.runs, e.finished, e.errors, e.n_latency), (40, 40, 12, 40));
        assert_eq!((e.input_tokens, e.cost_pico), (Some(280), Some(200)));
        assert_eq!((e.error_rate, e.p50, e.p95, e.p99), (c.error_rate, c.p50, c.p95, c.p99));
        assert_eq!(c.clone().scaled(1.0), Cells { estimated: true, ..c });
    }

    #[test]
    fn bins_cover_10ms_to_an_hour() {
        assert_eq!(bin_of(0), 0);
        assert_eq!(bin_of(9), 0);
        assert_eq!(bin_of(10), 1);
        assert_eq!(BIN_LOWER_MS[9], 20, "8 bins per doubling");
        assert!(BIN_LOWER_MS[BINS - 1] >= 3_600_000 && BIN_LOWER_MS[BINS - 2] < 3_600_000);
        assert_eq!(bin_of(i64::MAX), BINS - 1);
        for ms in [10, 11, 99, 1_000, 31_000, 3_599_999] {
            let b = bin_of(ms);
            assert!(BIN_LOWER_MS[b] <= ms && ms < BIN_LOWER_MS[b + 1], "{ms}");
        }
    }

    #[test]
    fn merging_is_associative_and_matches_one_pass() {
        let facts: Vec<Facts> = (0..60)
            .map(|i| Facts {
                status: [Status::Ok, Status::Error, Status::Running][i % 3],
                latency_ms: (i % 3 != 2).then_some((i * 977 % 5000) as i64),
                input_tokens: (i % 4 == 0).then_some(i as u64),
                output_tokens: (i % 5 == 0).then_some(2 * i as u64),
                total_tokens: (i % 4 == 0).then_some(3 * i as u64),
                cost: [
                    None,
                    Some(Cost::NoPrice),
                    Some(Cost::Computed(i as i64)),
                    Some(Cost::Reported(1)),
                ][i % 4],
                feedback: if i % 7 == 0 {
                    vec![("k".into(), i as f64 / 8.0)]
                } else {
                    vec![]
                },
                start_us: None,
            })
            .collect();
        let of = |fs: &[Facts]| {
            let mut a = Acc::new(Keep::Values);
            for f in fs {
                a.add(f);
            }
            a
        };
        let (x, y, z) = (of(&facts[..17]), of(&facts[17..40]), of(&facts[40..]));
        let mut left = x.clone();
        left.merge(&y);
        left.merge(&z);
        let mut yz = y.clone();
        yz.merge(&z);
        let mut right = x.clone();
        right.merge(&yz);
        let m = all_metrics();
        let (l, r, one) = (left.finish(&m), right.finish(&m), of(&facts).finish(&m));
        assert_eq!(l, r);
        assert_eq!(l, one);
        assert_eq!(one.feedback_n, 9);
        assert!(one.feedback_avg.is_some() && one.cost_pico.is_some() && one.input_tokens.is_some());
    }
}
