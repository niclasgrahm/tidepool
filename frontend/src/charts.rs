//! The dashboard's charts as geometry: where every bar, point, tick and
//! label goes for a size, which bar a position means, and the frames in
//! between two states. No DOM: `plot.rs` draws these as SVG, and `cargo
//! test` reaches everything a chart decides.
//!
//! Colours are not decided here. The SVG takes them from `style.css`
//! through classes, so the charts follow light and dark like the page.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;

use serde_json::Value;

use crate::board::{display, formatted, number};
use crate::scale::{Band, Linear, Points, compact};

/// How wide a piece of text is, in pixels, in the charts' font. The browser
/// measures (`plot::measure`); tests count characters.
pub type Measure<'a> = &'a dyn Fn(&str) -> f64;

/// The widest a bar gets.
pub const MAX_BAR: f64 = 28.0;
/// Bar charts lie down past this many bars: labels read better.
pub const UPRIGHT_BARS: usize = 6;
/// Lines show a dot per point up to this many points.
pub const DOTTED_POINTS: usize = 60;
/// How long a chart takes to move to new data.
pub const TWEEN_MS: f64 = 300.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    #[must_use]
    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    #[must_use]
    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }

    fn lerp(a: Self, b: Self, t: f64) -> Self {
        Self {
            x: lerp(a.x, b.x, t),
            y: lerp(a.y, b.y, t),
            w: lerp(a.w, b.w, t),
            h: lerp(a.h, b.h, t),
        }
    }
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// A tick on a value axis: its position along the axis and its label.
#[derive(Clone, Debug, PartialEq)]
pub struct Tick {
    pub at: f64,
    pub text: String,
}

/// A label placed on the chart: `text` is what fits, `full` what it says.
#[derive(Clone, Debug, PartialEq)]
pub struct Label {
    pub x: f64,
    pub y: f64,
    pub text: String,
    pub full: String,
    /// SVG `text-anchor`.
    pub anchor: &'static str,
}

/// A query's rows as a chart reads them: the first column names a category
/// (or a period), each further one is a metric.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Data {
    /// Each category as shown, which is also its identity across frames.
    pub keys: Vec<String>,
    pub names: Vec<String>,
    /// `values[metric][category]`; `None` for a missing or NULL value.
    pub values: Vec<Vec<Option<f64>>>,
    /// The same values as the tooltip shows them.
    pub texts: Vec<Vec<String>>,
}

impl Data {
    #[cfg(test)]
    #[must_use]
    pub fn new(rows: &[Vec<Value>], metrics: &[String]) -> Self {
        Self::formatted(rows, metrics, &BTreeMap::new())
    }

