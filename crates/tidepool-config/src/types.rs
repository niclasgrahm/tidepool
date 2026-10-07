//! Small value types that are written as strings in config files:
//! `decimal(14,2)`, `200ms`, `75%`.
//!
//! Each parses through `FromStr` and deserializes from a string, so a typo is
//! a serde error with a location, and each reports itself to JSON Schema as a
//! string with a pattern, so editors can check it as you type.

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Implements serde and JSON Schema for a type that round-trips via a string.
macro_rules! string_type {
    ($ty:ty, $name:literal, $pattern:literal, $description:literal) => {
        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $ty {
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed($name)
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({
                    "type": "string",
                    "pattern": $pattern,
                    "description": $description,
                })
            }
        }
    };
}

/// A column's physical type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColumnType {
    Bool,
    Int32,
    Int64,
    Float64,
    /// Exact decimal. `precision` ≤ 38, `scale` ≤ `precision`.
    Decimal {
        precision: u8,
        scale: u8,
    },
    String,
    Date,
    /// Without a time zone.
    Timestamp,
    /// An instant, stored as UTC microseconds.
    TimestampTz,
}

impl ColumnType {
    /// The spellings `FromStr` accepts, for error messages.
    pub const NAMES: &'static str =
        "bool, int32, int64, float64, decimal(p,s), string, date, timestamp, timestamptz";

    #[must_use]
    pub fn is_temporal(self) -> bool {
        matches!(self, Self::Date | Self::Timestamp | Self::TimestampTz)
    }

    #[must_use]
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::Int32 | Self::Int64 | Self::Float64 | Self::Decimal { .. }
        )
    }

    /// Whether a value of `self` can always be represented as `wider` without
    /// loss: the type changes that are a rebuild rather than destructive.
    #[must_use]
    pub fn widens_to(self, wider: Self) -> bool {
        match (self, wider) {
            (a, b) if a == b => true,
            (Self::Int32, Self::Int64) | (Self::Timestamp, Self::TimestampTz) => true,
            (Self::Int32 | Self::Int64, Self::Decimal { precision, scale }) => {
                let digits = if self == Self::Int32 { 10 } else { 19 };
                precision.saturating_sub(scale) >= digits
            }
            (
                Self::Decimal {
                    precision: p1,
                    scale: s1,
                },
                Self::Decimal {
                    precision: p2,
                    scale: s2,
                },
            ) => s2 >= s1 && p2.saturating_sub(s2) >= p1.saturating_sub(s1),
            _ => false,
        }
    }
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bool => write!(f, "bool"),
            Self::Int32 => write!(f, "int32"),
            Self::Int64 => write!(f, "int64"),
            Self::Float64 => write!(f, "float64"),
            Self::Decimal { precision, scale } => write!(f, "decimal({precision},{scale})"),
            Self::String => write!(f, "string"),
            Self::Date => write!(f, "date"),
            Self::Timestamp => write!(f, "timestamp"),
            Self::TimestampTz => write!(f, "timestamptz"),
        }
    }
}

impl FromStr for ColumnType {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let normalized: String = text
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_lowercase();
        let simple = match normalized.as_str() {
            "bool" => Some(Self::Bool),
            "int32" => Some(Self::Int32),
            "int64" => Some(Self::Int64),
            "float64" => Some(Self::Float64),
            "string" => Some(Self::String),
            "date" => Some(Self::Date),
            "timestamp" => Some(Self::Timestamp),
            "timestamptz" => Some(Self::TimestampTz),
            _ => None,
        };
        if let Some(simple) = simple {
            return Ok(simple);
        }
        if normalized.starts_with("decimal(") && !normalized.contains(')') {
            return Err(format!(
                "`{text}` is cut off at the comma: inside a {{ … }} mapping, quote the type, e.g. type: \"decimal(14,2)\""
            ));
        }
        let Some(args) = normalized
            .strip_prefix("decimal(")
            .and_then(|r| r.strip_suffix(')'))
        else {
            return Err(format!(
                "unknown column type `{text}`; expected one of {}",
                Self::NAMES
            ));
        };
        let Some((precision, scale)) = args.split_once(',') else {
            return Err(format!(
                "`{text}`: decimal needs a precision and a scale, e.g. decimal(14,2)"
            ));
        };
        let (Ok(precision), Ok(scale)) = (precision.parse::<u8>(), scale.parse::<u8>()) else {
            return Err(format!(
                "`{text}`: precision and scale must be whole numbers"
            ));
        };
        if precision == 0 || precision > 38 {
            return Err(format!("`{text}`: precision must be between 1 and 38"));
        }
        if scale > precision {
            return Err(format!("`{text}`: scale can't be larger than precision"));
        }
        Ok(Self::Decimal { precision, scale })
    }
}

string_type!(
    ColumnType,
    "ColumnType",
    "^(bool|int32|int64|float64|string|date|timestamp|timestamptz|decimal\\(\\s*\\d+\\s*,\\s*\\d+\\s*\\))$",
    "A column type: bool, int32, int64, float64, decimal(p,s), string, date, timestamp or timestamptz."
);

/// A duration written with a unit: `200ms`, `60s`, `5m`, `24h`, `730d`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HumanDuration(pub Duration);

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let millis = self.0.as_millis();
        let units: [(u128, &str); 5] = [
            (86_400_000, "d"),
            (3_600_000, "h"),
            (60_000, "m"),
            (1000, "s"),
            (1, "ms"),
        ];
        for (size, unit) in units {
            if millis >= size && millis.is_multiple_of(size) {
                return write!(f, "{}{unit}", millis / size);
            }
        }
        write!(f, "0ms")
    }
}

