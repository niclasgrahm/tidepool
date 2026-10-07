//! The columnar fast path.
//!
//! The general executor (`exec`) decodes every value of every row into a
//! [`Scalar`]: always correct, never fast. This path handles the common shape
//! of a dashboard query with integers instead:
//!
//! - **values** are dictionary codes, 64-bit integers or floats, read from
//!   column buffers decoded once per segment;
//! - **relationships** are pre-resolved into arrays indexed by the anchor's
//!   foreign-key code (`code → target code / value`), so following one costs
//!   a single array read per row;
//! - **conditions and filters** on dictionary columns are evaluated once per
//!   distinct value (`code → pass`), then looked up per row;
//! - **groups** get dense ids when every dimension is a dictionary code and
//!   the combinations are few, and a small hash map otherwise.
//!
//! [`prepare`] decides whether a plan qualifies; anything it can't do (derived
//! fields, computed arguments, `SUM(DISTINCT …)`, plain-string columns) falls
//! back to the general path. The oracle runs with the fast path both on and
//! off, so both stay checked against DuckDB.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;
use roaring::RoaringBitmap;
use tidepool_config::types::ColumnType;
use tidepool_expr::eval::{Row, eval};
use tidepool_expr::ir::{AggFunc, Expr};
use tidepool_expr::temporal::{Part, date_from_days, days_from_date, trunc_date};
use tidepool_expr::{Scalar, Type};
use tidepool_model::ir::{Model, TableId};
use tidepool_store::column::ColumnData;
use tidepool_store::dict::{DictSnapshot, DictValue};
use tidepool_store::store::{ColumnLayout, SegmentView, Snapshot, TableSnapshot};
use tidepool_store::value::{Storage, Value};

use crate::cache::{Caches, JoinColumn, Narrow};
use crate::exec::scalar;
use crate::plan::{AnchorPlan, Target};
use crate::query::Condition;

const MAX_DIMS: usize = 4;
/// Largest number of group combinations given dense ids.
const DENSE_LIMIT: usize = 1 << 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Codes,
    Ints,
    Floats,
}

/// How to read one access per anchor row.
struct Read<'s> {
    /// The anchor column read per row (the value itself, or the first
    /// foreign key of a path).
    column: usize,
    kind: Kind,
    /// Values reached through relationships, indexed by the anchor's
    /// foreign-key code (`column`).
    gathered: Option<Arc<Map>>,
    /// With caches, the gathered codes materialized per anchor segment.
    joined: Option<Vec<Arc<JoinColumn>>>,
    /// The column the value finally comes from.
    layout: ColumnLayout,
    dict: Option<&'s DictSnapshot>,
}

fn kind_of(layout: &ColumnLayout) -> Option<Kind> {
    match (layout.dict.is_some(), layout.storage) {
        (true, _) => Some(Kind::Codes),
        (false, Storage::I64) => Some(Kind::Ints),
        (false, Storage::F64) => Some(Kind::Floats),
        _ => None,
    }
}

/// Everything needed to run one anchor plan the fast way.
pub(crate) struct Prepared<'s> {
    reads: Vec<Read<'s>>,
    /// (read, truncation) per dimension.
    dims: Vec<(usize, Option<Part>)>,
    /// Per condition: its read and its test.
    conditions: Vec<(usize, Test)>,
    aggs: Vec<Agg>,
    /// Dense group ids: the code space of each dimension.
    dense: Option<Vec<usize>>,
}

/// A condition or filter, ready for per-row use.
enum Test {
    /// Indexed by final dictionary code.
    Codes(Vec<bool>),
    /// Raw integers: an IN list or a half-open range.
    IntIn(HashSet<i64>),
    IntRange(Option<i64>, Option<i64>),
    Floats(Condition),
}

struct Agg {
    func: AggFunc,
    distinct: bool,
    arg: Option<usize>,
    /// FILTER over one dictionary-coded read: (read, pass by final code).
    filter: Option<(usize, Vec<bool>)>,
    ty: Type,
}

/// `value` of `column_type` as a raw stored integer, rounded up: so
/// `raw >= ceil(v)` ⇔ `value >= v` and `raw < ceil(v)` ⇔ `value < v`.
/// Also says whether the conversion was exact (for IN lists).
fn raw_ceil(value: &Scalar, column_type: ColumnType) -> Option<(i64, bool)> {
    let scale = match column_type {
        ColumnType::Decimal { scale, .. } => scale,
        ColumnType::Int32 | ColumnType::Int64 => 0,
        ColumnType::Date => {
            return match value {
                Scalar::Date(d) => Some((i64::from(*d), true)),
                _ => None,
            };
        }
        ColumnType::Timestamp | ColumnType::TimestampTz => {
            return match value {
                Scalar::Timestamp(m) | Scalar::TimestampTz(m) => Some((*m, true)),
                _ => None,
            };
        }
        _ => return None,
    };
    match value {
        Scalar::Int(_) | Scalar::Decimal(..) => {
            let (v, s) = match value {
                Scalar::Int(i) => (i128::from(*i), 0u8),
                Scalar::Decimal(v, s) => (*v, *s),
                _ => return None,
            };
            let (raw, exact) = if s <= scale {
                (
                    v.checked_mul(tidepool_expr::scalar::pow10(scale - s))?,
                    true,
                )
            } else {
                let divisor = tidepool_expr::scalar::pow10(s - scale);
                let (q, r) = (v.div_euclid(divisor), v.rem_euclid(divisor));
                (if r > 0 { q + 1 } else { q }, r == 0)
            };
            Some((i64::try_from(raw).ok()?, exact))
        }
        Scalar::Float(f) => {
            let scaled = f * 10f64.powi(i32::from(scale));
            #[allow(clippy::cast_possible_truncation)]
            let ceil = scaled.ceil() as i64;
            #[allow(clippy::cast_precision_loss)]
            let exact = (ceil as f64 - scaled).abs() < f64::EPSILON;
            scaled.is_finite().then_some((ceil, exact))
        }
        _ => None,
    }
}

/// The value of every code of a dictionary, decoded.
fn dict_value(layout: &ColumnLayout, dict: Option<&DictSnapshot>, code: u32) -> Scalar {
    let Some(dict) = dict else {
        return Scalar::Null;
    };
    match dict.get(code) {
        DictValue::Null => Scalar::Null,
        DictValue::Str(s) => Scalar::String(s.to_string()),
        DictValue::Int(v) => match layout.column_type {
            ColumnType::Float64 => {
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                let bits = v as u64;
                Scalar::Float(f64::from_bits(bits))
            }
            ColumnType::Decimal { scale, .. } => Scalar::Decimal(v, scale),
            t => scalar(Value::from_i64(t, i64::try_from(v).unwrap_or(i64::MAX))),
        },
    }
}

/// One value per code of `read`'s final dictionary: for building tests.
fn code_space(read: &Read) -> usize {
    read.dict.map_or(1, |d| d.len() as usize + 1)
}

