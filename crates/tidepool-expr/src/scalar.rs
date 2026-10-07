//! Expression types and values, and the arithmetic and comparison rules
//! between them.
//!
//! The rules follow standard SQL as DuckDB implements it (the oracle checks
//! Tidepool against DuckDB), with one deliberate difference: dividing by
//! zero is NULL, not infinity. A dashboard tile that says "∞" helps nobody.
//!
//! - `+ - *` on integers give integers; with a decimal, a decimal (scale: the
//!   larger for `+ -`, the sum for `*`); with a float, a float.
//! - `/` always gives a float.
//! - `%` takes the sign of the dividend.
//! - Comparisons work between any two numbers, two strings, two values of
//!   the same temporal type, or two bools; NULL compared to anything is NULL.

use std::cmp::Ordering;
use std::fmt;

/// The type of an expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Type {
    /// The type of a bare `NULL` literal, which fits anywhere.
    Null,
    Bool,
    Int,
    Float,
    /// Exact decimal with this many digits after the point.
    Decimal(u8),
    String,
    Date,
    Timestamp,
    TimestampTz,
}

impl Type {
    #[must_use]
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::Int | Self::Float | Self::Decimal(_) | Self::Null
        )
    }

    #[must_use]
    pub fn is_temporal(self) -> bool {
        matches!(self, Self::Date | Self::Timestamp | Self::TimestampTz)
    }

    /// The type both `self` and `other` fit, for CASE branches, COALESCE and
    /// IN lists.
    #[must_use]
    pub fn unify(self, other: Self) -> Option<Self> {
        match (self, other) {
            (a, b) if a == b => Some(a),
            (Self::Null, t) | (t, Self::Null) => Some(t),
            (Self::Float, t) | (t, Self::Float) if t.is_numeric() => Some(Self::Float),
            (Self::Decimal(a), Self::Decimal(b)) => Some(Self::Decimal(a.max(b))),
            (Self::Decimal(s), Self::Int) | (Self::Int, Self::Decimal(s)) => Some(Self::Decimal(s)),
            _ => None,
        }
    }
}

impl Type {
    /// The type with its article, for messages: "an integer", "a string".
    #[must_use]
    pub fn a(self) -> String {
        let name = self.to_string();
        let article = if name.starts_with(['a', 'e', 'i', 'o', 'u']) {
            "an"
        } else {
            "a"
        };
        format!("{article} {name}")
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "null"),
            Self::Bool => write!(f, "bool"),
            Self::Int => write!(f, "integer"),
            Self::Float => write!(f, "float"),
            Self::Decimal(scale) => write!(f, "decimal (scale {scale})"),
            Self::String => write!(f, "string"),
            Self::Date => write!(f, "date"),
            Self::Timestamp => write!(f, "timestamp"),
            Self::TimestampTz => write!(f, "timestamptz"),
        }
    }
}

/// One value.
#[derive(Clone, Debug, PartialEq)]
pub enum Scalar {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// `value / 10^scale`.
    Decimal(i128, u8),
    String(String),
    /// Days since 1970-01-01.
    Date(i32),
    /// Microseconds since 1970-01-01T00:00:00, no zone.
    Timestamp(i64),
    /// Microseconds since the Unix epoch, UTC.
    TimestampTz(i64),
}

