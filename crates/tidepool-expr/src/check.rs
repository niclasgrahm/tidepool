//! Checking: SQL syntax → typed [`Expr`], with every name resolved through a
//! [`Scope`] and every operator's operand types checked.
//!
//! Two entry points: [`check_row`] for row-level expressions (fields, filter
//! clauses, the arguments of aggregates) and [`check_aggregation`] for a base
//! metric, which splits it into aggregate calls and the expression that
//! combines their results.

use sqlparser::ast::{
    BinaryOperator, CeilFloorKind, DataType, DuplicateTreatment, ExactNumberInfo, Expr as Ast,
    FunctionArg, FunctionArgExpr, FunctionArguments, ObjectNamePart, TimezoneInfo, UnaryOperator,
    Value,
};

use crate::ir::{AggCall, AggFunc, Aggregation, Cmp, Expr, Func};
use crate::scalar::{Arith, Scalar, Type, arith_type};
use crate::syntax::Name;
use crate::temporal::{Part, parse_date, parse_timestamp, parse_timestamptz};

/// Resolves names to slots for the checker.
pub trait Scope {
    /// The slot and type `parts` refers to (`amount`, `orders.amount`), or a
    /// message saying why it doesn't resolve.
    fn resolve(&self, parts: &[Name]) -> Result<(u32, Type), String>;
}

/// Check a row-level expression: no aggregates allowed.
///
/// # Errors
/// A name doesn't resolve, an operator doesn't apply to its operand types,
/// or the expression uses something Tidepool doesn't evaluate.
pub fn check_row(ast: &Ast, scope: &dyn Scope) -> Result<Expr, String> {
    Checker {
        scope,
        aggregates: None,
    }
    .expr(ast)
}

/// Check a base metric: aggregates, combined by row-free arithmetic.
///
/// # Errors
/// As [`check_row`], plus aggregates in the wrong places.
pub fn check_aggregation(ast: &Ast, scope: &dyn Scope) -> Result<Aggregation, String> {
    let mut calls = Vec::new();
    let combine = Checker {
        scope,
        aggregates: Some(&mut calls),
    }
    .expr(ast)?;
    Ok(Aggregation { calls, combine })
}

struct Checker<'a> {
    scope: &'a dyn Scope,
    /// `Some` while checking a metric outside any aggregate: aggregate calls
    /// are collected here and replaced by slots.
    aggregates: Option<&'a mut Vec<AggCall>>,
}

fn function_name(name: &sqlparser::ast::ObjectName) -> String {
    name.0
        .last()
        .map(|part| match part {
            ObjectNamePart::Identifier(ident) => ident.value.to_lowercase(),
            ObjectNamePart::Function(f) => f.name.value.to_lowercase(),
        })
        .unwrap_or_default()
}

fn literal_number(text: &str) -> Result<(Scalar, Type), String> {
    if text.contains(['e', 'E']) {
        return text
            .parse::<f64>()
            .map(|f| (Scalar::Float(f), Type::Float))
            .map_err(|_| format!("`{text}` is not a number"));
    }
    if let Some((whole, fraction)) = text.split_once('.') {
        let scale =
            u8::try_from(fraction.len()).map_err(|_| format!("`{text}` has too many decimals"))?;
        let digits = format!("{whole}{fraction}");
        let value: i128 = digits
            .parse()
            .map_err(|_| format!("`{text}` is not a number"))?;
        return Ok((Scalar::Decimal(value, scale), Type::Decimal(scale)));
    }
    text.parse::<i64>()
        .map(|v| (Scalar::Int(v), Type::Int))
        .map_err(|_| format!("`{text}` is not a number"))
}

/// Parse a string literal as a temporal value of type `ty`.
fn temporal_literal(text: &str, ty: Type) -> Result<Scalar, String> {
    let parsed = match ty {
        Type::Date => parse_date(text).map(Scalar::Date),
        Type::Timestamp => parse_timestamp(text).map(Scalar::Timestamp),
        Type::TimestampTz => parse_timestamptz(text).map(Scalar::TimestampTz),
        _ => None,
    };
    parsed.ok_or_else(|| format!("'{text}' is not a valid {ty}"))
}

