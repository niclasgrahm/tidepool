//! Planning: a [`Query`] against a model → one plan per anchor table.
//!
//! Every value a plan reads is an *access*: a path of relationship hops from
//! the anchor table to the table holding the field, and the field itself (a
//! column, or a derived expression over that table's columns). Dimensions,
//! selection conditions and aggregate arguments all refer to accesses by
//! index, so a field used three ways is read once per row.

use std::collections::BTreeSet;
use std::ops::Range;

use tidepool_config::ossie::TimeGrain;
use tidepool_expr::syntax::{self, Name};
use tidepool_expr::temporal::{Part, Shift};
use tidepool_expr::{AggCall, Expr, Scope, Type, check_aggregation, check_row};
use tidepool_model::ir::{
    FieldRef, FieldSource, Hierarchy, IgnoreSelection, MetricBody, MetricId, Model, TableId,
    type_of,
};

use crate::query::{Condition, Query, QueryError, Selection};

/// One step along a relationship, from the many side to the one side.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::too_many_lines)]
pub(crate) struct Hop {
    /// The foreign-key column, as a position in the current table's columns.
    pub fk_column: usize,
    pub to: TableId,
}

#[derive(Clone, Debug)]
pub(crate) enum Target {
    /// A column, by position in its table.
    Column(usize),
    /// An expression whose slots are column positions in its table.
    Derived(Expr),
    /// A hierarchy's key column (by position), read as each node's ancestor
    /// at `depth` (see [`crate::hierarchy`]).
    Ancestor { column: usize, depth: u32 },
}

#[derive(Clone, Debug)]
pub(crate) struct Access {
    pub field: FieldRef,
    /// The tree level a hierarchy field is read at, if any.
    pub level: Option<u32>,
    pub path: Vec<Hop>,
    pub target: Target,
    pub ty: Type,
    /// Reads moved in time, for a `time_offset` metric.
    pub shift: Option<AccessShift>,
}

/// What a `time_offset` moves: one column of one table (so every field
/// computed from it moves too), or a derived time field's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ShiftOn {
    Column(TableId, usize),
    Field(FieldRef),
}

/// How one access is moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AccessShift {
    pub by: Shift,
    /// The column of the access's table that reads moved (a derived
    /// field's slot); `None`: the access's value itself.
    pub column: Option<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct AggPlan {
    pub call: AggCall,
    /// Which of the anchor's conditions apply (a metric that ignores some
    /// selections sees fewer).
    pub conditions: Vec<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct MetricPlan {
    pub id: MetricId,
    /// This metric's aggregates, within the anchor's.
    pub aggs: Range<usize>,
    /// Combines the aggregates; slot `i` is aggregate `aggs.start + i`.
    pub combine: Expr,
}

#[derive(Clone, Debug)]
pub(crate) struct AnchorPlan {
    pub table: TableId,
    /// What every read of which is moved, for `time_offset` metrics: their
    /// rows are aggregated as if that much later.
    pub shift: Option<(ShiftOn, Shift)>,
    pub accesses: Vec<Access>,
    pub conditions: Vec<(usize, Condition)>,
    /// (access, truncation) per query dimension.
    pub dims: Vec<(usize, Option<Part>)>,
    pub aggs: Vec<AggPlan>,
    pub metrics: Vec<MetricPlan>,
}

#[derive(Clone, Debug)]
pub(crate) struct Plan {
    pub anchors: Vec<AnchorPlan>,
    /// Every metric to evaluate, base ones first, each derived one after its
    /// inputs.
    pub metrics: Vec<MetricId>,
    /// Derived metrics: combining expression whose slot `i` is `metrics[i]`.
    pub derived: Vec<(MetricId, Expr)>,
    pub dim_types: Vec<Type>,
}

pub(crate) fn part_of(grain: TimeGrain) -> Part {
    match grain {
        TimeGrain::Day => Part::Day,
        TimeGrain::Week => Part::Week,
        TimeGrain::Month => Part::Month,
        TimeGrain::Quarter => Part::Quarter,
        TimeGrain::Year => Part::Year,
    }
}

fn error(message: impl Into<String>) -> QueryError {
    QueryError(message.into())
}

/// `dataset.field` for messages.
pub(crate) fn label(model: &Model, field: &FieldRef) -> String {
    let dataset = model.dataset(field.table).map_or("?", |d| d.name.as_str());
    format!("{dataset}.{}", field.field)
}

/// The relationship path from `from` to `to` (empty for the same table);
/// `Ok(None)` if there is none.
fn path(model: &Model, from: TableId, to: TableId) -> Result<Option<Vec<Hop>>, QueryError> {
    let mut found: Vec<Vec<Hop>> = Vec::new();
    let mut stack: Vec<(TableId, Vec<Hop>)> = vec![(from, Vec::new())];
    while let Some((table, route)) = stack.pop() {
        if table == to {
            found.push(route);
            continue;
        }
        for relationship in model.relationships.iter().filter(|r| r.from == table) {
            let [fk] = relationship.from_columns.as_slice() else {
                return Err(error(format!(
                    "relationship `{}` joins on several columns, which queries don't support yet",
                    relationship.name
                )));
            };
            let Some(fk_column) = model
                .table(table)
                .and_then(|t| t.columns.iter().position(|c| c.id == *fk))
            else {
                continue;
            };
            let mut next = route.clone();
            next.push(Hop {
                fk_column,
                to: relationship.to,
            });
            stack.push((relationship.to, next));
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(error(
            "several relationship paths lead there; queries over role-playing relationships aren't supported yet",
        )),
    }
}

/// Names in a derived field: columns of its own table, by position.
struct ColumnScope<'a> {
    table: &'a tidepool_model::ir::Table,
}

impl Scope for ColumnScope<'_> {
    fn resolve(&self, parts: &[Name]) -> Result<(u32, Type), String> {
        let column = parts.last().ok_or("empty name")?;
        self.table
            .columns
            .iter()
            .enumerate()
            .find(|(_, c)| column.matches(&c.name))
            .map(|(i, c)| (u32::try_from(i).unwrap_or(u32::MAX), type_of(c.column_type)))
            .ok_or_else(|| format!("unknown column `{column}`"))
    }
}

