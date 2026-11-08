//! What a dashboard shows, as plain data: frames applied to it, values
//! formatted, bars scaled. No DOM here, so `cargo test` covers all of it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

use serde_json::Value;
use tidepool_config::format::{NumberFormat, Style};
use tidepool_protocol::live::{
    FilterFrame, RelativeUnit, RelativeWindow, SelectedField, ServerMessage, TreeNode, ValueState,
    VisualFrame, VisualInfo,
};

use crate::calendar::{civil_from_days, days_from_civil};

/// The open dashboard's latest content.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Board {
    pub epoch: u64,
    pub visuals: HashMap<String, VisualFrame>,
    pub filters: HashMap<String, FilterFrame>,
    /// Cells whose value the last frame changed: (visual, row, column).
    pub fresh: HashSet<(String, usize, usize)>,
}

impl Board {
    /// Apply a frame: everything in it replaces what was there, as a whole.
    /// A frame older than what's shown is ignored.
    pub fn apply(&mut self, message: &ServerMessage) -> bool {
        let ServerMessage::Frame {
            epoch,
            visuals,
            filters,
        } = message
        else {
            return false;
        };
        if *epoch < self.epoch {
            return false;
        }
        self.epoch = *epoch;
        self.fresh.clear();
        for visual in visuals {
            if let (Some(old), Some(new)) = (
                self.visuals.get(&visual.id).and_then(|v| v.result.as_ref()),
                visual.result.as_ref(),
            ) {
                // rows are matched by their dimension values, so a row
                // that only moved isn't new
                let dims = new.columns.iter().filter(|c| c.role == "dimension").count();
                let before: HashMap<&[Value], &Vec<Value>> = old
                    .rows
                    .iter()
                    .map(|r| (&r[..dims.min(r.len())], r))
                    .collect();
                for (i, row) in new.rows.iter().enumerate() {
                    let previous = before.get(&row[..dims.min(row.len())]);
                    for (j, value) in row.iter().enumerate().skip(dims) {
                        if previous.and_then(|p| p.get(j)) != Some(value) {
                            self.fresh.insert((visual.id.clone(), i, j));
                        }
                    }
                }
            }
            self.visuals.insert(visual.id.clone(), visual.clone());
        }
        for filter in filters {
            self.filters.insert(filter.field.clone(), filter.clone());
        }
        true
    }
}

/// A result value as a number, if it is one (decimals arrive as text).
#[must_use]
pub fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Digits grouped by thousands: `1234567.5` → `1,234,567.5`.
fn grouped(text: &str) -> String {
    let (sign, rest) = text.strip_prefix('-').map_or(("", text), |r| ("-", r));
    let (whole, fraction) = rest
        .split_once('.')
        .map_or((rest, None), |(w, f)| (w, Some(f)));
    let mut out = String::new();
    for (i, digit) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    match fraction {
        Some(f) => format!("{sign}{out}.{f}"),
        None => format!("{sign}{out}"),
    }
}

