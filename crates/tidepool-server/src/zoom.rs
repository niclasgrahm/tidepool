//! Time zoom (plan §5.8): which grain a visual's time axis is at, and what
//! its frame says about it.
//!
//! A grain is resolved before the engine sees the query, from the range on
//! screen (what the session selects on the field, or the extent of its live
//! values) and a budget of buckets from the dashboard file, never from the
//! browser. So every session gets the same grain for the same selection and
//! they go on sharing answers.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use chrono::Datelike;
use serde_json::Value as Json;
use tidepool_config::dashboard::Grain;
use tidepool_config::ossie::TimeGrain;
use tidepool_engine::{Condition, Engine, Selection};
use tidepool_expr::temporal::{
    Part, Shift, datetime_from_micros, days_from_date, trunc_date, trunc_micros,
};
use tidepool_expr::{Scalar, Type};
use tidepool_model::ir::{FieldRef, FieldSource, Model};
use tidepool_protocol::live::ZoomFrame;
use tidepool_store::column::Raw;
use tidepool_store::dict::DictValue;
use tidepool_store::store::{SegmentData, SegmentView};
use tidepool_store::{Snapshot, TableSnapshot};

use crate::query::json;

/// Buckets a visual can show per column of the grid it's wide.
const BUCKETS_PER_COLUMN: u64 = 40;

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// A half-open range of time, in microseconds since the Unix epoch.
pub(crate) type Span = (i64, i64);

/// The most buckets a visual `columns` wide shows.
pub(crate) fn budget(columns: u16) -> u64 {
    BUCKETS_PER_COLUMN * u64::from(columns)
}

fn part(grain: TimeGrain) -> Part {
    match grain {
        TimeGrain::Minute => Part::Minute,
        TimeGrain::Hour => Part::Hour,
        TimeGrain::Day => Part::Day,
        TimeGrain::Week => Part::Week,
        TimeGrain::Month => Part::Month,
        TimeGrain::Quarter => Part::Quarter,
        TimeGrain::Year => Part::Year,
    }
}

/// The start of the bucket that holds `micros`.
fn bucket(grain: TimeGrain, micros: i64) -> i64 {
    if grain.is_sub_day() {
        return trunc_micros(part(grain), micros);
    }
    datetime_from_micros(micros).map_or(micros, |t| {
        i64::from(days_from_date(trunc_date(part(grain), t.date()))) * MICROS_PER_DAY
    })
}

/// The start of the bucket after the one starting at `start`.
fn next_bucket(grain: TimeGrain, start: i64) -> i64 {
    let shift = |months, days| Shift { months, days }.micros(start);
    match grain {
        TimeGrain::Minute => start.checked_add(60_000_000),
        TimeGrain::Hour => start.checked_add(3_600_000_000),
        TimeGrain::Day => shift(0, 1),
        TimeGrain::Week => shift(0, 7),
        TimeGrain::Month => shift(1, 0),
        TimeGrain::Quarter => shift(3, 0),
        TimeGrain::Year => shift(12, 0),
    }
    .unwrap_or(i64::MAX)
}

/// How many buckets of `grain` the range touches.
pub(crate) fn buckets(grain: TimeGrain, (from, to): Span) -> u64 {
    if to <= from {
        return 0;
    }
    let (first, last) = (bucket(grain, from), bucket(grain, to - 1));
    let months = |micros| {
        datetime_from_micros(micros).map_or(0, |t| i64::from(t.year()) * 12 + i64::from(t.month0()))
    };
    let distance = match grain {
        TimeGrain::Minute => (last - first) / 60_000_000,
        TimeGrain::Hour => (last - first) / 3_600_000_000,
        TimeGrain::Day => (last - first) / MICROS_PER_DAY,
        TimeGrain::Week => (last - first) / (7 * MICROS_PER_DAY),
        TimeGrain::Month => months(last) - months(first),
        TimeGrain::Quarter => (months(last) - months(first)) / 3,
        TimeGrain::Year => (months(last) - months(first)) / 12,
    };
    u64::try_from(distance).map_or(0, |d| d + 1)
}

/// The steps that give between 2 buckets and the budget over the range.
pub(crate) fn open(steps: &[TimeGrain], span: Option<Span>, budget: u64) -> Vec<TimeGrain> {
    let Some(span) = span else {
        return Vec::new();
    };
    steps
        .iter()
        .copied()
        .filter(|g| (2..=budget).contains(&buckets(*g, span)))
        .collect()
}