    /// The same, with each metric's value shown by its format (by name).
    #[must_use]
    pub fn formatted(
        rows: &[Vec<Value>],
        metrics: &[String],
        formats: &BTreeMap<String, String>,
    ) -> Self {
        let keys = rows
            .iter()
            .map(|r| r.first().map(display).unwrap_or_default())
            .collect();
        let cell = |row: &Vec<Value>, m: usize| row.get(m + 1).cloned().unwrap_or(Value::Null);
        Self {
            keys,
            names: metrics.to_vec(),
            values: (0..metrics.len())
                .map(|m| rows.iter().map(|r| number(&cell(r, m))).collect())
                .collect(),
            texts: metrics
                .iter()
                .enumerate()
                .map(|(m, name)| {
                    let format = formats.get(name).map(String::as_str);
                    rows.iter()
                        .map(|r| formatted(&cell(r, m), format))
                        .collect()
                })
                .collect(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// What the tooltip says about category `i`: its name, then each
    /// metric's value.
    #[must_use]
    pub fn tip(&self, i: usize) -> Option<(String, Vec<(String, String)>)> {
        let key = self.keys.get(i)?;
        let rows = self
            .names
            .iter()
            .zip(&self.texts)
            .map(|(name, texts)| (name.clone(), texts.get(i).cloned().unwrap_or_default()))
            .collect();
        Some((key.clone(), rows))
    }

    /// Category `i` in words, for a screen reader.
    #[must_use]
    pub fn describe(&self, i: usize) -> String {
        match self.tip(i) {
            Some((key, rows)) => {
                let values: Vec<String> = rows.iter().map(|(n, v)| format!("{n} {v}")).collect();
                format!("{key}: {}", values.join(", "))
            }
            None => String::new(),
        }
    }
}

/// `text` cut with an ellipsis to fit `width`.
#[must_use]
pub fn fit(text: &str, width: f64, measure: Measure) -> String {
    if measure(text) <= width {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    // the longest prefix that fits with its ellipsis, by bisection
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let candidate: String = chars[..mid].iter().collect::<String>() + "…";
        if measure(&candidate) <= width {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        return String::new();
    }
    chars[..lo]
        .iter()
        .collect::<String>()
        .trim_end()
        .to_string()
        + "…"
}

fn widest<'a>(texts: impl IntoIterator<Item = &'a String>, measure: Measure) -> f64 {
    texts.into_iter().map(|t| measure(t)).fold(0.0, f64::max)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn tick_count(length: f64, spacing: f64, most: usize) -> usize {
    ((length / spacing).floor().max(0.0) as usize).clamp(2, most)
}

fn ticks(scale: &Linear) -> Vec<Tick> {
    scale
        .ticks()
        .into_iter()
        .map(|v| Tick {
            at: scale.at(v),
            text: compact(v),
        })
        .collect()
}

/// One bar: category `category`'s value of metric `metric`.
#[derive(Clone, Debug, PartialEq)]
pub struct Bar {
    pub category: usize,
    pub metric: usize,
    pub key: String,
    pub rect: Rect,
    pub selected: bool,
}

/// A bar chart of one dimension, laid out for a size.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BarLayout {
    pub width: f64,
    pub height: f64,
    pub horizontal: bool,
    pub plot: Rect,
    /// The categories along their axis.
    pub bands: Option<Band>,
    /// Where zero is on the value axis.
    pub zero: f64,
    pub ticks: Vec<Tick>,
    pub labels: Vec<Label>,
    pub bars: Vec<Bar>,
}

impl BarLayout {
    /// `selected` are the categories (as shown) the selection holds.
    #[must_use]
    pub fn new(
        data: &Data,
        (width, height): (f64, f64),
        selected: &[String],
        measure: Measure,
    ) -> Self {
        let horizontal = data.len() > UPRIGHT_BARS;
        let (plot, scale, bands, labels) = Self::axes(data, (width, height), horizontal, measure);
        let zero = scale.at(0.0);
        let metrics = data.values.len().max(1);
        #[allow(clippy::cast_precision_loss)]
        let each = (bands.width / metrics as f64).min(MAX_BAR);
        let gap = if metrics > 1 { 1.0 } else { 0.0 };
        let mut bars = Vec::new();
        for (m, series) in data.values.iter().enumerate() {
            for (i, value) in series.iter().enumerate() {
                let Some(value) = value else { continue };
                #[allow(clippy::cast_precision_loss)]
                let across = bands.middle(i) - each * metrics as f64 / 2.0 + each * m as f64;
                let end = scale.at(*value);
                let rect = if horizontal {
                    Rect {
                        x: zero.min(end),
                        y: across,
                        w: (end - zero).abs(),
                        h: (each - gap).max(0.0),
                    }
                } else {
                    Rect {
                        x: across,
                        y: zero.min(end),
                        w: (each - gap).max(0.0),
                        h: (end - zero).abs(),
                    }
                };
                bars.push(Bar {
                    category: i,
                    metric: m,
                    key: data.keys[i].clone(),
                    rect,
                    selected: selected.contains(&data.keys[i]),
                });
            }
        }
        Self {
            width,
            height,
            horizontal,
            plot,
            bands: Some(bands),
            zero,
            ticks: ticks(&scale),
            labels,
            bars,
        }
    }

    /// The plot, the value scale, the categories' bands and their labels:
    /// lying down, labels on the left take what they need (up to 40%);
    /// standing, the value ticks do.
    fn axes(
        data: &Data,
        (width, height): (f64, f64),
        horizontal: bool,
        measure: Measure,
    ) -> (Rect, Linear, Band, Vec<Label>) {
        let n = data.len();
        let values = || data.values.iter().flatten().flatten().copied();
        if horizontal {
            let room = widest(&data.keys, measure).min(width * 0.4).ceil();
            let p = Rect {
                x: room + 8.0,
                y: 4.0,
                w: (width - room - 8.0 - 14.0).max(0.0),
                h: (height - 4.0 - 20.0).max(0.0),
            };
            let scale = Linear::over(values(), (p.x, p.right()), tick_count(p.w, 80.0, 8));
            let b = Band::new(n, (p.y, p.bottom()), 0.2);
            let labels = data
                .keys
                .iter()
                .enumerate()
                .map(|(i, key)| Label {
                    x: p.x - 8.0,
                    y: b.middle(i),
                    text: fit(key, room, measure),
                    full: key.clone(),
                    anchor: "end",
                })
                .collect();
            (p, scale, b, labels)
        } else {
            let bottom = height - 22.0;
            let count = tick_count(bottom - 8.0, 40.0, 6);
            let tentative = Linear::over(values(), (bottom, 8.0), count);
            let room = widest(
                &ticks(&tentative)
                    .iter()
                    .map(|t| t.text.clone())
                    .collect::<Vec<_>>(),
                measure,
            )
            .ceil();
            let p = Rect {
                x: room + 8.0,
                y: 8.0,
                w: (width - room - 8.0 - 8.0).max(0.0),
                h: (bottom - 8.0).max(0.0),
            };
            let scale = Linear::over(values(), (p.bottom(), p.y), count);
            let b = Band::new(n, (p.x, p.right()), 0.2);
            let labels = data
                .keys
                .iter()
                .enumerate()
                .map(|(i, key)| Label {
                    x: b.middle(i),
                    y: p.bottom() + 16.0,
                    text: fit(key, b.step - 4.0, measure),
                    full: key.clone(),
                    anchor: "middle",
                })
                .collect();
            (p, scale, b, labels)
        }
    }

    /// Where `(x, y)` is along the category axis.
    fn along(&self, x: f64, y: f64) -> f64 {
        if self.horizontal { y } else { x }
    }

    /// The category a click at `(x, y)` means: anywhere in its band,
    /// its label included.
    #[must_use]
    pub fn hit(&self, x: f64, y: f64) -> Option<usize> {
        let bands = self.bands?;
        let across = if self.horizontal {
            (0.0..=self.width).contains(&x)
        } else {
            (self.plot.y..=self.height).contains(&y)
        };
        across.then(|| bands.index(self.along(x, y))).flatten()
    }

    /// The category nearest to `(x, y)` along its axis, for a drag that
    /// has left the chart.
    #[must_use]
    pub fn nearest(&self, x: f64, y: f64) -> Option<usize> {
        let bands = self.bands?;
        if bands.n == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let end = bands.start + bands.step * bands.n as f64;
        bands.index(self.along(x, y).clamp(bands.start, end - 0.01))
    }

    /// The band a brush over categories `a..=b` covers.
    #[must_use]
    pub fn span(&self, a: usize, b: usize) -> Rect {
        let Some(bands) = self.bands else {
            return Rect::default();
        };
        let (lo, hi) = (a.min(b), a.max(b));
        let from = bands.at(lo) - (bands.step - bands.width) / 2.0;
        let to = bands.at(hi) + bands.width + (bands.step - bands.width) / 2.0;
        if self.horizontal {
            Rect {
                x: self.plot.x,
                y: from,
                w: self.plot.w,
                h: to - from,
            }
        } else {
            Rect {
                x: from,
                y: self.plot.y,
                w: to - from,
                h: self.plot.h,
            }
        }
    }

    /// A frame between `self` and `to`, `t` of the way: bars of the same
    /// category and metric move, new ones grow from zero.
    #[must_use]
    pub fn blend(&self, to: &Self, t: f64) -> Self {
        let before: HashMap<(&str, usize), Rect> = self
            .bars
            .iter()
            .map(|b| ((b.key.as_str(), b.metric), b.rect))
            .collect();
        let bars = to
            .bars
            .iter()
            .map(|bar| {
                let from = before
                    .get(&(bar.key.as_str(), bar.metric))
                    .copied()
                    .filter(|_| self.horizontal == to.horizontal)
                    .unwrap_or_else(|| to.grown_from_zero(bar.rect));
                Bar {
                    rect: Rect::lerp(from, bar.rect, t),
                    ..bar.clone()
                }
            })
            .collect();
        Self { bars, ..to.clone() }
    }

    /// `rect` with no length, on the zero line.
    fn grown_from_zero(&self, rect: Rect) -> Rect {
        if self.horizontal {
            Rect {
                x: self.zero,
                w: 0.0,
                ..rect
            }
        } else {
            Rect {
                y: self.zero,
                h: 0.0,
                ..rect
            }
        }
    }
}

/// A value axis of a line chart.
#[derive(Clone, Debug, PartialEq)]
pub struct Axis {
    pub ticks: Vec<Tick>,
    /// On the right: a second metric's own scale.
    pub right: bool,
    pub zero: f64,
}

/// A line over periods, laid out for a size: one series per metric (two
/// at most), the second on its own axis.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LineLayout {
    pub width: f64,
    pub height: f64,
    pub plot: Rect,
    pub points: Option<Points>,
    pub axes: Vec<Axis>,
    pub keys: Vec<String>,
    /// `series[metric][period]`: where each value is, if it has one.
    pub series: Vec<Vec<Option<(f64, f64)>>>,
    pub labels: Vec<Label>,
    pub dots: bool,
}

/// A period's label on the time axis: `2026-10-01` by day is `Oct 1`,
/// `2026-10-01T19:00:00Z` by hour is `Oct 1 19:00`, by minute `19:00`.
#[must_use]
pub fn short_period(label: &str, grain: Option<&str>) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = label.get(..10).unwrap_or("").split('-');
    let (Some(y), Some(m), Some(d)) = (parts.next(), parts.next(), parts.next()) else {
        return label.to_string();
    };
    let (Ok(month), Ok(day)) = (m.parse::<usize>(), d.parse::<u32>()) else {
        return label.to_string();
    };
    let Some(name) = month.checked_sub(1).and_then(|i| MONTHS.get(i)) else {
        return label.to_string();
    };
    match grain {
        Some("year") => y.to_string(),
        Some("quarter") => format!("Q{} {y}", (month - 1) / 3 + 1),
        Some("month") => format!("{name} {y}"),
        Some("day" | "week") => format!("{name} {day}"),
        Some(grain @ ("hour" | "minute")) => match (grain, label.get(11..16)) {
            ("hour", Some(time)) => format!("{name} {day} {time}"),
            (_, Some(time)) => time.to_string(),
            _ => label.to_string(),
        },
        _ => label.to_string(),
    }
}

impl LineLayout {
    #[must_use]
    pub fn new(
        data: &Data,
        (width, height): (f64, f64),
        grain: Option<&str>,
        measure: Measure,
    ) -> Self {
        let shown: Vec<&Vec<Option<f64>>> = data.values.iter().take(2).collect();
        let bottom = height - 22.0;
        let count = tick_count(bottom - 8.0, 40.0, 6);
        let scale_of = |series: &Vec<Option<f64>>, range| {
            Linear::over(series.iter().flatten().copied(), range, count)
        };
        let room = |series: &Vec<Option<f64>>| {
            let texts: Vec<String> = ticks(&scale_of(series, (bottom, 8.0)))
                .into_iter()
                .map(|t| t.text)
                .collect();
            widest(&texts, measure).ceil()
        };
        let left = shown.first().map_or(0.0, |s| room(s));
        let right = shown.get(1).map_or(0.0, |s| room(s));
        let plot = Rect {
            x: left + 8.0,
            y: 8.0,
            w: (width - left - 8.0 - if right > 0.0 { right + 8.0 } else { 10.0 }).max(0.0),
            h: (bottom - 8.0).max(0.0),
        };
        let points = Points::new(data.len(), (plot.x, plot.right()));
        let mut axes = Vec::new();
        let mut series = Vec::new();
        for (m, values) in shown.iter().enumerate() {
            let scale = scale_of(values, (plot.bottom(), plot.y));
            axes.push(Axis {
                ticks: ticks(&scale),
                right: m == 1,
                zero: scale.at(0.0),
            });
            series.push(
                values
                    .iter()
                    .enumerate()
                    .map(|(i, v)| v.map(|v| (points.at(i), scale.at(v))))
                    .collect(),
            );
        }
        let short: Vec<String> = data.keys.iter().map(|k| short_period(k, grain)).collect();
        // every k-th label, k as small as lets them stand apart
        let room = widest(&short, measure) + 16.0;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let every = if points.step > 0.0 {
            (room / points.step).ceil().max(1.0) as usize
        } else {
            1
        };
        let labels = short
            .iter()
            .zip(&data.keys)
            .enumerate()
            .filter(|(i, _)| i % every == 0)
            .map(|(i, (text, full))| {
                // kept inside the chart, not cut at its edges
                let half = measure(text) / 2.0;
                Label {
                    x: points.at(i).clamp(half, (width - half).max(half)),
                    y: plot.bottom() + 16.0,
                    text: text.clone(),
                    full: full.clone(),
                    anchor: "middle",
                }
            })
            .collect();
        Self {
            width,
            height,
            plot,
            points: Some(points),
            axes,
            keys: data.keys.clone(),
            series,
            labels,
            dots: data.len() <= DOTTED_POINTS,
        }
    }

