//! Evaluating a checked [`Expr`] for one row.
//!
//! A straightforward interpreter over [`Scalar`]s: correct first. The engine
//! keeps it off the hot paths: plain column aggregates never come here, and
//! an expression over a single dictionary-encoded column is evaluated once
//! per distinct value rather than once per row.
//!
//! NULL follows SQL: it propagates through operators and functions, and
//! `AND`/`OR` use three-valued logic.

use std::cmp::Ordering;

use crate::ir::{Cmp, Expr, Func};
use crate::scalar::{Scalar, Type, arith, pow10};
use crate::temporal::{
    date_from_days, datetime_from_micros, days_from_date, extract, parse_date, parse_timestamp,
    parse_timestamptz, trunc_date, trunc_micros,
};

/// The values of the slots an expression reads.
pub trait Row {
    fn get(&self, slot: u32) -> Scalar;
}

impl Row for Vec<Scalar> {
    fn get(&self, slot: u32) -> Scalar {
        self.as_slice()
            .get(slot as usize)
            .cloned()
            .unwrap_or(Scalar::Null)
    }
}

/// `Some(true/false)`, or `None` for NULL.
fn truth(value: &Scalar) -> Option<bool> {
    match value {
        Scalar::Bool(b) => Some(*b),
        _ => None,
    }
}

fn from_truth(value: Option<bool>) -> Scalar {
    value.map_or(Scalar::Null, Scalar::Bool)
}

/// Evaluate `expr` for `row`.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn eval(expr: &Expr, row: &dyn Row) -> Scalar {
    match expr {
        Expr::Column { slot, .. } => row.get(*slot),
        Expr::Literal(value, _) => value.clone(),
        Expr::Neg(arg, ty) => match eval(arg, row) {
            Scalar::Int(v) => v.checked_neg().map_or(Scalar::Null, Scalar::Int),
            Scalar::Float(v) => Scalar::Float(-v),
            Scalar::Decimal(v, s) => Scalar::Decimal(-v, s),
            _ => {
                let _ = ty;
                Scalar::Null
            }
        },
        Expr::Not(arg) => from_truth(truth(&eval(arg, row)).map(|b| !b)),
        Expr::Arith {
            op,
            left,
            right,
            ty,
        } => arith(*op, *ty, &eval(left, row), &eval(right, row)),
        Expr::Compare { op, left, right } => {
            let ordering = eval(left, row).compare(&eval(right, row));
            from_truth(ordering.map(|o| compare(*op, o)))
        }
        Expr::And(left, right) => {
            let (a, b) = (truth(&eval(left, row)), truth(&eval(right, row)));
            from_truth(match (a, b) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            })
        }
        Expr::Or(left, right) => {
            let (a, b) = (truth(&eval(left, row)), truth(&eval(right, row)));
            from_truth(match (a, b) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            })
        }
        Expr::IsNull { arg, negated } => Scalar::Bool(eval(arg, row).is_null() != *negated),
        Expr::InList { arg, list, negated } => {
            let value = eval(arg, row);
            if value.is_null() {
                return Scalar::Null;
            }
            let mut unknown = false;
            for item in list {
                match value.compare(&eval(item, row)) {
                    Some(Ordering::Equal) => return Scalar::Bool(!negated),
                    None => unknown = true,
                    Some(_) => {}
                }
            }
            if unknown {
                Scalar::Null
            } else {
                Scalar::Bool(*negated)
            }
        }
        Expr::Between {
            arg,
            low,
            high,
            negated,
        } => {
            let value = eval(arg, row);
            let above = value.compare(&eval(low, row)).map(|o| o != Ordering::Less);
            let below = value
                .compare(&eval(high, row))
                .map(|o| o != Ordering::Greater);
            let within = match (above, below) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => Some(true),
                _ => None,
            };
            from_truth(within.map(|w| w != *negated))
        }
        Expr::Like {
            arg,
            pattern,
            negated,
            case_insensitive,
        } => match eval(arg, row) {
            Scalar::String(text) => {
                let matched = if *case_insensitive {
                    like(&text.to_lowercase(), &pattern.to_lowercase())
                } else {
                    like(&text, pattern)
                };
                Scalar::Bool(matched != *negated)
            }
            _ => Scalar::Null,
        },
        Expr::Case {
            operand,
            whens,
            otherwise,
            ty,
        } => {
            let operand = operand.as_ref().map(|o| eval(o, row));
            for (when, then) in whens {
                let hit = match &operand {
                    Some(value) => value.compare(&eval(when, row)) == Some(Ordering::Equal),
                    None => truth(&eval(when, row)) == Some(true),
                };
                if hit {
                    return cast(&eval(then, row), *ty);
                }
            }
            otherwise
                .as_ref()
                .map_or(Scalar::Null, |o| cast(&eval(o, row), *ty))
        }
        Expr::Coalesce(args, ty) => args
            .iter()
            .map(|a| eval(a, row))
            .find(|v| !v.is_null())
            .map_or(Scalar::Null, |v| cast(&v, *ty)),
        Expr::NullIf(a, b, _) => {
            let value = eval(a, row);
            if value.compare(&eval(b, row)) == Some(Ordering::Equal) {
                Scalar::Null
            } else {
                value
            }
        }
        Expr::Concat(a, b) => match (eval(a, row), eval(b, row)) {
            (Scalar::Null, _) | (_, Scalar::Null) => Scalar::Null,
            (a, b) => Scalar::String(format!("{}{}", text(&a), text(&b))),
        },
        Expr::Func { func, args, ty } => function(*func, &eval(&args[0], row), *ty),
        Expr::Cast { arg, to } => cast(&eval(arg, row), *to),
    }
}