/// When one side is temporal and the other a string literal, read the literal
/// as that temporal type (`ordered_at >= '2026-01-01'`).
fn coerce_literal(expr: Expr, target: Type) -> Result<Expr, String> {
    match (&expr, target) {
        (Expr::Literal(Scalar::String(text), _), t) if t.is_temporal() => {
            Ok(Expr::Literal(temporal_literal(text, t)?, t))
        }
        _ => Ok(expr),
    }
}

fn cast_type(data_type: &DataType) -> Result<Type, String> {
    Ok(match data_type {
        DataType::Int(_)
        | DataType::Integer(_)
        | DataType::BigInt(_)
        | DataType::Int32
        | DataType::Int64 => Type::Int,
        DataType::Double(_)
        | DataType::Float(_)
        | DataType::Real
        | DataType::Float64
        | DataType::Float8 => Type::Float,
        DataType::Decimal(info) | DataType::Numeric(info) => Type::Decimal(match info {
            ExactNumberInfo::PrecisionAndScale(_, scale) => u8::try_from(*scale).unwrap_or(0),
            _ => 0,
        }),
        DataType::Varchar(_) | DataType::Text | DataType::String(_) => Type::String,
        DataType::Boolean | DataType::Bool => Type::Bool,
        DataType::Date => Type::Date,
        DataType::Timestamp(_, TimezoneInfo::WithTimeZone) => Type::TimestampTz,
        DataType::Timestamp(..) | DataType::TimestampNtz(_) => Type::Timestamp,
        other => return Err(format!("CAST to {other} isn't supported")),
    })
}

/// Whether a cast from `from` to `to` is defined.
fn castable(from: Type, to: Type) -> bool {
    from == to
        || from == Type::Null
        || to == Type::String
        || (from.is_numeric() && to.is_numeric())
        || (from == Type::String && (to.is_numeric() || to.is_temporal() || to == Type::Bool))
        || (from.is_temporal() && to.is_temporal())
}

fn comparable(a: Type, b: Type) -> bool {
    a == Type::Null || b == Type::Null || a == b || (a.is_numeric() && b.is_numeric())
}