impl Scalar {
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// The value as a float, for numeric values.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int(v) => Some(*v as f64),
            Self::Float(v) => Some(*v),
            Self::Decimal(v, scale) => Some(*v as f64 / 10f64.powi(i32::from(*scale))),
            _ => None,
        }
    }

    /// The value as a decimal of `scale`, for integers and decimals.
    #[must_use]
    pub fn as_decimal(&self, scale: u8) -> Option<i128> {
        match self {
            Self::Int(v) => i128::from(*v).checked_mul(pow10(scale)),
            Self::Decimal(v, s) if *s <= scale => v.checked_mul(pow10(scale - s)),
            Self::Decimal(v, s) => Some(v / pow10(s - scale)),
            _ => None,
        }
    }

    /// SQL comparison: `None` when either side is NULL or the two can't be
    /// compared.
    #[must_use]
    #[allow(clippy::match_same_arms)]
    pub fn compare(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Null, _) | (_, Self::Null) => None,
            (Self::Bool(a), Self::Bool(b)) => Some(a.cmp(b)),
            (Self::Int(a), Self::Int(b)) => Some(a.cmp(b)),
            (Self::String(a), Self::String(b)) => Some(a.cmp(b)),
            (Self::Date(a), Self::Date(b)) => Some(a.cmp(b)),
            (Self::Timestamp(a), Self::Timestamp(b))
            | (Self::TimestampTz(a), Self::TimestampTz(b)) => Some(a.cmp(b)),
            (Self::Decimal(..) | Self::Int(_), Self::Decimal(..) | Self::Int(_)) => {
                let scale = scale_of(self).max(scale_of(other));
                Some(self.as_decimal(scale)?.cmp(&other.as_decimal(scale)?))
            }
            (a, b) => a.as_f64()?.partial_cmp(&b.as_f64()?),
        }
    }

    /// A total order for sorting results: NULLs last, then by value.
    #[must_use]
    pub fn sort_cmp(&self, other: &Self) -> Ordering {
        match (self.is_null(), other.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                self.compare(other)
                    .unwrap_or_else(|| match (self.as_f64(), other.as_f64()) {
                        (Some(a), Some(b)) => a.total_cmp(&b),
                        _ => Ordering::Equal,
                    })
            }
        }
    }
}

fn scale_of(value: &Scalar) -> u8 {
    match value {
        Scalar::Decimal(_, scale) => *scale,
        _ => 0,
    }
}

#[must_use]
pub fn pow10(exponent: u8) -> i128 {
    10i128.checked_pow(u32::from(exponent)).unwrap_or(i128::MAX)
}

/// Arithmetic operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arith {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl fmt::Display for Arith {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Mod => "%",
        })
    }
}

/// The result type of `left op right`, or `None` if it isn't defined.
#[must_use]
pub fn arith_type(op: Arith, left: Type, right: Type) -> Option<Type> {
    if !left.is_numeric() || !right.is_numeric() {
        return None;
    }
    if op == Arith::Div {
        return Some(Type::Float);
    }
    Some(match (left, right) {
        (Type::Float, _) | (_, Type::Float) => Type::Float,
        (Type::Null, t) | (t, Type::Null) => t,
        (Type::Int, Type::Int) => Type::Int,
        (a, b) => {
            let (sa, sb) = (decimal_scale(a), decimal_scale(b));
            Type::Decimal(if op == Arith::Mul {
                sa.saturating_add(sb)
            } else {
                sa.max(sb)
            })
        }
    })
}

fn decimal_scale(t: Type) -> u8 {
    match t {
        Type::Decimal(scale) => scale,
        _ => 0,
    }
}

