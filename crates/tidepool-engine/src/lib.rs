//! Query execution: plan a query against the model, scan the snapshot, and
//! return grouped, combined, sorted metric values.
//!
//! [`run`] is the whole surface. Correct before fast: results are checked
//! against DuckDB by `tidepool-oracle`, and every optimisation must keep that
//! green.

mod assoc;
mod cache;
mod derived;
mod exec;
mod fast;
pub mod hierarchy;
mod plan;
pub mod query;
mod rows;
mod stats;
mod text;

use std::sync::Arc;

use tidepool_expr::Scalar;
use tidepool_model::ir::FieldRef;
use tidepool_store::Snapshot;

pub use query::{
    Condition, Dim, Order, Query, QueryError, Relative, ResultColumn, ResultSet, Role, Selection,
    ValueSet,
};
pub use rows::{MAX_ROWS, RowsQuery, rows, rows_general};

/// [`relevant`] for a `rows` query.
pub use rows::relevant as rows_relevant;
pub use text::Needle;

/// Answer `query` from `snapshot`.
///
/// # Errors
/// The query refers to something the model doesn't have, groups a metric by
/// a field its table can't reach, or uses something not supported yet.
pub fn run(snapshot: &Snapshot, query: &Query) -> Result<ResultSet, QueryError> {
    answer(snapshot, query, true, None)
}

/// [`run`] without the columnar fast path: every value is decoded and
/// evaluated the general way. For checking the two agree.
///
/// # Errors
/// As [`run`].
pub fn run_general(snapshot: &Snapshot, query: &Query) -> Result<ResultSet, QueryError> {
    answer(snapshot, query, false, None)
}

/// The engine with its caches (plan §6.5): what a server keeps for its
/// lifetime and every query goes through. Answers are always the same as
/// [`run`]'s; only how much is recomputed differs.
pub struct Engine {
    caches: cache::Caches,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new(1 << 30)
    }
}

impl Engine {
    /// An engine whose join columns may hold up to `join_budget` bytes.
    #[must_use]
    pub fn new(join_budget: usize) -> Self {
        Self {
            caches: cache::Caches::new(join_budget),
        }
    }

    /// Answer `query` from `snapshot`, reusing what earlier queries built.
    ///
    /// # Errors
    /// As [`run`].
    pub fn run(&self, snapshot: &Snapshot, query: &Query) -> Result<ResultSet, QueryError> {
        let key = format!("{:p} {query:?}", std::sync::Arc::as_ptr(&snapshot.model));
        if let Some(result) = self.caches.result(snapshot.epoch, &key) {
            return Ok(result);
        }
        self.caches.sweep();
        let result = answer(snapshot, query, true, Some(&self.caches))?;
        self.caches.keep_result(snapshot.epoch, key, result.clone());
        Ok(result)
    }

    /// The newest rows passing a selection (plan §5.7), kept up to date
    /// like any answer.
    ///
    /// # Errors
    /// As [`rows`].
    pub fn rows(&self, snapshot: &Snapshot, query: &RowsQuery) -> Result<ResultSet, QueryError> {
        let key = format!(
            "rows {:p} {query:?}",
            std::sync::Arc::as_ptr(&snapshot.model)
        );
        if let Some(result) = self.caches.result(snapshot.epoch, &key) {
            return Ok(result);
        }
        self.caches.sweep();
        let result = rows::answer(snapshot, query, true, Some(&self.caches))?;
        self.caches.keep_result(snapshot.epoch, key, result.clone());
        Ok(result)
    }

    /// How many rows of `field`'s table have each value of `field`, under
    /// `selection`, kept up to date like any answer: a filter list's
    /// possible values.
    ///
    /// # Errors
    /// The field doesn't exist.
    pub fn values(
        &self,
        snapshot: &Snapshot,
        field: &FieldRef,
        selection: &Selection,
    ) -> Result<Vec<(Scalar, i64)>, QueryError> {
        values_with(snapshot, field, selection, Some(&self.caches))
    }

    /// The tree of a hierarchy field (plan §5.6), kept while its table is
    /// unchanged; `None` if `field` isn't one.
    ///
    /// # Errors
    /// The field's table isn't in the snapshot.
    pub fn hierarchy(
        &self,
        snapshot: &Snapshot,
        field: &FieldRef,
    ) -> Result<Option<Arc<hierarchy::Hierarchy>>, QueryError> {
        let Some(columns) = hierarchy::columns(&snapshot.model, field) else {
            return Ok(None);
        };
        hierarchy::get(snapshot, Some(&self.caches), field.table, &columns).map(Some)
    }

    /// Bytes held by the caches (join columns and kept answers).
    #[must_use]
    pub fn cache_bytes(&self) -> usize {
        self.caches.bytes()
    }

