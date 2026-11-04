//! Scales for the charts: a linear one for values, with "nice" ticks, and a
//! band one for categories. Plain arithmetic, no DOM.

/// A value domain mapped onto a pixel range. `range.0` is where `domain.0`
/// lands, so a vertical axis passes its bottom first.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Linear {
    pub domain: (f64, f64),
    pub range: (f64, f64),
    /// The distance between ticks; the domain's ends are multiples of it.
    pub step: f64,
}

impl Linear {
    /// A scale over `values` that includes zero (bars and areas grow from
    /// it) and ends on round numbers, with about `count` ticks.
    #[must_use]
    pub fn over(values: impl IntoIterator<Item = f64>, range: (f64, f64), count: usize) -> Self {
        let (mut lo, mut hi) = (0.0_f64, 0.0_f64);
        for v in values.into_iter().filter(|v| v.is_finite()) {
            lo = lo.min(v);
            hi = hi.max(v);
        }
        if (hi - lo).abs() < f64::EPSILON {
            hi = lo + 1.0;
        }
        let step = step(lo, hi, count);
        // a hair of slack, so 0.3 / 0.05 = 5.999… doesn't round a step away
        Self {
            domain: (
                (lo / step + 1e-9).floor() * step,
                (hi / step - 1e-9).ceil() * step,
            ),
            range,
            step,
        }
    }

    #[must_use]
    pub fn at(&self, value: f64) -> f64 {
        let (d0, d1) = self.domain;
        let (r0, r1) = self.range;
        if (d1 - d0).abs() < f64::EPSILON {
            return r0;
        }
        r0 + (value - d0) / (d1 - d0) * (r1 - r0)
    }

    /// Round values from one end of the domain to the other.
    #[must_use]
    pub fn ticks(&self) -> Vec<f64> {
        let (lo, hi) = self.domain;
        let step = self.step;
        let first = (lo / step - 1e-9).ceil();
        let last = (hi / step + 1e-9).floor();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = (last - first).max(0.0) as usize;
        (0..=n)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let k = first + i as f64;
                // k * step, not a running sum, so 0.1 * 3 stays 0.3-ish
                // and zero is exactly zero
                k * step
            })
            .collect()
    }
}

/// A step of 1, 2 or 5 × 10ⁿ that splits `lo..hi` into about `count` parts.
fn step(lo: f64, hi: f64, count: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let raw = (hi - lo) / count.max(1) as f64;
    let power = 10_f64.powf(raw.log10().floor());
    let error = raw / power;
    let factor = if error >= 7.5 {
        10.0
    } else if error >= 3.5 {
        5.0
    } else if error >= 1.5 {
        2.0
    } else {
        1.0
    };
    factor * power
}

/// `n` categories side by side along a pixel range, each a band with room
/// between them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    pub start: f64,
    pub step: f64,
    /// The part of `step` a band fills.
    pub width: f64,
    pub n: usize,
}

impl Band {
    #[must_use]
    pub fn new(n: usize, range: (f64, f64), padding: f64) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let step = (range.1 - range.0) / n.max(1) as f64;
        Self {
            start: range.0,
            step,
            width: step * (1.0 - padding),
            n,
        }
    }

    /// Where band `i` starts (its band, not its step).
    #[must_use]
    pub fn at(&self, i: usize) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let i = i as f64;
        self.start + i * self.step + (self.step - self.width) / 2.0
    }

    #[must_use]
    pub fn middle(&self, i: usize) -> f64 {
        self.at(i) + self.width / 2.0
    }

    /// The band whose step holds `position`, the gaps included: a click
    /// beside a thin bar still means that bar.
    #[must_use]
    pub fn index(&self, position: f64) -> Option<usize> {
        let offset = (position - self.start) / self.step;
        if !offset.is_finite() || offset < 0.0 {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let i = offset.floor() as usize;
        (i < self.n).then_some(i)
    }
}

/// `n` points evenly along a pixel range, the first and last on its ends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Points {
    pub start: f64,
    pub step: f64,
    pub n: usize,
}

impl Points {
    #[must_use]
    pub fn new(n: usize, range: (f64, f64)) -> Self {
        if n <= 1 {
            // a single point sits in the middle
            return Self {
                start: f64::midpoint(range.0, range.1),
                step: 0.0,
                n,
            };
        }
        #[allow(clippy::cast_precision_loss)]
        let step = (range.1 - range.0) / (n - 1) as f64;
        Self {
            start: range.0,
            step,
            n,
        }
    }