fn compare(op: Cmp, ordering: Ordering) -> bool {
    match op {
        Cmp::Eq => ordering == Ordering::Equal,
        Cmp::NotEq => ordering != Ordering::Equal,
        Cmp::Lt => ordering == Ordering::Less,
        Cmp::LtEq => ordering != Ordering::Greater,
        Cmp::Gt => ordering == Ordering::Greater,
        Cmp::GtEq => ordering != Ordering::Less,
    }
}

/// SQL `LIKE`: `%` is any run of characters, `_` any one.
fn like(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    // classic two-pointer match with backtracking to the last `%`
    let (mut t, mut p) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '_' || pattern[p] == text[t]) {
            t += 1;
            p += 1;
        } else if p < pattern.len() && pattern[p] == '%' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '%')
}

/// A value as text (for `||` and casts to string).
#[must_use]
pub fn text(value: &Scalar) -> String {
    match value {
        Scalar::Null => String::new(),
        Scalar::Bool(b) => b.to_string(),
        Scalar::Int(v) => v.to_string(),
        Scalar::Float(v) => v.to_string(),
        Scalar::Decimal(v, scale) => {
            let negative = *v < 0;
            let digits = v.unsigned_abs().to_string();
            let scale = usize::from(*scale);
            let body = if scale == 0 {
                digits
            } else if digits.len() <= scale {
                format!("0.{}{digits}", "0".repeat(scale - digits.len()))
            } else {
                format!(
                    "{}.{}",
                    &digits[..digits.len() - scale],
                    &digits[digits.len() - scale..]
                )
            };
            if negative { format!("-{body}") } else { body }
        }
        Scalar::String(s) => s.clone(),
        Scalar::Date(days) => {
            date_from_days(*days).map_or_else(|| "?".into(), |d| d.format("%Y-%m-%d").to_string())
        }
        Scalar::Timestamp(m) => datetime_from_micros(*m).map_or_else(
            || "?".into(),
            |t| t.format("%Y-%m-%d %H:%M:%S%.f").to_string(),
        ),
        Scalar::TimestampTz(m) => datetime_from_micros(*m).map_or_else(
            || "?".into(),
            |t| t.format("%Y-%m-%dT%H:%M:%S%.fZ").to_string(),
        ),
    }
}

