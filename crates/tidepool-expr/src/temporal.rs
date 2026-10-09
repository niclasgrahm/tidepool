//! Dates and timestamps: parsing literals, `EXTRACT` and `DATE_TRUNC`.
//!
//! Dates are days since 1970-01-01; timestamps are microseconds since
//! 1970-01-01T00:00:00 (no zone) or since the Unix epoch in UTC
//! (`timestamptz`). Truncation and extraction on a `timestamptz` happen in
//! UTC: per-dashboard time zones are an open question (plan §14).

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Timelike};

/// Date and time parts, for `EXTRACT` and `DATE_TRUNC`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Part {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
    /// Day of the week, 0 = Sunday (as SQL's `dow`).
    DayOfWeek,
    DayOfYear,
}

impl Part {
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "year" | "years" => Self::Year,
            "quarter" | "quarters" => Self::Quarter,
            "month" | "months" => Self::Month,
            "week" | "weeks" => Self::Week,
            "day" | "days" => Self::Day,
            "hour" | "hours" => Self::Hour,
            "minute" | "minutes" => Self::Minute,
            "second" | "seconds" => Self::Second,
            "dow" | "dayofweek" => Self::DayOfWeek,
            "doy" | "dayofyear" => Self::DayOfYear,
            _ => return None,
        })
    }

    /// Whether truncating to this part leaves a whole day (and so a date).
    #[must_use]
    pub fn is_calendar(self) -> bool {
        matches!(
            self,
            Self::Year | Self::Quarter | Self::Month | Self::Week | Self::Day
        )
    }
}

const EPOCH: NaiveDate = match NaiveDate::from_ymd_opt(1970, 1, 1) {
    Some(date) => date,
    None => panic!("1970-01-01 is a date"),
};

#[must_use]
pub fn date_from_days(days: i32) -> Option<NaiveDate> {
    EPOCH.checked_add_signed(chrono::Duration::days(i64::from(days)))
}

#[must_use]
pub fn days_from_date(date: NaiveDate) -> i32 {
    i32::try_from((date - EPOCH).num_days()).unwrap_or(i32::MAX)
}

#[must_use]
pub fn datetime_from_micros(micros: i64) -> Option<NaiveDateTime> {
    DateTime::from_timestamp_micros(micros).map(|t| t.naive_utc())
}

#[must_use]
pub fn parse_date(text: &str) -> Option<i32> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d")
        .ok()
        .map(days_from_date)
}

/// A timestamp without zone (`2026-10-07 12:00:00`, `T` allowed), or a bare
/// date (midnight).
#[must_use]
pub fn parse_timestamp(text: &str) -> Option<i64> {
    let text = text.trim();
    ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(text, f).ok())
        .or_else(|| {
            NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        })
        .map(|t| t.and_utc().timestamp_micros())
}

/// A timestamp with an offset (RFC 3339), or without one (read as UTC).
#[must_use]
pub fn parse_timestamptz(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(text.trim())
        .map(|t| t.timestamp_micros())
        .ok()
        .or_else(|| parse_timestamp(text))
}

/// `EXTRACT(part FROM value)` on a date (days) or timestamp (micros).
#[must_use]
pub fn extract(part: Part, value: NaiveDateTime) -> i64 {
    let date = value.date();
    i64::from(match part {
        Part::Year => date.year(),
        Part::Quarter => i32::try_from(date.month0() / 3 + 1).unwrap_or(0),
        Part::Month => i32::try_from(date.month()).unwrap_or(0),
        Part::Week => i32::try_from(date.iso_week().week()).unwrap_or(0),
        Part::Day => i32::try_from(date.day()).unwrap_or(0),
        Part::Hour => i32::try_from(value.hour()).unwrap_or(0),
        Part::Minute => i32::try_from(value.minute()).unwrap_or(0),
        Part::Second => i32::try_from(value.second()).unwrap_or(0),
        Part::DayOfWeek => i32::try_from(date.weekday().num_days_from_sunday()).unwrap_or(0),
        Part::DayOfYear => i32::try_from(date.ordinal()).unwrap_or(0),
    })
}

/// The start of the period containing `date`, for calendar parts. Weeks start
/// on Monday (ISO).
#[must_use]
pub fn trunc_date(part: Part, date: NaiveDate) -> NaiveDate {
    let first = |y, m| NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(date);
    match part {
        Part::Year => first(date.year(), 1),
        Part::Quarter => first(date.year(), date.month0() / 3 * 3 + 1),
        Part::Month => first(date.year(), date.month()),
        Part::Week => {
            date - chrono::Duration::days(i64::from(date.weekday().num_days_from_monday()))
        }
        _ => date,
    }
}

