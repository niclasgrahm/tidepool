//! The date range picker's arithmetic: day numbers, months and the grid a
//! month is drawn on, what typed text means, and how the ends the picker
//! shows map to the half-open range a selection holds. No DOM, so it's
//! tested plainly; `datepick.rs` draws it.
//!
//! A day is its number since 1970-01-01, a time of day its minutes since
//! midnight. Everything is UTC, as the server's timestamps are.

use tidepool_protocol::live::{RelativeUnit, RelativeWindow};

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
#[must_use]
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date (year, month 1–12, day) of a day number.
#[must_use]
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// A day as `YYYY-MM-DD`.
#[must_use]
pub fn iso(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The day of the month (1–31) of a day number.
#[must_use]
pub fn day_of_month(day: i64) -> i64 {
    civil_from_days(day).2
}

/// A day typed as `YYYY-MM-DD`, if it is one: `2026-02-30` isn't.
#[must_use]
pub fn parse_day(text: &str) -> Option<i64> {
    let text = text.trim();
    let mut parts = text.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let digits = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| s.parse::<i64>().ok())?
    };
    let (y, m, d) = (digits(y)?, digits(m)?, digits(d)?);
    let day = days_from_civil(y, m, d);
    // a day past the month's end comes back as another date
    ((1..=12).contains(&m) && d >= 1 && civil_from_days(day) == (y, m, d)).then_some(day)
}

/// A time of day typed as `HH:MM` (or `H:MM`), in minutes.
#[must_use]
pub fn parse_time(text: &str) -> Option<u32> {
    let (h, m) = text.trim().split_once(':')?;
    let digits = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| s.parse::<u32>().ok())?
    };
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let (h, m) = (digits(h)?, digits(m)?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Minutes since midnight as `HH:MM`.
#[must_use]
pub fn clock(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// A month of a year (`month` 1–12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Month {
    pub year: i64,
    pub month: i64,
}

impl Month {
    /// The month a day is in.
    #[must_use]
    pub fn of(day: i64) -> Self {
        let (year, month, _) = civil_from_days(day);
        Self { year, month }
    }

    /// Its first day.
    #[must_use]
    pub fn first(self) -> i64 {
        days_from_civil(self.year, self.month, 1)
    }

    /// The month `n` months later (earlier, for a negative `n`).
    #[must_use]
    pub fn add(self, n: i64) -> Self {
        let total = self.year * 12 + (self.month - 1) + n;
        Self {
            year: total.div_euclid(12),
            month: total.rem_euclid(12) + 1,
        }
    }

    #[must_use]
    pub fn contains(self, day: i64) -> bool {
        Self::of(day) == self
    }

    /// "November 2026".
    #[must_use]
    pub fn label(self) -> String {
        const NAMES: [&str; 12] = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        let name = usize::try_from(self.month - 1)
            .ok()
            .and_then(|i| NAMES.get(i))
            .unwrap_or(&"?");
        format!("{name} {}", self.year)
    }

    /// The days the month is drawn with: six whole weeks, Monday first,
    /// from the Monday on or before its first day. Always 42, so the
    /// popover doesn't change height between months.
    #[must_use]
    pub fn grid(self) -> Vec<i64> {
        let first = self.first();
        // 1970-01-01 was a Thursday: day 0 is weekday 3 counting from Monday
        let monday = first - (first + 3).rem_euclid(7);
        (monday..monday + 42).collect()
    }
}

/// One end of a range as the picker shows it: a day and, unless it's
/// midnight, a time of day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct End {
    pub day: i64,
    pub time: Option<u32>,
}

/// A selection bound (`2026-10-04`, `2026-10-04T13:12:00Z`, with or without
/// fractions) as a day and minutes since midnight. Seconds are dropped:
/// the picker works in minutes.
fn instant(value: &str) -> Option<(i64, u32)> {
    let day = parse_day(value.get(..10)?)?;
    match value.get(10..) {
        None | Some("") => Some((day, 0)),
        Some(rest) => {
            let time = rest.strip_prefix('T')?;
            Some((day, parse_time(time.get(..5)?)?))
        }
    }
}

/// A selected range's ends as the picker shows them. Ranges are half-open,
/// so an end at midnight shows the last day inside the range: `[2026-09-22,
/// 2026-09-28)` is 2026-09-22 until 2026-09-27.
#[must_use]
pub fn ends(from: Option<&str>, to: Option<&str>) -> (Option<End>, Option<End>) {
    let from = from.and_then(instant).map(|(day, time)| End {
        day,
        time: (time != 0).then_some(time),
    });
    let until = to.and_then(instant).map(|(day, time)| match time {
        0 => End {
            day: day - 1,
            time: None,
        },
        time => End {
            day,
            time: Some(time),
        },
    });
    (from, until)
}