struct AnchorBuilder<'a> {
    model: &'a Model,
    plan: AnchorPlan,
}

impl AnchorBuilder<'_> {
    /// The access for `field`, adding it if new. `Ok(None)` if the anchor
    /// can't reach the field's table.
    fn access(&mut self, field: &FieldRef) -> Result<Option<usize>, QueryError> {
        self.access_at(field, None)
    }

    /// [`Self::access`], for a hierarchy field read at a tree level.
    fn access_at(
        &mut self,
        field: &FieldRef,
        level: Option<u32>,
    ) -> Result<Option<usize>, QueryError> {
        if let Some(index) = self
            .plan
            .accesses
            .iter()
            .position(|a| a.field == *field && a.level == level)
        {
            return Ok(Some(index));
        }

        let Some(route) = path(self.model, self.plan.table, field.table)? else {
            return Ok(None);
        };
        let table = self
            .model
            .table(field.table)
            .ok_or_else(|| error("unknown table"))?;
        let found = self
            .model
            .dataset(field.table)
            .and_then(|d| d.field(&field.field))
            .ok_or_else(|| error(format!("unknown field `{}`", label(self.model, field))))?;
        let position = |id: &tidepool_model::ir::ColumnId| {
            table
                .columns
                .iter()
                .position(|c| c.id == *id)
                .ok_or_else(|| error("unknown column"))
        };
        let target = match (&found.source, level) {
            (FieldSource::Hierarchy(Hierarchy { key, .. }), Some(depth)) => Target::Ancestor {
                column: position(key)?,
                depth,
            },
            (_, Some(_)) => {
                return Err(error(format!(
                    "`{}` isn't a hierarchy, so it has no levels",
                    label(self.model, field)
                )));
            }
            // a hierarchy's values are its node keys
            (FieldSource::Column(id) | FieldSource::Hierarchy(Hierarchy { key: id, .. }), None) => {
                Target::Column(position(id)?)
            }
            (FieldSource::Derived { expression, .. }, None) => {
                let ast = syntax::parse(expression).map_err(|e| error(e.0))?;
                Target::Derived(check_row(&ast, &ColumnScope { table }).map_err(error)?)
            }
        };
        let shift = match &self.plan.shift {
            Some((ShiftOn::Field(f), by)) if f == field => Some(AccessShift {
                by: *by,
                column: None,
            }),
            Some((ShiftOn::Column(t, c), by)) if *t == field.table => {
                let reads = match &target {
                    Target::Column(position) => position == c,
                    Target::Derived(expr) => expr
                        .slots()
                        .contains(&u32::try_from(*c).unwrap_or(u32::MAX)),
                    Target::Ancestor { .. } => false,
                };
                reads.then_some(AccessShift {
                    by: *by,
                    column: Some(*c),
                })
            }
            _ => None,
        };
        self.plan.accesses.push(Access {
            field: field.clone(),
            level,
            path: route,
            target,
            ty: found.ty,
            shift,
        });
        Ok(Some(self.plan.accesses.len() - 1))
    }
}

