//! What a dashboard shows, as plain data: frames applied to it, values
//! formatted, bars scaled. No DOM here, so `cargo test` covers all of it.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use tidepool_protocol::live::{FilterFrame, ServerMessage, VisualFrame};

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

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
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
    let start = from.map(|v| day(v).map_or_else(|| display(v), civil));
    let end = to.map(|v| day(v).map_or_else(|| display(v), |d| civil(d - 1)));
    match (start, end) {
        (Some(start), Some(end)) if start == end => start,
        (Some(start), Some(end)) => format!("{start} to {end}"),
        (Some(start), None) => format!("from {start}"),
        (None, Some(end)) => format!("until {end}"),
        (None, None) => "everything".into(),
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
                from: s.from.clone(),
                to: s.to.clone(),
            })
            .collect(),
        sort: Vec::new(),
        limit: None,
    }
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

/// Positions of tables in a relationship diagram: tables nothing points at
/// (facts) on the left, each one side a column right of the many sides
/// that point at it. Returns (table, column, row).
#[must_use]
pub fn layers(tables: &[String], edges: &[(String, String)]) -> Vec<(String, usize, usize)> {
    let mut column: HashMap<&str, usize> = tables.iter().map(|t| (t.as_str(), 0)).collect();
    // longest path from a fact; relationships form a DAG, so this settles
    for _ in 0..tables.len() {
        for (from, to) in edges {
            let next = column.get(from.as_str()).copied().unwrap_or(0) + 1;
            if let Some(c) = column.get_mut(to.as_str())
                && *c < next
            {
                *c = next;
            }
        }
    }
    let mut rows: HashMap<usize, usize> = HashMap::new();
    tables
        .iter()
        .map(|t| {
            let c = column.get(t.as_str()).copied().unwrap_or(0);
            let r = rows.entry(c).or_insert(0);
            *r += 1;
            (t.clone(), c, *r - 1)
        })
        .collect()
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
            }],
            filters: vec![FilterFrame {
                field: "customers.region".into(),
                total: 1,
                offset: 0,
                values: vec![FilterValue {
                    value: json!("north"),
                    state: ValueState::Possible,
                    rows: 1,
                }],
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
        };
        let frame = |epoch: u64, rows: Value| ServerMessage::Frame {
            epoch,
            visuals: vec![VisualFrame {
                id: "v".into(),
                result: Some(result(rows)),
                error: None,
                compare: None,
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
    fn facts_sit_left_of_what_they_point_at() {
        let tables = vec!["orders".to_string(), "customers".into(), "regions".into()];
        let edges = vec![
            ("orders".to_string(), "customers".to_string()),
            ("customers".into(), "regions".into()),
        ];
        assert_eq!(
            layers(&tables, &edges),
            vec![
                ("orders".into(), 0, 0),
                ("customers".into(), 1, 0),
                ("regions".into(), 2, 0)
            ]
        );
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
            dimensions: vec![tidepool_protocol::QueryDimension {
                field: "customers.region".into(),
                grain: None,
            }],
            metrics: vec!["revenue".into()],
            compare: None,
            on_click: true,
            on_brush: false,
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
        }];
        let request = export_request(&info, &selection);
        assert_eq!(request.metrics, vec!["revenue".to_string()]);
        assert_eq!(request.dimensions[0].field, "customers.region");
        assert_eq!(request.filters[0].field, "orders.ordered_at");
        assert_eq!(request.filters[0].from, Some(json!("2026-01-01T00:00:00Z")));
    }
}