/// Convert a value to `to`; NULL when it doesn't convert.
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn cast(value: &Scalar, to: Type) -> Scalar {
    match (value, to) {
        (Scalar::Null, _) | (_, Type::Null) => Scalar::Null,
        (v, Type::String) => Scalar::String(text(v)),
        (Scalar::Int(_) | Scalar::Float(_) | Scalar::Decimal(..), Type::Float) => {
            value.as_f64().map_or(Scalar::Null, Scalar::Float)
        }
        (Scalar::Int(_) | Scalar::Decimal(..), Type::Decimal(scale)) => value
            .as_decimal(scale)
            .map_or(Scalar::Null, |v| Scalar::Decimal(v, scale)),
        (Scalar::Float(f), Type::Decimal(scale)) => {
            let scaled = (f * pow10(scale) as f64).round();
            if scaled.is_finite() {
                Scalar::Decimal(scaled as i128, scale)
            } else {
                Scalar::Null
            }
        }
        (Scalar::Int(v), Type::Int) => Scalar::Int(*v),
        (Scalar::Float(f), Type::Int) => {
            let rounded = f.round();
            if rounded.is_finite() && rounded.abs() < 9.2e18 {
                Scalar::Int(rounded as i64)
            } else {
                Scalar::Null
            }
        }
        (Scalar::Decimal(v, scale), Type::Int) => {
            let divisor = pow10(*scale);
            let rounded = (v + v.signum() * divisor / 2) / divisor;
            i64::try_from(rounded).map_or(Scalar::Null, Scalar::Int)
        }
        (Scalar::String(s), Type::Int) => s.trim().parse().map_or(Scalar::Null, Scalar::Int),
        (Scalar::String(s), Type::Float) => s.trim().parse().map_or(Scalar::Null, Scalar::Float),
        (Scalar::String(s), Type::Decimal(scale)) => cast(
            &Scalar::Float(s.trim().parse().unwrap_or(f64::NAN)),
            Type::Decimal(scale),
        ),
        (Scalar::String(s), Type::Bool) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "1" => Scalar::Bool(true),
            "false" | "f" | "0" => Scalar::Bool(false),
            _ => Scalar::Null,
        },
        (Scalar::String(s), Type::Date) => parse_date(s).map_or(Scalar::Null, Scalar::Date),
        (Scalar::String(s), Type::Timestamp) => {
            parse_timestamp(s).map_or(Scalar::Null, Scalar::Timestamp)
        }
        (Scalar::String(s), Type::TimestampTz) => {
            parse_timestamptz(s).map_or(Scalar::Null, Scalar::TimestampTz)
        }
        (Scalar::Date(d), Type::Timestamp) => Scalar::Timestamp(i64::from(*d) * 86_400_000_000),
        (Scalar::Date(d), Type::TimestampTz) => Scalar::TimestampTz(i64::from(*d) * 86_400_000_000),
        (Scalar::Timestamp(m) | Scalar::TimestampTz(m), Type::Date) => {
            Scalar::Date(i32::try_from(m.div_euclid(86_400_000_000)).unwrap_or(i32::MAX))
        }
        (Scalar::Timestamp(m), Type::TimestampTz) => Scalar::TimestampTz(*m),
        (Scalar::TimestampTz(m), Type::Timestamp) => Scalar::Timestamp(*m),
        (v, t) if scalar_type(v) == t => v.clone(),
        _ => Scalar::Null,
    }
}

fn scalar_type(value: &Scalar) -> Type {
    match value {
        Scalar::Null => Type::Null,
        Scalar::Bool(_) => Type::Bool,
        Scalar::Int(_) => Type::Int,
        Scalar::Float(_) => Type::Float,
        Scalar::Decimal(_, s) => Type::Decimal(*s),
        Scalar::String(_) => Type::String,
        Scalar::Date(_) => Type::Date,
        Scalar::Timestamp(_) => Type::Timestamp,
        Scalar::TimestampTz(_) => Type::TimestampTz,
    }
}