/// A value as it reads on screen.
#[must_use]
pub fn display(value: &Value) -> String {
    match value {
        Value::Null => "–".into(),
        Value::String(s) if s.parse::<f64>().is_ok() => grouped(s),
        Value::String(s) => s.clone(),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => grouped(&i.to_string()),
            (None, Some(f)) => grouped(&format!("{f:.2}")),
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

/// The live socket's address on the server the page came from, keeping a
/// `?token=` the page was opened with.
#[must_use]
pub fn socket_url(protocol: &str, host: &str, search: &str) -> String {
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    let token = search
        .trim_start_matches('?')
        .split('&')
        .find(|pair| pair.starts_with("token="));
    match token {
        Some(token) => format!("{scheme}://{host}/v1/ws?{token}"),
        None => format!("{scheme}://{host}/v1/ws"),
    }
}

/// The start of the period after the one starting at `date` (`YYYY-MM-DD`):
/// where a brushed range on a time axis ends, since ranges are half-open.
#[must_use]
pub fn next_period(date: &str, grain: &str) -> Option<String> {
    let mut parts = date.get(..10)?.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (parts.next()??, parts.next()??, parts.next()??);
    let (y, m, d) = match grain {
        "week" => civil_from_days(days_from_civil(y, m, d) + 7),
        "month" | "quarter" | "year" => {
            let months = match grain {
                "month" => 1,
                "quarter" => 3,
                _ => 12,
            };
            let total = y * 12 + (m - 1) + months;
            (total.div_euclid(12), total.rem_euclid(12) + 1, d)
        }
        _ => civil_from_days(days_from_civil(y, m, d) + 1),
    };
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// A timestamp as the server writes one (`2026-11-03T19:05:00Z`, maybe with
/// a fraction of a second, maybe without the `Z`): microseconds since the
/// Unix epoch, and whether it had the `Z`.
fn micros_of(text: &str) -> Option<(i64, bool)> {
    let (rest, zoned) = match text.strip_suffix('Z') {
        Some(rest) => (rest, true),
        None => (text, false),
    };
    let (date, time) = rest.split_once('T')?;
    let mut ymd = date.split('-').map(|p| p.parse::<i64>().ok());
    let days = days_from_civil(ymd.next()??, ymd.next()??, ymd.next()??);
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut hms = clock.split(':').map(|p| p.parse::<i64>().ok());
    let seconds = hms.next()?? * 3600 + hms.next()?? * 60 + hms.next()??;
    if fraction.len() > 6 || !fraction.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let micros = format!("{fraction:0<6}").parse::<i64>().ok()?;
    Some((days * MICROS_PER_DAY + seconds * 1_000_000 + micros, zoned))
}

/// [`micros_of`]'s inverse, with a fraction only when there is one.
fn timestamp_text(micros: i64, zoned: bool) -> String {
    let (y, m, d) = civil_from_days(micros.div_euclid(MICROS_PER_DAY));
    let of_day = micros.rem_euclid(MICROS_PER_DAY);
    let (seconds, fraction) = (of_day / 1_000_000, of_day % 1_000_000);
    let mut text = format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if fraction != 0 {
        let _ = write!(text, ".{fraction:06}");
    }
    if zoned {
        text.push('Z');
    }
    text
}

/// What brushing a line chart's categories `first` to `last` selects, as
/// `select_range` bounds (half-open): for a time field at a grain, whole
/// periods, from the first one's start to the start of the period after the
/// last (hours and minutes start at a time, the others on a date); for a
/// date, the days; for a timestamp, the values themselves, from the first to
/// just after the last.
#[must_use]
pub fn brush_range(first: &str, last: &str, grain: Option<&str>) -> Option<(String, String)> {
    let midnight = |date: String| format!("{date}T00:00:00Z");
    let span = |seconds: i64| {
        let (at, zoned) = micros_of(last)?;
        Some((
            first.to_string(),
            timestamp_text(at + seconds * 1_000_000, zoned),
        ))
    };
    match grain {
        Some("hour") => span(3600),
        Some("minute") => span(60),
        Some(grain) => Some((
            midnight(first.to_string()),
            midnight(next_period(last, grain)?),
        )),
        None if micros_of(last).is_none() => Some((first.to_string(), next_period(last, "day")?)),
        None => {
            let (at, zoned) = micros_of(last)?;
            Some((first.to_string(), timestamp_text(at + 1, zoned)))
        }
    }
}

/// A selected range as it reads in a chip. Ranges are half-open, so a range
/// of whole days shows the last day it includes: `2026-09-22T00:00:00Z` to
/// `2026-09-28T00:00:00Z` is "2026-09-22 to 2026-09-27".
#[must_use]
pub fn range_label(from: Option<&Value>, to: Option<&Value>) -> String {
    let day = |v: &Value| {
        let s = v.as_str()?;
        let date = s
            .strip_suffix("T00:00:00Z")
            .or_else(|| (s.len() == 10).then_some(s))?;
        let mut parts = date.split('-').map(|p| p.parse::<i64>().ok());
        Some(days_from_civil(
            parts.next()??,
            parts.next()??,
            parts.next()??,
        ))
    };
    let civil = |days: i64| {
        let (y, m, d) = civil_from_days(days);
        format!("{y:04}-{m:02}-{d:02}")
    };
    // a brushed range of timestamps ends just after its last value: shown
    // as that value
    let last = |v: &Value| {
        let (at, zoned) = micros_of(v.as_str()?)?;
        (at.rem_euclid(1_000_000) == 1).then(|| timestamp_text(at - 1, zoned))
    };
    // days only when both ends are: a range of hours may end at a midnight
    let days = [from, to].into_iter().flatten().all(|v| day(v).is_some());
    let start = from.map(|v| day(v).filter(|_| days).map_or_else(|| display(v), civil));
    let end = to.map(|v| {
        day(v)
            .filter(|_| days)
            .map_or_else(|| last(v).unwrap_or_else(|| display(v)), |d| civil(d - 1))
    });
    match (start, end) {
        (Some(start), Some(end)) if start == end => start,
        (Some(start), Some(end)) => format!("{start} to {end}"),
        (Some(start), None) => format!("from {start}"),
        (None, Some(end)) => format!("until {end}"),
        (None, None) => "everything".into(),
    }
}

/// The windows a date range offers, newest first: hours too when the field
/// has a time of day.
#[must_use]
pub fn presets(timed: bool) -> Vec<RelativeWindow> {
    let window = |unit, last, ago| RelativeWindow { unit, last, ago };
    let mut all = Vec::new();
    if timed {
        all.push(window(RelativeUnit::Hour, 1, 0));
        all.push(window(RelativeUnit::Hour, 24, 0));
    }
    all.extend([
        window(RelativeUnit::Day, 1, 0),
        window(RelativeUnit::Day, 1, 1),
        window(RelativeUnit::Day, 7, 0),
        window(RelativeUnit::Day, 30, 0),
        window(RelativeUnit::Day, 90, 0),
        window(RelativeUnit::Month, 1, 0),
        window(RelativeUnit::Month, 1, 1),
        window(RelativeUnit::Year, 1, 0),
    ]);
    all
}

/// What a selected field reads as in a chip: a window by name ("Last 7
/// days"), values listed, a range by its ends.
#[must_use]
pub fn selected_label(selected: &SelectedField) -> String {
    if let Some(text) = &selected.contains {
        return format!("“{text}”");
    }
    match (&selected.relative, &selected.values) {
        (Some(window), _) => window.label(),
        (None, Some(values)) => values.iter().map(display).collect::<Vec<_>>().join(", "),
        (None, None) => range_label(selected.from.as_ref(), selected.to.as_ref()),
    }
}

/// Which way a click on a table's column header sorts: the other way if it
/// is already the sorted column, otherwise largest first for numbers and A
/// to Z for text. `true` is descending.
#[must_use]
pub fn next_sort(current: Option<&(String, bool)>, column: &str, numeric: bool) -> bool {
    match current {
        Some((c, desc)) if c == column => !desc,
        _ => numeric,
    }
}

/// The query behind a visual, under a selection: what its CSV export runs
/// (the visual's dimensions and metrics, every selected field as a filter).
#[must_use]
pub fn export_request(
    info: &tidepool_protocol::live::VisualInfo,
    selection: &[tidepool_protocol::live::SelectedField],
) -> tidepool_protocol::QueryRequest {
    tidepool_protocol::QueryRequest {
        metrics: info.metrics.clone(),
        dimensions: info.dimensions.clone(),
        filters: selection
            .iter()
            .map(|s| tidepool_protocol::QueryFilter {
                field: s.field.clone(),
                values: s.values.clone(),
                // a relative window as it stands: the export is what's shown
                from: s.from.clone(),
                to: s.to.clone(),
                relative: None,
                contains: s.contains.clone(),
            })
            .collect(),
        sort: Vec::new(),
        limit: None,
    }
}

/// What a `rows` visual's CSV export reads: its rows under the selection,
/// as many as it shows.
#[must_use]
pub fn export_rows_request(
    info: &tidepool_protocol::live::VisualInfo,
    selection: &[tidepool_protocol::live::SelectedField],
    limit: usize,
) -> tidepool_protocol::RowsRequest {
    let query = export_request(info, selection);
    tidepool_protocol::RowsRequest {
        fields: info.dimensions.iter().map(|d| d.field.clone()).collect(),
        filters: query.filters,
        limit: Some(limit),
    }
}

/// `text` cut where `needle` is found, ignoring the case of ASCII letters
/// as the server does: (part, whether it's a match), in order. No needle,
/// or none found, is the whole text unmarked.
#[must_use]
pub fn marks(text: &str, needle: &str) -> Vec<(String, bool)> {
    let fold = |b: u8| b.to_ascii_lowercase();
    let (hay, find) = (text.as_bytes(), needle.as_bytes());
    let mut parts = Vec::new();
    if find.is_empty() || find.len() > hay.len() {
        return vec![(text.to_string(), false)];
    }
    let (mut from, mut at) = (0, 0);
    while at + find.len() <= hay.len() {
        let hit = hay[at..at + find.len()]
            .iter()
            .zip(find)
            .all(|(h, n)| fold(*h) == fold(*n));
        // a match of whole characters only: the needle is whole ones, so
        // it starts and ends where they do
        if hit && text.is_char_boundary(at) && text.is_char_boundary(at + find.len()) {
            if at > from {
                parts.push((text[from..at].to_string(), false));
            }
            parts.push((text[at..at + find.len()].to_string(), true));
            at += find.len();
            from = at;
        } else {
            at += 1;
        }
    }
    if from < text.len() {
        parts.push((text[from..].to_string(), false));
    }
    parts
}

/// The window a filter list should ask for, if the rows on screen
/// (`first`, `visible` of them) aren't all in the one it has (`have`:
/// offset and length). Asks for a margin either side, so scrolling a bit
/// doesn't ask again.
#[must_use]
pub fn wanted_window(
    first: usize,
    visible: usize,
    have: (usize, usize),
    total: usize,
) -> Option<(usize, usize)> {
    let end = (first + visible).min(total);
    let covered = first >= have.0 && end <= have.0 + have.1;
    (!covered).then(|| {
        let offset = first.saturating_sub(60);
        (offset, visible + 120)
    })
}

/// Bytes as a person reads them.
#[must_use]
pub fn bytes(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let n = n as f64;
    match n {
        n if n >= 1e9 => format!("{:.1} GB", n / 1e9),
        n if n >= 1e6 => format!("{:.1} MB", n / 1e6),
        n if n >= 1e3 => format!("{:.0} kB", n / 1e3),
        n => format!("{n:.0} B"),
    }
}

/// "updated 0.3 s ago" and friends.
#[must_use]
pub fn ago(millis: f64) -> String {
    let seconds = (millis / 1000.0).max(0.0);
    if seconds < 10.0 {
        format!("{seconds:.1} s ago")
    } else if seconds < 120.0 {
        format!("{seconds:.0} s ago")
    } else {
        format!("{:.0} min ago", seconds / 60.0)
    }
}

/// A row a client could post to a table with these (name, type) columns:
/// for the ingest docs.
#[must_use]
pub fn sample_row(columns: &[(String, String)]) -> Value {
    let mut row = serde_json::Map::new();
    for (name, ty) in columns {
        let value = match ty.as_str() {
            "string" => Value::String(format!("{name} 1")),
            "bool" => Value::Bool(true),
            "int32" | "int64" => serde_json::json!(1),
            "float64" => serde_json::json!(1.5),
            "date" => Value::String("2026-10-08".into()),
            "timestamp" | "timestamptz" => Value::String("2026-10-08T12:00:00Z".into()),
            t if t.starts_with("decimal") => Value::String("12.34".into()),
            _ => Value::Null,
        };
        row.insert(name.clone(), value);
    }
    Value::Object(row)
}

/// A filter row's classes: its state, and for a tree node whether it is
/// inside a selected branch or holds a selection below it.
#[must_use]
pub fn value_class(state: ValueState, inside: bool, holds: bool) -> String {
    let mut class = String::from(match state {
        ValueState::Selected => "value selected",
        ValueState::Possible => "value possible",
        ValueState::Excluded => "value excluded",
    });
    if inside && state != ValueState::Selected {
        class.push_str(" inside");
    }
    if holds {
        class.push_str(" holds");
    }
    class
}

/// A tree node's row: its classes, and what its twist shows.
#[must_use]
pub fn node_class(node: &TreeNode) -> String {
    value_class(node.state, node.inside, node.holds)
}

/// A metric's value as it reads on screen, by its format (see
/// [`tidepool_config::format`]): `compact` reads 167,791,795.92 as 167.8M.
/// Without a format, or one that doesn't read, as any value. The value
/// itself is never changed, only shown.
#[must_use]
pub fn formatted(value: &Value, format: Option<&str>) -> String {
    let (Some(format), Some(n)) = (
        format.and_then(|f| f.parse::<NumberFormat>().ok()),
        number(value),
    ) else {
        return display(value);
    };
    let (n, unit) = match format.style {
        Style::Percent => (n * 100.0, "%"),
        _ => (n, ""),
    };
    let places = |default: usize| format.decimals.map_or(default, usize::from);
    let digits = if format.compact && n.abs() >= 1000.0 {
        short(n, places(1))
    } else {
        match format.style {
            Style::Int => fixed(n, places(0)),
            Style::Percent => fixed(n, places(1)),
            Style::Decimal | Style::Currency(_) => fixed(n, places(2)),
            // as it comes, unless the format says how many places
            Style::Plain => match format.decimals {
                Some(d) => fixed(n, d.into()),
                None => display(value),
            },
        }
    };
    match format.style {
        Style::Currency(code) => {
            let (sign, digits) = digits
                .strip_prefix('-')
                .map_or(("", digits.as_str()), |d| ("-", d));
            match symbol(&code) {
                Some(symbol) => format!("{sign}{symbol}{digits}"),
                None => format!("{sign}{code} {digits}"),
            }
        }
        _ => format!("{digits}{unit}"),
    }
}

/// The few currency signs nobody misreads; other currencies by their code.
fn symbol(code: &str) -> Option<&'static str> {
    match code {
        "EUR" => Some("€"),
        "USD" => Some("$"),
        "GBP" => Some("£"),
        "JPY" => Some("¥"),
        _ => None,
    }
}

/// `n` with `places` decimals, digits grouped; never "-0".
fn fixed(n: f64, places: usize) -> String {
    let text = format!("{:.places$}", half_up(n, places));
    let zero = text
        .trim_start_matches('-')
        .chars()
        .all(|c| matches!(c, '0' | '.'));
    grouped(if zero {
        text.trim_start_matches('-')
    } else {
        &text
    })
}

/// `n` rounded to `places` decimals, halves away from zero (as people
/// round: 2.5 is 3), where `{:.N}` alone would round them to even.
fn half_up(n: f64, places: usize) -> f64 {
    let scale = 10f64.powi(i32::try_from(places).unwrap_or(0));
    let rounded = (n * scale).round() / scale;
    if rounded.is_finite() { rounded } else { n }
}

/// `n` (a thousand or more) in thousands, millions, billions or trillions,
/// with `places` decimals: 167.8M. A value that rounds up to the next unit
/// reads in it (999,960 is 1.0M, not 1000.0k).
fn short(n: f64, places: usize) -> String {
    const UNITS: [(f64, &str); 4] = [(1e3, "k"), (1e6, "M"), (1e9, "B"), (1e12, "T")];
    let mut unit = UNITS
        .iter()
        .rposition(|(size, _)| n.abs() >= *size)
        .unwrap_or(0);
    loop {
        let (size, suffix) = UNITS[unit];
        let text = format!("{:.places$}", half_up(n / size, places));
        let rounded_up = text
            .trim_start_matches('-')
            .parse::<f64>()
            .is_ok_and(|v| v >= 1000.0);
        if !rounded_up || unit + 1 == UNITS.len() {
            return format!("{}{suffix}", grouped(&text));
        }
        unit += 1;
    }
}

/// What a filter row shows on the right: its measure when the filter has one
/// (a missing one, sent as null, reads as no value), otherwise its rows.
#[must_use]
pub fn beside(
    measured_by: bool,
    measure: Option<&Value>,
    rows: i64,
    format: Option<&str>,
) -> String {
    if measured_by {
        return formatted(measure.unwrap_or(&Value::Null), format);
    }
    if rows > 0 {
        display(&Value::from(rows))
    } else {
        String::new()
    }
}

/// What a selected field reads as in a chip, with a tree's nodes by their
/// paths ("outdoor › water") when the tree's frame has them.
#[must_use]
pub fn chip_label(selected: &SelectedField, frame: Option<&FilterFrame>) -> String {
    match frame.filter(|f| !f.chosen.is_empty()) {
        Some(frame) => frame
            .chosen
            .iter()
            .map(|c| c.path.iter().map(display).collect::<Vec<_>>().join(" › "))
            .collect::<Vec<_>>()
            .join(", "),
        None => selected_label(selected),
    }
}

/// The messages sent and not answered yet, oldest first: whether each
/// changes the selection, and when it went out. The server answers each
/// with a `done`, in order, after everything it caused.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InFlight {
    queue: VecDeque<(bool, f64)>,
    /// How long the last selection took to be answered.
    answered: Option<f64>,
}

impl InFlight {
    /// A message of type `kind` went out at `at` (milliseconds).
    pub fn sent(&mut self, kind: &str, at: f64) {
        self.queue.push_back((reselects(kind), at));
    }

    /// The oldest one is answered at `at`: how long it took, when it was a
    /// selection.
    pub fn done(&mut self, at: f64) -> Option<f64> {
        let (reselected, sent) = self.queue.pop_front()?;
        let took = reselected.then(|| (at - sent).max(0.0));
        if took.is_some() {
            self.answered = took;
        }
        took
    }

    /// How long the last selection took to be answered (what the context
    /// bar shows as "answered in").
    pub fn answered(&self) -> Option<f64> {
        self.answered
    }

    /// The connection dropped: nothing is waiting any more.
    pub fn reset(&mut self) {
        self.queue.clear();
    }

    /// A selection is under way: what the dashboard shows is about to
    /// change.
    pub fn busy(&self) -> bool {
        self.queue.iter().any(|(reselects, _)| *reselects)
    }
}

/// A duration in milliseconds, compactly: "3.1 ms", "48 ms", "1.20 s".
#[must_use]
pub fn millis(ms: f64) -> String {
    if ms < 10.0 {
        format!("{ms:.1} ms")
    } else if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1000.0)
    }
}

