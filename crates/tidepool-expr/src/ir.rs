//! The typed expression tree that checking produces and evaluation runs.
//!
//! Column references are *slots*: numbers the caller's [`crate::check::Scope`]
//! handed out when it resolved a name. The expression crate never knows what
//! a slot means; the engine maps slots to columns (of a table, of a joined
//! table, or, in a metric's combining expression, to aggregate results).

use crate::scalar::{Arith, Scalar, Type};
use crate::temporal::Part;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Func {
    Lower,
    Upper,
    Length,
    Trim,
    Abs,
    /// `ROUND(x, digits)`; digits are a literal, taken at check time.
    Round(i32),
    Floor,
    Ceil,
    Extract(Part),
    DateTrunc(Part),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Column {
        slot: u32,
        ty: Type,
    },
    Literal(Scalar, Type),
    Neg(Box<Expr>, Type),
    Not(Box<Expr>),
    Arith {
        op: Arith,
        left: Box<Expr>,
        right: Box<Expr>,
        ty: Type,
    },
    Compare {
        op: Cmp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    IsNull {
        arg: Box<Expr>,
        negated: bool,
    },
    InList {
        arg: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Between {
        arg: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    Like {
        arg: Box<Expr>,
        pattern: String,
        negated: bool,
        case_insensitive: bool,
    },
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
        ty: Type,
    },
    Coalesce(Vec<Expr>, Type),
    NullIf(Box<Expr>, Box<Expr>, Type),
    Concat(Box<Expr>, Box<Expr>),
    Func {
        func: Func,
        args: Vec<Expr>,
        ty: Type,
    },
    Cast {
        arg: Box<Expr>,
        to: Type,
    },
}

impl Expr {
    #[must_use]
    pub fn ty(&self) -> Type {
        match self {
            Self::Column { ty, .. }
            | Self::Literal(_, ty)
            | Self::Neg(_, ty)
            | Self::Arith { ty, .. }
            | Self::Case { ty, .. }
            | Self::Coalesce(_, ty)
            | Self::NullIf(_, _, ty)
            | Self::Func { ty, .. } => *ty,
            Self::Cast { to, .. } => *to,
            Self::Not(_)
            | Self::Compare { .. }
            | Self::And(..)
            | Self::Or(..)
            | Self::IsNull { .. }
            | Self::InList { .. }
            | Self::Between { .. }
            | Self::Like { .. } => Type::Bool,
            Self::Concat(..) => Type::String,
        }
    }

    /// Every slot the expression reads, in first-use order, without repeats.
    #[must_use]
    pub fn slots(&self) -> Vec<u32> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if let Self::Column { slot, .. } = e
                && !out.contains(slot)
            {
                out.push(*slot);
            }
        });
        out
    }

    fn walk(&self, visit: &mut dyn FnMut(&Self)) {
        visit(self);
        let mut each = |children: &[&Self]| children.iter().for_each(|c| c.walk(visit));
        match self {
            Self::Column { .. } | Self::Literal(..) => {}
            Self::Neg(a, _)
            | Self::Not(a)
            | Self::IsNull { arg: a, .. }
            | Self::Like { arg: a, .. }
            | Self::Cast { arg: a, .. } => {
                each(&[a]);
            }
            Self::Arith { left, right, .. }
            | Self::Compare { left, right, .. }
            | Self::And(left, right)
            | Self::Or(left, right)
            | Self::NullIf(left, right, _)
            | Self::Concat(left, right) => {
                each(&[left, right]);
            }
            Self::InList { arg, list, .. } => {
                each(&[arg]);
                for e in list {
                    e.walk(visit);
                }
            }
            Self::Between { arg, low, high, .. } => each(&[arg, low, high]),
            Self::Case {
                operand,
                whens,
                otherwise,
                ..
            } => {
                if let Some(operand) = operand {
                    operand.walk(visit);
                }
                for (when, then) in whens {
                    when.walk(visit);
                    then.walk(visit);
                }
                if let Some(otherwise) = otherwise {
                    otherwise.walk(visit);
                }
            }
            Self::Coalesce(args, _) | Self::Func { args, .. } => {
                for e in args {
                    e.walk(visit);
                }
            }
        }
    }
}

impl AggFunc {
    /// As written in SQL.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Sum => "SUM",
            Self::Count | Self::CountStar => "COUNT",
            Self::Min => "MIN",
            Self::Max => "MAX",
            Self::Avg => "AVG",
            Self::Median => "MEDIAN",
            Self::Percentile => "PERCENTILE_CONT",
            Self::StddevSamp => "STDDEV_SAMP",
            Self::StddevPop => "STDDEV_POP",
            Self::VarSamp => "VAR_SAMP",
            Self::VarPop => "VAR_POP",
            Self::ArgMax => "ARG_MAX",
            Self::ArgMin => "ARG_MIN",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggFunc {
    Sum,
    Count,
    /// `COUNT(*)`.
    CountStar,
    Min,
    Max,
    Avg,
    /// The middle value: `PERCENTILE_CONT(0.5)`.
    Median,
    /// `PERCENTILE_CONT(p) WITHIN GROUP (ORDER BY x)`: interpolated between
    /// the two values around rank `p · (n − 1)`.
    Percentile,
    StddevSamp,
    StddevPop,
    VarSamp,
    VarPop,
    /// `ARG_MAX(x, t)`: `x` on the row with the largest `t` (of rows where
    /// neither is NULL; of those with that `t`, the largest `x`).
    ArgMax,
    /// `ARG_MIN(x, t)`: `x` on the row with the smallest `t` (ties: the
    /// smallest `x`).
    ArgMin,
}

impl AggFunc {
    /// Whether its state can take a row back out (deleted rows).
    #[must_use]
    pub fn subtracts(self) -> bool {
        matches!(self, Self::CountStar | Self::Count | Self::Sum | Self::Avg)
    }
}

/// One aggregate call inside a metric.
#[derive(Clone, Debug, PartialEq)]
pub struct AggCall {
    pub func: AggFunc,
    pub distinct: bool,
    /// The row-level argument; `None` for `COUNT(*)`.
    pub arg: Option<Expr>,
    /// `ARG_MAX`/`ARG_MIN`'s second argument: what the row is chosen by.
    pub by: Option<Expr>,
    /// `PERCENTILE_CONT`'s fraction, in millionths (`MEDIAN`: 500 000).
    pub fraction: Option<u32>,
    /// `FILTER (WHERE …)`.
    pub filter: Option<Expr>,
    /// The type of the aggregate's result.
    pub ty: Type,
}

/// A checked base metric: the aggregates it computes, and the expression
/// that combines their results (whose slot `i` is aggregate `i`).
#[derive(Clone, Debug, PartialEq)]
pub struct Aggregation {
    pub calls: Vec<AggCall>,
    pub combine: Expr,
}
