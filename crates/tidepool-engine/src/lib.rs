//! Query execution: plan a query against the model, scan the snapshot, and
//! return grouped, combined, sorted metric values.
//!
//! [`run`] is the whole surface. Correct before fast: results are checked
//! against DuckDB by `tidepool-oracle`, and every optimisation must keep that
//! green.

mod exec;
mod fast;
mod plan;
pub mod query;

use tidepool_expr::Scalar;
use tidepool_store::Snapshot;

pub use query::{
    Condition, Dim, Order, Query, QueryError, ResultColumn, ResultSet, Role, Selection,
};

/// Answer `query` from `snapshot`.
///
/// # Errors
/// The query refers to something the model doesn't have, groups a metric by
/// a field its table can't reach, or uses something not supported yet.
pub fn run(snapshot: &Snapshot, query: &Query) -> Result<ResultSet, QueryError> {
    answer(snapshot, query, true)
}

/// [`run`] without the columnar fast path: every value is decoded and
/// evaluated the general way. For checking the two agree.
///
/// # Errors
/// As [`run`].
pub fn run_general(snapshot: &Snapshot, query: &Query) -> Result<ResultSet, QueryError> {
    answer(snapshot, query, false)
}

fn answer(snapshot: &Snapshot, query: &Query, fast: bool) -> Result<ResultSet, QueryError> {
    let model = &snapshot.model;
    let plan = plan::plan(model, query)?;
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
    let mut rows: Vec<Vec<Scalar>> = exec::execute(model, snapshot, &plan, fast)
        .into_iter()
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