#[allow(clippy::cast_possible_truncation)]
#[allow(clippy::match_same_arms)]
fn function(func: Func, value: &Scalar, ty: Type) -> Scalar {
    if value.is_null() {
        return Scalar::Null;
    }
    match (func, value) {
        (Func::Lower, Scalar::String(s)) => Scalar::String(s.to_lowercase()),
        (Func::Upper, Scalar::String(s)) => Scalar::String(s.to_uppercase()),
        (Func::Trim, Scalar::String(s)) => Scalar::String(s.trim().to_string()),
        (Func::Length, Scalar::String(s)) => {
            Scalar::Int(i64::try_from(s.chars().count()).unwrap_or(i64::MAX))
        }
        (Func::Abs, Scalar::Int(v)) => v.checked_abs().map_or(Scalar::Null, Scalar::Int),
        (Func::Abs, Scalar::Float(v)) => Scalar::Float(v.abs()),
        (Func::Abs, Scalar::Decimal(v, s)) => Scalar::Decimal(v.abs(), *s),
        (Func::Round(digits), Scalar::Float(v)) => {
            let factor = 10f64.powi(digits);
            // half away from zero, as SQL
            Scalar::Float((v * factor).round() / factor)
        }
        (Func::Round(_), Scalar::Int(v)) => Scalar::Int(*v),
        (Func::Round(_), Scalar::Decimal(v, scale)) => {
            let Type::Decimal(to) = ty else {
                return Scalar::Null;
            };
            let divisor = pow10(scale - to.min(*scale));
            let half = v.signum() * divisor / 2;
            Scalar::Decimal((v + half) / divisor, to)
        }
        (Func::Floor | Func::Ceil, Scalar::Float(v)) => Scalar::Float(if func == Func::Floor {
            v.floor()
        } else {
            v.ceil()
        }),
        (Func::Floor | Func::Ceil, Scalar::Int(v)) => Scalar::Int(*v),
        (Func::Floor | Func::Ceil, Scalar::Decimal(v, scale)) => {
            let divisor = pow10(*scale);
            let floor = v.div_euclid(divisor);
            let result = if func == Func::Ceil && v.rem_euclid(divisor) != 0 {
                floor + 1
            } else {
                floor
            };
            Scalar::Decimal(result, 0)
        }
        (Func::Extract(part), _) => {
            temporal(value).map_or(Scalar::Null, |t| Scalar::Int(extract(part, t)))
        }
        (Func::DateTrunc(part), Scalar::Date(days)) if part.is_calendar() => date_from_days(*days)
            .map_or(Scalar::Null, |d| {
                Scalar::Date(days_from_date(trunc_date(part, d)))
            }),
        (Func::DateTrunc(part), Scalar::Timestamp(m) | Scalar::TimestampTz(m))
            if part.is_calendar() =>
        {
            datetime_from_micros(*m).map_or(Scalar::Null, |t| {
                Scalar::Date(days_from_date(trunc_date(part, t.date())))
            })
        }
        (Func::DateTrunc(part), Scalar::Timestamp(m)) => Scalar::Timestamp(trunc_micros(part, *m)),
        (Func::DateTrunc(part), Scalar::TimestampTz(m)) => {
            Scalar::TimestampTz(trunc_micros(part, *m))
        }
        (Func::DateTrunc(_), Scalar::Date(d)) => Scalar::Date(*d),
        _ => Scalar::Null,
    }
}