/// What `auto` resolves to: the finest open step. With none open, the one
/// nearest to readable: the coarsest that still splits the range, or, when
/// none does (a single instant, an empty table), the finest.
pub(crate) fn auto(steps: &[TimeGrain], span: Option<Span>, budget: u64) -> Option<TimeGrain> {
    let open = open(steps, span, budget);
    open.first()
        .or_else(|| {
            let span = span?;
            steps.iter().rev().find(|g| buckets(**g, span) >= 2)
        })
        .or_else(|| steps.first())
        .copied()
}

/// The open step nearest to `pin`; the coarser of two as near.
fn nearest(pin: TimeGrain, open: &[TimeGrain]) -> Option<TimeGrain> {
    let rank = |g: TimeGrain| TimeGrain::ALL.iter().position(|x| *x == g).unwrap_or(0);
    open.iter()
        .rev()
        .copied()
        .min_by_key(|g| rank(*g).abs_diff(rank(pin)))
}

/// The grain a visual's answer is at: what's pinned (or the open step nearest
/// to it), else what the dashboard says. A fixed grain stays what it is.
pub(crate) fn choose(
    steps: &[TimeGrain],
    own: Grain,
    pin: Option<TimeGrain>,
    span: Option<Span>,
    budget: u64,
) -> Option<TimeGrain> {
    match (pin, own) {
        (Some(pin), _) => {
            let open = open(steps, span, budget);
            if open.contains(&pin) {
                return Some(pin);
            }
            nearest(pin, &open).or_else(|| auto(steps, span, budget))
        }
        (None, Grain::Fixed(grain)) => Some(grain),
        (None, Grain::Auto) => auto(steps, span, budget),
    }
}

/// A value's time, in microseconds; a date's is its midnight.
fn micros_of(value: &Scalar) -> Option<i64> {
    match value {
        Scalar::Date(days) => Some(i64::from(*days) * MICROS_PER_DAY),
        Scalar::Timestamp(micros) | Scalar::TimestampTz(micros) => Some(*micros),
        _ => None,
    }
}

/// The bounds a condition puts on a time field, half-open: values selected
/// reach from the first to just after the last.
fn bounds(condition: &Condition) -> (Option<i64>, Option<i64>) {
    let listed = |values: &[Scalar]| {
        let times = || values.iter().filter_map(micros_of);
        (times().min(), times().max().map(|t| t.saturating_add(1)))
    };
    match condition {
        Condition::In(values) => listed(values),
        Condition::Set(set) => listed(set.values()),
        Condition::Range { from, to } => (
            from.as_ref().and_then(micros_of),
            to.as_ref().and_then(micros_of),
        ),
        Condition::Contains(_) => (None, None),
    }
}

/// The range on screen: what's selected on the field, with an open end (or
/// no selection) reaching as far as the field's values. The second value says
/// whether a selection set it.
pub(crate) fn range(
    selection: &Selection,
    field: &FieldRef,
    extent: impl FnOnce() -> Option<Span>,
) -> (Option<Span>, bool) {
    let (from, to) = selection.0.get(field).map_or((None, None), bounds);
    let selected = from.is_some() || to.is_some();
    if let (Some(from), Some(to)) = (from, to) {
        return (Some((from, to)), true);
    }
    let span = extent().map(|(low, high)| (from.unwrap_or(low), to.unwrap_or(high)));
    (span, selected)
}

/// A bucket as the result's first column has it: a timestamp of the field's
/// own type below a day, a date otherwise.
fn bucket_value(grain: TimeGrain, start: i64, ty: Type) -> Json {
    let scalar = if !grain.is_sub_day() {
        i32::try_from(start.div_euclid(MICROS_PER_DAY)).map_or(Scalar::Null, Scalar::Date)
    } else if ty == Type::TimestampTz {
        Scalar::TimestampTz(start)
    } else {
        Scalar::Timestamp(start)
    };
    json(&scalar)
}