impl Checker<'_> {
    fn row(&self) -> Checker<'_> {
        Checker {
            scope: self.scope,
            aggregates: None,
        }
    }

    fn boxed(&mut self, ast: &Ast) -> Result<Box<Expr>, String> {
        self.expr(ast).map(Box::new)
    }

    #[allow(clippy::too_many_lines)]
    fn expr(&mut self, ast: &Ast) -> Result<Expr, String> {
        match ast {
            Ast::Nested(inner) => self.expr(inner),
            Ast::Identifier(ident) => self.column(&[Name::from(ident)]),
            Ast::CompoundIdentifier(idents) => {
                let parts: Vec<Name> = idents.iter().map(Name::from).collect();
                self.column(&parts)
            }
            Ast::Value(value) => Ok(match &value.value {
                Value::Number(text, _) => {
                    let (scalar, ty) = literal_number(text)?;
                    Expr::Literal(scalar, ty)
                }
                Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                    Expr::Literal(Scalar::String(text.clone()), Type::String)
                }
                Value::Boolean(b) => Expr::Literal(Scalar::Bool(*b), Type::Bool),
                Value::Null => Expr::Literal(Scalar::Null, Type::Null),
                other => return Err(format!("the literal {other} isn't supported")),
            }),
            Ast::TypedString(typed) => {
                let ty = cast_type(&typed.data_type)?;
                let text = match &typed.value.value {
                    Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                        text.clone()
                    }
                    other => other.to_string(),
                };
                Ok(Expr::Literal(temporal_literal(&text, ty)?, ty))
            }
            Ast::UnaryOp { op, expr } => {
                let arg = self.expr(expr)?;
                match op {
                    UnaryOperator::Not => {
                        expect_bool(&arg, "NOT")?;
                        Ok(Expr::Not(Box::new(arg)))
                    }
                    UnaryOperator::Minus => {
                        let ty = arg.ty();
                        if !ty.is_numeric() {
                            return Err(format!("can't negate a {ty}"));
                        }
                        Ok(Expr::Neg(Box::new(arg), ty))
                    }
                    UnaryOperator::Plus => Ok(arg),
                    other => Err(format!("the operator {other} isn't supported")),
                }
            }
            Ast::BinaryOp { left, op, right } => self.binary(left, op, right),
            Ast::IsNull(arg) => Ok(Expr::IsNull {
                arg: self.boxed(arg)?,
                negated: false,
            }),
            Ast::IsNotNull(arg) => Ok(Expr::IsNull {
                arg: self.boxed(arg)?,
                negated: true,
            }),
            Ast::InList {
                expr,
                list,
                negated,
            } => {
                let arg = self.expr(expr)?;
                let ty = arg.ty();
                let mut items = Vec::new();
                for item in list {
                    let item = coerce_literal(self.expr(item)?, ty)?;
                    if !comparable(ty, item.ty()) {
                        return Err(format!("IN compares a {ty} with a {}", item.ty()));
                    }
                    items.push(item);
                }
                Ok(Expr::InList {
                    arg: Box::new(arg),
                    list: items,
                    negated: *negated,
                })
            }
            Ast::Between {
                expr,
                negated,
                low,
                high,
            } => {
                let arg = self.expr(expr)?;
                let ty = arg.ty();
                let low = coerce_literal(self.expr(low)?, ty)?;
                let high = coerce_literal(self.expr(high)?, ty)?;
                for bound in [&low, &high] {
                    if !comparable(ty, bound.ty()) {
                        return Err(format!("BETWEEN compares a {ty} with a {}", bound.ty()));
                    }
                }
                Ok(Expr::Between {
                    arg: Box::new(arg),
                    low: Box::new(low),
                    high: Box::new(high),
                    negated: *negated,
                })
            }
            Ast::Like {
                negated,
                expr,
                pattern,
                ..
            }
            | Ast::ILike {
                negated,
                expr,
                pattern,
                ..
            } => {
                let arg = self.expr(expr)?;
                if arg.ty() != Type::String {
                    return Err(format!("LIKE needs a string, not a {}", arg.ty()));
                }
                let Expr::Literal(Scalar::String(pattern), _) = self.expr(pattern)? else {
                    return Err("LIKE needs a literal pattern".to_string());
                };
                Ok(Expr::Like {
                    arg: Box::new(arg),
                    pattern,
                    negated: *negated,
                    case_insensitive: matches!(ast, Ast::ILike { .. }),
                })
            }
            Ast::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                let operand = operand.as_ref().map(|o| self.boxed(o)).transpose()?;
                let mut ty = Type::Null;
                let mut whens = Vec::new();
                for case in conditions {
                    let when = self.expr(&case.condition)?;
                    match &operand {
                        Some(operand) if !comparable(operand.ty(), when.ty()) => {
                            return Err(format!(
                                "CASE compares a {} with a {}",
                                operand.ty(),
                                when.ty()
                            ));
                        }
                        None => expect_bool(&when, "WHEN")?,
                        Some(_) => {}
                    }
                    let then = self.expr(&case.result)?;
                    ty = ty
                        .unify(then.ty())
                        .ok_or_else(|| format!("CASE results mix {ty} and {}", then.ty()))?;
                    whens.push((when, then));
                }
                let otherwise = else_result.as_ref().map(|e| self.boxed(e)).transpose()?;
                if let Some(otherwise) = &otherwise {
                    ty = ty
                        .unify(otherwise.ty())
                        .ok_or_else(|| format!("CASE results mix {ty} and {}", otherwise.ty()))?;
                }
                Ok(Expr::Case {
                    operand,
                    whens,
                    otherwise,
                    ty,
                })
            }
            Ast::Cast {
                expr, data_type, ..
            } => {
                let arg = self.expr(expr)?;
                let to = cast_type(data_type)?;
                if !castable(arg.ty(), to) {
                    return Err(format!("can't CAST a {} to {to}", arg.ty()));
                }
                Ok(Expr::Cast {
                    arg: Box::new(arg),
                    to,
                })
            }
            Ast::Extract { field, expr, .. } => {
                let part = Part::parse(&field.to_string())
                    .ok_or_else(|| format!("EXTRACT({field}) isn't supported"))?;
                let arg = self.expr(expr)?;
                if !arg.ty().is_temporal() {
                    return Err(format!(
                        "EXTRACT needs a date or timestamp, not a {}",
                        arg.ty()
                    ));
                }
                Ok(Expr::Func {
                    func: Func::Extract(part),
                    args: vec![arg],
                    ty: Type::Int,
                })
            }
            Ast::Floor { expr, field } | Ast::Ceil { expr, field } => {
                if !matches!(
                    field,
                    CeilFloorKind::DateTimeField(sqlparser::ast::DateTimeField::NoDateTime)
                ) {
                    return Err("FLOOR/CEIL with a unit or scale isn't supported".to_string());
                }
                let func = if matches!(ast, Ast::Floor { .. }) {
                    Func::Floor
                } else {
                    Func::Ceil
                };
                let arg = self.expr(expr)?;
                Self::numeric_func(func, vec![arg])
            }
            Ast::Trim {
                expr,
                trim_what: None,
                trim_where: None,
                ..
            } => {
                let arg = self.expr(expr)?;
                string_func(Func::Trim, arg, Type::String)
            }
            Ast::Function(function) => self.function(function),
            Ast::Subquery(_) | Ast::InSubquery { .. } | Ast::Exists { .. } => {
                Err("subqueries aren't supported".to_string())
            }
            other => Err(format!("`{other}` isn't supported")),
        }
    }

    fn column(&mut self, parts: &[Name]) -> Result<Expr, String> {
        if self.aggregates.is_some() {
            let name: Vec<&str> = parts.iter().map(|p| p.value.as_str()).collect();
            return Err(format!("`{}` is outside an aggregate", name.join(".")));
        }
        let (slot, ty) = self.scope.resolve(parts)?;
        Ok(Expr::Column { slot, ty })
    }

    fn binary(&mut self, left: &Ast, op: &BinaryOperator, right: &Ast) -> Result<Expr, String> {
        let left = self.expr(left)?;
        let right = self.expr(right)?;
        let arith = |op| -> Result<Expr, String> {
            let ty = arith_type(op, left.ty(), right.ty()).ok_or_else(|| {
                format!(
                    "`{op}` doesn't apply to {} and {}",
                    left.ty().a(),
                    right.ty().a()
                )
            })?;
            Ok(Expr::Arith {
                op,
                left: Box::new(left.clone()),
                right: Box::new(right.clone()),
                ty,
            })
        };
        let compare = |cmp| -> Result<Expr, String> {
            let left = coerce_literal(left.clone(), right.ty())?;
            let right = coerce_literal(right.clone(), left.ty())?;
            if !comparable(left.ty(), right.ty()) {
                return Err(format!(
                    "can't compare {} with {}",
                    left.ty().a(),
                    right.ty().a()
                ));
            }
            Ok(Expr::Compare {
                op: cmp,
                left: Box::new(left),
                right: Box::new(right),
            })
        };
        match op {
            BinaryOperator::Plus => arith(Arith::Add),
            BinaryOperator::Minus => arith(Arith::Sub),
            BinaryOperator::Multiply => arith(Arith::Mul),
            BinaryOperator::Divide => arith(Arith::Div),
            BinaryOperator::Modulo => arith(Arith::Mod),
            BinaryOperator::Eq => compare(Cmp::Eq),
            BinaryOperator::NotEq => compare(Cmp::NotEq),
            BinaryOperator::Lt => compare(Cmp::Lt),
            BinaryOperator::LtEq => compare(Cmp::LtEq),
            BinaryOperator::Gt => compare(Cmp::Gt),
            BinaryOperator::GtEq => compare(Cmp::GtEq),
            BinaryOperator::And | BinaryOperator::Or => {
                expect_bool(&left, &op.to_string())?;
                expect_bool(&right, &op.to_string())?;
                Ok(if *op == BinaryOperator::And {
                    Expr::And(Box::new(left), Box::new(right))
                } else {
                    Expr::Or(Box::new(left), Box::new(right))
                })
            }
            BinaryOperator::StringConcat => Ok(Expr::Concat(Box::new(left), Box::new(right))),
            other => Err(format!("the operator {other} isn't supported")),
        }
    }
}