/// The other way: the half-open bounds a selection takes for the ends the
/// picker shows. An until without a time takes in its whole day.
#[must_use]
pub fn bounds(from: Option<End>, until: Option<End>) -> (Option<String>, Option<String>) {
    let at = |day: i64, time: u32| format!("{}T{}:00Z", iso(day), clock(time));
    (
        from.map(|f| at(f.day, f.time.unwrap_or(0))),
        until.map(|u| match u.time {
            None => at(u.day + 1, 0),
            Some(time) => at(u.day, time),
        }),
    )
}

/// Minutes since 1970 where an end starts (`from`) or stops (`until`,
/// exclusive).
fn minutes(end: End, until: bool) -> i64 {
    match end.time {
        Some(t) => end.day * 1440 + i64::from(t),
        None if until => (end.day + 1) * 1440,
        None => end.day * 1440,
    }
}

/// How long a range is: "30 days", "1 day", "1h", "26h 30m".
#[must_use]
pub fn length(from: End, until: End) -> String {
    let span = minutes(until, true) - minutes(from, false);
    if span <= 0 {
        return "empty".into();
    }
    let (days, hours, mins) = (span / 1440, span / 60, span % 60);
    if span % 1440 == 0 {
        if days == 1 {
            "1 day".into()
        } else {
            format!("{days} days")
        }
    } else if mins == 0 {
        format!("{hours}h")
    } else if hours == 0 {
        format!("{mins}m")
    } else {
        format!("{hours}h {mins}m")
    }
}

/// A range compactly, as the trigger shows it beside its label: the year
/// left out when it's `today`'s, the second date left out when it's the
/// same day. "10-04 → 11-02", "11-02 13:12 → 14:12".
#[must_use]
pub fn short_span(from: Option<End>, until: Option<End>, today: i64) -> String {
    let year = Month::of(today).year;
    let date = |day: i64| {
        let text = iso(day);
        if Month::of(day).year == year {
            text[5..].to_string()
        } else {
            text
        }
    };
    let timed = from.is_some_and(|e| e.time.is_some()) || until.is_some_and(|e| e.time.is_some());
    let end = |e: End, until: bool| {
        if timed {
            // an until at midnight is the start of the next day
            let (day, time) = match e.time {
                Some(t) => (e.day, t),
                None if until => (e.day + 1, 0),
                None => (e.day, 0),
            };
            format!("{} {}", date(day), clock(time))
        } else {
            date(e.day)
        }
    };
    match (from, until) {
        (Some(f), Some(u)) if f.day == u.day && timed => {
            let until = end(u, true);
            let time = until.rsplit_once(' ').map_or(until.as_str(), |(_, t)| t);
            format!("{} → {time}", end(f, false))
        }
        (Some(f), Some(u)) if f.day == u.day => date(f.day),
        (Some(f), Some(u)) => format!("{} → {}", end(f, false), end(u, true)),
        (Some(f), None) => format!("from {}", end(f, false)),
        (None, Some(u)) => format!("until {}", end(u, true)),
        (None, None) => String::new(),
    }
}

/// Picking on the calendar: the first click starts a range, the second
/// ends it, either way round.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pick {
    pub start: Option<i64>,
    pub end: Option<i64>,
}

impl Pick {
    #[must_use]
    pub fn click(self, day: i64) -> Self {
        match (self.start, self.end) {
            (Some(start), None) => Self {
                start: Some(start.min(day)),
                end: Some(start.max(day)),
            },
            _ => Self {
                start: Some(day),
                end: None,
            },
        }
    }

    /// Whether a range has been started and not ended.
    #[must_use]
    pub fn picking(self) -> bool {
        self.start.is_some() && self.end.is_none()
    }

    /// The days shown as inside, first and last: while picking, from the
    /// start to the day pointed at.
    #[must_use]
    pub fn span(self, pointed: Option<i64>) -> Option<(i64, i64)> {
        let start = self.start?;
        let end = self.end.or(pointed).unwrap_or(start);
        Some((start.min(end), start.max(end)))
    }
}