/// What a frame says about a visual's time axis. Empty buckets past now
/// aren't missing yet, so a selection's range is cut there; an extent ends
/// where the values do.
pub(crate) fn frame(
    grain: TimeGrain,
    auto: bool,
    pinned: Option<TimeGrain>,
    open: &[TimeGrain],
    (span, selected): (Option<Span>, bool),
    now: i64,
    ty: Type,
) -> ZoomFrame {
    let axis = span.filter(|(from, to)| from < to).map(|(from, to)| {
        let first = bucket(grain, from);
        let end = next_bucket(grain, bucket(grain, to - 1));
        let after_now = next_bucket(grain, bucket(grain, now));
        let shown = if selected && first < after_now {
            end.min(after_now)
        } else {
            end
        };
        (first, shown)
    });
    ZoomFrame {
        grain: grain.name().to_string(),
        auto,
        pinned: pinned.map(|g| g.name().to_string()),
        open: open.iter().map(|g| g.name().to_string()).collect(),
        from: axis.map(|(from, _)| bucket_value(grain, from, ty)),
        to: axis.map(|(_, to)| bucket_value(grain, to, ty)),
        current: Some(bucket_value(grain, bucket(grain, now), ty)),
    }
}

/// What's known of one segment: its deletes' version, and its live values'
/// lowest and highest.
struct KeptSegment {
    data: Weak<SegmentData>,
    version: u64,
    range: Option<(i64, i64)>,
}

struct Kept {
    /// What the field read: a config change can point it elsewhere with the
    /// table's data untouched.
    source: Option<FieldSource>,
    table: Weak<TableSnapshot>,
    extent: Option<Span>,
    segments: HashMap<usize, KeptSegment>,
}

/// The extent of time fields' live values, until zone maps can answer (plan
/// §4.4). Keyed by the identity of immutable store data, like the engine's
/// caches: an unchanged table's answer is kept whole, and a changed one's only
/// reads the segments that are new or have new deletes.
#[derive(Default)]
pub(crate) struct Extents(Mutex<BTreeMap<FieldRef, Kept>>);

impl Extents {
    /// From the first live value of `field` to just after the last; `None`
    /// when it has none.
    pub(crate) fn of(
        &self,
        engine: &Engine,
        snapshot: &Snapshot,
        field: &FieldRef,
    ) -> Option<Span> {
        let table = snapshot.tables.get(&field.table)?;
        let source = snapshot
            .model
            .dataset(field.table)
            .and_then(|d| d.field(&field.field))
            .map(|f| f.source.clone());
        let mut kept = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = kept.entry(field.clone()).or_insert_with(|| Kept {
            source: None,
            table: Weak::new(),
            extent: None,
            segments: HashMap::new(),
        });
        if entry.source != source {
            entry.source = source;
            entry.table = Weak::new();
            entry.segments.clear();
        }
        if std::ptr::eq(entry.table.as_ptr(), Arc::as_ptr(table)) {
            return entry.extent;
        }
        entry.extent = match source_column(&snapshot.model, field) {
            Some(index) => scan(snapshot, table, index, &mut entry.segments),
            None => derived_extent(engine, snapshot, field),
        };
        entry.table = Arc::downgrade(table);
        entry.extent
    }
}

/// Where a field's column is among its table's, when it's a plain column.
fn source_column(model: &Model, field: &FieldRef) -> Option<(usize, bool)> {
    let found = model.dataset(field.table)?.field(&field.field)?;
    let FieldSource::Column(id) = &found.source else {
        return None;
    };
    let table = model.tables.iter().find(|t| t.id == field.table)?;
    let index = table.columns.iter().position(|c| c.id == *id)?;
    Some((index, found.ty == Type::Date))
}

fn scan(
    snapshot: &Snapshot,
    table: &TableSnapshot,
    (index, date): (usize, bool),
    kept: &mut HashMap<usize, KeptSegment>,
) -> Option<Span> {
    let layout = table.layouts.get(index)?;
    let dict = layout.dict.as_ref().and_then(|d| snapshot.dicts.get(d));
    let mut fresh = HashMap::with_capacity(table.segments.len());
    let mut whole: Option<(i64, i64)> = None;
    for segment in table.segments.iter() {
        let at = Arc::as_ptr(&segment.data) as usize;
        let range = kept
            .remove(&at)
            .filter(|k| {
                k.version == segment.version
                    && std::ptr::eq(k.data.as_ptr(), Arc::as_ptr(&segment.data))
            })
            .map_or_else(|| live_range(segment, index, dict), |k| k.range);
        fresh.insert(
            at,
            KeptSegment {
                data: Arc::downgrade(&segment.data),
                version: segment.version,
                range,
            },
        );
        if let Some((low, high)) = range {
            whole = Some(whole.map_or((low, high), |(l, h)| (l.min(low), h.max(high))));
        }
    }
    *kept = fresh;
    let scale = if date { MICROS_PER_DAY } else { 1 };
    whole.map(|(low, high)| (low * scale, high * scale + 1))
}