/// The line beside a filter's name: how many values and how many are
/// selected, and the measure shown beside each. "1 of 5 · revenue",
/// "12 values".
#[must_use]
pub fn filter_meta(
    total: Option<usize>,
    selected: usize,
    measure: Option<&str>,
    tree: bool,
) -> String {
    let count = match (total, selected) {
        (_, n) if n > 0 && tree => format!("{n} selected"),
        (Some(total), n) if n > 0 => format!("{n} of {total}"),
        (_, n) if n > 0 => format!("{n} selected"),
        (Some(1), _) if !tree => "1 value".into(),
        (Some(total), _) if !tree => format!("{total} values"),
        _ => "tree".into(),
    };
    match measure {
        Some(m) => format!("{count} · {m}"),
        None => count,
    }
}

/// How a visual is set up, as the tags in its header: "grain: day",
/// `on_click: select`, `on_brush: select_range`.
#[must_use]
pub fn visual_tags(info: &VisualInfo) -> Vec<String> {
    let mut tags: Vec<String> = info
        .dimensions
        .iter()
        .filter_map(|d| d.grain.as_ref().map(|g| format!("grain: {g}")))
        .collect();
    if info.on_click {
        tags.push("on_click: select".into());
    }
    if info.on_brush {
        tags.push("on_brush: select_range".into());
    }
    tags
}

