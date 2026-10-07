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

#[cfg(test)]
mod tests {
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