    /// Bytes held by materialized join columns.
    #[must_use]
    pub fn join_bytes(&self) -> usize {
        self.caches.join_bytes()
    }
}

/// [`Engine::values`] without caches.
///
/// # Errors
/// The field doesn't exist.
pub fn values(
    snapshot: &Snapshot,
    field: &FieldRef,
    selection: &Selection,
) -> Result<Vec<(Scalar, i64)>, QueryError> {
    values_with(snapshot, field, selection, None)
}

fn values_with(
    snapshot: &Snapshot,
    field: &FieldRef,
    selection: &Selection,
    caches: Option<&cache::Caches>,
) -> Result<Vec<(Scalar, i64)>, QueryError> {
    let selection = hierarchy::resolve(snapshot, caches, selection)?;
    let selection = assoc::Associations::new(snapshot, caches).expand(field.table, &selection)?;
    raw_values(snapshot, field, &selection, caches)
}

/// [`values_with`] for a selection as given: one-directional, nothing
/// added for selections flowing both ways.
pub(crate) fn raw_values(
    snapshot: &Snapshot,
    field: &FieldRef,
    selection: &Selection,
    caches: Option<&cache::Caches>,
) -> Result<Vec<(Scalar, i64)>, QueryError> {
    let plan = plan::values(&snapshot.model, field, selection)?;
    let mut rows: Vec<(Scalar, i64)> =
        exec::execute(&snapshot.model, snapshot, &plan, true, caches)
            .into_iter()
            .filter_map(|(mut dims, metrics)| match (dims.pop(), metrics.first()) {
                (Some(value), Some(Scalar::Int(n))) if *n > 0 => Some((value, *n)),
                _ => None,
            })
            .collect();
    rows.sort_by(|a, b| a.0.sort_cmp(&b.0));
    Ok(rows)
}

/// The part of `query`'s selection that can change its answer.
///
/// # Errors
/// As [`run`].
pub fn relevant(snapshot: &Snapshot, query: &Query) -> Result<Selection, QueryError> {
    // flowing both ways, anything selected anywhere connected can matter
    if snapshot.model.direction == tidepool_model::ir::Direction::Both {
        return Ok(query.selection.clone());
    }
    plan::relevant(&snapshot.model, query)
}

fn answer(
    snapshot: &Snapshot,
    query: &Query,
    fast: bool,
    caches: Option<&cache::Caches>,
) -> Result<ResultSet, QueryError> {
    let model = &snapshot.model;
    let associations = assoc::Associations::new(snapshot, caches);
    let plan = plan::plan(model, query, &|anchor, visible| {
        // after `ignore_selection`: a metric ignoring a hierarchy must not
        // see the subtrees it selects
        associations.expand(anchor, &hierarchy::resolve(snapshot, caches, visible)?)
    })?;
    let mut columns: Vec<ResultColumn> = query
        .dimensions
        .iter()
        .zip(&plan.dim_types)
        .map(|(dim, ty)| ResultColumn {
            name: match dim.grain {
                Some(grain) => format!(
                    "{}:{}",
                    plan::label(model, &dim.field),
                    format!("{grain:?}").to_lowercase()
                ),
                None => plan::label(model, &dim.field),
            },
            ty: *ty,
            role: Role::Dimension,
        })
        .collect();
    for id in &query.metrics {
        let metric = model
            .metric(*id)
            .ok_or_else(|| QueryError(format!("unknown metric {id}")))?;
        columns.push(ResultColumn {
            name: metric.name.clone(),
            ty: metric.ty,
            role: Role::Metric,
        });
    }

    let positions: Vec<usize> = query
        .metrics
        .iter()
        .filter_map(|id| plan.metrics.iter().position(|m| m == id))
        .collect();
    // without metrics, a group is a combination some row under the
    // selection has
    let counted = query.metrics.is_empty() && plan.metrics == [plan::ROWS];
    let mut rows: Vec<Vec<Scalar>> = exec::execute(model, snapshot, &plan, fast, caches)
        .into_iter()
        .filter(|(_, metrics)| {
            !counted || matches!(metrics.first(), Some(Scalar::Int(n)) if *n > 0)
        })
        .map(|(mut dims, metrics)| {
            dims.extend(positions.iter().map(|p| metrics[*p].clone()));
            dims
        })
        .collect();

    let order: Vec<Order> = if query.order.is_empty() {
        (0..query.dimensions.len())
            .map(|column| Order {
                column,
                desc: false,
            })
            .collect()
    } else {
        query.order.clone()
    };
    rows.sort_by(|a, b| {
        for o in &order {
            let ordering = a[o.column].sort_cmp(&b[o.column]);
            let ordering = if o.desc { ordering.reverse() } else { ordering };
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }
        std::cmp::Ordering::Equal
    });
    if let Some(limit) = query.limit {
        rows.truncate(limit);
    }
    Ok(ResultSet { columns, rows })
}