/// Names in a base metric: fields, resolved to accesses of the anchor.
struct MetricScope<'a, 'b> {
    builder: std::cell::RefCell<&'a mut AnchorBuilder<'b>>,
}

impl Scope for MetricScope<'_, '_> {
    fn resolve(&self, parts: &[Name]) -> Result<(u32, Type), String> {
        let mut builder = self.builder.borrow_mut();
        let model = builder.model;
        let field = model
            .datasets
            .iter()
            .find_map(|dataset| {
                let name = match parts {
                    [name] => name,
                    [d, name] if d.matches(&dataset.name) => name,
                    _ => return None,
                };
                dataset
                    .fields
                    .iter()
                    .find(|f| name.matches(&f.name))
                    .map(|f| FieldRef {
                        table: dataset.table,
                        field: f.name.clone(),
                    })
            })
            .ok_or("unknown field")?;
        let index = builder.access(&field).map_err(|e| e.0)?.ok_or_else(|| {
            format!(
                "`{}` isn't reachable from the metric's table",
                label(model, &field)
            )
        })?;
        let ty = builder.plan.accesses[index].ty;
        Ok((u32::try_from(index).unwrap_or(u32::MAX), ty))
    }
}

/// The selection a metric anchored on a table sees, given the part of the
/// query's selection it doesn't ignore: that part as it is, or with what
/// selections flowing both ways add (`assoc`).
pub(crate) type SelectionFor<'a> =
    dyn Fn(TableId, &Selection) -> Result<Selection, QueryError> + 'a;

