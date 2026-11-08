//! Executing a plan against a snapshot.
//!
//! Per anchor table, every segment is scanned in parallel into a *partial*:
//! group key → aggregate states. Partials merge (every state is mergeable),
//! the merged states are finalized into aggregate values, combined into
//! metrics, and the anchors' results are joined on the dimension values.
//! Partials per segment are the unit M4 will cache: a sealed segment's
//! partial never changes until its delete bitmap does.

use std::collections::HashMap;
use std::sync::Arc;

use rayon::prelude::*;
use tidepool_expr::eval::{Row, eval};
use tidepool_expr::ir::{AggFunc, Func};
use tidepool_expr::temporal::Part;
use tidepool_expr::{Expr, Scalar, Type};
use tidepool_model::ir::{Model, TableId};
use tidepool_store::column::Raw;
use tidepool_store::store::{SegmentView, Snapshot, TableSnapshot};
use tidepool_store::value::Value;

use crate::plan::{Access, AnchorPlan, Plan, Target};

/// A row of a table: segment and offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RowRef {
    segment: u32,
    offset: u32,
}

/// The live row holding each primary-key code, for following relationships
/// into a table.
pub(crate) struct KeyIndex(Vec<Option<RowRef>>);

impl KeyIndex {
    fn build(model: &Model, snapshot: &Snapshot, table: TableId) -> Self {
        let mut rows = Vec::new();
        let (Some(spec), Some(stored)) = (model.table(table), snapshot.table(table)) else {
            return Self(rows);
        };
        let Some(key) = spec
            .primary_key
            .first()
            .and_then(|id| spec.columns.iter().position(|c| c.id == *id))
        else {
            return Self(rows);
        };
        for (s, segment) in stored.segments.iter().enumerate() {
            for offset in 0..segment.data.rows {
                if segment.deletes.contains(offset) {
                    continue;
                }
                if let Raw::Code(code) = segment.data.columns[key].get(offset as usize) {
                    let code = code as usize;
                    if rows.len() <= code {
                        rows.resize(code + 1, None);
                    }
                    rows[code] = Some(RowRef {
                        segment: u32::try_from(s).unwrap_or(u32::MAX),
                        offset,
                    });
                }
            }
        }
        Self(rows)
    }
}

/// What a scan needs to read values.
struct Reader<'a> {
    snapshot: &'a Snapshot,
    indexes: HashMap<TableId, Arc<KeyIndex>>,
    /// The trees of hierarchies read at a level.
    trees: HashMap<TableId, Arc<crate::hierarchy::Hierarchy>>,
}

/// One stored value as an expression value.
pub(crate) fn scalar(value: Value) -> Scalar {
    match value {
        Value::Null => Scalar::Null,
        Value::Bool(b) => Scalar::Bool(b),
        Value::Int(i) => Scalar::Int(i),
        Value::Float(f) => Scalar::Float(f),
        Value::Decimal { value, scale } => Scalar::Decimal(value, scale),
        Value::String(s) => Scalar::String(s),
        Value::Date(d) => Scalar::Date(d),
        Value::Timestamp(t) => Scalar::Timestamp(t),
        Value::TimestampTz(t) => Scalar::TimestampTz(t),
    }
}

/// Column values of one row, for evaluating a derived field.
struct TableRow<'a> {
    snapshot: &'a Snapshot,
    table: &'a TableSnapshot,
    segment: &'a SegmentView,
    offset: usize,
    /// A column read moved in time (a `time_offset` metric's).
    moved: Option<(usize, tidepool_expr::temporal::Shift)>,
}

impl Row for TableRow<'_> {
    fn get(&self, slot: u32) -> Scalar {
        let index = slot as usize;
        let (Some(layout), Some(column)) = (
            self.table.layouts.get(index),
            self.segment.data.columns.get(index),
        ) else {
            return Scalar::Null;
        };
        let value = scalar(self.snapshot.decode(layout, column.get(self.offset)));
        match self.moved {
            Some((at, by)) if at == index => shifted(by, value),
            _ => value,
        }
    }
}