/// Can this anchor plan run the fast way? Prepare it if so.
#[allow(clippy::too_many_lines, clippy::match_same_arms)]
pub(crate) fn prepare<'s>(
    model: &Model,
    snapshot: &'s Snapshot,
    plan: &AnchorPlan,
    caches: Option<&Caches>,
) -> Option<Prepared<'s>> {
    if plan.dims.len() > MAX_DIMS {
        return None;
    }
    let mut reads = Vec::new();
    for access in &plan.accesses {
        let Target::Column(target_column) = access.target else {
            return None;
        };
        let target_table = access.path.last().map_or(plan.table, |h| h.to);
        let target = snapshot.table(target_table)?;
        let layout = target.layouts.get(target_column)?.clone();
        let kind = kind_of(&layout)?;
        let dict = layout.dict.and_then(|d| snapshot.dicts.get(&d));
        let (column, gathered) = match access.path.first() {
            None => (target_column, None),
            Some(first) => {
                // the target's values by its key, then back along the path:
                // each table's values by its own key
                let build = || {
                    let mut values =
                        scatter(model, snapshot, target_table, target_column, kind, None)?;
                    for (i, hop) in access.path.iter().enumerate().skip(1).rev() {
                        values = scatter(
                            model,
                            snapshot,
                            access.path[i - 1].to,
                            hop.fk_column,
                            kind,
                            Some(&values),
                        )?;
                    }
                    Some(Map::narrow(values))
                };
                let map = match caches {
                    Some(caches) => {
                        let tables = access
                            .path
                            .iter()
                            .map(|hop| snapshot.tables.get(&hop.to))
                            .collect::<Option<Vec<_>>>()?;
                        let hops = access.path.iter().map(|h| h.fk_column).collect();
                        caches.map(&tables, hops, target_column, build)?
                    }
                    None => Arc::new(build()?),
                };
                (first.fk_column, Some(map))
            }
        };
        // related codes are worth materializing per anchor row
        let joined = match (caches, &gathered) {
            (Some(caches), Some(map)) if map.is_codes() => {
                let anchor = snapshot.table(plan.table)?;
                Some(
                    anchor
                        .segments
                        .iter()
                        .map(|s| caches.join(&s.data, map))
                        .collect(),
                )
            }
            _ => None,
        };
        reads.push(Read {
            column,
            kind,
            gathered,
            joined,
            layout,
            dict,
        });
    }

    let conditions = plan
        .conditions
        .iter()
        .map(|(access, condition)| {
            let read = &reads[*access];
            let test = match read.kind {
                Kind::Codes => Test::Codes(
                    (0..code_space(read))
                        .map(|c| {
                            condition.matches(&dict_value(
                                &read.layout,
                                read.dict,
                                u32::try_from(c).unwrap_or(0),
                            ))
                        })
                        .collect(),
                ),
                Kind::Ints => match condition {
                    Condition::In(values) => Test::IntIn(
                        values
                            .iter()
                            .map(|v| raw_ceil(v, read.layout.column_type))
                            .collect::<Option<Vec<_>>>()?
                            .into_iter()
                            .filter(|(_, exact)| *exact)
                            .map(|(raw, _)| raw)
                            .collect(),
                    ),
                    Condition::Range { from, to } => {
                        let bound = |b: &Option<Scalar>| match b {
                            None => Some(None),
                            Some(v) => {
                                raw_ceil(v, read.layout.column_type).map(|(raw, _)| Some(raw))
                            }
                        };
                        Test::IntRange(bound(from)?, bound(to)?)
                    }
                },
                Kind::Floats => Test::Floats(condition.clone()),
            };
            Some((*access, test))
        })
        .collect::<Option<Vec<_>>>()?;

    let mut aggs = Vec::new();
    for agg in &plan.aggs {
        // every aggregate sees every condition (the planner splits anchors
        // by selection set, so this is the norm)
        if agg.conditions.len() != plan.conditions.len() {
            return None;
        }
        let call = &agg.call;
        let arg = match &call.arg {
            None => None,
            Some(Expr::Column { slot, .. }) => Some(*slot as usize),
            Some(_) => return None,
        };
        // what each function can do the fast way
        let arg_kind = arg.map(|a| reads[a].kind);
        let ok = match (call.func, call.distinct, arg_kind) {
            (AggFunc::CountStar, _, _) => true,
            (AggFunc::Count, _, Some(_)) => true,
            (AggFunc::Sum, false, Some(Kind::Ints)) => {
                let scale = match reads[arg.unwrap_or(0)].layout.column_type {
                    ColumnType::Decimal { scale, .. } => Some(scale),
                    ColumnType::Int32 | ColumnType::Int64 => None,
                    _ => return None,
                };
                match (call.ty, scale) {
                    (Type::Decimal(s), Some(scale)) => s == scale,
                    (Type::Int, None) => true,
                    _ => false,
                }
            }
            (AggFunc::Sum, false, Some(Kind::Floats)) => call.ty == Type::Float,
            (AggFunc::Avg, false, Some(Kind::Ints | Kind::Floats)) => true,
            (AggFunc::Min | AggFunc::Max, _, Some(_)) => true,
            _ => false,
        };
        if !ok {
            return None;
        }
        let filter = match &call.filter {
            None => None,
            Some(filter) => {
                let slots = filter.slots();
                let [slot] = slots.as_slice() else {
                    return None;
                };
                let read = &reads[*slot as usize];
                if read.kind != Kind::Codes {
                    return None;
                }
                let table = (0..code_space(read))
                    .map(|c| {
                        let value =
                            dict_value(&read.layout, read.dict, u32::try_from(c).unwrap_or(0));
                        eval(filter, &OneSlot { slot: *slot, value }) == Scalar::Bool(true)
                    })
                    .collect();
                Some((*slot as usize, table))
            }
        };
        aggs.push(Agg {
            func: call.func,
            distinct: call.distinct,
            arg,
            filter,
            ty: call.ty,
        });
    }

    let dims: Vec<(usize, Option<Part>)> = plan.dims.clone();
    for (access, part) in &dims {
        let read = &reads[*access];
        let temporal = matches!(
            read.layout.column_type,
            ColumnType::Date | ColumnType::Timestamp | ColumnType::TimestampTz
        );
        if part.is_some() && !temporal {
            return None;
        }
        if read.kind == Kind::Floats && part.is_some() {
            return None;
        }
    }
    let dense = dims
        .iter()
        .map(|(access, part)| {
            (reads[*access].kind == Kind::Codes && part.is_none())
                .then(|| code_space(&reads[*access]))
        })
        .collect::<Option<Vec<usize>>>()
        .filter(|spaces| {
            spaces
                .iter()
                .try_fold(1usize, |acc, s| acc.checked_mul(*s))
                .is_some_and(|n| n <= DENSE_LIMIT)
        });

    Some(Prepared {
        reads,
        dims,
        conditions,
        aggs,
        dense,
    })
}

/// A row with one slot set, for evaluating a filter per dictionary value.
struct OneSlot {
    slot: u32,
    value: Scalar,
}

