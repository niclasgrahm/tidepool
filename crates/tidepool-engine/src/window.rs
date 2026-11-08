//! `time_window` metrics (plan §6.8, item 5): where a window starts and
//! ends, in UTC microseconds (a date is its midnight).
//!
//! A window is half-open, `[start, end)`. Without a grouping by its time
//! field it ends where the time selection on that field ends, or else at
//! the end of the current period of its grain, so `ytd` is this year and
//! `last:7d` today and the six days before. Grouped by its time field, each
//! group's window ends where the group does, and a row counts in every
//! group whose window holds it.

use tidepool_config::ossie::TimeGrain;
use tidepool_expr::temporal::{
    Part, Shift, datetime_from_micros, days_from_date, trunc_date, trunc_micros,
};
use tidepool_expr::{Scalar, Type};
use tidepool_model::ir::WindowSpec;

pub(crate) const DAY: i64 = 86_400_000_000;

/// The grain's part, for truncation.
pub(crate) fn part(grain: TimeGrain) -> Part {
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

/// The start of the period of `part` that `micros` is in.
pub(crate) fn trunc(part: Part, micros: i64) -> Option<i64> {
    if part.is_calendar() {
        let day = datetime_from_micros(micros)?.date();
        i64::from(days_from_date(trunc_date(part, day))).checked_mul(DAY)
    } else {
        Some(trunc_micros(part, micros))
    }
}

/// `micros` moved by `n` periods of `part` (months as SQL adds them).
pub(crate) fn step(part: Part, micros: i64, n: i64) -> Option<i64> {
    let n32 = i32::try_from(n).ok()?;
    let calendar = |months: i32, days: i32| Shift { months, days }.micros(micros);
    match part {
        Part::Year => calendar(n32.checked_mul(12)?, 0),
        Part::Quarter => calendar(n32.checked_mul(3)?, 0),
        Part::Month => calendar(n32, 0),
        Part::Week => calendar(0, n32.checked_mul(7)?),
        Part::Day => calendar(0, n32),
        Part::Hour => micros.checked_add(n.checked_mul(3_600_000_000)?),
        Part::Minute => micros.checked_add(n.checked_mul(60_000_000)?),
        Part::Second => micros.checked_add(n.checked_mul(1_000_000)?),
        Part::DayOfWeek | Part::DayOfYear => None,
    }
}

/// Where a window ending at `end` starts.
pub(crate) fn start(spec: WindowSpec, end: i64) -> Option<i64> {
    match spec {
        // the period the last moment before the end is in
        WindowSpec::ToDate(grain) => trunc(part(grain), end.checked_sub(1)?),
        WindowSpec::Last(n, grain) => step(part(grain), end, -i64::from(n)),
    }
}

/// Where a window ends with no group: where the selection ends (`to`), or
/// at the end of the current period of the window's grain.
pub(crate) fn end(spec: WindowSpec, to: Option<i64>, now: i64) -> Option<i64> {
    to.or_else(|| {
        let p = part(spec.grain());
        step(p, trunc(p, now)?, 1)
    })
}

/// A time value as microseconds (a date at its midnight).
pub(crate) fn micros(value: &Scalar) -> Option<i64> {
    match value {
        Scalar::Date(days) => i64::from(*days).checked_mul(DAY),
        Scalar::Timestamp(m) | Scalar::TimestampTz(m) => Some(*m),
        _ => None,
    }
}

/// Microseconds as a value of `ty` (a date: the day they're in).
pub(crate) fn value(ty: Type, micros: i64) -> Option<Scalar> {
    Some(match ty {
        Type::Date => Scalar::Date(i32::try_from(micros.div_euclid(DAY)).ok()?),
        Type::Timestamp => Scalar::Timestamp(micros),
        Type::TimestampTz => Scalar::TimestampTz(micros),
        _ => return None,
    })
}

/// The groups at `grain` whose windows hold a row at `t` (microseconds),
/// as the start of each: from the one `t` is in, onwards while the window
/// ending with the group still reaches back to `t`. At most `limit`.
pub(crate) fn groups(spec: WindowSpec, grain: Part, t: i64, limit: usize) -> Vec<i64> {
    let mut out = Vec::new();
    let Some(mut group) = trunc(grain, t) else {
        return out;
    };
    while out.len() < limit {
        let Some(group_end) = step(grain, group, 1) else {
            break;
        };
        match start(spec, group_end) {
            Some(from) if from <= t => out.push(group),
            _ => break,
        }
        group = group_end;
    }
    out
}

/// How many groups of `grain` a row can count in, at most: the window's
/// longest span over the grain's shortest, and one more.
pub(crate) fn reach(spec: WindowSpec, grain: TimeGrain) -> u64 {
    // longest and shortest spans, in minutes
    let longest = |g: TimeGrain| -> u64 {
        match g {
            TimeGrain::Minute => 1,
            TimeGrain::Hour => 60,
            TimeGrain::Day => 1_440,
            TimeGrain::Week => 10_080,
            TimeGrain::Month => 31 * 1_440,
            TimeGrain::Quarter => 92 * 1_440,
            TimeGrain::Year => 366 * 1_440,
        }
    };
    let shortest = |g: TimeGrain| -> u64 {
        match g {
            TimeGrain::Month => 28 * 1_440,
            TimeGrain::Quarter => 89 * 1_440,
            TimeGrain::Year => 365 * 1_440,
            other => longest(other),
        }
    };
    let span = match spec {
        WindowSpec::ToDate(g) => longest(g),
        WindowSpec::Last(n, g) => u64::from(n).saturating_mul(longest(g)),
    };
    span / shortest(grain) + 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> i64 {
        match tidepool_expr::temporal::parse_timestamptz(text) {
            Some(t) => t,
            None => panic!("{text} isn't a timestamp"),
        }
    }

    #[test]
    fn to_date_windows_start_with_the_period_their_end_is_in() {
        let ytd = WindowSpec::ToDate(TimeGrain::Year);
        // a selection ending 1 July: from 1 January
        assert_eq!(
            start(ytd, at("2026-07-01T00:00:00Z")),
            Some(at("2026-01-01T00:00:00Z"))
        );
        // ending exactly at a year's start: the year before, whole
        assert_eq!(
            start(ytd, at("2026-01-01T00:00:00Z")),
            Some(at("2025-01-01T00:00:00Z"))
        );
        // no selection: this year, through its end
        assert_eq!(
            end(ytd, None, at("2026-03-15T12:00:00Z")),
            Some(at("2027-01-01T00:00:00Z"))
        );
        // weeks start on Monday (2026-03-11 is a Wednesday)
        let wtd = WindowSpec::ToDate(TimeGrain::Week);
        assert_eq!(
            start(wtd, at("2026-03-12T00:00:00Z")),
            Some(at("2026-03-09T00:00:00Z"))
        );
    }

    #[test]
    fn rolling_windows_count_back_from_their_end() {
        let week = WindowSpec::Last(7, TimeGrain::Day);
        let now = at("2026-03-15T12:00:00Z");
        let to = end(week, None, now).unwrap_or(0);
        assert_eq!(to, at("2026-03-16T00:00:00Z"));
        assert_eq!(start(week, to), Some(at("2026-03-09T00:00:00Z")));
        // months as SQL adds them: 31 March back one month is 28 February
        let month = WindowSpec::Last(1, TimeGrain::Month);
        assert_eq!(
            start(month, at("2026-03-31T00:00:00Z")),
            Some(at("2026-02-28T00:00:00Z"))
        );
    }

    #[test]
    fn a_row_counts_in_every_group_whose_window_holds_it() {
        // year to date by month: a row in March counts in March to December
        let ytd = WindowSpec::ToDate(TimeGrain::Year);
        let t = at("2026-03-10T08:00:00Z");
        let months = groups(ytd, Part::Month, t, 1000);
        assert_eq!(months.len(), 10);
        assert_eq!(months[0], at("2026-03-01T00:00:00Z"));
        assert_eq!(months[9], at("2026-12-01T00:00:00Z"));
        // the last 7 days by day: the day it's in and the six after
        let days = groups(WindowSpec::Last(7, TimeGrain::Day), Part::Day, t, 1000);
        assert_eq!(days.len(), 7);
        assert_eq!(days[6], at("2026-03-16T00:00:00Z"));
        // never more than asked
        assert_eq!(groups(ytd, Part::Day, t, 5).len(), 5);
        assert!(reach(ytd, TimeGrain::Day) <= 368);
        assert!(reach(WindowSpec::Last(12, TimeGrain::Month), TimeGrain::Minute) > 1000);
    }
}