/// Plan `query` against `model`.
#[allow(clippy::too_many_lines)]
pub(crate) fn plan(
    model: &Model,
    query: &Query,
    selection_for: &SelectionFor,
) -> Result<Plan, QueryError> {
    // every metric needed: requested ones and everything derived ones use
    let mut needed: Vec<MetricId> = Vec::new();
    let mut stack: Vec<MetricId> = query.metrics.clone();
    while let Some(id) = stack.pop() {
        if needed.contains(&id) {
            continue;
        }
        let metric = model
            .metric(id)
            .ok_or_else(|| error(format!("unknown metric {id}")))?;
        needed.push(id);
        if let MetricBody::Derived { inputs, .. } = &metric.body {
            stack.extend(inputs);
        }
    }

    let mut anchors: Vec<AnchorPlan> = Vec::new();
    let mut base_order: Vec<MetricId> = Vec::new();
    for id in &needed {
        let Some(metric) = model.metric(*id) else {
            continue;
        };
        let MetricBody::Base {
            expression, anchor, ..
        } = &metric.body
        else {
            continue;
        };
        // the selection this metric sees
        let ignored = |field: &FieldRef| match &metric.ignore_selection {
            IgnoreSelection::None => false,
            IgnoreSelection::All => true,
            IgnoreSelection::Fields(fields) => fields.contains(field),
        };
        // One plan per anchor *and* selection it sees: a metric that ignores
        // a selection groups its own rows, exactly as a separate SQL query
        // over that table would.
        let shift = match &metric.time_offset {
            None => None,
            Some(offset) => Some((shift_on(model, &offset.field)?, shift_of(offset))),
        };
        let visible = Selection(
            query
                .selection
                .0
                .iter()
                .filter(|(f, _)| !ignored(f))
                .map(|(f, c)| (f.clone(), c.clone()))
                .collect(),
        );
        let seen = Query {
            selection: selection_for(*anchor, &visible)?,
            ..Query::default()
        };
        let fresh = anchor_plan(model, *anchor, &seen, &|_| false, shift)?;
        let signature = |plan: &AnchorPlan| -> Vec<String> {
            plan.conditions
                .iter()
                .map(|(a, c)| format!("{:?} {c:?}", plan.accesses[*a].field))
                .collect()
        };
        let index = if let Some(index) = anchors.iter().position(|a| {
            a.table == *anchor && a.shift == fresh.shift && signature(a) == signature(&fresh)
        }) {
            index
        } else {
            anchors.push(fresh);
            anchors.len() - 1
        };
        let mut builder = AnchorBuilder {
            model,
            plan: std::mem::replace(&mut anchors[index], empty(*anchor)),
        };
        let ast = syntax::parse(expression).map_err(|e| error(e.0))?;
        let aggregation = {
            let scope = MetricScope {
                builder: std::cell::RefCell::new(&mut builder),
            };
            check_aggregation(&ast, &scope)
                .map_err(|e| error(format!("metric `{}`: {e}", metric.name)))?
        };
        let conditions: Vec<usize> = (0..builder.plan.conditions.len()).collect();
        let start = builder.plan.aggs.len();
        builder
            .plan
            .aggs
            .extend(aggregation.calls.into_iter().map(|call| AggPlan {
                call,
                conditions: conditions.clone(),
            }));
        let end = builder.plan.aggs.len();
        builder.plan.metrics.push(MetricPlan {
            id: *id,
            aggs: start..end,
            combine: aggregation.combine,
        });
        anchors[index] = builder.plan;
        base_order.push(*id);
    }

    // derived metrics, each after its inputs
    let mut metrics = base_order;
    let mut derived = Vec::new();
    let mut remaining: Vec<MetricId> = needed
        .iter()
        .copied()
        .filter(|id| !metrics.contains(id))
        .collect();
    while !remaining.is_empty() {
        let before = remaining.len();
        let mut i = 0;
        while i < remaining.len() {
            let Some(metric) = model.metric(remaining[i]) else {
                remaining.remove(i);
                continue;
            };
            let MetricBody::Derived { expression, inputs } = &metric.body else {
                remaining.remove(i);
                continue;
            };
            if !inputs.iter().all(|input| metrics.contains(input)) {
                i += 1;
                continue;
            }
            let ast = syntax::parse(expression).map_err(|e| error(e.0))?;
            let scope = DerivedScope {
                model,
                metrics: &metrics,
            };
            let combine = check_row(&ast, &scope)
                .map_err(|e| error(format!("metric `{}`: {e}", metric.name)))?;
            derived.push((metric.id, combine));
            metrics.push(metric.id);
            remaining.remove(i);
        }
        if remaining.len() == before {
            return Err(error("derived metrics depend on each other in a loop"));
        }
    }

    // every dimension must be reachable from every anchor
    let dim_types = query
        .dimensions
        .iter()
        .map(|dim| {
            let field = model
                .dataset(dim.field.table)
                .and_then(|d| d.field(&dim.field.field))
                .ok_or_else(|| error(format!("unknown field `{}`", label(model, &dim.field))))?;
            Ok(if dim.grain.is_some() {
                Type::Date
            } else {
                field.ty
            })
        })
        .collect::<Result<Vec<_>, QueryError>>()?;
    for anchor in &mut anchors {
        let mut builder = AnchorBuilder {
            model,
            plan: std::mem::replace(anchor, empty(TableId(0))),
        };
        for dim in &query.dimensions {
            let access = builder.access_at(&dim.field, dim.level)?.ok_or_else(|| {
                let table = model
                    .dataset(builder.plan.table)
                    .map_or("?", |d| d.name.as_str());
                error(format!(
                    "metrics on `{table}` can't be grouped by `{}`",
                    label(model, &dim.field)
                ))
            })?;
            builder.plan.dims.push((access, dim.grain.map(part_of)));
        }
        *anchor = builder.plan;
    }

    Ok(Plan {
        anchors,
        metrics,
        derived,
        dim_types,
    })
}

/// Stands for "rows" in a [`values`] plan: no model metric has it.
pub(crate) const ROWS: MetricId = MetricId(u32::MAX);

/// The rows of `field`'s table per value of `field`, under `selection`
/// (one-directional, as for metrics on that table): what a filter list
/// shows as possible values.
pub(crate) fn values(
    model: &Model,
    field: &FieldRef,
    selection: &Selection,
) -> Result<Plan, QueryError> {
    let query = Query {
        dimensions: vec![crate::query::Dim::of(field.clone())],
        selection: selection.clone(),
        ..Query::default()
    };
    let mut builder = AnchorBuilder {
        model,
        plan: anchor_plan(model, field.table, &query, &|_| false, None)?,
    };
    let access = builder
        .access(field)?
        .ok_or_else(|| error(format!("unknown field `{}`", label(model, field))))?;
    builder.plan.dims.push((access, None));
    let conditions = (0..builder.plan.conditions.len()).collect();
    builder.plan.aggs.push(AggPlan {
        call: AggCall {
            func: tidepool_expr::ir::AggFunc::CountStar,
            distinct: false,
            arg: None,
            filter: None,
            ty: Type::Int,
        },
        conditions,
    });
    builder.plan.metrics.push(MetricPlan {
        id: ROWS,
        aggs: 0..1,
        combine: Expr::Column {
            slot: 0,
            ty: Type::Int,
        },
    });
    let ty = builder.plan.accesses[access].ty;
    Ok(Plan {
        anchors: vec![builder.plan],
        metrics: vec![ROWS],
        derived: Vec::new(),
        dim_types: vec![ty],
    })
}