impl Row for OneSlot {
    fn get(&self, slot: u32) -> Scalar {
        if slot == self.slot {
            self.value.clone()
        } else {
            Scalar::Null
        }
    }
}

/// Decode `rows` of a column into `out` (of the column's kind), reusing
/// its buffers.
fn decode_into(column: &ColumnData, rows: Range<usize>, out: &mut Values) {
    match out {
        Values::Codes(v) => column.decode_codes(rows, v),
        Values::Ints(v, n) => column.decode_i64(rows, v, n),
        Values::Floats(v, n) => column.decode_f64(rows, v, n),
    }
}

/// Decode `rows` of a column as `kind`.
fn decode(column: &ColumnData, kind: Kind, rows: Range<usize>) -> Values {
    let mut out = Values::empty(kind);
    decode_into(column, rows, &mut out);
    out
}

/// Values by a referenced key, for following a relationship. Codes are
/// stored as narrow as they fit: these maps are read at random, so the
/// smaller they are, the more of them stays in cache.
pub(crate) enum Map {
    Codes8(Vec<u8>),
    Codes16(Vec<u16>),
    Values(Values),
}

impl Map {
    fn is_codes(&self) -> bool {
        matches!(
            self,
            Self::Codes8(_) | Self::Codes16(_) | Self::Values(Values::Codes(_))
        )
    }

    /// One morsel of a join column: `rows` of the foreign key, followed.
    fn narrow_rows(&self, column: &ColumnData, rows: Range<usize>) -> Narrow {
        match self {
            Self::Codes8(m) => {
                let mut out = Vec::new();
                column.map_codes(rows, m, 0u8, &mut out);
                Narrow::U8(out.into())
            }
            Self::Codes16(m) => {
                let mut out = Vec::new();
                column.map_codes(rows, m, 0u16, &mut out);
                Narrow::U16(out.into())
            }
            Self::Values(Values::Codes(m)) => {
                let mut out = Vec::new();
                column.map_codes(rows, m, 0u32, &mut out);
                Narrow::U32(out.into())
            }
            Self::Values(_) => Narrow::U32(Box::new([])),
        }
    }

    fn narrow(values: Values) -> Self {
        match values {
            Values::Codes(codes) => {
                let max = codes.iter().max().copied().unwrap_or(0);
                if let Ok(codes) = codes
                    .iter()
                    .map(|c| u8::try_from(*c))
                    .collect::<Result<Vec<_>, _>>()
                {
                    Self::Codes8(codes)
                } else if u16::try_from(max).is_ok() {
                    Self::Codes16(
                        codes
                            .iter()
                            .map(|c| u16::try_from(*c).unwrap_or(0))
                            .collect(),
                    )
                } else {
                    Self::Values(Values::Codes(codes))
                }
            }
            other => Self::Values(other),
        }
    }
}

/// Decode `rows` of a foreign-key column through `map` into `out`:
/// following the relationship.
fn follow_into(column: &ColumnData, map: &Map, rows: Range<usize>, out: &mut Values) {
    match (map, out) {
        (Map::Codes8(m), Values::Codes(v)) => column.map_codes(rows, m, 0u32, v),
        (Map::Codes16(m), Values::Codes(v)) => column.map_codes(rows, m, 0u32, v),
        (Map::Values(values), out) => through_into(column, values, rows, out),
        _ => {}
    }
}

/// Decode `rows` of a foreign-key column through `map` (values by the
/// referenced key) into `out`.
fn through_into(column: &ColumnData, map: &Values, rows: Range<usize>, out: &mut Values) {
    match (map, out) {
        (Values::Codes(m), Values::Codes(v)) => column.map_codes(rows, m, 0, v),
        (Values::Ints(m, nulls), Values::Ints(v, n)) => {
            column.map_codes(rows.clone(), m, 0, v);
            column.map_codes(rows, nulls, true, n);
        }
        (Values::Floats(m, nulls), Values::Floats(v, n)) => {
            column.map_codes(rows.clone(), m, 0.0, v);
            column.map_codes(rows, nulls, true, n);
        }
        _ => {}
    }
}

/// `column` of `table`'s live rows (followed through `inner` when it is a
/// foreign key), indexed by the row's primary-key code. Missing keys are
/// NULL.
fn scatter(
    model: &Model,
    snapshot: &Snapshot,
    table: TableId,
    column: usize,
    kind: Kind,
    inner: Option<&Values>,
) -> Option<Values> {
    let spec = model.table(table)?;
    let stored = snapshot.table(table)?;
    let key = spec
        .primary_key
        .first()
        .and_then(|id| spec.columns.iter().position(|c| c.id == *id))?;
    let keys = stored
        .layouts
        .get(key)?
        .dict
        .and_then(|d| snapshot.dicts.get(&d))?
        .len() as usize
        + 1;
    let mut out = match kind {
        Kind::Codes => Values::Codes(vec![0; keys]),
        Kind::Ints => Values::Ints(vec![0; keys], vec![true; keys]),
        Kind::Floats => Values::Floats(vec![0.0; keys], vec![true; keys]),
    };
    let mut codes = Vec::new();
    for segment in stored.segments.iter() {
        let rows = 0..segment.data.rows as usize;
        segment.data.columns[key].decode_codes(rows.clone(), &mut codes);
        let values = match inner {
            None => decode(&segment.data.columns[column], kind, rows),
            Some(map) => {
                let mut out = Values::empty(kind);
                through_into(&segment.data.columns[column], map, rows, &mut out);
                out
            }
        };
        let deleted: Vec<u32> = segment.deletes.iter().collect();
        let mut deleted = deleted.into_iter().peekable();
        for (row, code) in codes.iter().enumerate() {
            if deleted.peek().is_some_and(|d| *d as usize == row) {
                deleted.next();
                continue;
            }
            out.set(*code as usize, &values, row);
        }
    }
    Some(out)
}

/// Keep the rows of `sel` that pass `keep`, in order. Branch-free: about
/// half the rows pass a typical selection, which is the worst case for a
/// predicted branch.
fn compact(sel: &mut Vec<u32>, keep: impl Fn(u32) -> bool) {
    let mut kept = 0;
    for i in 0..sel.len() {
        let r = sel[i];
        sel[kept] = r;
        kept += usize::from(keep(r));
    }
    sel.truncate(kept);
}

/// Decode only the rows in `sel` when fewer than one in this many are left.
const SPARSE: usize = 4;

/// A join column's chunk as codes in `out`: every row, or `sel`'s.
fn widen(chunk: &Narrow, sel: Option<&[u32]>, out: &mut Vec<u32>) {
    fn copy<T: Copy + Into<u32>>(codes: &[T], sel: Option<&[u32]>, out: &mut Vec<u32>) {
        match sel {
            None => {
                out.clear();
                out.extend(codes.iter().map(|c| (*c).into()));
            }
            Some(sel) => {
                out.resize(codes.len(), 0);
                for r in sel {
                    out[*r as usize] = codes[*r as usize].into();
                }
            }
        }
    }
    match chunk {
        Narrow::U8(c) => copy(c, sel, out),
        Narrow::U16(c) => copy(c, sel, out),
        Narrow::U32(c) => copy(c, sel, out),
    }
}