    /// The period nearest to `x`.
    #[must_use]
    pub fn nearest(&self, x: f64) -> Option<usize> {
        self.points?.nearest(x)
    }

    /// The band a brush over periods `a..=b` covers: half a step either
    /// side, inside the plot.
    #[must_use]
    pub fn span(&self, a: usize, b: usize) -> Rect {
        let Some(points) = self.points else {
            return Rect::default();
        };
        let (lo, hi) = (a.min(b), a.max(b));
        let half = if points.step > 0.0 {
            points.step / 2.0
        } else {
            self.plot.w / 2.0
        };
        let from = (points.at(lo) - half).max(self.plot.x);
        let to = (points.at(hi) + half).min(self.plot.right());
        Rect {
            x: from,
            y: self.plot.y,
            w: (to - from).max(1.0),
            h: self.plot.h,
        }
    }

    /// A frame between `self` and `to`: the values of the same period
    /// move; new periods are where they will be.
    #[must_use]
    pub fn blend(&self, to: &Self, t: f64) -> Self {
        let index: HashMap<&str, usize> = self
            .keys
            .iter()
            .enumerate()
            .map(|(i, k)| (k.as_str(), i))
            .collect();
        let series = to
            .series
            .iter()
            .enumerate()
            .map(|(m, values)| {
                values
                    .iter()
                    .zip(&to.keys)
                    .map(|(at, key)| {
                        let (x, y) = (*at)?;
                        let before = index
                            .get(key.as_str())
                            .and_then(|&i| self.series.get(m)?.get(i).copied().flatten());
                        Some(match before {
                            Some((x0, y0)) => (lerp(x0, x, t), lerp(y0, y, t)),
                            None => (x, y),
                        })
                    })
                    .collect()
            })
            .collect();
        Self {
            series,
            ..to.clone()
        }
    }
}

/// An SVG path through the points, broken where a value is missing.
#[must_use]
pub fn line_path(points: &[Option<(f64, f64)>]) -> String {
    let mut path = String::new();
    let mut pen_down = false;
    for point in points {
        match point {
            Some((x, y)) => {
                let _ = write!(path, "{}{x:.1},{y:.1}", if pen_down { "L" } else { "M" });
                pen_down = true;
            }
            None => pen_down = false,
        }
    }
    path
}

/// An SVG path filling down to `base` under each unbroken run of points.
#[must_use]
pub fn area_path(points: &[Option<(f64, f64)>], base: f64) -> String {
    let mut path = String::new();
    for run in points.split(Option::is_none) {
        let run: Vec<(f64, f64)> = run.iter().flatten().copied().collect();
        let (Some(first), Some(last)) = (run.first(), run.last()) else {
            continue;
        };
        let _ = write!(path, "M{:.1},{base:.1}", first.0);
        for (x, y) in &run {
            let _ = write!(path, "L{x:.1},{y:.1}");
        }
        let _ = write!(path, "L{:.1},{base:.1}Z", last.0);
    }
    path
}

/// Ease out (cubic): fast at first, settling at the end.
#[must_use]
pub fn ease(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Colours a pie cycles through (`.c0` … in `style.css`).
pub const COLOURS: usize = 8;

/// The angle a pie starts at: twelve o'clock, going clockwise.
const TOP: f64 = -std::f64::consts::FRAC_PI_2;

/// One slice of a pie: category `category`, or (`None`) the rest of a
/// whole that the visual's largest categories don't show.
#[derive(Clone, Debug, PartialEq)]
pub struct Slice {
    pub category: Option<usize>,
    pub key: String,
    /// Angles in radians, clockwise from twelve o'clock's `TOP`.
    pub start: f64,
    pub end: f64,
    pub colour: usize,
    pub selected: bool,
}

/// A pie (a ring) of one metric by one dimension, laid out for a size.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PieLayout {
    pub width: f64,
    pub height: f64,
    pub cx: f64,
    pub cy: f64,
    pub radius: f64,
    /// The hole's radius: a ring reads better than a disc.
    pub hole: f64,
    pub slices: Vec<Slice>,
    pub labels: Vec<Label>,
    /// Categories not drawn: no value, or one a slice can't be (0 or less).
    pub left_out: usize,
}

impl PieLayout {
    /// `selected` are the categories (as shown) the selection holds; `whole`
    /// the metric over everything, when the rest can be a slice.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn new(
        data: &Data,
        (width, height): (f64, f64),
        selected: &[String],
        whole: Option<f64>,
        measure: Measure,
    ) -> Self {
        let values = data.values.first().cloned().unwrap_or_default();
        let shown: Vec<(usize, f64)> = values
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.filter(|v| v.is_finite() && *v > 0.0).map(|v| (i, v)))
            .collect();
        let left_out = values.len() - shown.len();
        let sum: f64 = shown.iter().map(|(_, v)| v).sum();
        let listed: f64 = values.iter().flatten().filter(|v| v.is_finite()).sum();
        let rest = whole
            .map(|w| w - listed)
            .filter(|r| *r > sum.abs().max(1.0) * 1e-9);
        let total = sum + rest.unwrap_or(0.0);
        let mut layout = Self {
            width,
            height,
            cx: width / 2.0,
            cy: height / 2.0,
            left_out,
            ..Self::default()
        };
        if total <= 0.0 || width <= 0.0 || height <= 0.0 {
            return layout;
        }
        // room beside the ring for labels, a quarter of the width at most
        let room = widest(data.keys.iter(), measure).min(width * 0.25) + 10.0;
        layout.radius = (height / 2.0 - 8.0).min(width / 2.0 - room).max(10.0);
        layout.hole = layout.radius * 0.55;
        let full = std::f64::consts::TAU;
        let mut at = TOP;
        let mut slices: Vec<Slice> = shown
            .iter()
            .map(|(i, v)| {
                let start = at;
                at += full * v / total;
                Slice {
                    category: Some(*i),
                    key: data.keys[*i].clone(),
                    start,
                    end: at,
                    colour: i % COLOURS,
                    selected: selected.contains(&data.keys[*i]),
                }
            })
            .collect();
        if let Some(rest) = rest {
            slices.push(Slice {
                category: None,
                key: "Other".into(),
                start: at,
                end: at + full * rest / total,
                colour: COLOURS,
                selected: false,
            });
        }
        // a label beside each slice big enough to name, on its side
        for slice in &slices {
            if slice.end - slice.start < full * 0.04 {
                continue;
            }
            let mid = f64::midpoint(slice.start, slice.end);
            let (x, y) = (
                layout.cx + (layout.radius + 6.0) * mid.cos(),
                layout.cy + (layout.radius + 6.0) * mid.sin(),
            );
            let right = mid.cos() >= 0.0;
            let space = if right { width - x } else { x };
            layout.labels.push(Label {
                x,
                y,
                text: fit(&slice.key, space - 2.0, measure),
                full: slice.key.clone(),
                anchor: if right { "start" } else { "end" },
            });
        }
        layout.slices = slices;
        layout
    }

    /// The category at `(x, y)`, if the pointer is on a slice of one.
    #[must_use]
    pub fn hit(&self, x: f64, y: f64) -> Option<usize> {
        let (dx, dy) = (x - self.cx, y - self.cy);
        let distance = dx.hypot(dy);
        if distance > self.radius || distance < self.hole * 0.5 {
            return None;
        }
        let mut angle = dy.atan2(dx);
        if angle < TOP {
            angle += std::f64::consts::TAU;
        }
        self.slices
            .iter()
            .find(|s| s.start <= angle && angle < s.end)
            .and_then(|s| s.category)
    }

    /// Where category `i`'s slice is: its middle, for the tooltip and the
    /// keyboard.
    #[must_use]
    pub fn middle(&self, i: usize) -> Option<(f64, f64)> {
        let slice = self.slices.iter().find(|s| s.category == Some(i))?;
        let mid = f64::midpoint(slice.start, slice.end);
        let r = f64::midpoint(self.radius, self.hole);
        Some((self.cx + r * mid.cos(), self.cy + r * mid.sin()))
    }

    /// The SVG path of a slice: a piece of the ring.
    #[must_use]
    pub fn path(&self, slice: &Slice) -> String {
        let (outer, inner) = (self.radius, self.hole);
        // a whole ring can't be one arc: stop just short of the start
        let end = if slice.end - slice.start >= std::f64::consts::TAU - 1e-9 {
            slice.start + std::f64::consts::TAU - 1e-6
        } else {
            slice.end
        };
        let large = u8::from(end - slice.start > std::f64::consts::PI);
        let point = |radius: f64, angle: f64| {
            (
                self.cx + radius * angle.cos(),
                self.cy + radius * angle.sin(),
            )
        };
        let (from, to) = (point(outer, slice.start), point(outer, end));
        let (back, home) = (point(inner, end), point(inner, slice.start));
        let mut out = String::new();
        let _ = write!(
            out,
            "M{:.2},{:.2}A{outer:.2},{outer:.2} 0 {large} 1 {:.2},{:.2}L{:.2},{:.2}A{inner:.2},{inner:.2} 0 {large} 0 {:.2},{:.2}Z",
            from.0, from.1, to.0, to.1, back.0, back.1, home.0, home.1
        );
        out
    }

    /// Part of the way (`t`, 0 to 1) from `self` to `to`: slices matched by
    /// key; new ones grow from nothing where they'll be.
    #[must_use]
    pub fn blend(&self, to: &Self, t: f64) -> Self {
        let slices = to
            .slices
            .iter()
            .map(|slice| {
                let (start, end) = self
                    .slices
                    .iter()
                    .find(|s| s.key == slice.key)
                    .map_or((slice.start, slice.start), |s| (s.start, s.end));
                Slice {
                    start: lerp(start, slice.start, t),
                    end: lerp(end, slice.end, t),
                    ..slice.clone()
                }
            })
            .collect();
        Self {
            slices,
            radius: lerp(self.radius, to.radius, t),
            hole: lerp(self.hole, to.hole, t),
            ..to.clone()
        }
    }
}