/// The arguments of a call, and whether it says DISTINCT or `*`.
fn function_args(function: &sqlparser::ast::Function) -> Result<(Vec<&Ast>, bool, bool), String> {
    {
        match &function.args {
            FunctionArguments::None => Ok((Vec::new(), false, false)),
            FunctionArguments::List(list) => {
                let distinct =
                    matches!(list.duplicate_treatment, Some(DuplicateTreatment::Distinct));
                let mut args = Vec::new();
                let mut star = false;
                for arg in &list.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => args.push(e),
                        FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => star = true,
                        _ => return Err(format!("unsupported argument in {}", function.name)),
                    }
                }
                Ok((args, distinct, star))
            }
            FunctionArguments::Subquery(_) => Err("subqueries aren't supported".to_string()),
        }
    }
}

fn arity(name: &str, got: usize, n: usize) -> Result<(), String> {
    if got == n {
        Ok(())
    } else {
        Err(format!(
            "{name} takes {n} argument{}",
            if n == 1 { "" } else { "s" }
        ))
    }
}

impl Checker<'_> {
    #[allow(clippy::too_many_lines)]
    fn function(&mut self, function: &sqlparser::ast::Function) -> Result<Expr, String> {
        let name = function_name(&function.name);
        if function.over.is_some() {
            return Err("window functions aren't supported".to_string());
        }
        let (args, distinct, star) = function_args(function)?;
        if let Some(aggregate) = aggregate_func(&name) {
            return self.aggregate(aggregate, &args, distinct, star, function.filter.as_deref());
        }
        let mut checked = Vec::new();
        for arg in &args {
            checked.push(self.expr(arg)?);
        }
        match name.as_str() {
            "lower" | "upper" | "trim" => {
                arity(&name, checked.len(), 1)?;
                let func = match name.as_str() {
                    "lower" => Func::Lower,
                    "upper" => Func::Upper,
                    _ => Func::Trim,
                };
                string_func(func, checked.remove(0), Type::String)
            }
            "length" | "len" | "char_length" => {
                arity(&name, checked.len(), 1)?;
                string_func(Func::Length, checked.remove(0), Type::Int)
            }
            "abs" => {
                arity(&name, checked.len(), 1)?;
                Self::numeric_func(Func::Abs, checked)
            }
            "floor" | "ceil" | "ceiling" => {
                arity(&name, checked.len(), 1)?;
                Self::numeric_func(
                    if name == "floor" {
                        Func::Floor
                    } else {
                        Func::Ceil
                    },
                    checked,
                )
            }
            "round" => {
                let digits = match checked.get(1) {
                    None => 0,
                    Some(Expr::Literal(Scalar::Int(d), _)) => i32::try_from(*d).unwrap_or(0),
                    Some(_) => {
                        return Err("ROUND's second argument must be a literal integer".to_string());
                    }
                };
                checked.truncate(1);
                arity(&name, checked.len(), 1)?;
                Self::numeric_func(Func::Round(digits), checked)
            }
            "coalesce" => {
                if checked.is_empty() {
                    return Err("COALESCE takes at least one argument".to_string());
                }
                let mut ty = Type::Null;
                for arg in &checked {
                    ty = ty
                        .unify(arg.ty())
                        .ok_or_else(|| format!("COALESCE mixes {ty} and {}", arg.ty()))?;
                }
                Ok(Expr::Coalesce(checked, ty))
            }
            "nullif" => {
                arity(&name, checked.len(), 2)?;
                let (a, b) = (checked.remove(0), checked.remove(0));
                if !comparable(a.ty(), b.ty()) {
                    return Err(format!("NULLIF compares a {} with a {}", a.ty(), b.ty()));
                }
                let ty = a.ty();
                Ok(Expr::NullIf(Box::new(a), Box::new(b), ty))
            }
            "date_trunc" => {
                arity(&name, checked.len(), 2)?;
                let Expr::Literal(Scalar::String(part), _) = &checked[0] else {
                    return Err(
                        "DATE_TRUNC's first argument must be a literal part, e.g. 'month'"
                            .to_string(),
                    );
                };
                let part = Part::parse(part)
                    .ok_or_else(|| format!("DATE_TRUNC('{part}') isn't supported"))?;
                let arg = checked.remove(1);
                if !arg.ty().is_temporal() {
                    return Err(format!(
                        "DATE_TRUNC needs a date or timestamp, not a {}",
                        arg.ty()
                    ));
                }
                let ty = if part.is_calendar() {
                    Type::Date
                } else {
                    arg.ty()
                };
                Ok(Expr::Func {
                    func: Func::DateTrunc(part),
                    args: vec![arg],
                    ty,
                })
            }
            other => Err(format!("the function {other} isn't supported")),
        }
    }

    fn numeric_func(func: Func, args: Vec<Expr>) -> Result<Expr, String> {
        let ty = args[0].ty();
        let ty = match (func, ty) {
            (_, t) if !t.is_numeric() => return Err(format!("{func:?} needs a number, not a {t}")),
            (Func::Round(digits), Type::Decimal(scale)) => {
                Type::Decimal(u8::try_from(digits.clamp(0, i32::from(scale))).unwrap_or(scale))
            }
            (Func::Floor | Func::Ceil, Type::Decimal(_)) => Type::Decimal(0),
            (_, t) => t,
        };
        Ok(Expr::Func { func, args, ty })
    }

    fn aggregate(
        &mut self,
        func: AggFunc,
        args: &[&Ast],
        distinct: bool,
        star: bool,
        filter: Option<&Ast>,
    ) -> Result<Expr, String> {
        if self.aggregates.is_none() {
            return Err("an aggregate can't be used here".to_string());
        }
        // inside an aggregate, expressions are row-level again
        let (func, arg) = match (func, star, args) {
            (AggFunc::Count, true, []) => (AggFunc::CountStar, None),
            (_, false, [arg]) => (func, Some(self.row().expr(arg)?)),
            _ => return Err(format!("{} takes one argument", func.name())),
        };
        let filter = filter.map(|f| self.row().expr(f)).transpose()?;
        if let Some(filter) = &filter {
            expect_bool(filter, "FILTER")?;
        }
        let arg_ty = arg.as_ref().map_or(Type::Null, Expr::ty);
        let ty = match func {
            AggFunc::Count | AggFunc::CountStar => Type::Int,
            AggFunc::Avg if arg_ty.is_numeric() => Type::Float,
            AggFunc::Sum if arg_ty.is_numeric() => match arg_ty {
                Type::Null => Type::Int,
                t => t,
            },
            AggFunc::Min | AggFunc::Max => arg_ty,
            _ => {
                return Err(format!(
                    "{} needs a number, not {}",
                    func.name(),
                    arg_ty.a()
                ));
            }
        };
        let Some(calls) = self.aggregates.as_mut() else {
            return Err("an aggregate can't be used here".to_string());
        };
        let slot = u32::try_from(calls.len()).unwrap_or(u32::MAX);
        calls.push(AggCall {
            func,
            distinct,
            arg,
            filter,
            ty,
        });
        Ok(Expr::Column { slot, ty })
    }
}