/// Truncate a timestamp (micros) to a sub-day part.
#[must_use]
pub fn trunc_micros(part: Part, micros: i64) -> i64 {
    let unit: i64 = match part {
        Part::Hour => 3_600_000_000,
        Part::Minute => 60_000_000,
        Part::Second => 1_000_000,
        _ => 86_400_000_000,
    };
    micros.div_euclid(unit) * unit
}

/// A move in time: whole calendar months, then days. What a metric's
/// `time_offset` does to its time field (plan §3.4): a value of that field
/// is read as if it were this much later, so `-1y` (a year back) is read
/// as `+12 months` and last year's rows land in this year's periods.
///
/// Months are added as SQL's `+ INTERVAL` does: a day past the end of the
/// target month becomes its last day (2024-02-29 plus a year is 2025-02-28).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Shift {
    pub months: i32,
    pub days: i32,
}

const MICROS_PER_DAY: i64 = 86_400_000_000;

impl Shift {
    /// `days` (since 1970-01-01) moved; `None` past the calendar's range.
    #[must_use]
    pub fn days(self, days: i64) -> Option<i64> {
        let moved = if self.months == 0 {
            days
        } else {
            let date = date_from_days(i32::try_from(days).ok()?)?;
            let months = chrono::Months::new(self.months.unsigned_abs());
            let date = if self.months > 0 {
                date.checked_add_months(months)?
            } else {
                date.checked_sub_months(months)?
            };
            i64::from(days_from_date(date))
        };
        moved.checked_add(i64::from(self.days))
    }

    /// Microseconds moved: the day as [`Shift::days`], the time of day kept.
    #[must_use]
    pub fn micros(self, micros: i64) -> Option<i64> {
        let (day, time) = (
            micros.div_euclid(MICROS_PER_DAY),
            micros.rem_euclid(MICROS_PER_DAY),
        );
        self.days(day)?
            .checked_mul(MICROS_PER_DAY)?
            .checked_add(time)
    }
}

/// The span of a relative window (plan §5.5): `last` whole periods of
/// `part` ending with the one `ago` periods before the one `now` is in, as
/// half-open microseconds `[from, to)` in UTC. `last: 7, ago: 0` in days is
/// today and the six days before it; `last: 1, ago: 1` in months is last
/// month. Periods start where `DATE_TRUNC` puts them (weeks on Monday).
///
/// `None` for parts that aren't periods (`DayOfWeek`), `last: 0`, or a span
/// past the calendar's range.
#[must_use]
pub fn window(part: Part, last: u32, ago: u32, now: i64) -> Option<(i64, i64)> {
    if last == 0 {
        return None;
    }
    let current = if part.is_calendar() {
        let day = datetime_from_micros(now)?.date();
        i64::from(days_from_date(trunc_date(part, day))).checked_mul(MICROS_PER_DAY)?
    } else {
        trunc_micros(part, now)
    };
    let step = |periods: i64| -> Option<i64> {
        let n = i32::try_from(periods).ok()?;
        let calendar = |months: i32, days: i32| Shift { months, days }.micros(current);
        match part {
            Part::Year => calendar(n.checked_mul(12)?, 0),
            Part::Quarter => calendar(n.checked_mul(3)?, 0),
            Part::Month => calendar(n, 0),
            Part::Week => calendar(0, n.checked_mul(7)?),
            Part::Day => calendar(0, n),
            Part::Hour => current.checked_add(periods.checked_mul(3_600_000_000)?),
            Part::Minute => current.checked_add(periods.checked_mul(60_000_000)?),
            Part::Second => current.checked_add(periods.checked_mul(1_000_000)?),
            Part::DayOfWeek | Part::DayOfYear => None,
        }
    };
    let (last, ago) = (i64::from(last), i64::from(ago));
    Some((step(-(ago + last - 1))?, step(1 - ago)?))
}

#[cfg(test)]
mod tests {