/// A date or timestamp moved by `shift`; anything else (NULL) as it is.
fn shifted(shift: tidepool_expr::temporal::Shift, value: Scalar) -> Scalar {
    let moved = match value {
        Scalar::Date(days) => shift
            .days(i64::from(days))
            .and_then(|d| i32::try_from(d).ok())
            .map(Scalar::Date),
        Scalar::Timestamp(micros) => shift.micros(micros).map(Scalar::Timestamp),
        Scalar::TimestampTz(micros) => shift.micros(micros).map(Scalar::TimestampTz),
        other => Some(other),
    };
    moved.unwrap_or(Scalar::Null)
}

/// The value of `access` for a row whose related row doesn't exist: as in
/// a SQL `LEFT JOIN`, every column of the missing row is NULL, and a derived
/// field is evaluated over those NULLs (`COALESCE(name, 'n/a')` is 'n/a').
fn missing(access: &Access) -> Scalar {
    match &access.target {
        Target::Column(_) | Target::Ancestor { .. } => Scalar::Null,
        Target::Derived(expr) => eval(expr, &Vec::new()),
    }
}

impl Reader<'_> {
    /// The value of `access` for an anchor row, moved in time if the access
    /// is shifted (a `time_offset` metric's time field).
    fn value(&self, anchor: &TableSnapshot, at: RowRef, access: &Access) -> Scalar {
        let value = self.stored(anchor, at, access);
        match access.shift {
            // a derived field over the moved column moved its input instead
            Some(shift) if matches!(access.target, Target::Column(_)) || shift.column.is_none() => {
                shifted(shift.by, value)
            }
            _ => value,
        }
    }

    fn stored(&self, anchor: &TableSnapshot, at: RowRef, access: &Access) -> Scalar {
        let mut table = anchor;
        let mut row = at;
        for hop in &access.path {
            let segment = &table.segments[row.segment as usize];
            let Raw::Code(code) = segment.data.columns[hop.fk_column].get(row.offset as usize)
            else {
                return missing(access);
            };
            let Some(next) = self
                .indexes
                .get(&hop.to)
                .and_then(|index| index.0.get(code as usize))
                .copied()
                .flatten()
            else {
                return missing(access);
            };
            let Some(next_table) = self.snapshot.table(hop.to) else {
                return missing(access);
            };
            table = next_table;
            row = next;
        }
        let segment = &table.segments[row.segment as usize];
        match &access.target {
            Target::Column(index) => scalar(self.snapshot.decode(
                &table.layouts[*index],
                segment.data.columns[*index].get(row.offset as usize),
            )),
            Target::Ancestor { column, depth } => {
                let Raw::Code(code) = segment.data.columns[*column].get(row.offset as usize) else {
                    return Scalar::Null;
                };
                self.trees
                    .get(&access.field.table)
                    .and_then(|tree| {
                        let node = tree.ancestor(tree.of_code(code)?, *depth)?;
                        Some(tree.nodes()[node].key.clone())
                    })
                    .unwrap_or(Scalar::Null)
            }
            Target::Derived(expr) => eval(
                expr,
                &TableRow {
                    snapshot: self.snapshot,
                    table,
                    segment,
                    offset: row.offset as usize,
                    moved: access.shift.and_then(|s| s.column.map(|c| (c, s.by))),
                },
            ),
        }
    }
}

/// A hashable form of a value, for group keys and DISTINCT.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    Null,
    Bool(bool),
    Int(i64),
    Float(u64),
    Decimal(i128, u8),
    String(String),
    Date(i32),
    Timestamp(i64),
}