fn aggregate_func(name: &str) -> Option<AggFunc> {
    Some(match name {
        "sum" => AggFunc::Sum,
        "count" => AggFunc::Count,
        "min" => AggFunc::Min,
        "max" => AggFunc::Max,
        "avg" => AggFunc::Avg,
        _ => return None,
    })
}

fn expect_bool(expr: &Expr, what: &str) -> Result<(), String> {
    match expr.ty() {
        Type::Bool | Type::Null => Ok(()),
        other => Err(format!(
            "{what} needs a true/false condition, not a {other}"
        )),
    }
}

fn string_func(func: Func, arg: Expr, ty: Type) -> Result<Expr, String> {
    if !matches!(arg.ty(), Type::String | Type::Null) {
        return Err(format!("{func:?} needs a string, not a {}", arg.ty()));
    }
    Ok(Expr::Func {
        func,
        args: vec![arg],
        ty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::parse;

    /// `amount` decimal(2), `qty` int, `status` string, `at` timestamptz,
    /// each also reachable as `orders.<name>`.
    struct Orders;

    impl Scope for Orders {
        fn resolve(&self, parts: &[Name]) -> Result<(u32, Type), String> {
            let name = match parts {
                [name] | [_, name] => name.value.as_str(),
                _ => return Err("too many parts".to_string()),
            };
            match name {
                "amount" => Ok((0, Type::Decimal(2))),
                "qty" => Ok((1, Type::Int)),
                "status" => Ok((2, Type::String)),
                "at" => Ok((3, Type::TimestampTz)),
                other => Err(format!("unknown `{other}`")),
            }
        }
    }

    fn row(text: &str) -> Result<Expr, String> {
        check_row(&parse(text).map_err(|e| e.0)?, &Orders)
    }

    fn metric(text: &str) -> Result<Aggregation, String> {
        check_aggregation(&parse(text).map_err(|e| e.0)?, &Orders)
    }

    #[test]
    fn types_flow_through_operators() {
        assert_eq!(row("amount * qty").map(|e| e.ty()), Ok(Type::Decimal(2)));
        assert_eq!(row("amount * 1.5").map(|e| e.ty()), Ok(Type::Decimal(3)));
        assert_eq!(row("qty / 2").map(|e| e.ty()), Ok(Type::Float));
        assert_eq!(
            row("CASE WHEN qty > 1 THEN amount ELSE 0 END").map(|e| e.ty()),
            Ok(Type::Decimal(2))
        );
        assert_eq!(
            row("DATE_TRUNC('month', at)").map(|e| e.ty()),
            Ok(Type::Date)
        );
        assert_eq!(row("EXTRACT(YEAR FROM at)").map(|e| e.ty()), Ok(Type::Int));
    }

    #[test]
    fn mismatched_operands_are_errors() {
        assert!(row("status + 1").is_err_and(|e| e.contains("string")));
        assert!(row("status = 1").is_err_and(|e| e.contains("compare")));
        assert!(row("qty AND true").is_err_and(|e| e.contains("true/false")));
        assert!(row("nope").is_err_and(|e| e.contains("unknown")));
        assert!(row("SUM(qty)").is_err_and(|e| e.contains("aggregate")));
    }

    #[test]
    fn string_literals_compared_with_timestamps_are_read_as_timestamps() {
        let Ok(Expr::Compare { right, .. }) = row("at >= '2026-01-01'") else {
            panic!("should check");
        };
        assert_eq!(right.ty(), Type::TimestampTz);
        assert!(row("at >= 'yesterday'").is_err_and(|e| e.contains("not a valid timestamptz")));
    }

    #[test]
    fn metrics_split_into_aggregates_and_a_combiner() {
        let Ok(aggregation) = metric("SUM(amount) FILTER (WHERE status = 'paid') / COUNT(*)")
        else {
            panic!("should check");
        };
        assert_eq!(aggregation.calls.len(), 2);
        assert_eq!(aggregation.calls[0].func, AggFunc::Sum);
        assert_eq!(aggregation.calls[0].ty, Type::Decimal(2));
        assert!(aggregation.calls[0].filter.is_some());
        assert_eq!(aggregation.calls[1].func, AggFunc::CountStar);
        assert_eq!(aggregation.combine.slots(), [0, 1]);
        assert_eq!(aggregation.combine.ty(), Type::Float);
    }

    #[test]
    fn aggregates_check_their_argument_types() {
        assert!(metric("SUM(status)").is_err_and(|e| e.contains("needs a number")));
        assert_eq!(
            metric("MAX(status)").map(|a| a.calls[0].ty),
            Ok(Type::String)
        );
        assert_eq!(metric("AVG(qty)").map(|a| a.calls[0].ty), Ok(Type::Float));
        assert!(metric("SUM(SUM(qty))").is_err());
        assert!(metric("SUM(qty) + qty").is_err_and(|e| e.contains("outside an aggregate")));
    }
}
