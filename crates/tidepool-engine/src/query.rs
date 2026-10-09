//! What a query asks for, and what it answers.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::Arc;

use tidepool_config::ossie::TimeGrain;
use tidepool_expr::temporal::Part;
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
    /// One of a computed set of values: what a selection elsewhere leaves
    /// of a key, when selections flow both ways (plan §5.4). Looked up by
    /// hash, and printed as a fingerprint (it can hold a million keys).
    Set(Arc<ValueSet>),
}

/// A set of values for [`Condition::Set`].
pub struct ValueSet {
    values: Vec<Scalar>,
    keys: HashSet<crate::exec::Key>,
    fingerprint: u64,
}

impl ValueSet {
    #[must_use]
    pub fn new(values: impl IntoIterator<Item = Scalar>) -> Self {
        use std::hash::{Hash, Hasher};
        let mut keys = HashSet::new();
        let mut kept = Vec::new();
        for value in values {
            if !value.is_null() && keys.insert(crate::exec::key(&value)) {
                kept.push(value);
            }
        }
        let mut sorted: Vec<String> = kept.iter().map(|v| format!("{v:?}")).collect();
        sorted.sort_unstable();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        sorted.hash(&mut hasher);
        Self {
            values: kept,
            keys,
            fingerprint: hasher.finish(),
        }
    }

    #[must_use]
    pub fn contains(&self, value: &Scalar) -> bool {
        self.keys.contains(&crate::exec::key(value))
    }

    #[must_use]
    pub fn values(&self) -> &[Scalar] {
        &self.values
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl fmt::Debug for ValueSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ValueSet({} values, #{:016x})",
            self.values.len(),
            self.fingerprint
        )
    }
}

impl PartialEq for ValueSet {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint && self.keys == other.keys
    }
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
            Self::Set(set) => set.contains(value),
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

/// A window of whole periods relative to now: "the last 7 days", "last
/// month" (plan §5.5). What it selects moves as time passes, so a session
/// keeps it as this and turns it into a [`Condition::Range`] against the
/// clock whenever it runs a query; the engine only ever sees the range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Relative {
    /// The period: year, quarter, month, week, day, hour or minute.
    pub unit: Part,
    /// How many periods, the newest one included.
    pub last: u32,
    /// How many periods before the current one the window ends: 0 includes
    /// the current period, 1 ends with the one before it.
    pub ago: u32,
}

impl Relative {
    /// The range this window selects of a field of type `ty` at `now`
    /// (microseconds since the Unix epoch, UTC).
    ///
    /// # Errors
    /// The field isn't a date or a timestamp, the unit is shorter than a day
    /// for a date, or the window is empty or out of range.
    pub fn condition(&self, now: i64, ty: Type) -> Result<Condition, QueryError> {
        const MICROS_PER_DAY: i64 = 86_400_000_000;
        let (from, to) = tidepool_expr::temporal::window(self.unit, self.last, self.ago, now)
            .ok_or_else(|| QueryError(format!("{self:?} isn't a window of time")))?;
        let bounds = |bound: fn(i64) -> Scalar| (Some(bound(from)), Some(bound(to)));
        let (from, to) = match ty {
            Type::Timestamp => bounds(Scalar::Timestamp),
            Type::TimestampTz => bounds(Scalar::TimestampTz),
            Type::Date if self.unit.is_calendar() => {
                // calendar periods start at midnight: whole days
                let day = |micros: i64| {
                    i32::try_from(micros.div_euclid(MICROS_PER_DAY))
                        .map_or(Scalar::Null, Scalar::Date)
                };
                (Some(day(from)), Some(day(to)))
            }
            Type::Date => {
                return Err(QueryError(format!(
                    "a date has no {:?}s: use days or longer",
                    self.unit
                )));
            }
            _ => {
                return Err(QueryError(
                    "only dates and times have a relative window".into(),
                ));
            }
        };
        Ok(Condition::Range { from, to })
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
