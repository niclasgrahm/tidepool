//! What a dashboard shows, as plain data: frames applied to it, values
//! formatted, bars scaled. No DOM here, so `cargo test` covers all of it.

use std::collections::HashMap;

use serde_json::Value;
use tidepool_protocol::live::{FilterFrame, ServerMessage, VisualFrame};

/// The open dashboard's latest content.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Board {
    pub epoch: u64,
    pub visuals: HashMap<String, VisualFrame>,
    pub filters: HashMap<String, FilterFrame>,
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
        for visual in visuals {
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

/// Each row's share of the largest value in `column`, for bar lengths.
#[must_use]
pub fn shares(rows: &[Vec<Value>], column: usize) -> Vec<f64> {
    let values: Vec<f64> = rows
        .iter()
        .map(|r| r.get(column).and_then(number).unwrap_or(0.0).max(0.0))
        .collect();
    let top = values.iter().copied().fold(0.0, f64::max);
    values
        .iter()
        .map(|v| if top > 0.0 { v / top } else { 0.0 })
        .collect()
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

/// A line chart's points in a `width` × `height` box: x by position, y by
/// `column`'s value, top being the largest.
#[must_use]
pub fn line_points(rows: &[Vec<Value>], column: usize, width: f64, height: f64) -> String {
    let values: Vec<f64> = rows
        .iter()
        .map(|r| r.get(column).and_then(number).unwrap_or(0.0))
        .collect();
    let (low, high) = values
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), v| {
            (l.min(*v), h.max(*v))
        });
    let span = if high > low { high - low } else { 1.0 };
    #[allow(clippy::cast_precision_loss)]
    let step = if values.len() > 1 {
        width / (values.len() - 1) as f64
    } else {
        0.0
    };
    values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            #[allow(clippy::cast_precision_loss)]
            let x = i as f64 * step;
            let y = height - (v - low) / span * height;
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    fn bars_are_shares_of_the_largest() {
        let rows = vec![vec![json!("a"), json!("10.00")], vec![json!("b"), json!(5)]];
        assert_eq!(shares(&rows, 1), vec![1.0, 0.5]);
        assert_eq!(shares(&[], 1), Vec::<f64>::new());
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
    fn a_line_spans_its_box() {
        let rows = vec![vec![json!("d1"), json!(0)], vec![json!("d2"), json!(10)]];
        assert_eq!(line_points(&rows, 1, 100.0, 50.0), "0.0,50.0 100.0,0.0");
    }
}