/// A bar's length in percent of the longest, for meters beside values: 0
/// for what isn't a number or a largest that isn't positive.
#[must_use]
pub fn share(value: Option<&Value>, largest: f64) -> f64 {
    match value.and_then(number) {
        Some(n) if largest > 0.0 && n > 0.0 => (n / largest * 100.0).min(100.0),
        _ => 0.0,
    }
}

/// What the server holds, for the status bar: "2.42M rows · 148 segments
/// · 6 tables".
#[must_use]
pub fn data_summary(tables: &[tidepool_protocol::TableStatus]) -> String {
    let rows: u64 = tables.iter().map(|t| t.rows).sum();
    let segments: usize = tables.iter().map(|t| t.segments).sum();
    #[allow(clippy::cast_precision_loss)] // a display figure
    let rows = compact(rows as f64);
    let plural = |n: usize, one: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {one}s")
        }
    };
    format!(
        "{rows} rows · {} · {}",
        plural(segments, "segment"),
        plural(tables.len(), "table")
    )
}

/// A count compactly, for the status bar: "950", "12.4k", "2.42M".
#[must_use]
pub fn compact(n: f64) -> String {
    let n = n.max(0.0);
    if n < 1000.0 {
        format!("{n:.0}")
    } else if n < 1_000_000.0 {
        format!("{:.1}k", n / 1000.0)
    } else if n < 1_000_000_000.0 {
        format!("{:.2}M", n / 1_000_000.0)
    } else {
        format!("{:.2}G", n / 1_000_000_000.0)
    }
}