/// The lowest and highest of a segment's live values of column `index`.
fn live_range(
    segment: &SegmentView,
    index: usize,
    dict: Option<&tidepool_store::dict::DictSnapshot>,
) -> Option<(i64, i64)> {
    let column = segment.data.columns.get(index)?;
    let mut range: Option<(i64, i64)> = None;
    for row in 0..segment.data.rows {
        if segment.deletes.contains(row) {
            continue;
        }
        let value = match column.get(row as usize) {
            Raw::I64(value) => Some(value),
            Raw::Code(code) => match dict.map(|d| d.get(code)) {
                Some(DictValue::Int(value)) => i64::try_from(value).ok(),
                _ => None,
            },
            _ => None,
        };
        if let Some(value) = value {
            range = Some(range.map_or((value, value), |(l, h)| (l.min(value), h.max(value))));
        }
    }
    range
}

/// A derived field's extent: its values are worked out by the engine.
fn derived_extent(engine: &Engine, snapshot: &Snapshot, field: &FieldRef) -> Option<Span> {
    let values = engine.values(snapshot, field, &Selection::default()).ok()?;
    let times = || values.iter().filter_map(|(value, _)| micros_of(value));
    Some((times().min()?, times().max()?.saturating_add(1)))
}

#[cfg(test)]
mod tests {
    use tidepool_expr::temporal::parse_timestamptz;

    use super::*;

    const ALL: [TimeGrain; 7] = TimeGrain::ALL;

    fn at(text: &str) -> i64 {
        parse_timestamptz(text).unwrap_or_else(|| panic!("{text} isn't a time"))
    }

    #[allow(clippy::unnecessary_wraps)] // as the functions under test take it
    fn span(from: &str, to: &str) -> Option<Span> {
        Some((at(from), at(to)))
    }

