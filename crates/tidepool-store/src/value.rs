//! Values: how each column type is represented in memory, parsed from
//! ingest text, and written back out.
//!
//! Physically everything is one of five storage kinds ([`Storage`]): an
//! integer (`i64`, which also carries bools, dates as days, timestamps as
//! microseconds and decimals of up to 18 digits as scaled integers), a wide
//! integer (`i128`, for decimals of more than 18 digits), a float, or a
//! string. Dictionary-encoded columns store `u32` codes instead, and the
//! dictionary holds the values in the same kinds.

use std::fmt::Write as _;

use chrono::{DateTime, NaiveDate, NaiveDateTime};
use tidepool_config::types::ColumnType;

/// How values of a column type are held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Storage {
    Bool,
    I64,
    I128,
    F64,
    Str,
}

impl Storage {
    #[must_use]
    pub fn of(column_type: ColumnType) -> Self {
        match column_type {
            ColumnType::Bool => Self::Bool,
            ColumnType::Int32
            | ColumnType::Int64
            | ColumnType::Date
            | ColumnType::Timestamp
            | ColumnType::TimestampTz => Self::I64,
            ColumnType::Decimal { precision, .. } if precision <= 18 => Self::I64,
            ColumnType::Decimal { .. } => Self::I128,
            ColumnType::Float64 => Self::F64,
            ColumnType::String => Self::Str,
        }
    }
}

/// One decoded value, for output and tests.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// `value / 10^scale`.
    Decimal {
        value: i128,
        scale: u8,
    },
    String(String),
    /// Days since 1970-01-01.
    Date(i32),
    /// Microseconds since 1970-01-01T00:00:00, no time zone.
    Timestamp(i64),
    /// Microseconds since the Unix epoch, UTC.
    TimestampTz(i64),
}

impl Value {
    /// The value as JSON: numbers for integers and floats, strings for
    /// decimals (so no digits are lost), dates and timestamps (ISO 8601).
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Self::Null => J::Null,
            Self::Bool(b) => J::Bool(*b),
            Self::Int(i) => J::from(*i),
            Self::Float(f) => serde_json::Number::from_f64(*f).map_or(J::Null, J::Number),
            Self::Decimal { value, scale } => J::String(format_decimal(*value, *scale)),
            Self::String(s) => J::String(s.clone()),
            Self::Date(days) => J::String(format_date(*days)),
            Self::Timestamp(micros) => J::String(format_timestamp(*micros, false)),
            Self::TimestampTz(micros) => J::String(format_timestamp(*micros, true)),
        }
    }

    /// Build a value from its stored integer form.
    #[must_use]
    pub fn from_i64(column_type: ColumnType, raw: i64) -> Self {
        match column_type {
            ColumnType::Bool => Self::Bool(raw != 0),
            ColumnType::Int32 | ColumnType::Int64 => Self::Int(raw),
            ColumnType::Decimal { scale, .. } => Self::Decimal {
                value: i128::from(raw),
                scale,
            },
            ColumnType::Date => Self::Date(i32::try_from(raw).unwrap_or(i32::MAX)),
            ColumnType::Timestamp => Self::Timestamp(raw),
            ColumnType::TimestampTz => Self::TimestampTz(raw),
            // not stored as i64
            ColumnType::Float64 | ColumnType::String => Self::Null,
        }
    }
}

/// Why a value didn't parse, as one line for a row error.
pub type ParseError = String;