/// One point of a scatter: category `category` at `(x, y)`, `r` across.
#[derive(Clone, Debug, PartialEq)]
pub struct Point {
    pub category: usize,
    pub key: String,
    pub x: f64,
    pub y: f64,
    pub r: f64,
    pub selected: bool,
}

/// A scatter of one dimension's values by two metrics (a third as size),
/// laid out for a size.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScatterLayout {
    pub width: f64,
    pub height: f64,
    pub plot: Rect,
    pub x_ticks: Vec<Tick>,
    pub y_ticks: Vec<Tick>,
    /// The metrics' names along the axes.
    pub x_label: String,
    pub y_label: String,
    pub points: Vec<Point>,
    /// Categories not drawn: an axis value missing.
    pub left_out: usize,
}

impl ScatterLayout {
    #[must_use]
    pub fn new(
        data: &Data,
        (width, height): (f64, f64),
        selected: &[String],
        measure: Measure,
    ) -> Self {
        let column = |m: usize| data.values.get(m).cloned().unwrap_or_default();
        let (xs, ys, sizes) = (column(0), column(1), column(2));
        let both: Vec<(usize, f64, f64)> = xs
            .iter()
            .zip(&ys)
            .enumerate()
            .filter_map(|(i, (x, y))| Some((i, (*x)?, (*y)?)))
            .filter(|(_, x, y)| x.is_finite() && y.is_finite())
            .collect();
        let rough_y = Linear::fit(both.iter().map(|p| p.2), (height - 28.0, 8.0), 5);
        let left = widest(ticks(&rough_y).iter().map(|t| &t.text), measure) + 12.0;
        let plot = Rect {
            x: left,
            y: 8.0,
            w: (width - left - 12.0).max(1.0),
            h: (height - 36.0).max(1.0),
        };
        let x = Linear::fit(
            both.iter().map(|p| p.1),
            (plot.x, plot.right()),
            tick_count(plot.w, 70.0, 8),
        );
        let y = Linear::fit(
            both.iter().map(|p| p.2),
            (plot.bottom(), plot.y),
            tick_count(plot.h, 40.0, 6),
        );
        // sizes by area: the radius grows with the square root
        let largest = sizes.iter().flatten().fold(0.0_f64, |m, v| m.max(v.abs()));
        let radius = |i: usize| match sizes.get(i).copied().flatten() {
            Some(v) if largest > 0.0 => 3.0 + 9.0 * (v.abs() / largest).sqrt(),
            _ => 4.0,
        };
        Self {
            width,
            height,
            plot,
            x_ticks: ticks(&x),
            y_ticks: ticks(&y),
            x_label: data.names.first().cloned().unwrap_or_default(),
            y_label: data.names.get(1).cloned().unwrap_or_default(),
            points: both
                .iter()
                .map(|(i, vx, vy)| Point {
                    category: *i,
                    key: data.keys[*i].clone(),
                    x: x.at(*vx),
                    y: y.at(*vy),
                    r: radius(*i),
                    selected: selected.contains(&data.keys[*i]),
                })
                .collect(),
            left_out: data.len() - both.len(),
        }
    }