pub(crate) fn key(value: &Scalar) -> Key {
    match value {
        Scalar::Null => Key::Null,
        Scalar::Bool(b) => Key::Bool(*b),
        Scalar::Int(i) => Key::Int(*i),
        // -0.0 and 0.0 group together
        Scalar::Float(f) => Key::Float(if *f == 0.0 { 0 } else { f.to_bits() }),
        Scalar::Decimal(v, s) => Key::Decimal(*v, *s),
        Scalar::String(s) => Key::String(s.clone()),
        Scalar::Date(d) => Key::Date(*d),
        Scalar::Timestamp(t) | Scalar::TimestampTz(t) => Key::Timestamp(*t),
    }
}

/// One aggregate's running state.
#[derive(Clone, Debug)]
enum State {
    Sum {
        int: i128,
        float: f64,
        any: bool,
    },
    Count(i64),
    Min(Scalar),
    Max(Scalar),
    Avg {
        sum: f64,
        count: i64,
    },
    /// DISTINCT aggregates collect their values first.
    Distinct(HashMap<Key, Scalar>),
}

fn new_state(call: &tidepool_expr::AggCall) -> State {
    if call.distinct {
        return State::Distinct(HashMap::new());
    }
    match call.func {
        AggFunc::Sum => State::Sum {
            int: 0,
            float: 0.0,
            any: false,
        },
        AggFunc::Count | AggFunc::CountStar => State::Count(0),
        AggFunc::Min => State::Min(Scalar::Null),
        AggFunc::Max => State::Max(Scalar::Null),
        AggFunc::Avg => State::Avg { sum: 0.0, count: 0 },
    }
}

/// The scale sums of this type keep (0 for integers).
fn sum_scale(ty: Type) -> u8 {
    match ty {
        Type::Decimal(scale) => scale,
        _ => 0,
    }
}

fn update(state: &mut State, ty: Type, value: Scalar) {
    if value.is_null() {
        // aggregates skip NULLs; COUNT(*) is fed a non-null marker
        return;
    }
    match state {
        State::Count(n) => *n += 1,
        State::Sum { int, float, any } => {
            *any = true;
            match ty {
                Type::Float => *float += value.as_f64().unwrap_or(0.0),
                _ => *int += value.as_decimal(sum_scale(ty)).unwrap_or(0),
            }
        }
        State::Min(current) => {
            if current.is_null() || value.compare(current) == Some(std::cmp::Ordering::Less) {
                *current = value;
            }
        }
        State::Max(current) => {
            if current.is_null() || value.compare(current) == Some(std::cmp::Ordering::Greater) {
                *current = value;
            }
        }
        State::Avg { sum, count } => {
            *sum += value.as_f64().unwrap_or(0.0);
            *count += 1;
        }
        State::Distinct(values) => {
            values.insert(key(&value), value);
        }
    }
}

fn merge(into: &mut State, from: State) {
    match (into, from) {
        (
            State::Sum { int, float, any },
            State::Sum {
                int: i,
                float: f,
                any: a,
            },
        ) => {
            *int += i;
            *float += f;
            *any |= a;
        }
        (State::Count(n), State::Count(m)) => *n += m,
        (State::Min(a), State::Min(b))
            if (a.is_null() || b.compare(a) == Some(std::cmp::Ordering::Less)) =>
        {
            *a = b;
        }
        (State::Max(a), State::Max(b))
            if (a.is_null() || b.compare(a) == Some(std::cmp::Ordering::Greater)) =>
        {
            *a = b;
        }
        (State::Avg { sum, count }, State::Avg { sum: s, count: c }) => {
            *sum += s;
            *count += c;
        }
        (State::Distinct(a), State::Distinct(b)) => a.extend(b),
        _ => {}
    }
}