    #[must_use]
    pub fn at(&self, i: usize) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let i = i as f64;
        self.start + i * self.step
    }

    /// The point nearest to `position`, clamped to the ends.
    #[must_use]
    pub fn nearest(&self, position: f64) -> Option<usize> {
        if self.n == 0 {
            return None;
        }
        if self.step == 0.0 {
            return Some(0);
        }
        let offset = ((position - self.start) / self.step).round();
        #[allow(clippy::cast_precision_loss)]
        let last = (self.n - 1) as f64;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(offset.clamp(0.0, last) as usize)
    }
}

/// Buckets along a pixel range by when they start, the first and last on its
/// ends: a time axis, where a month is as wide as its days.
#[derive(Clone, Debug, PartialEq)]
pub struct Times {
    positions: Vec<f64>,
}

impl Times {
    /// `starts` in order, in any unit of time.
    #[must_use]
    pub fn new(starts: &[i64], range: (f64, f64)) -> Self {
        let (Some(first), Some(last)) = (starts.first(), starts.last()) else {
            return Self {
                positions: Vec::new(),
            };
        };
        #[allow(clippy::cast_precision_loss)]
        let span = (last - first) as f64;
        let positions = starts
            .iter()
            .map(|start| {
                if span <= 0.0 {
                    return f64::midpoint(range.0, range.1);
                }
                #[allow(clippy::cast_precision_loss)]
                let part = (start - first) as f64 / span;
                range.0 + part * (range.1 - range.0)
            })
            .collect();
        Self { positions }
    }
}

/// How a line chart's periods lie along its axis.
#[derive(Clone, Debug, PartialEq)]
pub enum Along {
    /// Evenly: categories.
    Even(Points),
    /// By time.
    Time(Times),
}

impl Along {
    #[must_use]
    pub fn at(&self, i: usize) -> f64 {
        match self {
            Self::Even(points) => points.at(i),
            Self::Time(times) => times.positions.get(i).copied().unwrap_or(0.0),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Even(points) => points.n,
            Self::Time(times) => times.positions.len(),
        }
    }

    /// The period nearest to `position`, clamped to the ends.
    #[must_use]
    pub fn nearest(&self, position: f64) -> Option<usize> {
        match self {
            Self::Even(points) => points.nearest(position),
            Self::Time(times) => {
                let after = times.positions.partition_point(|x| *x < position);
                match (after.checked_sub(1), times.positions.get(after)) {
                    (Some(before), Some(next))
                        if position - times.positions[before] < next - position =>
                    {
                        Some(before)
                    }
                    (_, Some(_)) => Some(after),
                    (Some(before), None) => Some(before),
                    (None, None) => None,
                }
            }
        }
    }

    /// Where period `i`'s part of the axis starts and ends: half way to its
    /// neighbours, as far on the other side at the ends.
    #[must_use]
    pub fn reach(&self, i: usize) -> (f64, f64) {
        let at = self.at(i);
        let before = i.checked_sub(1).map(|j| self.at(j));
        let after = (i + 1 < self.len()).then(|| self.at(i + 1));
        match (before, after) {
            (Some(b), Some(a)) => (f64::midpoint(b, at), f64::midpoint(at, a)),
            (Some(b), None) => (f64::midpoint(b, at), at + (at - b) / 2.0),
            (None, Some(a)) => (at - (a - at) / 2.0, f64::midpoint(at, a)),
            (None, None) => (at, at),
        }
    }
}