    /// The point under `(x, y)`, the topmost (drawn last) when they
    /// overlap; a few pixels of slack around small ones.
    #[must_use]
    pub fn hit(&self, x: f64, y: f64) -> Option<usize> {
        self.points
            .iter()
            .rev()
            .find(|p| (p.x - x).hypot(p.y - y) <= p.r + 3.0)
            .map(|p| p.category)
    }

    /// The categories of the points inside a rectangle (a drag).
    #[must_use]
    pub fn within(&self, area: Rect) -> Vec<usize> {
        self.points
            .iter()
            .filter(|p| {
                p.x >= area.x && p.x <= area.right() && p.y >= area.y && p.y <= area.bottom()
            })
            .map(|p| p.category)
            .collect()
    }

    /// The categories in the order the keyboard walks them: left to right.
    #[must_use]
    pub fn order(&self) -> Vec<usize> {
        let mut points: Vec<&Point> = self.points.iter().collect();
        points.sort_by(|a, b| a.x.total_cmp(&b.x));
        points.iter().map(|p| p.category).collect()
    }

    /// Where category `i`'s point is.
    #[must_use]
    pub fn point(&self, i: usize) -> Option<(f64, f64)> {
        self.points
            .iter()
            .find(|p| p.category == i)
            .map(|p| (p.x, p.y))
    }