    #[test]
    fn windows_are_whole_periods_counted_back_from_now() {
        let at = |text: &str| parse_timestamp(text).unwrap_or(i64::MIN);
        // a Thursday afternoon
        let now = at("2026-10-08 14:35:10");
        let span = |part, last, ago| match window(part, last, ago, now) {
            Some((from, to)) => (
                datetime_from_micros(from).map(|t| t.to_string()),
                datetime_from_micros(to).map(|t| t.to_string()),
            ),
            None => panic!("{part:?} {last} {ago} should be a window"),
        };
        let both = |from: &str, to: &str| (Some(from.to_string()), Some(to.to_string()));
        assert_eq!(
            span(Part::Day, 7, 0),
            both("2026-10-02 00:00:00", "2026-10-09 00:00:00"),
            "today and the six days before"
        );
        assert_eq!(
            span(Part::Day, 1, 1),
            both("2026-10-07 00:00:00", "2026-10-08 00:00:00"),
            "yesterday"
        );
        assert_eq!(
            span(Part::Week, 1, 0),
            both("2026-10-05 00:00:00", "2026-10-12 00:00:00"),
            "this week, from Monday"
        );
        assert_eq!(
            span(Part::Month, 1, 1),
            both("2026-09-01 00:00:00", "2026-10-01 00:00:00"),
            "last month"
        );
        assert_eq!(
            span(Part::Quarter, 2, 0),
            both("2026-07-01 00:00:00", "2027-01-01 00:00:00")
        );
        assert_eq!(
            span(Part::Year, 1, 0),
            both("2026-01-01 00:00:00", "2027-01-01 00:00:00")
        );
        assert_eq!(
            span(Part::Hour, 24, 0),
            both("2026-10-07 15:00:00", "2026-10-08 15:00:00")
        );
        assert_eq!(
            span(Part::Minute, 15, 0),
            both("2026-10-08 14:21:00", "2026-10-08 14:36:00")
        );
        assert_eq!(window(Part::Day, 0, 0, now), None, "no periods");
        assert_eq!(window(Part::DayOfWeek, 1, 0, now), None);
        // before 1970 too
        let early = at("1969-12-31 23:00:00");
        assert_eq!(
            window(Part::Day, 1, 0, early),
            Some((-MICROS_PER_DAY, 0)),
            "the last day of 1969"
        );
    }

    #[test]
    fn shifts_move_by_calendar_months_and_clamp_to_the_month_end() {
        let day = |y, m, d| {
            i64::from(days_from_date(
                NaiveDate::from_ymd_opt(y, m, d).unwrap_or(EPOCH),
            ))
        };
        let year = Shift {
            months: 12,
            days: 0,
        };
        assert_eq!(year.days(day(2024, 2, 29)), Some(day(2025, 2, 28)));
        assert_eq!(year.days(day(2025, 3, 31)), Some(day(2026, 3, 31)));
        let back = Shift {
            months: -1,
            days: 0,
        };
        assert_eq!(back.days(day(2026, 3, 31)), Some(day(2026, 2, 28)));
        let week = Shift { months: 0, days: 7 };
        assert_eq!(week.days(day(2026, 12, 28)), Some(day(2027, 1, 4)));
        // times of day stay, before 1970 too
        let micros = day(1969, 12, 31) * MICROS_PER_DAY + 3_600_000_000;
        assert_eq!(
            year.micros(micros),
            Some(day(1970, 12, 31) * MICROS_PER_DAY + 3_600_000_000)
        );
    }

    use super::*;

    #[test]
    fn calendar_truncation_matches_sql() {
        let date = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or(EPOCH);
        // a Wednesday
        let day = date("2026-10-07");
        assert_eq!(trunc_date(Part::Week, day), date("2026-10-05"));
        assert_eq!(trunc_date(Part::Quarter, day), date("2026-10-01"));
        assert_eq!(trunc_date(Part::Year, day), date("2026-01-01"));
    }

    #[test]
    fn extraction_and_parsing() {
        let Some(micros) = parse_timestamptz("2026-10-07T14:30:00+02:00") else {
            panic!("should parse");
        };
        let Some(time) = datetime_from_micros(micros) else {
            panic!("in range");
        };
        assert_eq!(extract(Part::Hour, time), 12, "UTC");
        assert_eq!(extract(Part::DayOfWeek, time), 3);
        assert_eq!(extract(Part::Quarter, time), 4);
        assert_eq!(
            parse_date("2026-10-07").map(date_from_days),
            Some(NaiveDate::from_ymd_opt(2026, 10, 7))
        );
        assert_eq!(
            trunc_micros(Part::Hour, micros + 59_000_000),
            micros - 30 * 60_000_000,
            "12:30:59 → 12:00"
        );
    }
}