#[allow(clippy::cast_precision_loss)]
#[allow(clippy::match_same_arms)]
fn finish(state: State, call: &tidepool_expr::AggCall) -> Scalar {
    let state = match state {
        // DISTINCT: aggregate the distinct values now
        State::Distinct(values) => {
            let mut plain = new_state(&tidepool_expr::AggCall {
                distinct: false,
                ..call.clone()
            });
            for value in values.into_values() {
                update(
                    &mut plain,
                    call.arg.as_ref().map_or(Type::Null, Expr::ty),
                    value,
                );
            }
            plain
        }
        other => other,
    };
    match state {
        State::Sum { any: false, .. } => Scalar::Null,
        State::Sum { float, .. } if call.ty == Type::Float => Scalar::Float(float),
        State::Sum { int, .. } => match call.ty {
            Type::Decimal(scale) => Scalar::Decimal(int, scale),
            _ => i64::try_from(int).map_or(Scalar::Float(int as f64), Scalar::Int),
        },
        State::Count(n) => Scalar::Int(n),
        State::Min(v) | State::Max(v) => v,
        State::Avg { count: 0, .. } => Scalar::Null,
        State::Avg { sum, count } => Scalar::Float(sum / count as f64),
        State::Distinct(_) => Scalar::Null,
    }
}

/// Group key → (dimension values, aggregate states).
type Partial = HashMap<Vec<Key>, (Vec<Scalar>, Vec<State>)>;

fn truncate(part: Part, value: Scalar) -> Scalar {
    eval(
        &Expr::Func {
            func: Func::DateTrunc(part),
            args: vec![Expr::Literal(value, Type::Null)],
            ty: Type::Date,
        },
        &Vec::new(),
    )
}

/// Scan one segment of the anchor into a partial.
fn scan(reader: &Reader, plan: &AnchorPlan, anchor: &TableSnapshot, segment: usize) -> Partial {
    let mut partial: Partial = HashMap::new();
    let view = &anchor.segments[segment];
    let types: Vec<Type> = plan
        .aggs
        .iter()
        .map(|a| a.call.arg.as_ref().map_or(Type::Null, Expr::ty))
        .collect();
    for offset in 0..view.data.rows {
        if view.deletes.contains(offset) {
            continue;
        }
        let at = RowRef {
            segment: u32::try_from(segment).unwrap_or(u32::MAX),
            offset,
        };
        let values: Vec<Scalar> = plan
            .accesses
            .iter()
            .map(|a| reader.value(anchor, at, a))
            .collect();
        let passed: Vec<bool> = plan
            .conditions
            .iter()
            .map(|(access, c)| c.matches(&values[*access]))
            .collect();
        // a row no aggregate counts doesn't make a group either
        if !plan
            .aggs
            .iter()
            .any(|a| a.conditions.iter().all(|c| passed[*c]))
        {
            continue;
        }
        let dims: Vec<Scalar> = plan
            .dims
            .iter()
            .map(|(access, part)| {
                let value = values[*access].clone();
                match part {
                    Some(part) => truncate(*part, value),
                    None => value,
                }
            })
            .collect();
        let group_key: Vec<Key> = dims.iter().map(key).collect();
        let entry = partial
            .entry(group_key)
            .or_insert_with(|| (dims, plan.aggs.iter().map(|a| new_state(&a.call)).collect()));
        for (i, agg) in plan.aggs.iter().enumerate() {
            if !agg.conditions.iter().all(|c| passed[*c]) {
                continue;
            }
            if let Some(filter) = &agg.call.filter
                && eval(filter, &values) != Scalar::Bool(true)
            {
                continue;
            }
            let value = match &agg.call.arg {
                Some(arg) => eval(arg, &values),
                None => Scalar::Bool(true),
            };
            update(&mut entry.1[i], types[i], value);
        }
    }
    partial
}

fn merge_partials(mut a: Partial, b: Partial) -> Partial {
    for (key, (dims, states)) in b {
        match a.get_mut(&key) {
            Some((_, into)) => {
                for (into, from) in into.iter_mut().zip(states) {
                    merge(into, from);
                }
            }
            None => {
                a.insert(key, (dims, states));
            }
        }
    }
    a
}