/// A tick label: short, as an axis wants it (`1.5k`, `2M`, `0.25`).
#[must_use]
pub fn compact(value: f64) -> String {
    let magnitude = value.abs();
    let (scaled, suffix) = if magnitude >= 1e12 {
        (value / 1e12, "T")
    } else if magnitude >= 1e9 {
        (value / 1e9, "B")
    } else if magnitude >= 1e6 {
        (value / 1e6, "M")
    } else if magnitude >= 1e3 {
        (value / 1e3, "k")
    } else {
        (value, "")
    };
    let mut text = format!("{scaled:.2}");
    while text.contains('.') && (text.ends_with('0') || text.ends_with('.')) {
        text.pop();
    }
    if text == "-0" {
        text = "0".into();
    }
    format!("{text}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_scale_includes_zero_and_ends_round() {
        let s = Linear::over([13.0, 87.0], (200.0, 0.0), 5);
        assert_eq!(s.domain, (0.0, 100.0));
        assert_eq!(s.ticks(), vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0]);
        assert!(
            (s.at(50.0) - 100.0).abs() < 1e-9,
            "the middle of an inverted range"
        );
        let negative = Linear::over([-30.0, 120.0], (0.0, 1.0), 5);
        assert_eq!(negative.domain, (-40.0, 120.0));
        assert!(negative.ticks().contains(&0.0));
    }

    #[test]
    fn nothing_or_all_zero_still_has_a_scale() {
        let s = Linear::over([], (0.0, 100.0), 4);
        assert!(s.domain.1 > s.domain.0);
        let z = Linear::over([0.0, 0.0, f64::NAN], (0.0, 100.0), 4);
        assert_eq!(z.at(0.0), 0.0);
    }

    #[test]
    fn small_and_huge_values_get_sensible_steps() {
        let s = Linear::over([0.03, 0.27], (0.0, 1.0), 5);
        assert!((s.domain.1 - 0.3).abs() < 1e-9, "{:?}", s.domain);
        let ticks = s.ticks();
        assert_eq!(ticks.len(), 7, "0 to 0.3 by 0.05: {ticks:?}");
        let big = Linear::over([1_234_567.0], (0.0, 1.0), 4);
        assert_eq!(big.ticks().last().copied(), Some(1_400_000.0));
        assert_eq!(big.ticks().len(), 8);
    }

    #[test]
    fn bands_split_a_range_and_find_a_position() {
        let b = Band::new(4, (0.0, 400.0), 0.25);
        assert_eq!(b.width, 75.0);
        assert_eq!(b.at(0), 12.5);
        assert_eq!(b.middle(1), 150.0);
        assert_eq!(
            b.index(5.0),
            Some(0),
            "the gap before a bar is still its band"
        );
        assert_eq!(b.index(399.0), Some(3));
        assert_eq!(b.index(400.0), None);
        assert_eq!(b.index(-1.0), None);
    }

    #[test]
    fn points_run_end_to_end_and_snap_to_the_nearest() {
        let p = Points::new(5, (0.0, 100.0));
        assert_eq!(p.at(4), 100.0);
        assert_eq!(p.nearest(37.0), Some(1));
        assert_eq!(p.nearest(38.0), Some(2));
        assert_eq!(p.nearest(-50.0), Some(0));
        assert_eq!(p.nearest(500.0), Some(4));
        let one = Points::new(1, (0.0, 100.0));
        assert_eq!((one.at(0), one.nearest(3.0)), (50.0, Some(0)));
        assert_eq!(Points::new(0, (0.0, 1.0)).nearest(0.0), None);
    }

    #[test]
    fn times_place_buckets_by_when_they_start() {
        // January (31 days), February (28), March: the second gap is smaller
        let months = [0, 31, 59];
        let t = Along::Time(Times::new(&months, (0.0, 590.0)));
        assert_eq!(t.at(0), 0.0);
        assert_eq!(t.at(1), 310.0);
        assert_eq!(t.at(2), 590.0);
        assert_eq!(t.nearest(100.0), Some(0));
        assert_eq!(t.nearest(200.0), Some(1));
        assert_eq!(t.nearest(-5.0), Some(0));
        assert_eq!(t.nearest(9000.0), Some(2));
        let one = Along::Time(Times::new(&[7], (0.0, 100.0)));
        assert_eq!((one.at(0), one.nearest(3.0)), (50.0, Some(0)), "the middle");
        let none = Along::Time(Times::new(&[], (0.0, 100.0)));
        assert_eq!((none.len(), none.nearest(3.0)), (0, None));
    }

    #[test]
    fn a_period_reaches_half_way_to_its_neighbours() {
        let t = Along::Time(Times::new(&[0, 10, 30], (0.0, 300.0)));
        assert_eq!(t.reach(1), (50.0, 200.0));
        assert_eq!(t.reach(0), (-50.0, 50.0), "as far on its other side");
        assert_eq!(t.reach(2), (200.0, 400.0));
        let even = Along::Even(Points::new(3, (0.0, 100.0)));
        assert_eq!(even.reach(1), (25.0, 75.0));
        assert_eq!(
            Along::Even(Points::new(1, (0.0, 100.0))).reach(0),
            (50.0, 50.0)
        );
    }

    #[test]
    fn tick_labels_are_short() {
        assert_eq!(compact(0.0), "0");
        assert_eq!(compact(250.0), "250");
        assert_eq!(compact(0.25), "0.25");
        assert_eq!(compact(1500.0), "1.5k");
        assert_eq!(compact(2_000_000.0), "2M");
        assert_eq!(compact(-40_000.0), "-40k");
        assert_eq!(compact(3.0e9), "3B");
    }
}
