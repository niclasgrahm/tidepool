//! The statistics both executors compute alike (plan §6.8, item 3):
//! quantiles from counted values, and running moments for standard
//! deviations and variances.
//!
//! Quantiles are exact: every value is kept, counted, so repeats (prices,
//! quantities, codes) cost one entry each, and a group's memory is its
//! number of distinct values. Moments use Welford's update and Chan's
//! merge, which stay accurate where sums of squares cancel.

use hashbrown::HashMap;

use tidepool_expr::ir::AggFunc;

/// Values and how many times each was seen, keyed by the value's bits.
#[derive(Clone, Debug, Default)]
pub(crate) struct Counted(HashMap<u64, u64>);

/// The key of `value`: its bits, with -0 as 0 so the two count as one.
fn bits(value: f64) -> u64 {
    if value == 0.0 { 0.0f64 } else { value }.to_bits()
}

impl Counted {
    pub(crate) fn add(&mut self, value: f64) {
        if !value.is_nan() {
            *self.0.entry(bits(value)).or_insert(0) += 1;
        }
    }

    pub(crate) fn merge(&mut self, other: Self) {
        for (value, n) in other.0 {
            *self.0.entry(value).or_insert(0) += n;
        }
    }

    /// The value at `fraction` (in millionths) of the way through the
    /// sorted values, interpolated between the two around rank
    /// `fraction · (n − 1)`, as `PERCENTILE_CONT` (and DuckDB's
    /// `quantile_cont`) does. `None` with no values.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn quantile(&self, fraction: u32) -> Option<f64> {
        let mut values: Vec<(f64, u64)> = self
            .0
            .iter()
            .map(|(b, n)| (f64::from_bits(*b), *n))
            .collect();
        values.sort_by(|a, b| a.0.total_cmp(&b.0));
        let n: u64 = values.iter().map(|(_, c)| c).sum();
        if n == 0 {
            return None;
        }
        let rank = f64::from(fraction) / 1_000_000.0 * (n - 1) as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (lo, hi) = (rank.floor() as u64, rank.ceil() as u64);
        let at = |position: u64| {
            let mut seen = 0;
            for (value, count) in &values {
                seen += count;
                if position < seen {
                    return *value;
                }
            }
            values.last().map_or(f64::NAN, |v| v.0)
        };
        let (low, high) = (at(lo), at(hi));
        Some(low + (rank - lo as f64) * (high - low))
    }
}

/// A running count, mean and sum of squared differences from it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Moments {
    pub n: f64,
    pub mean: f64,
    pub m2: f64,
}

impl Moments {
    pub(crate) fn add(&mut self, value: f64) {
        self.n += 1.0;
        let delta = value - self.mean;
        self.mean += delta / self.n;
        self.m2 += delta * (value - self.mean);
    }

    pub(crate) fn merge(&mut self, other: Self) {
        if other.n == 0.0 {
            return;
        }
        if self.n == 0.0 {
            *self = other;
            return;
        }
        let n = self.n + other.n;
        let delta = other.mean - self.mean;
        self.mean += delta * other.n / n;
        self.m2 += other.m2 + delta * delta * self.n * other.n / n;
        self.n = n;
    }

    /// The moments of values all multiplied by `factor` (decimals kept as
    /// scaled integers are divided back by 10^scale).
    pub(crate) fn scaled(self, factor: f64) -> Self {
        Self {
            n: self.n,
            mean: self.mean * factor,
            m2: self.m2 * factor * factor,
        }
    }

    /// `func`'s value: a sample statistic needs two values, a population
    /// one one. `None` otherwise (NULL, as in SQL).
    pub(crate) fn finish(self, func: AggFunc) -> Option<f64> {
        let var = match func {
            AggFunc::VarSamp | AggFunc::StddevSamp if self.n >= 2.0 => self.m2 / (self.n - 1.0),
            AggFunc::VarPop | AggFunc::StddevPop if self.n >= 1.0 => self.m2 / self.n,
            _ => return None,
        };
        // rounding can leave a tiny negative where the spread is none
        let var = var.max(0.0);
        Some(match func {
            AggFunc::StddevSamp | AggFunc::StddevPop => var.sqrt(),
            _ => var,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counted(values: &[f64]) -> Counted {
        let mut c = Counted::default();
        for v in values {
            c.add(*v);
        }
        c
    }

    #[test]
    fn quantiles_interpolate_between_the_values_around_their_rank() {
        let c = counted(&[3.0, 1.0, 4.0, 1.0, 5.0]);
        assert_eq!(c.quantile(500_000), Some(3.0));
        assert_eq!(c.quantile(0), Some(1.0));
        assert_eq!(c.quantile(1_000_000), Some(5.0));
        // rank 0.25 · 4 = 1, between the second 1 and the 3: 1
        assert_eq!(c.quantile(250_000), Some(1.0));
        // an even count: halfway between the middle two
        assert_eq!(counted(&[1.0, 2.0, 3.0, 4.0]).quantile(500_000), Some(2.5));
        assert_eq!(counted(&[]).quantile(500_000), None);
        // -0 and 0 are one value; negatives sort first
        assert_eq!(counted(&[-0.0, 0.0, -2.0]).quantile(500_000), Some(0.0));
        let mut a = counted(&[1.0, 2.0]);
        a.merge(counted(&[3.0, 4.0]));
        assert_eq!(a.quantile(500_000), Some(2.5));
    }

    #[test]
    fn moments_merge_to_what_one_pass_gives_and_stay_accurate() {
        let values = [2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0];
        let mut one = Moments::default();
        for v in values {
            one.add(v);
        }
        assert_eq!(one.finish(AggFunc::StddevPop), Some(2.0));
        let (mut a, mut b) = (Moments::default(), Moments::default());
        values[..3].iter().for_each(|v| a.add(*v));
        values[3..].iter().for_each(|v| b.add(*v));
        a.merge(b);
        assert!((a.finish(AggFunc::VarSamp).unwrap_or(0.0) - 32.0 / 7.0).abs() < 1e-12);
        // one value has no sample spread, and a population spread of 0
        let mut single = Moments::default();
        single.add(3.0);
        assert_eq!(single.finish(AggFunc::StddevSamp), None);
        assert_eq!(single.finish(AggFunc::VarPop), Some(0.0));
        // large values close together: a sum of squares would lose them
        let mut close = Moments::default();
        for v in [1e9 + 4.0, 1e9 + 7.0, 1e9 + 13.0, 1e9 + 16.0] {
            close.add(v);
        }
        assert!((close.finish(AggFunc::VarSamp).unwrap_or(0.0) - 30.0).abs() < 1e-6);
    }
}
