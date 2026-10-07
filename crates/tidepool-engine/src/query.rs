//! What a query asks for, and what it answers.

use std::collections::BTreeMap;
use std::fmt;

use tidepool_config::ossie::TimeGrain;
use tidepool_expr::{Scalar, Type};
use tidepool_model::ir::{FieldRef, MetricId};

/// A dimension: group by a field, optionally truncated to a time grain.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Dim {
    pub field: FieldRef,
    pub grain: Option<TimeGrain>,
}

/// What a selected field must satisfy.
#[derive(Clone, Debug, PartialEq)]
pub enum Condition {
    /// One of these values.
    In(Vec<Scalar>),
    /// `from <= value < to`; either end may be open.
    Range {
        from: Option<Scalar>,
        to: Option<Scalar>,
    },
}

impl Condition {
    /// Whether `value` satisfies the condition. NULL never does.
    #[must_use]
    pub fn matches(&self, value: &Scalar) -> bool {
        if value.is_null() {
            return false;
        }
        match self {
            Self::In(values) => values
                .iter()
                .any(|v| value.compare(v) == Some(std::cmp::Ordering::Equal)),
            Self::Range { from, to } => {
                from.as_ref().is_none_or(|f| {
                    value
                        .compare(f)
                        .is_some_and(|o| o != std::cmp::Ordering::Less)
                }) && to
                    .as_ref()
                    .is_none_or(|t| value.compare(t) == Some(std::cmp::Ordering::Less))
            }
        }
    }
}

/// The current selection: conditions per field. A metric is filtered by the
/// conditions on fields its anchor table can reach (plan §5.2, one
/// direction: from the one side to the many side).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection(pub BTreeMap<FieldRef, Condition>);

/// A result column to sort by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Order {
    /// Position in the result's columns (dimensions first, then metrics).
    pub column: usize,
    pub desc: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub dimensions: Vec<Dim>,
    pub metrics: Vec<MetricId>,
    pub selection: Selection,
    /// Defaults to the dimensions, ascending.
    pub order: Vec<Order>,
    pub limit: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Dimension,
    Metric,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResultColumn {
    pub name: String,
    pub ty: Type,
    pub role: Role,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResultSet {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Scalar>>,
}

/// Why a query can't run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryError(pub String);

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for QueryError {}