    fn names(grains: &[TimeGrain]) -> Vec<&'static str> {
        grains.iter().map(|g| g.name()).collect()
    }

    #[test]
    fn a_range_is_counted_in_whole_buckets() {
        let range = (at("2026-10-01T10:30:00Z"), at("2026-10-01T12:30:00Z"));
        assert_eq!(buckets(TimeGrain::Minute, range), 120);
        assert_eq!(buckets(TimeGrain::Hour, range), 3, "it touches three");
        assert_eq!(buckets(TimeGrain::Day, range), 1);
        // half-open: the end isn't in it
        let day = (at("2026-10-01T00:00:00Z"), at("2026-10-02T00:00:00Z"));
        assert_eq!(buckets(TimeGrain::Day, day), 1);
        assert_eq!(buckets(TimeGrain::Hour, day), 24);
        // a Monday to the next one: weeks start on Monday
        let week = (at("2026-10-05T00:00:00Z"), at("2026-10-13T00:00:00Z"));
        assert_eq!(buckets(TimeGrain::Week, week), 2);
        let months = (at("2025-11-15T00:00:00Z"), at("2026-02-02T00:00:00Z"));
        assert_eq!(buckets(TimeGrain::Month, months), 4);
        assert_eq!(buckets(TimeGrain::Quarter, months), 2);
        assert_eq!(buckets(TimeGrain::Year, months), 2);
        assert_eq!(buckets(TimeGrain::Day, (5, 5)), 0, "an empty range");
        assert_eq!(buckets(TimeGrain::Day, (9, 5)), 0, "a backwards one");
        // before 1970, truncating down rather than toward zero
        let old = (at("1969-12-31T23:30:00Z"), at("1970-01-01T00:30:00Z"));
        assert_eq!(buckets(TimeGrain::Hour, old), 2);
        assert_eq!(buckets(TimeGrain::Day, old), 2);
    }

    #[test]
    fn open_steps_give_two_buckets_up_to_the_budget() {
        // ninety days on 12 columns: 2,160 hours are too many, one quarter
        // too few
        let ninety = span("2026-07-01T00:00:00Z", "2026-09-29T00:00:00Z");
        assert_eq!(budget(12), 480);
        assert_eq!(
            names(&open(&ALL, ninety, budget(12))),
            ["day", "week", "month"]
        );
        assert_eq!(auto(&ALL, ninety, budget(12)), Some(TimeGrain::Day));
        // a narrower tile has a smaller budget
        assert_eq!(budget(2), 80);
        assert_eq!(names(&open(&ALL, ninety, budget(2))), ["week", "month"]);
        assert_eq!(auto(&ALL, ninety, budget(2)), Some(TimeGrain::Week));
        // exactly on the budget is still open, one past it isn't
        let hours = |n: i64| Some((0, n * 3_600_000_000));
        assert!(open(&ALL, hours(80), 80).contains(&TimeGrain::Hour));
        assert!(!open(&ALL, hours(81), 80).contains(&TimeGrain::Hour));
    }

    #[test]
    fn auto_refines_with_the_range() {
        let auto = |from, to| auto(&ALL, span(from, to), budget(12)).map(TimeGrain::name);
        assert_eq!(
            auto("2026-10-01T10:00:00Z", "2026-10-01T21:00:00Z"),
            Some("hour"),
            "eleven hours: 660 minutes are too many"
        );
        assert_eq!(
            auto("2026-10-01T10:00:00Z", "2026-10-01T11:00:00Z"),
            Some("minute")
        );
        assert_eq!(
            auto("2026-10-01T00:00:00Z", "2026-10-08T00:00:00Z"),
            Some("hour")
        );
        assert_eq!(
            auto("2026-01-01T00:00:00Z", "2026-12-01T00:00:00Z"),
            Some("day")
        );
        assert_eq!(
            auto("2000-01-01T00:00:00Z", "2026-12-01T00:00:00Z"),
            Some("month"),
            "26 years: 1,400 weeks are too many as well"
        );
    }

    #[test]
    fn with_nothing_open_auto_takes_what_comes_nearest() {
        let dates = [
            TimeGrain::Day,
            TimeGrain::Week,
            TimeGrain::Month,
            TimeGrain::Quarter,
            TimeGrain::Year,
        ];
        // an empty table has no range and no open steps
        assert_eq!(open(&ALL, None, 480), Vec::<TimeGrain>::new());
        assert_eq!(auto(&ALL, None, 480), Some(TimeGrain::Minute));
        assert_eq!(auto(&dates, None, 480), Some(TimeGrain::Day));
        // a single instant has one bucket at every grain
        let instant = Some((at("2026-10-01T12:00:00Z"), at("2026-10-01T12:00:00Z") + 1));
        assert_eq!(open(&ALL, instant, 480), Vec::<TimeGrain>::new());
        assert_eq!(auto(&ALL, instant, 480), Some(TimeGrain::Minute));
        // five hours when only minutes and years are allowed: too many
        // minutes, a single year: the minutes at least split it
        let both = [TimeGrain::Minute, TimeGrain::Year];
        let hours = span("2026-10-01T00:00:00Z", "2026-10-02T00:00:00Z");
        assert_eq!(auto(&both, hours, 480), Some(TimeGrain::Minute));
        assert_eq!(auto(&[], hours, 480), None);
    }

    #[test]
    fn a_pin_holds_and_falls_back_to_the_nearest_open_step() {
        let choose = |own, pin, range| choose(&ALL, own, pin, range, 480).map(TimeGrain::name);
        let auto = Grain::Auto;
        let hour = Some(TimeGrain::Hour);
        let week = span("2026-10-05T00:00:00Z", "2026-10-12T00:00:00Z");
        let years = span("2000-01-01T00:00:00Z", "2026-01-01T00:00:00Z");
        assert_eq!(choose(auto, hour, week), Some("hour"), "open: as pinned");
        // over 26 years, hours are closed: the nearest open one is a month
        assert_eq!(choose(auto, hour, years), Some("month"));
        // nearest either way, the coarser of two as near
        assert_eq!(nearest(TimeGrain::Week, &[TimeGrain::Hour]), hour);
        assert_eq!(
            nearest(TimeGrain::Day, &[TimeGrain::Hour, TimeGrain::Week]),
            Some(TimeGrain::Week)
        );
        // a pin wins over a fixed grain too
        let fixed = Grain::Fixed(TimeGrain::Month);
        assert_eq!(choose(fixed, hour, week), Some("hour"));
        // and with nothing open it comes to what auto would take
        assert_eq!(choose(auto, hour, None), Some("minute"));
    }

    #[test]
    fn a_fixed_grain_stays_even_when_it_is_closed() {
        let years = span("2000-01-01T00:00:00Z", "2026-01-01T00:00:00Z");
        let fixed = Grain::Fixed(TimeGrain::Hour);
        assert_eq!(choose(&ALL, fixed, None, years, 480), Some(TimeGrain::Hour));
    }

    fn field() -> FieldRef {
        FieldRef {
            table: tidepool_model::ir::TableId(1),
            field: "t".into(),
        }
    }

    fn selecting(condition: Condition) -> Selection {
        Selection([(field(), condition)].into_iter().collect())
    }

    #[test]
    fn the_range_is_the_selection_then_the_extent() {
        let extent = || Some((100, 200));
        let nothing = range(&Selection::default(), &field(), extent);
        assert_eq!(nothing, (Some((100, 200)), false));
        // both ends selected: the data isn't asked
        let both = selecting(Condition::Range {
            from: Some(Scalar::TimestampTz(120)),
            to: Some(Scalar::TimestampTz(130)),
        });
        let asked = range(&both, &field(), || panic!("asked for the extent"));
        assert_eq!(asked, (Some((120, 130)), true));
        // an open end reaches to where the values do
        let open_end = selecting(Condition::Range {
            from: Some(Scalar::TimestampTz(120)),
            to: None,
        });
        assert_eq!(range(&open_end, &field(), extent), (Some((120, 200)), true));
        // values reach from the first to just after the last, nulls aside
        let values = selecting(Condition::In(vec![
            Scalar::Null,
            Scalar::TimestampTz(150),
            Scalar::TimestampTz(110),
        ]));
        assert_eq!(range(&values, &field(), extent), (Some((110, 151)), true));
        // dates are their midnights
        let dates = selecting(Condition::Range {
            from: Some(Scalar::Date(1)),
            to: Some(Scalar::Date(3)),
        });
        assert_eq!(
            range(&dates, &field(), extent),
            (Some((MICROS_PER_DAY, 3 * MICROS_PER_DAY)), true)
        );
        // an empty table
        assert_eq!(
            range(&Selection::default(), &field(), || None),
            (None, false)
        );
    }

    #[test]
    fn a_frame_names_its_axis_in_the_results_terms() {
        let now = at("2026-10-07T14:35:10Z");
        let week = span("2026-10-01T00:00:00Z", "2026-10-08T00:00:00Z");
        let shown = |grain, range, ty| frame(grain, true, None, &[grain], range, now, ty);
        // a selection's range stops at the current bucket: what's later
        // hasn't happened, so it isn't missing
        let hours = shown(TimeGrain::Hour, (week, true), Type::TimestampTz);
        assert_eq!(hours.from, Some(Json::from("2026-10-01T00:00:00Z")));
        assert_eq!(hours.to, Some(Json::from("2026-10-07T15:00:00Z")));
        assert_eq!(hours.current, Some(Json::from("2026-10-07T14:00:00Z")));
        // an extent goes where the values do, even past now
        let past = span("2026-10-01T00:00:00Z", "2026-11-08T00:00:00Z");
        let days = shown(TimeGrain::Day, (past, false), Type::TimestampTz);
        assert_eq!(days.from, Some(Json::from("2026-10-01")));
        assert_eq!(days.to, Some(Json::from("2026-11-08")));
        assert_eq!(days.current, Some(Json::from("2026-10-07")));
        // a range wholly in the future stays what it is
        let later = span("2027-01-01T00:00:00Z", "2027-01-03T00:00:00Z");
        let future = shown(TimeGrain::Day, (later, true), Type::TimestampTz);
        assert_eq!(future.to, Some(Json::from("2027-01-03")));
        // a timestamp without a zone reads as one
        let plain = shown(TimeGrain::Hour, (week, false), Type::Timestamp);
        assert_eq!(plain.from, Some(Json::from("2026-10-01T00:00:00")));
        // nothing to show: no axis, but the frame still says its grain
        let empty = shown(TimeGrain::Day, (None, false), Type::Date);
        assert_eq!((empty.from, empty.to), (None, None));
        assert_eq!(empty.grain, "day");
    }
}