/// [`Prepared::load`] for a few rows: `sel`'s, relative to `rows.start`.
/// Other rows of `out` keep whatever they held.
fn sparse_into(
    column: &ColumnData,
    read: &Read,
    rows: Range<usize>,
    sel: &[u32],
    out: &mut Values,
) {
    let (start, len) = (rows.start, rows.len());
    match (read.gathered.as_deref(), out) {
        (None, Values::Codes(o)) => column.gather_codes(start, sel, len, o),
        (None, Values::Ints(v, n)) => column.gather_i64(start, sel, len, v, n),
        (None, Values::Floats(v, n)) => column.gather_f64(start, sel, len, v, n),
        (Some(Map::Codes8(m)), Values::Codes(o)) => {
            o.resize(len, 0);
            column.gather_map_codes(start, sel, m, 0u32, o);
        }
        (Some(Map::Codes16(m)), Values::Codes(o)) => {
            o.resize(len, 0);
            column.gather_map_codes(start, sel, m, 0u32, o);
        }
        (Some(Map::Values(Values::Codes(m))), Values::Codes(o)) => {
            o.resize(len, 0);
            column.gather_map_codes(start, sel, m, 0, o);
        }
        (Some(Map::Values(Values::Ints(m, nulls))), Values::Ints(v, n)) => {
            v.resize(len, 0);
            n.resize(len, true);
            column.gather_map_codes(start, sel, m, 0, v);
            column.gather_map_codes(start, sel, nulls, true, n);
        }
        (Some(Map::Values(Values::Floats(m, nulls))), Values::Floats(v, n)) => {
            v.resize(len, 0.0);
            n.resize(len, true);
            column.gather_map_codes(start, sel, m, 0.0, v);
            column.gather_map_codes(start, sel, nulls, true, n);
        }
        _ => {}
    }
}

/// One value, for the less common paths (non-dense group keys, MIN/MAX).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cell {
    Null,
    Code(u32),
    Int(i64),
    Float(f64),
}

/// One read's values for the rows of a morsel.
pub(crate) enum Values {
    /// 0 is NULL.
    Codes(Vec<u32>),
    Ints(Vec<i64>, Vec<bool>),
    Floats(Vec<f64>, Vec<bool>),
}

impl Values {
    fn empty(kind: Kind) -> Self {
        match kind {
            Kind::Codes => Self::Codes(Vec::new()),
            Kind::Ints => Self::Ints(Vec::new(), Vec::new()),
            Kind::Floats => Self::Floats(Vec::new(), Vec::new()),
        }
    }

    fn cell(&self, row: usize) -> Cell {
        match self {
            Self::Codes(c) => match c[row] {
                0 => Cell::Null,
                code => Cell::Code(code),
            },
            Self::Ints(v, n) => {
                if n[row] {
                    Cell::Null
                } else {
                    Cell::Int(v[row])
                }
            }
            Self::Floats(v, n) => {
                if n[row] {
                    Cell::Null
                } else {
                    Cell::Float(v[row])
                }
            }
        }
    }

    /// `self[at] = from[row]`.
    fn set(&mut self, at: usize, from: &Self, row: usize) {
        match (self, from) {
            (Self::Codes(to), Self::Codes(c)) => to[at] = c[row],
            (Self::Ints(to, to_nulls), Self::Ints(v, n)) => {
                to[at] = v[row];
                to_nulls[at] = n[row];
            }
            (Self::Floats(to, to_nulls), Self::Floats(v, n)) => {
                to[at] = v[row];
                to_nulls[at] = n[row];
            }
            _ => {}
        }
    }

    fn is_null(&self, row: usize) -> bool {
        match self {
            Self::Codes(c) => c[row] == 0,
            Self::Ints(_, n) | Self::Floats(_, n) => n[row],
        }
    }
}

/// Rows scanned per task. Tiny in debug builds, so tests over small data
/// still split segments.
pub(crate) const MORSEL: usize = if cfg!(debug_assertions) { 13 } else { 1 << 14 };

/// A group key: one part per dimension, and which parts are NULL.
type Key = ([i64; MAX_DIMS], u8);

/// A float as a group-key part; -0.0 and 0.0 group together.
fn float_part(f: f64) -> i64 {
    i64::from_ne_bytes(if f == 0.0 { 0.0f64 } else { f }.to_ne_bytes())
}

/// One aggregate's running values, one entry per group.
#[derive(Clone, Debug)]
enum State {
    Count(Vec<i64>),
    SumInt(Vec<i128>, Vec<bool>),
    SumFloat(Vec<f64>, Vec<bool>),
    AvgInt(Vec<i128>, Vec<i64>),
    AvgFloat(Vec<f64>, Vec<i64>),
    Min(Vec<Option<Cell>>),
    Max(Vec<Option<Cell>>),
    DistinctCodes(Vec<RoaringBitmap>),
    DistinctNumbers(Vec<HashSet<i64>>),
}

impl State {
    fn new(agg: &Agg, reads: &[Read]) -> Self {
        let kind = agg.arg.map(|a| reads[a].kind);
        match (agg.func, agg.distinct, kind) {
            (AggFunc::Count, true, Some(Kind::Codes)) => Self::DistinctCodes(Vec::new()),
            (AggFunc::Count, true, _) => Self::DistinctNumbers(Vec::new()),
            (AggFunc::Count | AggFunc::CountStar, ..) => Self::Count(Vec::new()),
            (AggFunc::Sum, _, Some(Kind::Floats)) => Self::SumFloat(Vec::new(), Vec::new()),
            (AggFunc::Sum, ..) => Self::SumInt(Vec::new(), Vec::new()),
            (AggFunc::Avg, _, Some(Kind::Floats)) => Self::AvgFloat(Vec::new(), Vec::new()),
            (AggFunc::Avg, ..) => Self::AvgInt(Vec::new(), Vec::new()),
            (AggFunc::Min, ..) => Self::Min(Vec::new()),
            (AggFunc::Max, ..) => Self::Max(Vec::new()),
        }
    }

    fn grow(&mut self, groups: usize) {
        match self {
            Self::Count(v) => v.resize(groups, 0),
            Self::SumInt(s, a) => {
                s.resize(groups, 0);
                a.resize(groups, false);
            }
            Self::SumFloat(s, a) => {
                s.resize(groups, 0.0);
                a.resize(groups, false);
            }
            Self::AvgInt(s, c) => {
                s.resize(groups, 0);
                c.resize(groups, 0);
            }
            Self::AvgFloat(s, c) => {
                s.resize(groups, 0.0);
                c.resize(groups, 0);
            }
            Self::Min(v) | Self::Max(v) => v.resize(groups, None),
            Self::DistinctCodes(v) => v.resize_with(groups, RoaringBitmap::new),
            Self::DistinctNumbers(v) => v.resize_with(groups, HashSet::new),
        }
    }