/// The general way to run one anchor plan: (dimension values, aggregate
/// values) per group.
fn slow(
    reader: &Reader,
    anchor: &AnchorPlan,
    table: &TableSnapshot,
) -> Vec<(Vec<Scalar>, Vec<Scalar>)> {
    let mut partial = (0..table.segments.len())
        .into_par_iter()
        .map(|segment| scan(reader, anchor, table, segment))
        .reduce(HashMap::new, merge_partials);
    // without dimensions there is always exactly one row, as in SQL
    if anchor.dims.is_empty() && partial.is_empty() {
        partial.insert(
            Vec::new(),
            (
                Vec::new(),
                anchor.aggs.iter().map(|a| new_state(&a.call)).collect(),
            ),
        );
    }
    partial
        .into_values()
        .map(|(dims, states)| {
            (
                dims,
                states
                    .into_iter()
                    .zip(&anchor.aggs)
                    .map(|(s, a)| finish(s, &a.call))
                    .collect(),
            )
        })
        .collect()
}

/// A reader for the accesses of `anchors`: key indexes for every table a
/// path leads into, and the trees of hierarchies read at a level.
fn reader<'a, 'p>(
    model: &Model,
    snapshot: &'a Snapshot,
    anchors: impl Iterator<Item = &'p AnchorPlan> + Clone,
    caches: Option<&crate::cache::Caches>,
) -> Reader<'a> {
    let mut indexes = HashMap::new();
    for anchor in anchors.clone() {
        for access in &anchor.accesses {
            for hop in &access.path {
                indexes.entry(hop.to).or_insert_with(|| {
                    let build = || KeyIndex::build(model, snapshot, hop.to);
                    // kept while the table is unchanged: a rows view with a
                    // related column reads it every epoch
                    match (caches, snapshot.tables.get(&hop.to)) {
                        (Some(caches), Some(table)) => caches.key_index(table, build),
                        _ => Arc::new(build()),
                    }
                });
            }
        }
    }
    let trees = anchors
        .flat_map(|a| &a.accesses)
        .filter(|a| matches!(a.target, Target::Ancestor { .. }))
        .filter_map(|a| {
            let columns = crate::hierarchy::columns(model, &a.field)?;
            let tree = crate::hierarchy::get(snapshot, caches, a.field.table, &columns).ok()?;
            Some((a.field.table, tree))
        })
        .collect();
    Reader {
        snapshot,
        indexes,
        trees,
    }
}

/// One anchor plan's rows, read the general way: whether a row passes the
/// plan's conditions, and the values of its accesses.
pub(crate) struct Rows<'a> {
    reader: Reader<'a>,
    plan: &'a AnchorPlan,
    table: &'a TableSnapshot,
}

impl<'a> Rows<'a> {
    pub(crate) fn new(
        model: &Model,
        snapshot: &'a Snapshot,
        plan: &'a AnchorPlan,
        caches: Option<&crate::cache::Caches>,
    ) -> Option<Self> {
        Some(Self {
            reader: reader(model, snapshot, std::iter::once(plan), caches),
            plan,
            table: snapshot.table(plan.table)?,
        })
    }

    /// Whether the live or deleted row at `offset` of `segment` passes
    /// every condition.
    pub(crate) fn passes(&self, segment: usize, offset: u32) -> bool {
        let at = RowRef {
            segment: u32::try_from(segment).unwrap_or(u32::MAX),
            offset,
        };
        self.plan.conditions.iter().all(|(access, condition)| {
            condition.matches(
                &self
                    .reader
                    .value(self.table, at, &self.plan.accesses[*access]),
            )
        })
    }

    /// The values of `accesses` at a row.
    pub(crate) fn values(&self, segment: usize, offset: u32, accesses: &[usize]) -> Vec<Scalar> {
        let at = RowRef {
            segment: u32::try_from(segment).unwrap_or(u32::MAX),
            offset,
        };
        accesses
            .iter()
            .map(|a| self.reader.value(self.table, at, &self.plan.accesses[*a]))
            .collect()
    }
}