/// Apply an arithmetic operator; the result has [`arith_type`]'s type.
#[must_use]
#[allow(clippy::match_same_arms)]
pub fn arith(op: Arith, result: Type, left: &Scalar, right: &Scalar) -> Scalar {
    if left.is_null() || right.is_null() {
        return Scalar::Null;
    }
    match result {
        Type::Float => {
            let (Some(a), Some(b)) = (left.as_f64(), right.as_f64()) else {
                return Scalar::Null;
            };
            let value = match op {
                Arith::Add => a + b,
                Arith::Sub => a - b,
                Arith::Mul => a * b,
                Arith::Div if b == 0.0 => return Scalar::Null,
                Arith::Div => a / b,
                Arith::Mod if b == 0.0 => return Scalar::Null,
                Arith::Mod => a % b,
            };
            Scalar::Float(value)
        }
        Type::Int => {
            let (Scalar::Int(a), Scalar::Int(b)) = (left, right) else {
                return Scalar::Null;
            };
            let value = match op {
                Arith::Add => a.checked_add(*b),
                Arith::Sub => a.checked_sub(*b),
                Arith::Mul => a.checked_mul(*b),
                Arith::Mod => a.checked_rem(*b),
                Arith::Div => None,
            };
            value.map_or(Scalar::Null, Scalar::Int)
        }
        Type::Decimal(scale) => {
            let value = if op == Arith::Mul {
                let (sa, sb) = (scale_of(left), scale_of(right));
                let (Some(a), Some(b)) = (left.as_decimal(sa), right.as_decimal(sb)) else {
                    return Scalar::Null;
                };
                a.checked_mul(b).and_then(|v| rescale(v, sa + sb, scale))
            } else {
                let (Some(a), Some(b)) = (left.as_decimal(scale), right.as_decimal(scale)) else {
                    return Scalar::Null;
                };
                match op {
                    Arith::Add => a.checked_add(b),
                    Arith::Sub => a.checked_sub(b),
                    Arith::Mod => a.checked_rem(b),
                    _ => None,
                }
            };
            value.map_or(Scalar::Null, |v| Scalar::Decimal(v, scale))
        }
        _ => Scalar::Null,
    }
}

fn rescale(value: i128, from: u8, to: u8) -> Option<i128> {
    if to >= from {
        value.checked_mul(pow10(to - from))
    } else {
        Some(value / pow10(from - to))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_types_follow_sql() {
        assert_eq!(
            arith_type(Arith::Add, Type::Int, Type::Int),
            Some(Type::Int)
        );
        assert_eq!(
            arith_type(Arith::Div, Type::Int, Type::Int),
            Some(Type::Float)
        );
        assert_eq!(
            arith_type(Arith::Add, Type::Decimal(2), Type::Int),
            Some(Type::Decimal(2))
        );
        assert_eq!(
            arith_type(Arith::Mul, Type::Decimal(2), Type::Decimal(2)),
            Some(Type::Decimal(4))
        );
        assert_eq!(
            arith_type(Arith::Sub, Type::Decimal(1), Type::Float),
            Some(Type::Float)
        );
        assert_eq!(arith_type(Arith::Add, Type::String, Type::Int), None);
    }

    #[test]
    fn decimals_stay_exact() {
        let a = Scalar::Decimal(150, 2); // 1.50
        let b = Scalar::Decimal(225, 2); // 2.25
        assert_eq!(
            arith(Arith::Add, Type::Decimal(2), &a, &b),
            Scalar::Decimal(375, 2)
        );
        assert_eq!(
            arith(Arith::Mul, Type::Decimal(4), &a, &b),
            Scalar::Decimal(33750, 4)
        );
        assert_eq!(
            arith(Arith::Add, Type::Decimal(2), &a, &Scalar::Int(1)),
            Scalar::Decimal(250, 2)
        );
    }

    #[test]
    fn division_by_zero_is_null_and_division_is_float() {
        assert_eq!(
            arith(Arith::Div, Type::Float, &Scalar::Int(5), &Scalar::Int(2)),
            Scalar::Float(2.5)
        );
        assert_eq!(
            arith(Arith::Div, Type::Float, &Scalar::Int(5), &Scalar::Int(0)),
            Scalar::Null
        );
        assert_eq!(
            arith(Arith::Mod, Type::Int, &Scalar::Int(-7), &Scalar::Int(3)),
            Scalar::Int(-1)
        );
    }

    #[test]
    fn comparisons_cross_numeric_types_and_null_is_unknown() {
        assert_eq!(
            Scalar::Int(2).compare(&Scalar::Decimal(150, 2)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            Scalar::Float(1.5).compare(&Scalar::Decimal(150, 2)),
            Some(Ordering::Equal)
        );
        assert_eq!(Scalar::Null.compare(&Scalar::Int(1)), None);
        assert_eq!(Scalar::String("a".into()).compare(&Scalar::Int(1)), None);
        assert_eq!(Scalar::Null.sort_cmp(&Scalar::Int(1)), Ordering::Greater);
    }
}