    /// Part of the way from `self` to `to`: points matched by key; new
    /// ones grow from nothing where they'll be.
    #[must_use]
    pub fn blend(&self, to: &Self, t: f64) -> Self {
        let points = to
            .points
            .iter()
            .map(|p| match self.points.iter().find(|q| q.key == p.key) {
                Some(q) => Point {
                    x: lerp(q.x, p.x, t),
                    y: lerp(q.y, p.y, t),
                    r: lerp(q.r, p.r, t),
                    ..p.clone()
                },
                None => Point {
                    r: p.r * t,
                    ..p.clone()
                },
            })
            .collect();
        Self {
            points,
            ..to.clone()
        }
    }
}

/// Where a key moves the keyboard's place among `n` categories.
#[must_use]
pub fn step_key(key: &str, at: Option<usize>, n: usize) -> Option<usize> {
    if n == 0 {
        return None;
    }
    let last = n - 1;
    Some(match (key, at) {
        ("Home", _) | ("ArrowRight" | "ArrowDown", None) => 0,
        ("End", _) | ("ArrowLeft" | "ArrowUp", None) => last,
        ("ArrowRight" | "ArrowDown", Some(i)) => (i + 1).min(last),
        ("ArrowLeft" | "ArrowUp", Some(i)) => i.saturating_sub(1),
        _ => return at,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[allow(clippy::cast_precision_loss)]
    fn chars(text: &str) -> f64 {
        text.chars().count() as f64 * 7.0
    }

    fn data(n: usize) -> Data {
        let rows: Vec<Vec<Value>> = (0..n)
            .map(|i| vec![json!(format!("r{i}")), json!(format!("{}.50", i + 1))])
            .collect();
        Data::new(&rows, &["revenue".into()])
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn a_few_bars_stand_and_many_lie_down() {
        let few = BarLayout::new(&data(3), (300.0, 200.0), &[], &chars);
        assert!(!few.horizontal);
        assert_eq!(few.bars.len(), 3);
        let tallest = few.bars.iter().map(|b| b.rect.h).fold(0.0, f64::max);
        assert!(
            close(few.bars[2].rect.h, tallest),
            "the biggest value is the tallest bar"
        );
        assert!(
            close(few.bars[0].rect.bottom(), few.zero),
            "bars stand on zero"
        );
        let many = BarLayout::new(&data(10), (300.0, 400.0), &[], &chars);
        assert!(many.horizontal);
        assert!(
            many.bars[0].rect.y < many.bars[9].rect.y,
            "the first row on top"
        );
        assert!(close(many.bars[0].rect.x, many.zero), "bars start at zero");
    }

    #[test]
    fn bars_are_no_wider_than_the_most() {
        let wide = BarLayout::new(&data(2), (1200.0, 200.0), &[], &chars);
        assert!(wide.bars.iter().all(|b| close(b.rect.w, MAX_BAR)));
    }

    #[test]
    fn selected_bars_are_marked() {
        let layout = BarLayout::new(&data(3), (300.0, 200.0), &["r1".into()], &chars);
        let marked: Vec<bool> = layout.bars.iter().map(|b| b.selected).collect();
        assert_eq!(marked, [false, true, false]);
    }

    #[test]
    fn a_second_metric_stands_beside_the_first() {
        let rows = vec![
            vec![json!("a"), json!(10), json!(5)],
            vec![json!("b"), json!(20), json!(Value::Null)],
        ];
        let layout = BarLayout::new(
            &Data::new(&rows, &["x".into(), "y".into()]),
            (300.0, 200.0),
            &[],
            &chars,
        );
        assert_eq!(layout.bars.len(), 3, "no bar for a NULL");
        let (x, y) = (&layout.bars[0], &layout.bars[2]);
        assert_eq!((x.metric, y.metric, y.category), (0, 1, 0));
        assert!(
            close(x.rect.right() + 1.0, y.rect.x),
            "side by side, a pixel apart"
        );
    }

    #[test]
    fn negative_bars_hang_below_zero() {
        let rows = vec![
            vec![json!("loss"), json!(-50)],
            vec![json!("gain"), json!(100)],
        ];
        let layout = BarLayout::new(
            &Data::new(&rows, &["profit".into()]),
            (300.0, 200.0),
            &[],
            &chars,
        );
        assert!(
            close(layout.bars[0].rect.y, layout.zero),
            "it starts at zero and goes down"
        );
        assert!(layout.ticks.iter().any(|t| t.text == "0"));
    }

    #[test]
    fn a_click_anywhere_in_a_band_means_its_bar() {
        let layout = BarLayout::new(&data(3), (300.0, 200.0), &[], &chars);
        let bar = &layout.bars[1].rect;
        assert_eq!(layout.hit(bar.x + bar.w / 2.0, bar.y + 1.0), Some(1));
        let bands = layout
            .bands
            .unwrap_or_else(|| panic!("bands for three bars"));
        assert_eq!(
            layout.hit(bands.start + bands.step * 1.02, 10.0),
            Some(1),
            "the gap beside it"
        );
        assert_eq!(
            layout.hit(bar.x + 2.0, layout.height - 4.0),
            Some(1),
            "its label"
        );
        assert_eq!(layout.hit(2.0, 20.0), None, "the value axis");
        assert_eq!(layout.nearest(-50.0, 20.0), Some(0));
        assert_eq!(layout.nearest(5000.0, 20.0), Some(2));
    }

    #[test]
    fn a_bar_brush_covers_whole_bands() {
        let layout = BarLayout::new(&data(10), (300.0, 400.0), &[], &chars);
        let bands = layout.bands.unwrap_or_else(|| panic!("bands for ten bars"));
        let span = layout.span(4, 2);
        assert!(close(span.y, bands.start + 2.0 * bands.step));
        assert!(close(span.h, 3.0 * bands.step));
        assert!(close(span.w, layout.plot.w));
    }

    #[test]
    fn long_labels_are_cut_to_fit() {
        assert_eq!(fit("north", 100.0, &chars), "north");
        assert_eq!(fit("a very long region name", 70.0, &chars), "a very lo…");
        assert!(chars(&fit("a very long region name", 70.0, &chars)) <= 70.0);
        assert_eq!(fit("anything", 3.0, &chars), "");
        let rows: Vec<Vec<Value>> = (0..8)
            .map(|i| vec![json!(format!("{i} {}", "x".repeat(80))), json!(i)])
            .collect();
        let layout = BarLayout::new(
            &Data::new(&rows, &["n".into()]),
            (400.0, 300.0),
            &[],
            &chars,
        );
        assert!(
            layout
                .labels
                .iter()
                .all(|l| chars(&l.text) <= 160.0 && l.full.len() > 80)
        );
        assert!(
            layout.plot.x <= 400.0 * 0.4 + 8.0,
            "labels take at most 40% of the width"
        );
    }

    #[test]
    fn a_bar_grows_from_zero_and_moves_to_its_new_length() {
        let before = BarLayout::new(&data(2), (300.0, 200.0), &[], &chars);
        let rows = vec![
            vec![json!("r0"), json!(2)],
            vec![json!("r1"), json!(2)],
            vec![json!("new"), json!(2)],
        ];
        let after = BarLayout::new(
            &Data::new(&rows, &["revenue".into()]),
            (300.0, 200.0),
            &[],
            &chars,
        );
        let start = before.blend(&after, 0.0);
        assert!(close(start.bars[2].rect.h, 0.0), "a new bar starts flat");
        assert!(
            close(start.bars[0].rect.h, before.bars[0].rect.h),
            "an old one where it was"
        );
        assert_eq!(before.blend(&after, 1.0), after);
        let middle = before.blend(&after, 0.5).bars[0].rect.h;
        assert!(close(
            middle,
            f64::midpoint(before.bars[0].rect.h, after.bars[0].rect.h)
        ));
    }

    #[test]
    fn a_second_metric_gets_its_own_axis() {
        let rows = vec![
            vec![json!("2026-10-01"), json!("10.00"), json!(3)],
            vec![json!("2026-10-02"), json!("20.00"), json!(Value::Null)],
        ];
        let layout = LineLayout::new(
            &Data::new(&rows, &["revenue".into(), "orders".into()]),
            (400.0, 200.0),
            Some("day"),
            &chars,
        );
        assert_eq!(layout.axes.len(), 2);
        assert!(layout.axes[1].right);
        assert_eq!(layout.series[1][1], None, "a gap for the NULL");
        let top = |m: usize, i: usize| layout.series[m][i].map(|(_, y)| y);
        assert!(top(0, 1) < top(0, 0), "20 above 10");
        assert_eq!(layout.labels[0].text, "Oct 1");
        assert!(
            layout.plot.right() < 400.0 - 10.0,
            "room for the right axis"
        );
    }

    #[test]
    fn crowded_period_labels_thin_out_and_stay_inside() {
        let rows: Vec<Vec<Value>> = (1..=30)
            .map(|d| vec![json!(format!("2026-10-{d:02}")), json!(d)])
            .collect();
        let layout = LineLayout::new(
            &Data::new(&rows, &["n".into()]),
            (300.0, 200.0),
            Some("day"),
            &chars,
        );
        assert!(layout.labels.len() < 10, "{} labels", layout.labels.len());
        assert!(
            layout
                .labels
                .windows(2)
                .all(|w| w[1].x - w[0].x >= chars("Oct 30"))
        );
        assert!(
            layout
                .labels
                .iter()
                .all(|l| l.x - chars(&l.text) / 2.0 >= 0.0)
        );
        assert!(layout.dots);
        let dense: Vec<Vec<Value>> = (0..90)
            .map(|d| vec![json!(d.to_string()), json!(d)])
            .collect();
        assert!(
            !LineLayout::new(
                &Data::new(&dense, &["n".into()]),
                (300.0, 200.0),
                None,
                &chars
            )
            .dots
        );
    }

    #[test]
    fn periods_are_found_and_brushed_whole() {
        let layout = LineLayout::new(&data(5), (400.0, 200.0), None, &chars);
        let points = layout
            .points
            .unwrap_or_else(|| panic!("points for five periods"));
        assert_eq!(layout.nearest(points.at(3) + 1.0), Some(3));
        let span = layout.span(3, 1);
        assert!(close(span.x, points.at(1) - points.step / 2.0));
        assert!(close(span.right(), points.at(3) + points.step / 2.0));
        let ends = layout.span(0, 4);
        assert!(
            close(ends.x, layout.plot.x) && close(ends.right(), layout.plot.right()),
            "clipped to the plot"
        );
    }

    #[test]
    fn paths_break_at_missing_values() {
        let points = [
            Some((0.0, 10.0)),
            Some((10.0, 5.0)),
            None,
            Some((30.0, 1.0)),
            Some((40.0, 2.0)),
        ];
        assert_eq!(line_path(&points), "M0.0,10.0L10.0,5.0M30.0,1.0L40.0,2.0");
        assert_eq!(
            area_path(&points, 20.0),
            "M0.0,20.0L0.0,10.0L10.0,5.0L10.0,20.0ZM30.0,20.0L30.0,1.0L40.0,2.0L40.0,20.0Z"
        );
    }

    #[test]
    fn a_line_slides_its_periods_along() {
        let before = LineLayout::new(&data(3), (400.0, 200.0), None, &chars);
        let after = LineLayout::new(&data(4), (400.0, 200.0), None, &chars);
        let start = before.blend(&after, 0.0);
        assert_eq!(
            start.series[0][1], before.series[0][1],
            "an old period starts where it was"
        );
        assert_eq!(
            start.series[0][3], after.series[0][3],
            "a new one where it goes"
        );
        assert_eq!(before.blend(&after, 1.0), after);
    }

    #[test]
    fn periods_read_short_by_grain() {
        assert_eq!(short_period("2026-10-07", Some("day")), "Oct 7");
        assert_eq!(short_period("2026-10-01", Some("month")), "Oct 2026");
        assert_eq!(short_period("2026-10-01", Some("quarter")), "Q4 2026");
        assert_eq!(short_period("2026-01-01", Some("year")), "2026");
        assert_eq!(
            short_period("2026-10-07T19:00:00Z", Some("hour")),
            "Oct 7 19:00"
        );
        assert_eq!(short_period("2026-10-07T19:05:00", Some("minute")), "19:05");
        assert_eq!(short_period("north", Some("day")), "north");
        assert_eq!(short_period("2026-13-01", Some("day")), "2026-13-01");
    }

    #[test]
    fn the_tooltip_reads_each_metric_by_its_format() {
        let rows = vec![vec![json!("north"), json!("1500000.00"), json!(3)]];
        let formats = [("revenue".to_string(), "currency:EUR compact".to_string())].into();
        let data = Data::formatted(&rows, &["revenue".into(), "orders".into()], &formats);
        let Some((_, values)) = data.tip(0) else {
            panic!("a tip for the first row");
        };
        assert_eq!(values[0].1, "€1.5M");
        assert_eq!(
            values[1].1, "3",
            "a metric without a format reads as it comes"
        );
        // the values the chart scales by are untouched
        assert_eq!(data.values[0][0], Some(1_500_000.0));
    }

    #[test]
    fn the_tooltip_and_the_screen_reader_name_every_metric() {
        let rows = vec![vec![json!("north"), json!("1500.00"), json!(3)]];
        let data = Data::new(&rows, &["revenue".into(), "orders".into()]);
        let (title, values) = data
            .tip(0)
            .unwrap_or_else(|| panic!("a tip for the first row"));
        assert_eq!(title, "north");
        assert_eq!(
            values,
            [
                ("revenue".into(), "1,500.00".into()),
                ("orders".into(), "3".into())
            ]
        );
        assert_eq!(data.describe(0), "north: revenue 1,500.00, orders 3");
        assert_eq!(data.tip(1), None);
    }

    #[test]
    fn keys_move_through_the_categories() {
        assert_eq!(step_key("ArrowRight", None, 4), Some(0));
        assert_eq!(step_key("ArrowLeft", None, 4), Some(3));
        assert_eq!(
            step_key("ArrowDown", Some(3), 4),
            Some(3),
            "stops at the end"
        );
        assert_eq!(step_key("ArrowUp", Some(0), 4), Some(0));
        assert_eq!(step_key("End", Some(1), 4), Some(3));
        assert_eq!(step_key("x", Some(1), 4), Some(1));
        assert_eq!(step_key("Home", None, 0), None);
    }

    #[test]
    fn easing_ends_where_it_should() {
        assert!(close(ease(0.0), 0.0) && close(ease(1.0), 1.0) && close(ease(2.0), 1.0));
        assert!(ease(0.5) > 0.5, "more than half way at half time");
    }

    #[test]
    fn a_pie_shares_out_the_whole_and_leaves_out_what_cant_be_a_slice() {
        let rows = vec![
            vec![json!("north"), json!(30)],
            vec![json!("south"), json!(10)],
            vec![json!("east"), json!(0)],
            vec![json!("west"), Value::Null],
        ];
        let data = Data::new(&rows, &["revenue".into()]);
        // the whole is 50: 10 more than the slices shown, as one more slice
        let pie = PieLayout::new(&data, (400.0, 300.0), &["south".into()], Some(50.0), &chars);
        assert_eq!(pie.left_out, 2);
        assert_eq!(pie.slices.len(), 3);
        let turn = |s: &Slice| (s.end - s.start) / std::f64::consts::TAU;
        assert!((turn(&pie.slices[0]) - 0.6).abs() < 1e-9);
        assert!((turn(&pie.slices[1]) - 0.2).abs() < 1e-9);
        assert_eq!(pie.slices[2].category, None);
        assert!(pie.slices[1].selected && !pie.slices[0].selected);
        // twelve o'clock and a little clockwise is north's; the hole is nothing
        let (x, y) = (pie.cx + 5.0, pie.cy - pie.radius * 0.9);
        assert_eq!(pie.hit(x, y), Some(0));
        assert_eq!(pie.hit(pie.cx, pie.cy), None);
        assert_eq!(
            pie.hit(pie.cx - 1.0, pie.cy - pie.radius * 0.9),
            None,
            "the rest selects nothing"
        );
        let (mx, my) = pie.middle(1).unwrap_or_default();
        assert_eq!(pie.hit(mx, my), Some(1));
        // without a whole (an average, say), no rest
        let alone = PieLayout::new(&data, (400.0, 300.0), &[], None, &chars);
        assert_eq!(alone.slices.len(), 2);
        // new slices grow from where they'll be
        let half = PieLayout::default().blend(&alone, 0.5);
        assert!(
            (half.slices[0].end - half.slices[0].start)
                < (alone.slices[0].end - alone.slices[0].start)
        );
        assert!(alone.path(&alone.slices[0]).starts_with('M'));
    }

    #[test]
    fn a_scatter_places_points_by_two_metrics_and_selects_by_area() {
        let rows = vec![
            vec![json!("a"), json!(10), json!(100), json!(4)],
            vec![json!("b"), json!(20), json!(300), json!(1)],
            vec![json!("c"), json!(30), Value::Null, json!(9)],
        ];
        let data = Data::new(
            &rows,
            &["revenue".into(), "orders".into(), "customers".into()],
        );
        let s = ScatterLayout::new(&data, (400.0, 300.0), &["b".into()], &chars);
        assert_eq!(s.points.len(), 2);
        assert_eq!(s.left_out, 1);
        let (a, b) = (&s.points[0], &s.points[1]);
        assert!(a.x < b.x && a.y > b.y, "b is right of a and higher up");
        assert!(a.r > b.r, "a larger third metric is a larger point");
        assert!(b.selected && !a.selected);
        assert_eq!(s.hit(a.x + 1.0, a.y), Some(0));
        assert_eq!(s.hit(s.plot.x, s.plot.y), None);
        let all = Rect {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 300.0,
        };
        assert_eq!(s.within(all), vec![0, 1]);
        assert_eq!(s.order(), vec![0, 1]);
        assert_eq!(
            (s.x_label.as_str(), s.y_label.as_str()),
            ("revenue", "orders")
        );
        let grown = ScatterLayout::default().blend(&s, 0.5);
        assert!(grown.points[0].r < s.points[0].r);
    }
}