/// The part of `query`'s selection that changes its answer: conditions on
/// fields some metric's table reaches and doesn't ignore. Sessions whose
/// selections differ only elsewhere can share one answer.
pub(crate) fn relevant(model: &Model, query: &Query) -> Result<Selection, QueryError> {
    let plan = plan(model, query, &|_, visible| Ok(visible.clone()))?;
    let used: BTreeSet<FieldRef> = plan
        .anchors
        .iter()
        .flat_map(|a| {
            a.conditions
                .iter()
                .map(|(access, _)| a.accesses[*access].field.clone())
        })
        .collect();
    Ok(Selection(
        query
            .selection
            .0
            .iter()
            .filter(|(field, _)| used.contains(*field))
            .map(|(f, c)| (f.clone(), c.clone()))
            .collect(),
    ))
}

/// The column a `time_offset`'s field is (or the field, when derived).
fn shift_on(model: &Model, field: &FieldRef) -> Result<ShiftOn, QueryError> {
    let found = model
        .dataset(field.table)
        .and_then(|d| d.field(&field.field))
        .ok_or_else(|| error(format!("unknown field `{}`", label(model, field))))?;
    Ok(match &found.source {
        FieldSource::Column(id) => ShiftOn::Column(
            field.table,
            model
                .table(field.table)
                .and_then(|t| t.columns.iter().position(|c| c.id == *id))
                .ok_or_else(|| error("unknown column"))?,
        ),
        FieldSource::Derived { .. } | FieldSource::Hierarchy(_) => ShiftOn::Field(field.clone()),
    })
}

/// How a `time_offset` moves its field's values: back by the offset reads
/// as forward, so `-1y` is `+12 months`.
fn shift_of(offset: &tidepool_model::ir::TimeOffset) -> Shift {
    let amount = -offset.amount;
    match offset.unit {
        TimeGrain::Day => Shift {
            months: 0,
            days: amount,
        },
        TimeGrain::Week => Shift {
            months: 0,
            days: amount.saturating_mul(7),
        },
        TimeGrain::Month => Shift {
            months: amount,
            days: 0,
        },
        TimeGrain::Quarter => Shift {
            months: amount.saturating_mul(3),
            days: 0,
        },
        TimeGrain::Year => Shift {
            months: amount.saturating_mul(12),
            days: 0,
        },
    }
}

fn empty(table: TableId) -> AnchorPlan {
    AnchorPlan {
        table,
        shift: None,
        accesses: Vec::new(),
        conditions: Vec::new(),
        dims: Vec::new(),
        aggs: Vec::new(),
        metrics: Vec::new(),
    }
}

/// A fresh plan for `anchor`, with the query's selection conditions on the
/// fields it can reach and doesn't ignore.
fn anchor_plan(
    model: &Model,
    anchor: TableId,
    query: &Query,
    ignored: &dyn Fn(&FieldRef) -> bool,
    shift: Option<(ShiftOn, Shift)>,
) -> Result<AnchorPlan, QueryError> {
    let mut builder = AnchorBuilder {
        model,
        plan: AnchorPlan {
            shift,
            ..empty(anchor)
        },
    };
    let mut seen = BTreeSet::new();
    for (field, condition) in &query.selection.0 {
        if !seen.insert(field.clone()) || ignored(field) {
            continue;
        }
        if let Some(access) = builder.access(field)? {
            builder.plan.conditions.push((access, condition.clone()));
        }
    }
    Ok(builder.plan)
}

/// Names in a derived metric: metrics evaluated so far, by position.
struct DerivedScope<'a> {
    model: &'a Model,
    metrics: &'a [MetricId],
}

impl Scope for DerivedScope<'_> {
    fn resolve(&self, parts: &[Name]) -> Result<(u32, Type), String> {
        let [name] = parts else {
            return Err("a derived metric refers to metrics by name".to_string());
        };
        self.metrics
            .iter()
            .enumerate()
            .find_map(|(i, id)| {
                self.model
                    .metric(*id)
                    .filter(|m| name.matches(&m.name))
                    .map(|m| (u32::try_from(i).unwrap_or(u32::MAX), m.ty))
            })
            .ok_or_else(|| format!("unknown metric `{name}`"))
    }
}