    /// Feed the rows `sel` (in groups `groups`) of `values`.
    fn update(
        &mut self,
        sel: &[u32],
        groups: &[u32],
        values: Option<&Values>,
        ranks: Option<&[u32]>,
    ) {
        let rows = sel
            .iter()
            .map(|r| *r as usize)
            .zip(groups.iter().map(|g| *g as usize));
        match (self, values) {
            (Self::Count(counts), None) => {
                for g in groups {
                    counts[*g as usize] += 1;
                }
            }
            (Self::Count(counts), Some(values)) => {
                for (r, g) in rows {
                    counts[g] += i64::from(!values.is_null(r));
                }
            }
            (Self::SumInt(sums, any), Some(Values::Ints(v, n))) => {
                for (r, g) in rows {
                    if !n[r] {
                        sums[g] += i128::from(v[r]);
                        any[g] = true;
                    }
                }
            }
            (Self::SumFloat(sums, any), Some(Values::Floats(v, n))) => {
                for (r, g) in rows {
                    if !n[r] {
                        sums[g] += v[r];
                        any[g] = true;
                    }
                }
            }
            (Self::AvgInt(sums, counts), Some(Values::Ints(v, n))) => {
                for (r, g) in rows {
                    if !n[r] {
                        sums[g] += i128::from(v[r]);
                        counts[g] += 1;
                    }
                }
            }
            (Self::AvgFloat(sums, counts), Some(Values::Floats(v, n))) => {
                for (r, g) in rows {
                    if !n[r] {
                        sums[g] += v[r];
                        counts[g] += 1;
                    }
                }
            }
            (Self::Min(current), Some(values)) => {
                for (r, g) in rows {
                    keep(&mut current[g], values.cell(r), ranks, true);
                }
            }
            (Self::Max(current), Some(values)) => {
                for (r, g) in rows {
                    keep(&mut current[g], values.cell(r), ranks, false);
                }
            }
            (Self::DistinctCodes(sets), Some(Values::Codes(c))) => {
                // sorted (group, code) pairs union into each group's bitmap
                // far faster than one random insert per row
                let mut pairs: Vec<u64> = rows
                    .filter(|(r, _)| c[*r] != 0)
                    .map(|(r, g)| (g as u64) << 32 | u64::from(c[r]))
                    .collect();
                pairs.sort_unstable();
                pairs.dedup();
                for run in pairs.chunk_by(|a, b| a >> 32 == b >> 32) {
                    #[allow(clippy::cast_possible_truncation)]
                    let codes = run.iter().map(|p| *p as u32);
                    if let Ok(found) = RoaringBitmap::from_sorted_iter(codes) {
                        sets[(run[0] >> 32) as usize] |= found;
                    }
                }
            }
            (Self::DistinctNumbers(sets), Some(Values::Ints(v, n))) => {
                for (r, g) in rows {
                    if !n[r] {
                        sets[g].insert(v[r]);
                    }
                }
            }
            (Self::DistinctNumbers(sets), Some(Values::Floats(v, n))) => {
                for (r, g) in rows {
                    if !n[r] {
                        sets[g].insert(float_part(v[r]));
                    }
                }
            }
            _ => {}
        }
    }