/// Parse a value of `column_type` from the raw JSON text of one field
/// (`"abc"`, `12.50`, `true`). `null` is handled by the caller.
pub fn parse_json(column_type: ColumnType, raw: &str) -> Result<Parsed, ParseError> {
    let raw = raw.trim();
    let string = || -> Result<String, ParseError> {
        if raw.starts_with('"') {
            serde_json::from_str::<String>(raw).map_err(|e| e.to_string())
        } else {
            Err(format!("expected a string, got `{}`", shorten(raw)))
        }
    };
    match column_type {
        ColumnType::String => string().map(Parsed::Str),
        ColumnType::Bool => match raw {
            "true" => Ok(Parsed::Bool(true)),
            "false" => Ok(Parsed::Bool(false)),
            _ => Err(format!("expected true or false, got `{}`", shorten(raw))),
        },
        ColumnType::Int32 | ColumnType::Int64 => {
            let value: i64 = raw
                .parse()
                .map_err(|_| format!("expected an integer, got `{}`", shorten(raw)))?;
            if column_type == ColumnType::Int32 && i32::try_from(value).is_err() {
                return Err(format!("{value} doesn't fit in int32"));
            }
            Ok(Parsed::I64(value))
        }
        ColumnType::Float64 => {
            if raw.starts_with('"') || raw == "true" || raw == "false" {
                return Err(format!("expected a number, got `{}`", shorten(raw)));
            }
            raw.parse::<f64>()
                .map(Parsed::F64)
                .map_err(|_| format!("expected a number, got `{}`", shorten(raw)))
        }
        ColumnType::Decimal { precision, scale } => {
            // a JSON number, or a string holding one (which keeps every digit
            // through any JSON tooling on the way)
            let text = if raw.starts_with('"') {
                string()?
            } else {
                raw.to_string()
            };
            let value = parse_decimal(&text, precision, scale)?;
            Ok(if precision <= 18 {
                Parsed::I64(i64::try_from(value).map_err(|_| format!("{text} is too large"))?)
            } else {
                Parsed::I128(value)
            })
        }
        ColumnType::Date => parse_date(&string()?).map(|d| Parsed::I64(i64::from(d))),
        ColumnType::Timestamp => parse_timestamp(&string()?).map(Parsed::I64),
        ColumnType::TimestampTz => parse_timestamptz(&string()?).map(Parsed::I64),
    }
}

/// A parsed, not yet encoded value, in its storage kind.
#[derive(Clone, Debug, PartialEq)]
pub enum Parsed {
    Bool(bool),
    I64(i64),
    I128(i128),
    F64(f64),
    Str(String),
}

fn shorten(text: &str) -> String {
    if text.chars().count() > 40 {
        format!("{}…", text.chars().take(40).collect::<String>())
    } else {
        text.to_string()
    }
}

/// Parse a decimal exactly into `value * 10^scale`, checking it fits
/// `decimal(precision, scale)`.
pub fn parse_decimal(text: &str, precision: u8, scale: u8) -> Result<i128, ParseError> {
    let bad = || format!("`{}` is not a decimal number", shorten(text));
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    if digits.contains(['e', 'E']) {
        return Err(format!("`{text}`: write decimals without an exponent"));
    }
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty() && fraction.is_empty() {
        return Err(bad());
    }
    if !whole
        .chars()
        .chain(fraction.chars())
        .all(|c| c.is_ascii_digit())
    {
        return Err(bad());
    }
    let fraction = fraction.trim_end_matches('0');
    if fraction.len() > usize::from(scale) {
        return Err(format!("`{text}` has more than {scale} decimals"));
    }
    let whole = whole.trim_start_matches('0');
    if whole.len() > usize::from(precision - scale) {
        return Err(format!(
            "`{text}` has more than {} digits before the point",
            precision - scale
        ));
    }
    let mut value: i128 = 0;
    for c in whole.chars().chain(fraction.chars()) {
        value = value * 10 + i128::from(c as u8 - b'0');
    }
    for _ in fraction.len()..usize::from(scale) {
        value *= 10;
    }
    Ok(if negative { -value } else { value })
}

#[must_use]
pub fn format_decimal(value: i128, scale: u8) -> String {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let scale = usize::from(scale);
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if scale == 0 {
        out.push_str(&digits);
    } else if digits.len() <= scale {
        let _ = write!(out, "0.{}{digits}", "0".repeat(scale - digits.len()));
    } else {
        let (whole, fraction) = digits.split_at(digits.len() - scale);
        let _ = write!(out, "{whole}.{fraction}");
    }
    out
}

const EPOCH: NaiveDate = match NaiveDate::from_ymd_opt(1970, 1, 1) {
    Some(date) => date,
    None => panic!("1970-01-01 is a date"),
};

/// `YYYY-MM-DD` → days since 1970-01-01.
pub fn parse_date(text: &str) -> Result<i32, ParseError> {
    let date = NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| format!("`{}` is not a date; write YYYY-MM-DD", shorten(text)))?;
    i32::try_from((date - EPOCH).num_days()).map_err(|_| format!("`{text}` is out of range"))
}

#[must_use]
pub fn format_date(days: i32) -> String {
    EPOCH
        .checked_add_signed(chrono::Duration::days(i64::from(days)))
        .map_or_else(|| "?".to_string(), |d| d.format("%Y-%m-%d").to_string())
}

/// A timestamp without a time zone: `YYYY-MM-DDTHH:MM:SS[.ffffff]` (a space
/// instead of `T` is fine) → microseconds.
pub fn parse_timestamp(text: &str) -> Result<i64, ParseError> {
    let formats = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"];
    formats
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(text, f).ok())
        .map(|t| t.and_utc().timestamp_micros())
        .ok_or_else(|| {
            format!(
                "`{}` is not a timestamp; write YYYY-MM-DDTHH:MM:SS, without a time zone",
                shorten(text)
            )
        })
}