fn temporal(value: &Scalar) -> Option<chrono::NaiveDateTime> {
    match value {
        Scalar::Date(days) => date_from_days(*days).and_then(|d| d.and_hms_opt(0, 0, 0)),
        Scalar::Timestamp(m) | Scalar::TimestampTz(m) => datetime_from_micros(*m),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::{Scope, check_row};
    use crate::syntax::{Name, parse};

    /// Slots: 0 = qty (int), 1 = status (string), 2 = amount (decimal 2), 3 = at (timestamptz).
    struct Columns;

    impl Scope for Columns {
        fn resolve(&self, parts: &[Name]) -> Result<(u32, Type), String> {
            match parts[0].value.as_str() {
                "qty" => Ok((0, Type::Int)),
                "status" => Ok((1, Type::String)),
                "amount" => Ok((2, Type::Decimal(2))),
                "at" => Ok((3, Type::TimestampTz)),
                other => Err(other.to_string()),
            }
        }
    }

    fn run(text: &str, row: &[Scalar]) -> Scalar {
        let row = row.to_vec();
        let expr = match parse(text)
            .map_err(|e| e.0)
            .and_then(|ast| check_row(&ast, &Columns))
        {
            Ok(expr) => expr,
            Err(err) => panic!("{text}: {err}"),
        };
        eval(&expr, &row)
    }

    fn row() -> Vec<Scalar> {
        vec![
            Scalar::Int(3),
            Scalar::String("paid".into()),
            Scalar::Decimal(1250, 2),
            Scalar::TimestampTz(parse_timestamptz("2026-10-07T12:00:00Z").unwrap_or(0)),
        ]
    }

    #[test]
    fn arithmetic_and_comparison() {
        assert_eq!(run("qty * amount", &row()), Scalar::Decimal(3750, 2));
        assert_eq!(run("qty / 2", &row()), Scalar::Float(1.5));
        assert_eq!(run("amount > 12", &row()), Scalar::Bool(true));
        assert_eq!(
            run("status = 'paid' AND qty BETWEEN 1 AND 3", &row()),
            Scalar::Bool(true)
        );
        assert_eq!(run("at >= '2026-10-01'", &row()), Scalar::Bool(true));
    }

    #[test]
    fn null_follows_three_valued_logic() {
        let nulls = vec![Scalar::Null, Scalar::Null, Scalar::Null, Scalar::Null];
        assert_eq!(run("qty > 1", &nulls), Scalar::Null);
        assert_eq!(run("qty > 1 AND false", &nulls), Scalar::Bool(false));
        assert_eq!(run("qty > 1 OR true", &nulls), Scalar::Bool(true));
        assert_eq!(run("qty IS NULL", &nulls), Scalar::Bool(true));
        assert_eq!(run("qty IN (1, 2)", &nulls), Scalar::Null);
        assert_eq!(run("COALESCE(qty, 7)", &nulls), Scalar::Int(7));
    }

    #[test]
    fn functions() {
        assert_eq!(
            run("UPPER(status) || '!'", &row()),
            Scalar::String("PAID!".into())
        );
        assert_eq!(run("status LIKE 'p_i%'", &row()), Scalar::Bool(true));
        assert_eq!(run("status LIKE 'x%'", &row()), Scalar::Bool(false));
        assert_eq!(run("ROUND(amount, 1)", &row()), Scalar::Decimal(125, 1));
        assert_eq!(run("ROUND(2.5)", &row()), Scalar::Decimal(3, 0));
        assert_eq!(run("ROUND(-2.5)", &row()), Scalar::Decimal(-3, 0));
        assert_eq!(run("EXTRACT(MONTH FROM at)", &row()), Scalar::Int(10));
        assert_eq!(
            run("DATE_TRUNC('week', at)", &row()),
            Scalar::Date(parse_date("2026-10-05").unwrap_or(0))
        );
        assert_eq!(
            run("CASE status WHEN 'paid' THEN 1 ELSE 0 END", &row()),
            Scalar::Int(1)
        );
        assert_eq!(run("CAST(amount AS INTEGER)", &row()), Scalar::Int(13));
        assert_eq!(run("NULLIF(qty, 3)", &row()), Scalar::Null);
    }
}