/// Whether a client message of type `kind` changes the selection, so every
/// visual waits for its answer. (Paging a list or sorting a table doesn't.)
fn reselects(kind: &str) -> bool {
    matches!(
        kind,
        "select" | "select_range" | "select_relative" | "clear" | "back" | "forward"
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tidepool_protocol::live::{FilterValue, ValueState};

    use super::*;

    fn frame(epoch: u64, id: &str) -> ServerMessage {
        ServerMessage::Frame {
            epoch,
            visuals: vec![VisualFrame {
                id: id.into(),
                result: None,
                error: Some(format!("at {epoch}")),
                compare: None,
                totals: None,
            }],
            filters: vec![FilterFrame {
                field: "customers.region".into(),
                total: 1,
                offset: 0,
                values: vec![FilterValue {
                    value: json!("north"),
                    state: ValueState::Possible,
                    rows: 1,
                    measure: None,
                }],
                nodes: Vec::new(),
                chosen: Vec::new(),
            }],
        }
    }

    #[test]
    fn a_frame_replaces_what_it_carries_and_keeps_the_rest() {
        let mut board = Board::default();
        assert!(board.apply(&frame(3, "a")));
        assert!(board.apply(&frame(4, "b")));
        assert_eq!(board.epoch, 4);
        assert_eq!(board.visuals.len(), 2);
        assert_eq!(board.visuals["b"].error.as_deref(), Some("at 4"));
    }

    #[test]
    fn an_older_frame_is_ignored() {
        let mut board = Board::default();
        board.apply(&frame(5, "a"));
        assert!(!board.apply(&frame(4, "a")));
        assert_eq!(board.visuals["a"].error.as_deref(), Some("at 5"));
    }

    #[test]
    fn numbers_read_with_grouped_digits() {
        assert_eq!(display(&json!("1234567.50")), "1,234,567.50");
        assert_eq!(display(&json!(-12345)), "-12,345");
        assert_eq!(display(&json!(7.5)), "7.50");
        assert_eq!(display(&json!("north")), "north");
        assert_eq!(display(&Value::Null), "–");
    }

    #[test]
    fn the_socket_follows_the_page_and_keeps_its_token() {
        assert_eq!(
            socket_url("http:", "localhost:7070", ""),
            "ws://localhost:7070/v1/ws"
        );
        assert_eq!(
            socket_url("https:", "bi.example", "?dash=1&token=abc"),
            "wss://bi.example/v1/ws?token=abc"
        );
    }

    #[test]
    fn changed_cells_are_found_by_row_key() {
        use tidepool_protocol::{QueryColumn, QueryResult};
        let result = |rows: Value| QueryResult {
            columns: vec![
                QueryColumn {
                    name: "region".into(),
                    ty: "string".into(),
                    role: "dimension".into(),
                },
                QueryColumn {
                    name: "revenue".into(),
                    ty: "decimal(2)".into(),
                    role: "metric".into(),
                },
            ],
            rows: serde_json::from_value(rows).unwrap_or_default(),
            epoch: 0,
            as_of: None,
        };
        let frame = |epoch: u64, rows: Value| ServerMessage::Frame {
            epoch,
            visuals: vec![VisualFrame {
                id: "v".into(),
                result: Some(result(rows)),
                error: None,
                compare: None,
                totals: None,
            }],
            filters: Vec::new(),
        };
        let mut board = Board::default();
        board.apply(&frame(1, json!([["north", "1.00"], ["south", "2.00"]])));
        assert!(board.fresh.is_empty(), "nothing to compare with yet");
        // south grew and moved to the top; north didn't change
        board.apply(&frame(2, json!([["south", "3.00"], ["north", "1.00"]])));
        assert_eq!(board.fresh, HashSet::from([("v".to_string(), 0, 1)]));
    }

    #[test]
    fn periods_step_by_their_grain() {
        assert_eq!(
            next_period("2026-12-31", "day").as_deref(),
            Some("2027-01-01")
        );
        assert_eq!(
            next_period("2026-02-23", "week").as_deref(),
            Some("2026-03-02")
        );
        assert_eq!(
            next_period("2026-11-01", "quarter").as_deref(),
            Some("2027-02-01")
        );
        assert_eq!(
            next_period("2026-01-01", "year").as_deref(),
            Some("2027-01-01")
        );
        assert_eq!(next_period("not a date", "day"), None);
    }

    #[test]
    fn a_window_is_asked_for_only_when_the_screen_leaves_it() {
        assert_eq!(wanted_window(0, 12, (0, 100), 500), None);
        assert_eq!(wanted_window(95, 12, (0, 100), 500), Some((35, 132)));
        // near the end the window is short: the list ends there
        assert_eq!(wanted_window(495, 12, (480, 20), 500), None);
    }

    #[test]
    fn sizes_and_ages_read_like_words() {
        assert_eq!(bytes(2_500_000), "2.5 MB");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(ago(300.0), "0.3 s ago");
        assert_eq!(ago(180_000.0), "3 min ago");
    }

    #[test]
    fn a_sample_row_has_a_value_of_each_type() {
        let row = sample_row(&[
            ("id".into(), "string".into()),
            ("amount".into(), "decimal(14,2)".into()),
        ]);
        assert_eq!(row, json!({"id": "id 1", "amount": "12.34"}));
    }

    #[test]
    fn a_brush_selects_periods_days_or_the_values_themselves() {
        let range = |first: &str, last: &str, grain: Option<&str>| brush_range(first, last, grain);
        let both = |f: &str, t: &str| Some((f.to_string(), t.to_string()));
        // a timestamp at a grain: whole periods
        assert_eq!(
            range("2026-09-22", "2026-09-28", Some("day")),
            both("2026-09-22T00:00:00Z", "2026-09-29T00:00:00Z")
        );
        assert_eq!(
            range("2026-01-05", "2026-02-02", Some("week")),
            both("2026-01-05T00:00:00Z", "2026-02-09T00:00:00Z")
        );
        // hours and minutes: whole periods, as the timestamps they start at
        assert_eq!(
            range("2026-11-03T19:00:00Z", "2026-11-03T23:00:00Z", Some("hour")),
            both("2026-11-03T19:00:00Z", "2026-11-04T00:00:00Z")
        );
        assert_eq!(
            range("2026-11-03T19:05:00", "2026-11-03T19:05:00", Some("minute")),
            both("2026-11-03T19:05:00", "2026-11-03T19:06:00")
        );
        // a date: the days, as dates
        assert_eq!(
            range("2026-02-27", "2026-02-28", None),
            both("2026-02-27", "2026-03-01")
        );
        // a timestamp as it is (a minute column): the values, the last
        // included
        assert_eq!(
            range("2026-11-03T19:05:00Z", "2026-11-03T19:09:00Z", None),
            both("2026-11-03T19:05:00Z", "2026-11-03T19:09:00.000001Z")
        );
        assert_eq!(
            range(
                "2026-12-31T23:59:59.999999Z",
                "2026-12-31T23:59:59.999999Z",
                None
            ),
            both("2026-12-31T23:59:59.999999Z", "2027-01-01T00:00:00Z")
        );
        assert_eq!(
            range("2026-11-03T19:05:00.25", "2026-11-03T19:05:00.25", None),
            both("2026-11-03T19:05:00.25", "2026-11-03T19:05:00.250001")
        );
        // and the chip reads it as the values brushed
        assert_eq!(
            range_label(
                Some(&json!("2026-11-03T19:05:00Z")),
                Some(&json!("2026-11-03T19:09:00.000001Z"))
            ),
            "2026-11-03T19:05:00Z to 2026-11-03T19:09:00Z"
        );
        assert_eq!(
            range_label(
                Some(&json!("2026-11-03T19:05:00Z")),
                Some(&json!("2026-11-03T19:05:00.000001Z"))
            ),
            "2026-11-03T19:05:00Z"
        );
        assert_eq!(range("x", "y", Some("day")), None);
    }

    #[test]
    fn whole_day_ranges_show_the_last_day_they_include() {
        let at = |s: &str| json!(s);
        assert_eq!(
            range_label(
                Some(&at("2026-09-22T00:00:00Z")),
                Some(&at("2026-09-28T00:00:00Z"))
            ),
            "2026-09-22 to 2026-09-27"
        );
        assert_eq!(
            range_label(Some(&at("2026-03-01")), Some(&at("2026-03-02"))),
            "2026-03-01"
        );
        assert_eq!(
            range_label(None, Some(&at("2024-03-01T00:00:00Z"))),
            "until 2024-02-29"
        );
        assert_eq!(
            range_label(Some(&at("2026-09-22T10:30:00Z")), None),
            "from 2026-09-22T10:30:00Z"
        );
        // hours that end at a midnight read as times, both ends alike
        assert_eq!(
            range_label(
                Some(&at("2026-10-31T05:00:00Z")),
                Some(&at("2026-11-03T00:00:00Z"))
            ),
            "2026-10-31T05:00:00Z to 2026-11-03T00:00:00Z"
        );
        assert_eq!(range_label(Some(&json!(5)), Some(&json!(10))), "5 to 10");
    }

    #[test]
    fn a_header_click_sorts_numbers_down_text_up_and_then_flips() {
        assert!(next_sort(None, "revenue", true));
        assert!(!next_sort(None, "name", false));
        let by_name = ("name".to_string(), false);
        assert!(next_sort(Some(&by_name), "name", false));
        assert!(next_sort(Some(&by_name), "revenue", true));
        let by_revenue = ("revenue".to_string(), true);
        assert!(!next_sort(Some(&by_revenue), "revenue", true));
    }

    #[test]
    fn an_export_is_the_visual_under_the_selection() {
        use tidepool_protocol::live::{GridAt, SelectedField, VisualInfo};
        let info = VisualInfo {
            id: "by_region".into(),
            visual_type: "bar".into(),
            title: None,
            description: None,
            dimensions: vec![tidepool_protocol::QueryDimension {
                field: "customers.region".into(),
                grain: None,
                level: None,
            }],
            across: Vec::new(),
            totals: false,
            metrics: vec!["revenue".into()],
            compare: None,
            on_click: true,
            on_brush: false,
            widget: None,
            formats: std::collections::BTreeMap::new(),
            at: GridAt {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
        };
        let selection = vec![SelectedField {
            field: "orders.ordered_at".into(),
            values: None,
            from: Some(json!("2026-01-01T00:00:00Z")),
            to: None,
            relative: None,
            contains: None,
        }];
        let request = export_request(&info, &selection);
        assert_eq!(request.metrics, vec!["revenue".to_string()]);
        assert_eq!(request.dimensions[0].field, "customers.region");
        assert_eq!(request.filters[0].field, "orders.ordered_at");
        assert_eq!(request.filters[0].from, Some(json!("2026-01-01T00:00:00Z")));
    }

    #[test]
    fn a_window_reads_by_name_and_exports_where_it_is() {
        let window = RelativeWindow {
            unit: RelativeUnit::Day,
            last: 7,
            ago: 0,
        };
        let selected = SelectedField {
            field: "orders.ordered_at".into(),
            values: None,
            from: Some(json!("2026-10-02T00:00:00Z")),
            to: Some(json!("2026-10-09T00:00:00Z")),
            relative: Some(window),
            contains: None,
        };
        assert_eq!(selected_label(&selected), "Last 7 days");
        let plain = SelectedField {
            relative: None,
            ..selected.clone()
        };
        assert_eq!(selected_label(&plain), "2026-10-02 to 2026-10-08");
        assert!(presets(false).contains(&window));
        assert!(
            presets(false).iter().all(|w| w.unit.fits_dates()),
            "a date has no hours"
        );
        assert!(presets(true).iter().any(|w| w.unit == RelativeUnit::Hour));
    }

    fn node(state: ValueState, inside: bool, holds: bool) -> TreeNode {
        TreeNode {
            key: json!("water"),
            label: json!("water"),
            kind: None,
            depth: 1,
            has_children: true,
            open: false,
            state,
            inside,
            holds,
            rows: 3,
            measure: None,
        }
    }

    #[test]
    fn tree_rows_say_inside_and_holds_beside_their_state() {
        assert_eq!(
            node_class(&node(ValueState::Possible, false, false)),
            "value possible"
        );
        assert_eq!(
            node_class(&node(ValueState::Possible, true, false)),
            "value possible inside"
        );
        // selected says it all; holding a selection below still shows
        assert_eq!(
            node_class(&node(ValueState::Selected, true, true)),
            "value selected holds"
        );
        assert_eq!(
            value_class(ValueState::Excluded, false, true),
            "value excluded holds"
        );
    }

    #[test]
    fn a_measured_row_shows_its_measure_even_when_there_is_none() {
        assert_eq!(beside(false, None, 1234, None), "1,234");
        assert_eq!(beside(false, None, 0, None), "");
        assert_eq!(beside(true, Some(&json!(0.5)), 9, Some("percent")), "50.0%");
        // null arrives as None: no value, not the rows
        assert_eq!(beside(true, None, 9, Some("percent")), "–");
    }

    #[test]
    fn measures_read_by_their_metrics_hint() {
        assert_eq!(formatted(&json!(0.8234), Some("percent")), "82.3%");
        assert_eq!(
            formatted(&json!("1234.5"), Some("currency:EUR")),
            "€1,234.50"
        );
        assert_eq!(formatted(&json!(7.333_333), None), "7.33");
        assert_eq!(formatted(&json!("10.00"), None), "10.00");
        assert_eq!(formatted(&json!(null), Some("percent")), "–");
    }

    #[test]
    fn compact_makes_a_large_total_short() {
        let f = |value: Value, format: &str| formatted(&value, Some(format));
        // the KPI that moves all the time
        assert_eq!(f(json!("167791795.92"), "compact"), "167.8M");
        assert_eq!(f(json!("167791795.92"), "currency:EUR compact"), "€167.8M");
        assert_eq!(f(json!(167_791_795.92), "compact decimals:2"), "167.79M");
        assert_eq!(f(json!(1234), "compact"), "1.2k");
        assert_eq!(f(json!(-2_500_000), "compact decimals:0"), "-3M");
        assert_eq!(f(json!(4.2e12), "compact"), "4.2T");
        assert_eq!(f(json!(5.0e15), "compact"), "5,000.0T");
        // rounding up into the next unit reads in it
        assert_eq!(f(json!(999_960), "compact"), "1.0M");
        assert_eq!(f(json!(999_940), "compact"), "999.9k");
        // under a thousand there is nothing to shorten: the style decides
        assert_eq!(f(json!("950.37"), "compact"), "950.37");
        assert_eq!(f(json!(950.37), "int compact"), "950");
        assert_eq!(f(json!("10.00"), "compact"), "10.00");
        assert_eq!(f(json!(0.25), "percent compact"), "25.0%");
    }

    #[test]
    fn styles_and_decimals() {
        let f = |value: Value, format: &str| formatted(&value, Some(format));
        assert_eq!(f(json!("1234.567"), "int"), "1,235");
        assert_eq!(f(json!(3), "decimal"), "3.00");
        assert_eq!(f(json!("1234.5"), "decimals:0"), "1,235");
        assert_eq!(f(json!(0.123_45), "percent decimals:2"), "12.35%");
        assert_eq!(f(json!(-0.001), "decimal decimals:1"), "0.0");
        assert_eq!(f(json!("12"), "currency:SEK decimals:0"), "SEK 12");
        assert_eq!(f(json!(-5), "currency:USD"), "-$5.00");
        assert_eq!(f(json!("-1234"), "currency:SEK"), "-SEK 1,234.00");
        // not a number, or a format that doesn't read: as it comes
        assert_eq!(f(json!("north"), "compact"), "north");
        assert_eq!(f(json!("1234567"), "compakt"), "1,234,567");
    }

    #[test]
    fn matches_are_marked_as_the_server_finds_them() {
        let parts = |text: &str, needle: &str| -> Vec<(String, bool)> { marks(text, needle) };
        let owned = |p: &[(&str, bool)]| -> Vec<(String, bool)> {
            p.iter().map(|(t, m)| ((*t).to_string(), *m)).collect()
        };
        assert_eq!(
            parts("canceling due to Lock Timeout", "lock timeout"),
            owned(&[("canceling due to ", false), ("Lock Timeout", true)])
        );
        assert_eq!(
            parts("ERROR: error", "Error"),
            owned(&[("ERROR", true), (": ", false), ("error", true)])
        );
        assert_eq!(parts("café", "é"), owned(&[("caf", false), ("é", true)]));
        // no needle, none found, or longer than the text: as it is
        assert_eq!(parts("x", ""), owned(&[("x", false)]));
        assert_eq!(parts("abc", "zz"), owned(&[("abc", false)]));
        assert_eq!(parts("ab", "abc"), owned(&[("ab", false)]));
        // matches don't overlap
        assert_eq!(parts("aaa", "aa"), owned(&[("aa", true), ("a", false)]));
    }

    #[test]
    fn a_search_reads_as_its_text_and_exports_as_contains() {
        let selected = SelectedField {
            field: "logs.message".into(),
            values: None,
            from: None,
            to: None,
            relative: None,
            contains: Some("timeout".into()),
        };
        assert_eq!(selected_label(&selected), "“timeout”");
        let info = VisualInfo {
            id: "tail".into(),
            visual_type: "rows".into(),
            title: None,
            description: None,
            dimensions: vec![tidepool_protocol::QueryDimension {
                field: "logs.message".into(),
                grain: None,
                level: None,
            }],
            across: Vec::new(),
            totals: false,
            metrics: Vec::new(),
            compare: None,
            on_click: false,
            on_brush: false,
            widget: None,
            formats: std::collections::BTreeMap::new(),
            at: tidepool_protocol::live::GridAt {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            },
        };
        let request = export_rows_request(&info, &[selected], 50);
        assert_eq!(request.fields, vec!["logs.message".to_string()]);
        assert_eq!(request.filters[0].contains.as_deref(), Some("timeout"));
        assert_eq!(request.limit, Some(50));
    }

    #[test]
    fn a_tree_chip_shows_the_paths_of_its_nodes() {
        let selected = SelectedField {
            field: "categories.tree".into(),
            values: Some(vec![json!("water"), json!("maps")]),
            from: None,
            to: None,
            relative: None,
            contains: None,
        };
        let mut frame = FilterFrame {
            field: "categories.tree".into(),
            total: 0,
            offset: 0,
            values: Vec::new(),
            nodes: Vec::new(),
            chosen: Vec::new(),
        };
        assert_eq!(chip_label(&selected, Some(&frame)), "water, maps");
        frame.chosen = vec![
            tidepool_protocol::live::ChosenNode {
                key: json!("water"),
                path: vec![json!("outdoor"), json!("water")],
            },
            tidepool_protocol::live::ChosenNode {
                key: json!("maps"),
                path: vec![json!("navigation"), json!("maps")],
            },
        ];
        assert_eq!(
            chip_label(&selected, Some(&frame)),
            "outdoor › water, navigation › maps"
        );
    }

    #[test]
    fn a_selection_is_under_way_until_its_done_arrives() {
        let mut flight = InFlight::default();
        flight.sent("hello", 0.0);
        flight.sent("open_dashboard", 1.0);
        assert!(!flight.busy(), "nothing selected yet");
        flight.sent("select", 10.0);
        flight.sent("filter_window", 11.0);
        assert!(flight.busy());
        assert_eq!(flight.done(12.0), None, "hello isn't a selection");
        assert_eq!(flight.done(13.0), None, "nor is open_dashboard");
        assert!(flight.busy(), "the select isn't answered yet");
        assert_eq!(flight.done(14.5), Some(4.5), "the select took 4.5 ms");
        assert!(!flight.busy(), "only a list page is left");
        assert_eq!(flight.done(15.0), None);
        flight.sent("back", 20.0);
        flight.sent("forward", 21.0);
        flight.done(22.0);
        assert!(flight.busy(), "forward isn't answered yet");
        assert_eq!(flight.done(30.0), Some(9.0));
        assert!(!flight.busy());
        assert_eq!(flight.done(31.0), None, "one too many does no harm");
        assert_eq!(
            flight.answered(),
            Some(9.0),
            "the last selection's time is kept"
        );
        flight.reset();
        assert!(!flight.busy());
    }

    #[test]
    fn durations_and_counts_read_compactly() {
        assert_eq!(millis(3.06), "3.1 ms");
        assert_eq!(millis(48.4), "48 ms");
        assert_eq!(millis(1204.0), "1.20 s");
        assert_eq!(compact(950.0), "950");
        assert_eq!(compact(12_400.0), "12.4k");
        assert_eq!(compact(2_418_330.0), "2.42M");
        assert_eq!(compact(3.5e9), "3.50G");
        assert_eq!(compact(-1.0), "0");
    }

    #[test]
    fn a_filter_says_how_many_and_what_it_measures() {
        assert_eq!(
            filter_meta(Some(5), 1, Some("revenue"), false),
            "1 of 5 · revenue"
        );
        assert_eq!(filter_meta(Some(12), 0, None, false), "12 values");
        assert_eq!(filter_meta(Some(1), 0, None, false), "1 value");
        assert_eq!(
            filter_meta(None, 2, None, false),
            "2 selected",
            "before its first frame"
        );
        assert_eq!(filter_meta(Some(14), 0, None, true), "tree");
        assert_eq!(
            filter_meta(Some(14), 2, Some("revenue"), true),
            "2 selected · revenue"
        );
    }

    #[test]
    fn a_visual_is_tagged_with_its_setup() {
        let info: VisualInfo = match serde_json::from_value(json!({
            "id": "trend", "type": "line",
            "dimensions": [{"field": "orders.ordered_at", "grain": "day"}],
            "metrics": ["revenue"], "on_click": false, "on_brush": true,
            "at": {"x": 0, "y": 0, "w": 6, "h": 4},
        })) {
            Ok(info) => info,
            Err(e) => panic!("a visual: {e}"),
        };
        assert_eq!(visual_tags(&info), ["grain: day", "on_brush: select_range"]);
    }

    #[test]
    fn meters_are_shares_of_the_largest() {
        assert!((share(Some(&json!(50)), 200.0) - 25.0).abs() < 1e-9);
        assert!(
            (share(Some(&json!("412.30")), 412.3) - 100.0).abs() < 1e-9,
            "decimals are strings"
        );
        assert!(share(Some(&json!(-3)), 10.0).abs() < 1e-9);
        assert!(share(None, 10.0).abs() < 1e-9);
        assert!(share(Some(&json!(3)), 0.0).abs() < 1e-9);
    }

    #[test]
    fn the_status_bar_sums_the_tables() {
        let table = |rows, segments| tidepool_protocol::TableStatus {
            name: "t".into(),
            rows,
            bytes: 0,
            segments,
            rows_per_second: 0.0,
        };
        assert_eq!(
            data_summary(&[table(2_400_000, 140), table(18_330, 8)]),
            "2.42M rows · 148 segments · 2 tables"
        );
        assert_eq!(data_summary(&[table(5, 1)]), "5 rows · 1 segment · 1 table");
    }
}