/// An RFC 3339 timestamp with an offset (`…Z`, `…+02:00`) → UTC microseconds.
pub fn parse_timestamptz(text: &str) -> Result<i64, ParseError> {
    DateTime::parse_from_rfc3339(text)
        .map(|t| t.timestamp_micros())
        .map_err(|_| {
            format!(
                "`{}` is not a timestamp with a time zone; write RFC 3339, e.g. 2026-10-07T12:00:00Z",
                shorten(text)
            )
        })
}

#[must_use]
pub fn format_timestamp(micros: i64, utc: bool) -> String {
    let Some(time) = DateTime::from_timestamp_micros(micros) else {
        return "?".to_string();
    };
    let naive = time.naive_utc();
    let base = if naive.and_utc().timestamp_subsec_micros() == 0 {
        naive.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        naive.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
    };
    if utc { format!("{base}Z") } else { base }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(precision: u8, scale: u8) -> ColumnType {
        ColumnType::Decimal { precision, scale }
    }

    #[test]
    fn decimals_parse_exactly_and_round_trip() {
        assert_eq!(parse_decimal("12.5", 14, 2), Ok(1250));
        assert_eq!(parse_decimal("-0.07", 14, 2), Ok(-7));
        assert_eq!(parse_decimal("100", 14, 2), Ok(10000));
        assert_eq!(parse_decimal("12.500", 14, 2), Ok(1250));
        assert_eq!(format_decimal(1250, 2), "12.50");
        assert_eq!(format_decimal(-7, 2), "-0.07");
        assert_eq!(format_decimal(42, 0), "42");
    }

    #[test]
    fn decimals_out_of_shape_are_errors() {
        assert!(parse_decimal("12.345", 14, 2).is_err_and(|e| e.contains("more than 2 decimals")));
        assert!(parse_decimal("1234", 5, 2).is_err_and(|e| e.contains("before the point")));
        assert!(parse_decimal("1e3", 14, 2).is_err_and(|e| e.contains("exponent")));
        assert!(parse_decimal("abc", 14, 2).is_err());
        assert!(parse_decimal(".", 14, 2).is_err());
    }

    #[test]
    fn json_values_parse_by_column_type() {
        assert_eq!(
            parse_json(ColumnType::String, "\"a\\\"b\""),
            Ok(Parsed::Str("a\"b".into()))
        );
        assert_eq!(parse_json(ColumnType::Int32, "42"), Ok(Parsed::I64(42)));
        assert!(parse_json(ColumnType::Int32, "4294967296").is_err());
        assert!(parse_json(ColumnType::Int64, "1.5").is_err());
        assert_eq!(
            parse_json(ColumnType::Float64, "1.5e2"),
            Ok(Parsed::F64(150.0))
        );
        assert_eq!(parse_json(dec(14, 2), "\"12.50\""), Ok(Parsed::I64(1250)));
        assert_eq!(parse_json(dec(30, 2), "12.5"), Ok(Parsed::I128(1250)));
        assert_eq!(parse_json(ColumnType::Bool, "true"), Ok(Parsed::Bool(true)));
        assert!(
            parse_json(ColumnType::String, "42").is_err_and(|e| e.contains("expected a string"))
        );
    }

    #[test]
    fn dates_and_timestamps_round_trip() {
        let day = parse_date("2026-10-07").unwrap_or_default();
        assert_eq!(format_date(day), "2026-10-07");
        let naive = parse_timestamp("2026-10-07 12:30:00.25").unwrap_or_default();
        assert_eq!(format_timestamp(naive, false), "2026-10-07T12:30:00.250000");
        let utc = parse_timestamptz("2026-10-07T14:30:00+02:00").unwrap_or_default();
        assert_eq!(format_timestamp(utc, true), "2026-10-07T12:30:00Z");
        assert!(parse_timestamptz("2026-10-07T12:30:00").is_err_and(|e| e.contains("RFC 3339")));
        assert!(parse_timestamp("2026-10-07T12:30:00Z").is_err());
    }

    #[test]
    fn json_output_keeps_decimals_exact() {
        assert_eq!(
            Value::Decimal {
                value: 1250,
                scale: 2
            }
            .to_json(),
            serde_json::json!("12.50")
        );
        assert_eq!(Value::Date(0).to_json(), serde_json::json!("1970-01-01"));
        assert_eq!(Value::Int(7).to_json(), serde_json::json!(7));
    }
}