impl FromStr for HumanDuration {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let trimmed = text.trim();
        let split = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len());
        let (number, unit) = trimmed.split_at(split);
        let Ok(number) = number.parse::<u64>() else {
            return Err(format!(
                "`{text}` is not a duration; write a number and a unit, e.g. 200ms or 30d"
            ));
        };
        let millis = match unit {
            "ms" => 1,
            "s" => 1000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            _ => {
                return Err(format!(
                    "`{text}`: unknown unit `{unit}`; use ms, s, m, h or d"
                ));
            }
        };
        Ok(Self(Duration::from_millis(number.saturating_mul(millis))))
    }
}

string_type!(
    HumanDuration,
    "Duration",
    "^\\s*\\d+(ms|s|m|h|d)\\s*$",
    "A duration with a unit: ms, s, m, h or d (e.g. 200ms, 730d)."
);

/// An amount of memory: a share of the machine (`75%`) or a size (`8GiB`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemorySize {
    Percent(u8),
    Bytes(u64),
}

impl fmt::Display for MemorySize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Percent(p) => write!(f, "{p}%"),
            Self::Bytes(b) => {
                let units = [
                    (1u64 << 40, "TiB"),
                    (1 << 30, "GiB"),
                    (1 << 20, "MiB"),
                    (1 << 10, "KiB"),
                ];
                for (size, unit) in units {
                    if *b >= size && b % size == 0 {
                        return write!(f, "{}{unit}", b / size);
                    }
                }
                write!(f, "{b}B")
            }
        }
    }
}

impl FromStr for MemorySize {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let trimmed = text.trim();
        if let Some(percent) = trimmed.strip_suffix('%') {
            return match percent.trim().parse::<u8>() {
                Ok(p) if (1..=100).contains(&p) => Ok(Self::Percent(p)),
                _ => Err(format!(
                    "`{text}`: a percentage must be between 1% and 100%"
                )),
            };
        }
        let split = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len());
        let (number, unit) = trimmed.split_at(split);
        let Ok(number) = number.parse::<u64>() else {
            return Err(format!(
                "`{text}` is not a memory size; write e.g. 75% or 8GiB"
            ));
        };
        let multiplier: u64 = match unit.trim() {
            "B" | "" => 1,
            "KiB" => 1 << 10,
            "MiB" => 1 << 20,
            "GiB" => 1 << 30,
            "TiB" => 1 << 40,
            other => {
                return Err(format!(
                    "`{text}`: unknown unit `{other}`; use B, KiB, MiB, GiB, TiB or %"
                ));
            }
        };
        Ok(Self::Bytes(number.saturating_mul(multiplier)))
    }
}

string_type!(
    MemorySize,
    "MemorySize",
    "^\\s*\\d+\\s*(%|B|KiB|MiB|GiB|TiB)?\\s*$",
    "A share of system memory (75%) or a size (8GiB)."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_types_round_trip() {
        for text in [
            "bool",
            "int32",
            "int64",
            "float64",
            "decimal(14,2)",
            "string",
            "date",
            "timestamp",
            "timestamptz",
        ] {
            let parsed: ColumnType = match text.parse() {
                Ok(t) => t,
                Err(err) => panic!("{text}: {err}"),
            };
            assert_eq!(parsed.to_string(), text);
        }
        assert_eq!(
            "Decimal( 10 , 0 )".parse(),
            Ok(ColumnType::Decimal {
                precision: 10,
                scale: 0
            })
        );
    }

    #[test]
    fn bad_column_types_say_what_is_wrong() {
        let err = |text: &str| text.parse::<ColumnType>().err().unwrap_or_default();
        assert!(err("varchar").contains("expected one of"));
        assert!(err("decimal(14)").contains("precision and a scale"));
        assert!(err("decimal(40,2)").contains("between 1 and 38"));
        assert!(err("decimal(14").contains("quote the type"));
        assert!(err("decimal(4,6)").contains("larger than precision"));
    }

    #[test]
    fn widening_is_lossless_only() {
        use ColumnType::{Decimal, Int32, Int64, String as Str};
        assert!(Int32.widens_to(Int64));
        assert!(!Int64.widens_to(Int32));
        assert!(Int32.widens_to(Decimal {
            precision: 12,
            scale: 2
        }));
        assert!(!Int64.widens_to(Decimal {
            precision: 12,
            scale: 2
        }));
        assert!(
            Decimal {
                precision: 10,
                scale: 2
            }
            .widens_to(Decimal {
                precision: 12,
                scale: 3
            })
        );
        assert!(
            !Decimal {
                precision: 10,
                scale: 2
            }
            .widens_to(Decimal {
                precision: 10,
                scale: 3
            })
        );
        assert!(!Int32.widens_to(Str));
    }

    #[test]
    fn durations_parse_and_print() {
        assert_eq!(
            "200ms".parse(),
            Ok(HumanDuration(Duration::from_millis(200)))
        );
        assert_eq!(
            "730d".parse::<HumanDuration>().map(|d| d.to_string()),
            Ok("730d".to_string())
        );
        assert!("5 parsecs".parse::<HumanDuration>().is_err());
        assert!("ms".parse::<HumanDuration>().is_err());
    }

    #[test]
    fn memory_sizes_parse_and_print() {
        assert_eq!("75%".parse(), Ok(MemorySize::Percent(75)));
        assert_eq!("8GiB".parse(), Ok(MemorySize::Bytes(8 << 30)));
        assert_eq!(MemorySize::Bytes(8 << 30).to_string(), "8GiB");
        assert!("150%".parse::<MemorySize>().is_err());
        assert!("8GB".parse::<MemorySize>().is_err());
    }
}