/// A total plan's answers: by the values of the dimensions it keeps, each
/// metric's (position, value).
type Answers = HashMap<Vec<Key>, Vec<(usize, Scalar)>>;

/// Run `plan`: rows of dimension values and every planned metric's value
/// (in `plan.metrics` order).
pub(crate) fn execute(
    model: &Model,
    snapshot: &Snapshot,
    plan: &Plan,
    fast: bool,
    caches: Option<&crate::cache::Caches>,
) -> Vec<(Vec<Scalar>, Vec<Scalar>)> {
    let prepared: Vec<_> = plan
        .anchors
        .iter()
        .map(|anchor| {
            if fast {
                crate::fast::prepare(model, snapshot, anchor, caches)
            } else {
                None
            }
        })
        .collect();
    // key indexes for every table some path of the general path leads into
    let reader = reader(
        model,
        snapshot,
        plan.anchors
            .iter()
            .zip(&prepared)
            .filter(|(_, p)| p.is_none())
            .map(|(a, _)| a),
        caches,
    );

    let mut rows: HashMap<Vec<Key>, (Vec<Scalar>, Vec<Scalar>)> = HashMap::new();
    let width = plan.metrics.len();
    // `total` metrics' answers, by the dimensions they keep: they make no
    // rows of their own, and are looked up by each row's
    let mut totals: Vec<(&[usize], Answers)> = Vec::new();
    for (anchor, prepared) in plan.anchors.iter().zip(prepared) {
        let Some(table) = snapshot.table(anchor.table) else {
            continue;
        };
        let groups = match prepared {
            Some(mut prepared) => match caches {
                // a kept answer for this anchor is brought up to date
                Some(caches) => {
                    let key = format!("{:p} {anchor:?}", Arc::as_ptr(&snapshot.model));
                    let mut kept = caches.take_anchor(&key);
                    let groups = prepared.run(table, snapshot.epoch, Some(&mut kept));
                    if let Some(kept) = kept {
                        caches.keep_anchor(key, kept);
                    }
                    groups
                }
                None => prepared.run(table, snapshot.epoch, None),
            },
            None => slow(&reader, anchor, table),
        };
        if anchor.keep.len() < plan.dim_types.len() {
            let mut answers = HashMap::new();
            for (dims, finished) in groups {
                let values: Vec<(usize, Scalar)> = anchor
                    .metrics
                    .iter()
                    .filter_map(|metric| {
                        let position = plan.metrics.iter().position(|m| *m == metric.id)?;
                        let inputs: Vec<Scalar> = finished[metric.aggs.clone()].to_vec();
                        Some((position, eval(&metric.combine, &inputs)))
                    })
                    .collect();
                answers.insert(dims.iter().map(key).collect::<Vec<Key>>(), values);
            }
            totals.push((&anchor.keep, answers));
            continue;
        }
        for (dims, finished) in groups {
            let group: Vec<Key> = dims.iter().map(key).collect();
            let row = rows
                .entry(group)
                .or_insert_with(|| (dims, vec![Scalar::Null; width]));
            for metric in &anchor.metrics {
                let inputs: Vec<Scalar> = finished[metric.aggs.clone()].to_vec();
                if let Some(position) = plan.metrics.iter().position(|m| *m == metric.id) {
                    row.1[position] = eval(&metric.combine, &inputs);
                }
            }
        }
    }
    let mut rows: Vec<(Vec<Scalar>, Vec<Scalar>)> = rows.into_values().collect();
    for (dims, metrics) in &mut rows {
        for (keep, answers) in &totals {
            let projected: Vec<Key> = keep.iter().map(|i| key(&dims[*i])).collect();
            if let Some(values) = answers.get(&projected) {
                for (position, value) in values {
                    metrics[*position] = value.clone();
                }
            }
        }
        for (id, combine) in &plan.derived {
            let value = eval(combine, metrics);
            if let Some(position) = plan.metrics.iter().position(|m| m == id) {
                metrics[position] = value;
            }
        }
    }
    rows
}