/// A window's shorthand beside its name in the list: "7d", "24h", "MTD",
/// "−1d".
#[must_use]
pub fn window_code(window: &RelativeWindow) -> String {
    let unit = match window.unit {
        RelativeUnit::Minute => "min",
        RelativeUnit::Hour => "h",
        RelativeUnit::Day => "d",
        RelativeUnit::Week => "w",
        RelativeUnit::Month => "M",
        RelativeUnit::Quarter => "Q",
        RelativeUnit::Year => "y",
    };
    let to_date = match window.unit {
        RelativeUnit::Week => "WTD",
        RelativeUnit::Month => "MTD",
        RelativeUnit::Quarter => "QTD",
        RelativeUnit::Year => "YTD",
        _ => "",
    };
    match (window.last, window.ago) {
        (1, 0) if !to_date.is_empty() => to_date.into(),
        (1, 0) => format!("0{unit}"),
        (1, 1) => format!("−1{unit}"),
        (n, 0) => format!("{n}{unit}"),
        (n, ago) => format!("{n}{unit}−{ago}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(text: &str) -> i64 {
        match parse_day(text) {
            Some(d) => d,
            None => panic!("{text} should be a day"),
        }
    }

    #[test]
    fn days_are_typed_strictly() {
        assert_eq!(parse_day("1970-01-01"), Some(0));
        assert_eq!(parse_day(" 2026-11-02 "), Some(day("2026-11-02")));
        assert_eq!(iso(day("2024-02-29")), "2024-02-29");
        for bad in [
            "2026-02-30",
            "2025-02-29",
            "2026-13-01",
            "2026-00-10",
            "2026-1-01",
            "26-01-01",
            "2026-01-01x",
            "",
            "2026-01",
        ] {
            assert_eq!(parse_day(bad), None, "{bad:?} isn't a day");
        }
    }

    #[test]
    fn times_are_typed_strictly() {
        assert_eq!(parse_time("00:00"), Some(0));
        assert_eq!(parse_time("9:05"), Some(545));
        assert_eq!(parse_time("23:59"), Some(1439));
        for bad in ["24:00", "12:60", "12", "12:5", ":30", "ab:cd", "123:00"] {
            assert_eq!(parse_time(bad), None, "{bad:?} isn't a time");
        }
        assert_eq!(clock(545), "09:05");
    }

    #[test]
    fn a_month_is_six_weeks_from_a_monday() {
        let november = Month::of(day("2026-11-02"));
        assert_eq!(
            november,
            Month {
                year: 2026,
                month: 11
            }
        );
        assert_eq!(november.label(), "November 2026");
        let grid = november.grid();
        assert_eq!(grid.len(), 42);
        // 1 November 2026 is a Sunday: the grid starts on Monday 26 October
        assert_eq!(grid.first().copied().map(iso), Some("2026-10-26".into()));
        assert_eq!(grid.last().copied().map(iso), Some("2026-12-06".into()));
        // a month starting on a Monday starts its own grid (June 2026)
        assert_eq!(
            Month {
                year: 2026,
                month: 6
            }
            .grid()
            .first()
            .copied()
            .map(iso),
            Some("2026-06-01".into())
        );
        assert!(november.contains(day("2026-11-30")) && !november.contains(day("2026-12-01")));
    }

    #[test]
    fn months_step_across_years() {
        let january = Month {
            year: 2026,
            month: 1,
        };
        assert_eq!(
            january.add(-1),
            Month {
                year: 2025,
                month: 12
            }
        );
        assert_eq!(
            january.add(12),
            Month {
                year: 2027,
                month: 1
            }
        );
        assert_eq!(
            Month {
                year: 2026,
                month: 12
            }
            .add(1),
            Month {
                year: 2027,
                month: 1
            }
        );
    }

    #[test]
    fn a_range_of_whole_days_shows_its_last_day() {
        let (from, until) = ends(Some("2026-09-22T00:00:00Z"), Some("2026-09-28T00:00:00Z"));
        assert_eq!(
            from,
            Some(End {
                day: day("2026-09-22"),
                time: None
            })
        );
        assert_eq!(
            until,
            Some(End {
                day: day("2026-09-27"),
                time: None
            })
        );
        // and picking those days sends the same half-open range back
        assert_eq!(
            bounds(from, until),
            (
                Some("2026-09-22T00:00:00Z".into()),
                Some("2026-09-28T00:00:00Z".into())
            )
        );
        // plain dates read the same
        assert_eq!(ends(Some("2026-09-22"), Some("2026-09-28")), (from, until));
    }

    #[test]
    fn times_of_day_are_kept_both_ways() {
        let (from, until) = ends(
            Some("2026-11-02T13:12:07.123456Z"),
            Some("2026-11-02T14:12:00Z"),
        );
        assert_eq!(
            from,
            Some(End {
                day: day("2026-11-02"),
                time: Some(13 * 60 + 12)
            })
        );
        assert_eq!(
            until,
            Some(End {
                day: day("2026-11-02"),
                time: Some(14 * 60 + 12)
            })
        );
        assert_eq!(
            bounds(from, until),
            (
                Some("2026-11-02T13:12:00Z".into()),
                Some("2026-11-02T14:12:00Z".into())
            )
        );
        assert_eq!(ends(Some("not a time"), None), (None, None));
    }

    #[test]
    fn lengths_read_in_days_or_hours() {
        let end = |d: &str, t: Option<u32>| End {
            day: day(d),
            time: t,
        };
        assert_eq!(
            length(end("2026-10-04", None), end("2026-11-02", None)),
            "30 days"
        );
        assert_eq!(
            length(end("2026-11-02", None), end("2026-11-02", None)),
            "1 day"
        );
        assert_eq!(
            length(end("2026-11-02", Some(780)), end("2026-11-02", Some(840))),
            "1h"
        );
        assert_eq!(
            length(end("2026-11-01", Some(840)), end("2026-11-02", Some(870))),
            "24h 30m"
        );
        assert_eq!(
            length(end("2026-11-02", Some(840)), end("2026-11-02", Some(845))),
            "5m"
        );
        assert_eq!(
            length(end("2026-11-02", Some(840)), end("2026-11-02", Some(600))),
            "empty"
        );
    }

    #[test]
    fn spans_are_short() {
        let today = day("2026-11-02");
        let end = |d: &str, t: Option<u32>| {
            Some(End {
                day: day(d),
                time: t,
            })
        };
        assert_eq!(
            short_span(end("2026-10-04", None), end("2026-11-02", None), today),
            "10-04 → 11-02"
        );
        assert_eq!(
            short_span(end("2025-12-01", None), end("2026-01-31", None), today),
            "2025-12-01 → 01-31"
        );
        assert_eq!(
            short_span(end("2026-11-02", None), end("2026-11-02", None), today),
            "11-02"
        );
        assert_eq!(
            short_span(
                end("2026-11-02", Some(792)),
                end("2026-11-02", Some(852)),
                today
            ),
            "11-02 13:12 → 14:12"
        );
        // a whole day beside a timed end: the until is the next midnight
        assert_eq!(
            short_span(end("2026-11-01", Some(852)), end("2026-11-02", None), today),
            "11-01 14:12 → 11-03 00:00"
        );
        assert_eq!(
            short_span(end("2026-10-04", None), None, today),
            "from 10-04"
        );
        assert_eq!(short_span(None, None, today), "");
    }

    #[test]
    fn a_second_click_ends_the_range_either_way_round() {
        let first = Pick::default().click(10);
        assert!(first.picking());
        assert_eq!(
            first.span(Some(7)),
            Some((7, 10)),
            "pointing before the start"
        );
        assert_eq!(first.span(None), Some((10, 10)));
        let done = first.click(4);
        assert_eq!(
            done,
            Pick {
                start: Some(4),
                end: Some(10)
            }
        );
        assert!(!done.picking());
        assert_eq!(
            done.span(Some(20)),
            Some((4, 10)),
            "pointing doesn't move a finished range"
        );
        // a third click starts over
        assert_eq!(
            done.click(12),
            Pick {
                start: Some(12),
                end: None
            }
        );
        assert_eq!(Pick::default().span(Some(3)), None);
    }

    #[test]
    fn windows_have_codes() {
        let w = |unit, last, ago| RelativeWindow { unit, last, ago };
        assert_eq!(window_code(&w(RelativeUnit::Day, 7, 0)), "7d");
        assert_eq!(window_code(&w(RelativeUnit::Hour, 24, 0)), "24h");
        assert_eq!(window_code(&w(RelativeUnit::Hour, 1, 0)), "0h");
        assert_eq!(window_code(&w(RelativeUnit::Day, 1, 0)), "0d");
        assert_eq!(window_code(&w(RelativeUnit::Day, 1, 1)), "−1d");
        assert_eq!(window_code(&w(RelativeUnit::Month, 1, 0)), "MTD");
        assert_eq!(window_code(&w(RelativeUnit::Month, 1, 1)), "−1M");
        assert_eq!(window_code(&w(RelativeUnit::Year, 1, 0)), "YTD");
        assert_eq!(window_code(&w(RelativeUnit::Week, 2, 3)), "2w−3");
    }
}