    /// Fold group `from` of `other` into group `into`.
    fn merge(&mut self, into: usize, other: &mut Self, from: usize, ranks: Option<&[u32]>) {
        match (self, other) {
            (Self::Count(a), Self::Count(b)) => a[into] += b[from],
            (Self::SumInt(s, a), Self::SumInt(t, b)) => {
                s[into] += t[from];
                a[into] |= b[from];
            }
            (Self::SumFloat(s, a), Self::SumFloat(t, b)) => {
                s[into] += t[from];
                a[into] |= b[from];
            }
            (Self::AvgInt(s, c), Self::AvgInt(t, d)) => {
                s[into] += t[from];
                c[into] += d[from];
            }
            (Self::AvgFloat(s, c), Self::AvgFloat(t, d)) => {
                s[into] += t[from];
                c[into] += d[from];
            }
            (Self::Min(a), Self::Min(b)) => {
                if let Some(cell) = b[from] {
                    keep(&mut a[into], cell, ranks, true);
                }
            }
            (Self::Max(a), Self::Max(b)) => {
                if let Some(cell) = b[from] {
                    keep(&mut a[into], cell, ranks, false);
                }
            }
            (Self::DistinctCodes(a), Self::DistinctCodes(b)) => {
                a[into] |= std::mem::take(&mut b[from]);
            }
            (Self::DistinctNumbers(a), Self::DistinctNumbers(b)) => {
                let taken = std::mem::take(&mut b[from]);
                a[into].extend(taken);
            }
            _ => {}
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn finish(&mut self, group: usize, agg: &Agg, reads: &[Read]) -> Scalar {
        let read = agg.arg.map(|a| &reads[a]);
        let scale = read.map_or(0, |r| match r.layout.column_type {
            ColumnType::Decimal { scale, .. } => scale,
            _ => 0,
        });
        match self {
            Self::Count(v) => Scalar::Int(v[group]),
            Self::SumInt(_, any) | Self::SumFloat(_, any) if !any[group] => Scalar::Null,
            Self::SumInt(s, _) => match agg.ty {
                Type::Decimal(scale) => Scalar::Decimal(s[group], scale),
                _ => i64::try_from(s[group]).map_or(Scalar::Float(s[group] as f64), Scalar::Int),
            },
            Self::SumFloat(s, _) => Scalar::Float(s[group]),
            Self::AvgInt(_, c) | Self::AvgFloat(_, c) if c[group] == 0 => Scalar::Null,
            Self::AvgInt(s, c) => {
                Scalar::Float(s[group] as f64 / 10f64.powi(i32::from(scale)) / c[group] as f64)
            }
            Self::AvgFloat(s, c) => Scalar::Float(s[group] / c[group] as f64),
            Self::Min(v) | Self::Max(v) => match (v[group], read) {
                (Some(cell), Some(read)) => cell_scalar(read, cell),
                _ => Scalar::Null,
            },
            Self::DistinctCodes(v) => {
                Scalar::Int(i64::try_from(v[group].len()).unwrap_or(i64::MAX))
            }
            Self::DistinctNumbers(v) => {
                Scalar::Int(i64::try_from(v[group].len()).unwrap_or(i64::MAX))
            }
        }
    }
}

/// Keep the smaller (`min`) or larger of `current` and `cell`; codes compare
/// by their value's rank.
fn keep(current: &mut Option<Cell>, cell: Cell, ranks: Option<&[u32]>, min: bool) {
    let less = |a: Cell, b: Cell| match (a, b) {
        (Cell::Int(x), Cell::Int(y)) => x < y,
        (Cell::Float(x), Cell::Float(y)) => x < y,
        (Cell::Code(x), Cell::Code(y)) => {
            ranks.is_some_and(|r| r.get(x as usize) < r.get(y as usize))
        }
        _ => false,
    };
    if cell == Cell::Null {
        return;
    }
    let replace = match *current {
        None => true,
        Some(c) if min => less(cell, c),
        Some(c) => less(c, cell),
    };
    if replace {
        *current = Some(cell);
    }
}

/// A cell of `read` as a value.
fn cell_scalar(read: &Read, cell: Cell) -> Scalar {
    match cell {
        Cell::Null => Scalar::Null,
        Cell::Code(code) => dict_value(&read.layout, read.dict, code),
        Cell::Int(v) => scalar(Value::from_i64(read.layout.column_type, v)),
        Cell::Float(f) => Scalar::Float(f),
    }
}

/// Largest shared bitset for a distinct count, in bits (32 MiB).
const BITSET_LIMIT: usize = 1 << 28;

/// `COUNT(DISTINCT code)` for every dense group at once: bit
/// `group * space + code`, set by every scanning task. Cheaper than
/// per-task sets merged afterwards when the code space is moderate.
///
/// The bitset is larger than any cache, and rows hit it at random, so a
/// task doesn't set bits as it scans: it collects their positions into
/// [`Pending`] buckets by range and sets them a bucket at a time, each
/// bucket's slice of the bitset small enough to stay in cache.
struct Bitset {
    space: usize,
    /// Bucket of a bit: `bit >> shift`.
    shift: u32,
    words: Vec<AtomicU64>,
    /// One per bucket: whoever holds it owns that slice of `words`, so
    /// setting bits there needs no atomic read-modify-write.
    locks: Vec<std::sync::Mutex<()>>,
}

/// Bit positions a task has yet to set, by bucket.
#[derive(Default)]
struct Pending {
    buckets: Vec<Vec<u32>>,
    count: usize,
}

/// Buckets of a [`Bitset`], at most.
const BUCKETS: usize = 64;
/// Positions a task collects before setting them.
const PENDING: usize = 1 << 17;

impl Bitset {
    fn new(groups: usize, space: usize) -> Self {
        let bits = groups * space;
        // at least 6: a word (64 bits) always lies in one bucket
        let shift = (usize::BITS - bits.leading_zeros())
            .saturating_sub(BUCKETS.trailing_zeros())
            .max(6);
        Self {
            space,
            shift,
            // zeroed in parallel: tens of megabytes, touched for the first time
            words: (0..bits.div_ceil(64))
                .into_par_iter()
                .map(|_| AtomicU64::new(0))
                .collect(),
            locks: (0..=BUCKETS).map(|_| std::sync::Mutex::new(())).collect(),
        }
    }

    fn insert(&self, rows: &[u32], groups: &[u32], codes: &[u32], pending: &mut Pending) {
        if pending.buckets.is_empty() {
            pending.buckets = (0..=BUCKETS).map(|_| Vec::new()).collect();
        }
        for (r, g) in rows.iter().zip(groups) {
            let code = codes[*r as usize];
            if code == 0 {
                continue;
            }
            // < BITSET_LIMIT, so it fits
            #[allow(clippy::cast_possible_truncation)]
            let bit = (*g as usize * self.space + code as usize) as u32;
            pending.buckets[(bit >> self.shift) as usize].push(bit);
        }
        pending.count += rows.len();
        if pending.count >= PENDING {
            self.flush(pending);
        }
    }

    /// Set every pending bit.
    fn flush(&self, pending: &mut Pending) {
        for (bucket, lock) in pending.buckets.iter_mut().zip(&self.locks) {
            if bucket.is_empty() {
                continue;
            }
            // nothing guarded can be left half-done: a poisoned lock is fine
            let _owned = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for bit in bucket.drain(..) {
                let (word, mask) = (&self.words[bit as usize / 64], 1u64 << (bit % 64));
                // a bucket's words are only written under its lock, and a
                // word never straddles two buckets (`shift` >= 6)
                word.store(word.load(Ordering::Relaxed) | mask, Ordering::Relaxed);
            }
        }
        pending.count = 0;
    }

    fn count(&self, group: usize) -> i64 {
        let (start, end) = (group * self.space, (group + 1) * self.space);
        let mut count = 0u32;
        let mut bit = start;
        while bit < end {
            let (word, offset) = (bit / 64, bit % 64);
            let take = (64 - offset).min(end - bit);
            let mask = if take == 64 {
                u64::MAX
            } else {
                ((1u64 << take) - 1) << offset
            };
            count += (self.words[word].load(Ordering::Relaxed) & mask).count_ones();
            bit += take;
        }
        i64::from(count)
    }
}

/// Groups found so far, and every aggregate's values for them.
struct Partial {
    /// Group id → key (sparse groups only; dense ids are their own key).
    keys: Vec<Key>,
    lookup: hashbrown::HashMap<Key, u32>,
    seen: Vec<bool>,
    states: Vec<State>,
}

/// Days truncated to a calendar part, memoized.
fn truncate(part: Part, days: i64, memo: &mut hashbrown::HashMap<(Part, i64), i64>) -> i64 {
    *memo.entry((part, days)).or_insert_with(|| {
        i32::try_from(days)
            .ok()
            .and_then(date_from_days)
            .map_or(days, |d| i64::from(days_from_date(trunc_date(part, d))))
    })
}

/// Reusable buffers of one scanning task.
#[derive(Default)]
struct Scratch {
    /// Decoded values per read, reused from morsel to morsel.
    buffers: Vec<Values>,
    loaded: Vec<bool>,
    sel: Vec<u32>,
    groups: Vec<u32>,
    filtered_sel: Vec<u32>,
    filtered_groups: Vec<u32>,
    /// Per aggregate: distinct-count bits not set yet.
    pending: Vec<Pending>,
    memo: hashbrown::HashMap<(Part, i64), i64>,
}

impl Prepared<'_> {
    fn empty(&self) -> Partial {
        let groups = self
            .dense
            .as_ref()
            .map_or(0, |spaces| spaces.iter().product());
        let mut states: Vec<State> = self
            .aggs
            .iter()
            .map(|a| State::new(a, &self.reads))
            .collect();
        for state in &mut states {
            state.grow(groups);
        }
        Partial {
            keys: Vec::new(),
            lookup: hashbrown::HashMap::new(),
            seen: vec![false; groups],
            states,
        }
    }

    /// Decode read `read` for `rows` of a segment into `out`: every row, or
    /// only the rows in `sel` (relative to `rows.start`) when they are few.
    /// Other rows then hold arbitrary values, and nothing reads them.
    #[allow(clippy::too_many_arguments)]
    fn load(
        &self,
        view: &SegmentView,
        segment: usize,
        read: usize,
        rows: Range<usize>,
        sel: &[u32],
        out: &mut Values,
    ) {
        let read = &self.reads[read];
        let column = &view.data.columns[read.column];
        let sparse = sel.len() * SPARSE < rows.len();
        if let (Some(joined), Some(map), Values::Codes(out)) =
            (&read.joined, &read.gathered, &mut *out)
            && let Some(chunk) = joined
                .get(segment)
                .and_then(|j| j.chunk(rows.start, || map.narrow_rows(column, rows.clone())))
        {
            return widen(chunk, sparse.then_some(sel), out);
        }
        if sparse {
            return sparse_into(column, read, rows, sel, out);
        }
        match &read.gathered {
            Some(map) => follow_into(column, map, rows, out),
            None => decode_into(column, rows, out),
        }
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn scan(
        &self,
        partial: &mut Partial,
        table: &TableSnapshot,
        segment: usize,
        rows: Range<usize>,
        ranks: &[Option<Vec<u32>>],
        shared: &[Option<Bitset>],
        scratch: &mut Scratch,
    ) {
        let view = &table.segments[segment];
        // the selection: live rows passing every condition. Columns are
        // decoded as needed: a condition's before it filters, the rest after
        // the conditions, only at the rows left when few are.
        let Scratch {
            buffers,
            loaded,
            sel,
            ..
        } = scratch;
        if buffers.len() != self.reads.len() {
            *buffers = self.reads.iter().map(|r| Values::empty(r.kind)).collect();
        }
        loaded.clear();
        loaded.resize(self.reads.len(), false);
        sel.clear();
        let len = u32::try_from(rows.len()).unwrap_or(u32::MAX);
        let start = u32::try_from(rows.start).unwrap_or(u32::MAX);
        if view.deletes.is_empty() {
            sel.extend(0..len);
        } else {
            let mut deleted = view
                .deletes
                .range(start..start + len)
                .map(|d| d - start)
                .peekable();
            for row in 0..len {
                if deleted.peek() == Some(&row) {
                    deleted.next();
                } else {
                    sel.push(row);
                }
            }
        }
        for (read, test) in &self.conditions {
            if !loaded[*read] {
                self.load(view, segment, *read, rows.clone(), sel, &mut buffers[*read]);
                loaded[*read] = true;
            }
            match (test, &buffers[*read]) {
                (Test::Codes(pass), Values::Codes(c)) => {
                    compact(sel, |r| {
                        pass.get(c[r as usize] as usize).copied().unwrap_or(false)
                    });
                }
                (Test::IntIn(set), Values::Ints(v, n)) => {
                    compact(sel, |r| !n[r as usize] && set.contains(&v[r as usize]));
                }
                (Test::IntRange(from, to), Values::Ints(v, n)) => compact(sel, |r| {
                    let (v, n) = (v[r as usize], n[r as usize]);
                    !n & from.is_none_or(|f| v >= f) & to.is_none_or(|t| v < t)
                }),
                (Test::Floats(condition), Values::Floats(v, n)) => {
                    compact(sel, |r| {
                        !n[r as usize] && condition.matches(&Scalar::Float(v[r as usize]))
                    });
                }
                _ => sel.clear(),
            }
        }
        for read in 0..self.reads.len() {
            if !loaded[read] {
                self.load(view, segment, read, rows.clone(), sel, &mut buffers[read]);
            }
        }
        let values: &[Values] = buffers;

        // group ids
        let groups = &mut scratch.groups;
        groups.clear();
        match &self.dense {
            Some(spaces) => {
                groups.resize(sel.len(), 0);
                for ((read, _), space) in self.dims.iter().zip(spaces) {
                    let Values::Codes(codes) = &values[*read] else {
                        continue;
                    };
                    let space = u32::try_from(*space).unwrap_or(u32::MAX);
                    for (g, r) in groups.iter_mut().zip(sel.iter()) {
                        *g = *g * space + codes[*r as usize];
                    }
                }
            }
            None => {
                for r in sel.iter() {
                    let key = self.key(values, *r as usize, &mut scratch.memo);
                    let id = if let Some(id) = partial.lookup.get(&key) {
                        *id
                    } else {
                        let id = u32::try_from(partial.keys.len()).unwrap_or(u32::MAX);
                        partial.lookup.insert(key, id);
                        partial.keys.push(key);
                        partial.seen.push(false);
                        for state in &mut partial.states {
                            state.grow(partial.keys.len());
                        }
                        id
                    };
                    groups.push(id);
                }
            }
        }
        for g in groups.iter() {
            partial.seen[*g as usize] = true;
        }

        if scratch.pending.len() != self.aggs.len() {
            scratch.pending = self.aggs.iter().map(|_| Pending::default()).collect();
        }
        for (a, (((agg, state), ranks), bits)) in self
            .aggs
            .iter()
            .zip(&mut partial.states)
            .zip(ranks)
            .zip(shared)
            .enumerate()
        {
            let arg = agg.arg.map(|a| &values[a]);
            let (rows, groups): (&[u32], &[u32]) = match &agg.filter {
                None => (sel, groups),
                Some((read, pass)) => {
                    let Values::Codes(codes) = &values[*read] else {
                        continue;
                    };
                    // branch-free, as `compact`
                    let (fs, fg) = (&mut scratch.filtered_sel, &mut scratch.filtered_groups);
                    fs.resize(sel.len(), 0);
                    fg.resize(sel.len(), 0);
                    let mut kept = 0;
                    for (r, g) in sel.iter().zip(groups.iter()) {
                        fs[kept] = *r;
                        fg[kept] = *g;
                        kept += usize::from(
                            pass.get(codes[*r as usize] as usize)
                                .copied()
                                .unwrap_or(false),
                        );
                    }
                    fs.truncate(kept);
                    fg.truncate(kept);
                    (&scratch.filtered_sel, &scratch.filtered_groups)
                }
            };
            match (bits, arg) {
                (Some(bits), Some(Values::Codes(codes))) => {
                    bits.insert(rows, groups, codes, &mut scratch.pending[a]);
                }
                _ => state.update(rows, groups, arg, ranks.as_deref()),
            }
        }
    }

    /// The group key of row `row` (sparse groups).
    fn key(
        &self,
        values: &[Values],
        row: usize,
        memo: &mut hashbrown::HashMap<(Part, i64), i64>,
    ) -> Key {
        let mut key: Key = ([0; MAX_DIMS], 0);
        for (i, (read, part)) in self.dims.iter().enumerate() {
            let layout = &self.reads[*read].layout;
            let days = |micros: i64| micros.div_euclid(86_400_000_000);
            let part_value = match (values[*read].cell(row), part) {
                (Cell::Null, _) => None,
                (Cell::Code(c), None) => Some(i64::from(c)),
                // a dictionary-coded date or timestamp: truncate its value
                (Cell::Code(c), Some(part)) => {
                    match dict_value(layout, self.reads[*read].dict, c) {
                        Scalar::Date(d) => Some(truncate(*part, i64::from(d), memo)),
                        Scalar::Timestamp(m) | Scalar::TimestampTz(m) => {
                            Some(truncate(*part, days(m), memo))
                        }
                        _ => None,
                    }
                }
                (Cell::Int(v), None) => Some(v),
                (Cell::Int(v), Some(part)) => {
                    let d = if layout.column_type == ColumnType::Date {
                        v
                    } else {
                        days(v)
                    };
                    Some(truncate(*part, d, memo))
                }
                (Cell::Float(f), _) => Some(float_part(f)),
            };
            match part_value {
                Some(v) => key.0[i] = v,
                None => key.1 |= 1 << i,
            }
        }
        key
    }

    fn merge(&self, mut a: Partial, mut b: Partial, ranks: &[Option<Vec<u32>>]) -> Partial {
        if self.dense.is_some() {
            for (group, seen) in b.seen.iter().enumerate() {
                if *seen {
                    a.seen[group] = true;
                    for ((into, from), ranks) in a.states.iter_mut().zip(&mut b.states).zip(ranks) {
                        into.merge(group, from, group, ranks.as_deref());
                    }
                }
            }
            return a;
        }
        for (group, key) in b.keys.iter().enumerate() {
            let target = if let Some(id) = a.lookup.get(key) {
                *id as usize
            } else {
                let id = a.keys.len();
                a.lookup.insert(*key, u32::try_from(id).unwrap_or(u32::MAX));
                a.keys.push(*key);
                a.seen.push(false);
                for state in &mut a.states {
                    state.grow(a.keys.len());
                }
                id
            };
            a.seen[target] |= b.seen[group];
            for ((into, from), ranks) in a.states.iter_mut().zip(&mut b.states).zip(ranks) {
                into.merge(target, from, group, ranks.as_deref());
            }
        }
        a
    }

    /// The value of dimension `i` for a group key part.
    fn dim_value(&self, i: usize, part: i64, null: bool) -> Scalar {
        if null {
            return Scalar::Null;
        }
        let (read, grain) = &self.dims[i];
        let read = &self.reads[*read];
        match (grain, read.kind) {
            (Some(_), _) => Scalar::Date(i32::try_from(part).unwrap_or(0)),
            (None, Kind::Codes) => {
                dict_value(&read.layout, read.dict, u32::try_from(part).unwrap_or(0))
            }
            (None, Kind::Ints) => scalar(Value::from_i64(read.layout.column_type, part)),
            (None, Kind::Floats) => Scalar::Float(f64::from_ne_bytes(part.to_ne_bytes())),
        }
    }

    /// Shared bitsets for `COUNT(DISTINCT code)` over dense groups, per
    /// aggregate, when they are small enough.
    fn bitsets(&self) -> Vec<Option<Bitset>> {
        self.aggs
            .iter()
            .map(|agg| {
                let groups: usize = self.dense.as_ref()?.iter().product();
                let read = &self.reads[agg.arg?];
                let ok = agg.func == AggFunc::Count && agg.distinct && read.kind == Kind::Codes;
                let space = code_space(read);
                (ok && groups.checked_mul(space)? <= BITSET_LIMIT)
                    .then(|| Bitset::new(groups, space))
            })
            .collect()
    }

    /// Value ranks for MIN/MAX over dictionary codes, per aggregate.
    fn ranks(&self) -> Vec<Option<Vec<u32>>> {
        self.aggs
            .iter()
            .map(|agg| {
                if !matches!(agg.func, AggFunc::Min | AggFunc::Max) {
                    return None;
                }
                let read = &self.reads[agg.arg?];
                if read.kind != Kind::Codes {
                    return None;
                }
                let sorted = read
                    .dict?
                    .sorted_codes(read.layout.column_type == ColumnType::Float64);
                let mut ranks = vec![0u32; sorted.iter().max().map_or(0, |m| *m as usize) + 1];
                for (rank, code) in sorted.iter().enumerate() {
                    ranks[*code as usize] = u32::try_from(rank + 1).unwrap_or(u32::MAX);
                }
                Some(ranks)
            })
            .collect()
    }

    /// Run over every live row: (dimension values, aggregate values) per group.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn run(&self, table: &TableSnapshot) -> Vec<(Vec<Scalar>, Vec<Scalar>)> {
        let ranks = self.ranks();
        let shared = self.bitsets();
        let morsels: Vec<(usize, Range<usize>)> = table
            .segments
            .iter()
            .enumerate()
            .flat_map(|(s, view)| {
                let rows = view.data.rows as usize;
                (0..rows)
                    .step_by(MORSEL)
                    .map(move |start| (s, start..(start + MORSEL).min(rows)))
            })
            .collect();
        let mut partial = morsels
            .into_par_iter()
            .fold(
                || (self.empty(), Scratch::default()),
                |(mut partial, mut scratch), (segment, rows)| {
                    self.scan(
                        &mut partial,
                        table,
                        segment,
                        rows,
                        &ranks,
                        &shared,
                        &mut scratch,
                    );
                    (partial, scratch)
                },
            )
            .map(|(partial, mut scratch)| {
                for (bits, pending) in shared.iter().zip(&mut scratch.pending) {
                    if let Some(bits) = bits {
                        bits.flush(pending);
                    }
                }
                partial
            })
            .reduce(|| self.empty(), |a, b| self.merge(a, b, &ranks));

        // distinct counts per group, counted in parallel
        let groups = partial.seen.len();
        let counts: Vec<Option<Vec<i64>>> = shared
            .iter()
            .map(|bits| {
                bits.as_ref()
                    .map(|bits| (0..groups).into_par_iter().map(|g| bits.count(g)).collect())
            })
            .collect();
        let mut out = Vec::new();
        for group in 0..groups {
            // without dimensions there is always exactly one row, as in SQL
            if !partial.seen[group] && !self.dims.is_empty() {
                continue;
            }
            let dims: Vec<Scalar> = if let Some(spaces) = &self.dense {
                // undo `id = id * space + code`
                let mut id = group;
                let mut parts = vec![0i64; spaces.len()];
                for (i, space) in spaces.iter().enumerate().rev() {
                    parts[i] = i64::try_from(id % space).unwrap_or(0);
                    id /= space;
                }
                parts
                    .iter()
                    .enumerate()
                    .map(|(i, p)| self.dim_value(i, *p, *p == 0))
                    .collect()
            } else {
                let (parts, nulls) = partial.keys[group];
                (0..self.dims.len())
                    .map(|i| self.dim_value(i, parts[i], nulls & (1 << i) != 0))
                    .collect()
            };
            let values = partial
                .states
                .iter_mut()
                .zip(&self.aggs)
                .zip(&counts)
                .map(|((state, agg), counts)| match counts {
                    Some(counts) => Scalar::Int(counts[group]),
                    None => state.finish(group, agg, &self.reads),
                })
                .collect();
            out.push((dims, values));
        }
        if self.dims.is_empty() && out.is_empty() {
            let mut fresh = self.empty();
            for state in &mut fresh.states {
                state.grow(1);
            }
            let values = fresh
                .states
                .iter_mut()
                .zip(&self.aggs)
                .map(|(s, a)| s.finish(0, a, &self.reads))
                .collect();
            out.push((Vec::new(), values));
        }
        out
    }
}
