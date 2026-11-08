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
//! - **text searches** on plain string columns run over the segment's byte
//!   arena a morsel at a time (`crate::text`), their hits cached per segment.
//!
//! [`prepare`] decides whether a plan qualifies; anything it can't do (derived
//! fields, computed arguments, `SUM(DISTINCT …)`, plain-string columns read
//! for anything but a search) falls back to the general path. The oracle runs with the fast path both on and
//! off, so both stay checked against DuckDB.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use rayon::prelude::*;
use roaring::{RoaringBitmap, RoaringTreemap};
use tidepool_config::types::ColumnType;
use tidepool_expr::eval::{Row, eval};
use tidepool_expr::ir::{AggFunc, Expr};
use tidepool_expr::temporal::{
    Part, Shift, date_from_days, days_from_date, trunc_date, trunc_micros,
};
use tidepool_expr::{Scalar, Type};
use tidepool_model::ir::{Model, TableId};
use tidepool_store::column::ColumnData;
use tidepool_store::dict::{DictSnapshot, DictValue};
use tidepool_store::store::{ColumnLayout, SegmentView, Snapshot, TableSnapshot};
use tidepool_store::value::{Storage, Value};

use crate::cache::{Caches, JoinColumn, Narrow, TextHits};
use crate::derived::DerivedDict;
use crate::exec::scalar;
use crate::plan::{AnchorPlan, Target};
use crate::query::Condition;
use crate::text::Needle;

const MAX_DIMS: usize = 4;
/// Largest number of group combinations given dense ids.
const DENSE_LIMIT: usize = 1 << 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Codes,
    Ints,
    Floats,
    /// A plain string column: only searched (`Test::Text`), never decoded.
    Text,
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
    /// The value's type (a derived field's, which its column's isn't).
    ty: Type,
    /// A derived field's dictionary: codes read are base codes, mapped
    /// through it (already done in `gathered` maps).
    derived: Option<Arc<DerivedDict>>,
    /// The column the value finally comes from.
    layout: ColumnLayout,
    dict: Option<&'s DictSnapshot>,
    /// A time read moved by a `time_offset` (dates as days, timestamps as
    /// microseconds), applied as values are loaded.
    shift: Option<Shift>,
}

impl Read<'_> {
    /// The value of a code this read produces.
    fn code_value(&self, code: u32) -> Scalar {
        match &self.derived {
            Some(derived) => derived
                .values
                .as_slice()
                .get(code as usize)
                .cloned()
                .unwrap_or(Scalar::Null),
            None => dict_value(&self.layout, self.dict, code),
        }
    }
}

fn kind_of(layout: &ColumnLayout) -> Option<Kind> {
    match (layout.dict.is_some(), layout.storage) {
        (true, _) => Some(Kind::Codes),
        (false, Storage::I64) => Some(Kind::Ints),
        (false, Storage::F64) => Some(Kind::Floats),
        (false, Storage::Str) => Some(Kind::Text),
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
    /// A search of a plain string column, with its hits per anchor segment
    /// when the engine caches them.
    Text {
        needle: Arc<Needle>,
        hits: Option<Vec<Arc<TextHits>>>,
    },
}

struct Agg {
    func: AggFunc,
    distinct: bool,
    arg: Option<usize>,
    /// FILTER over one dictionary-coded read: (read, pass by final code).
    filter: Option<(usize, Vec<bool>)>,
    ty: Type,
    /// A percentile's fraction, in millionths.
    fraction: Option<u32>,
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
pub(crate) fn dict_value(layout: &ColumnLayout, dict: Option<&DictSnapshot>, code: u32) -> Scalar {
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
    match &read.derived {
        Some(derived) => derived.values.len(),
        None => read.dict.map_or(1, |d| d.len() as usize + 1),
    }
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
        // a column, or a derived field over one dictionary column
        let (target_column, expr) = match &access.target {
            Target::Column(column) | Target::Ancestor { column, .. } => (*column, None),
            Target::Derived(expr) => {
                let mut slots = expr.slots();
                slots.dedup();
                let [slot] = slots.as_slice() else {
                    return None;
                };
                (*slot as usize, Some((expr, *slot)))
            }
        };
        let target_table = access.path.last().map_or(plan.table, |h| h.to);
        let target = snapshot.table(target_table)?;
        let layout = target.layouts.get(target_column)?.clone();
        let kind = kind_of(&layout)?;
        // text is searched where it's stored: the anchor's own column
        if kind == Kind::Text && (!access.path.is_empty() || expr.is_some()) {
            return None;
        }
        let dict = layout.dict.and_then(|d| snapshot.dicts.get(&d));
        let derived = match (expr, &access.target) {
            (None, Target::Ancestor { depth, .. }) => {
                let (Kind::Codes, Some(base)) = (kind, dict) else {
                    return None;
                };
                let columns = crate::hierarchy::columns(model, &access.field)?;
                let tree = crate::hierarchy::get(snapshot, caches, target_table, &columns).ok()?;
                Some(tree.ancestors(*depth, base))
            }
            (None, _) => None,
            (Some((expr, slot)), _) => {
                let (Kind::Codes, Some(id), Some(base)) = (kind, layout.dict, dict) else {
                    return None;
                };
                Some(match caches {
                    Some(caches) => caches.derived(id, expr, slot, &layout, base),
                    None => Arc::new(DerivedDict::empty().extend(expr, slot, &layout, base)),
                })
            }
        };
        // a derived field's values, for keying caches built from them
        let variant = match (&access.target, expr) {
            (Target::Ancestor { depth, .. }, _) => format!("ancestor:{depth}"),
            (_, expr) => expr.map(|(e, _)| format!("{e:?}")).unwrap_or_default(),
        };
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
                    // base codes → derived codes (a missing row's 0 too)
                    let mut missing = 0;
                    if let (Some(derived), Values::Codes(codes)) = (&derived, &mut values) {
                        let to = |code: u32| derived.map.get(code as usize).copied().unwrap_or(0);
                        for c in codes.iter_mut() {
                            *c = to(*c);
                        }
                        missing = to(0);
                    }
                    Some(Map::narrow(values, missing))
                };
                let map = match caches {
                    Some(caches) => {
                        let tables = access
                            .path
                            .iter()
                            .map(|hop| snapshot.tables.get(&hop.to))
                            .collect::<Option<Vec<_>>>()?;
                        let hops = access.path.iter().map(|h| h.fk_column).collect();
                        caches.map(&tables, hops, target_column, &variant, build)?
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
        // a shifted time column read as itself: stored integers are moved
        // as they load; anything computed from it takes the general path
        if access.shift.is_some()
            && !(kind == Kind::Ints
                && matches!(access.target, Target::Column(_))
                && matches!(access.ty, Type::Date | Type::Timestamp | Type::TimestampTz))
        {
            return None;
        }
        reads.push(Read {
            column,
            kind,
            ty: access.ty,
            derived,
            gathered,
            joined,
            layout,
            dict,
            shift: access.shift.map(|s| s.by),
        });
    }

    let mut conditions = plan
        .conditions
        .iter()
        .map(|(access, condition)| {
            let read = &reads[*access];
            let test = match read.kind {
                Kind::Text => {
                    let Condition::Contains(needle) = condition else {
                        return None;
                    };
                    let hits = caches.and_then(|caches| {
                        let anchor = snapshot.table(plan.table)?;
                        Some(
                            anchor
                                .segments
                                .iter()
                                .map(|s| caches.text(&s.data, read.column, needle))
                                .collect(),
                        )
                    });
                    Test::Text {
                        needle: Arc::clone(needle),
                        hits,
                    }
                }
                Kind::Codes => Test::Codes(
                    (0..code_space(read))
                        .map(|c| condition.matches(&read.code_value(u32::try_from(c).unwrap_or(0))))
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
                    Condition::Set(set) => Test::IntIn(
                        set.values()
                            .iter()
                            .map(|v| raw_ceil(v, read.layout.column_type))
                            .collect::<Option<Vec<_>>>()?
                            .into_iter()
                            .filter(|(_, exact)| *exact)
                            .map(|(raw, _)| raw)
                            .collect(),
                    ),
                    Condition::Contains(_) => return None,
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
    // searches last: they cost the most per row, so the cheaper tests
    // leave them fewer rows
    conditions.sort_by_key(|(_, test)| matches!(test, Test::Text { .. }));

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
        if arg_kind == Some(Kind::Text) {
            return None;
        }
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
            // over plain numbers (a decimal is its scaled integer)
            (
                AggFunc::Median
                | AggFunc::Percentile
                | AggFunc::StddevSamp
                | AggFunc::StddevPop
                | AggFunc::VarSamp
                | AggFunc::VarPop,
                false,
                Some(Kind::Ints | Kind::Floats),
            ) => matches!(
                reads[arg.unwrap_or(0)].layout.column_type,
                ColumnType::Int32
                    | ColumnType::Int64
                    | ColumnType::Float64
                    | ColumnType::Decimal { .. }
            ),
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
                        let value = read.code_value(u32::try_from(c).unwrap_or(0));
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
            fraction: call.fraction,
        });
    }

    let dims: Vec<(usize, Option<Part>)> = plan.dims.clone();
    for (access, part) in &dims {
        let read = &reads[*access];
        let temporal = matches!(read.ty, Type::Date | Type::Timestamp | Type::TimestampTz);
        if part.is_some() && !temporal {
            return None;
        }
        if read.kind == Kind::Floats && part.is_some() || read.kind == Kind::Text {
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

/// Move stored dates (days) or timestamps (microseconds) by `shift`: every
/// value, or those at `sel`. Rows of one morsel are mostly a few days, so
/// the last day's answer is kept. A value past the calendar's range (and a
/// NULL's placeholder) stays as it is.
fn move_in_time(shift: Shift, ty: Type, values: &mut [i64], sel: Option<&[u32]>) {
    let mut last: Option<(i64, i64)> = None;
    let mut day = |d: i64| -> i64 {
        match last {
            Some((from, to)) if from == d => to,
            _ => {
                let to = shift.days(d).unwrap_or(d);
                last = Some((d, to));
                to
            }
        }
    };
    let mut one = |v: &mut i64| {
        *v = if ty == Type::Date {
            day(*v)
        } else {
            let (d, time) = (v.div_euclid(DAY), v.rem_euclid(DAY));
            day(d).saturating_mul(DAY).saturating_add(time)
        };
    };
    match sel {
        Some(sel) => {
            for r in sel {
                if let Some(v) = values.get_mut(*r as usize) {
                    one(v);
                }
            }
        }
        None => values.iter_mut().for_each(one),
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
pub(crate) struct Map {
    data: MapData,
    /// The code of a key with no row: past the end of the map (a cached map
    /// predates keys added since), or a key whose row doesn't exist. Code 0
    /// (NULL), except for a derived field that is non-NULL over NULLs.
    missing: u32,
}

enum MapData {
    Codes8(Vec<u8>),
    Codes16(Vec<u16>),
    Values(Values),
}

impl Map {
    /// The code for key `code` (a code map's).
    fn code(&self, code: usize) -> u32 {
        match &self.data {
            MapData::Codes8(m) => m.get(code).map_or(self.missing, |c| u32::from(*c)),
            MapData::Codes16(m) => m.get(code).map_or(self.missing, |c| u32::from(*c)),
            MapData::Values(Values::Codes(m)) => m.get(code).copied().unwrap_or(self.missing),
            MapData::Values(_) => 0,
        }
    }

    fn is_codes(&self) -> bool {
        matches!(
            self.data,
            MapData::Codes8(_) | MapData::Codes16(_) | MapData::Values(Values::Codes(_))
        )
    }

    /// One morsel of a join column: `rows` of the foreign key, followed.
    fn narrow_rows(&self, column: &ColumnData, rows: Range<usize>) -> Narrow {
        match &self.data {
            MapData::Codes8(m) => {
                let mut out = Vec::new();
                // `missing` is one of the map's codes, so it fits too
                column.map_codes(rows, m, u8::try_from(self.missing).unwrap_or(0), &mut out);
                Narrow::U8(out.into())
            }
            MapData::Codes16(m) => {
                let mut out = Vec::new();
                column.map_codes(rows, m, u16::try_from(self.missing).unwrap_or(0), &mut out);
                Narrow::U16(out.into())
            }
            MapData::Values(Values::Codes(m)) => {
                let mut out = Vec::new();
                column.map_codes(rows, m, self.missing, &mut out);
                Narrow::U32(out.into())
            }
            MapData::Values(_) => Narrow::U32(Box::new([])),
        }
    }

    /// `values` by key, `missing` for keys past the end.
    fn narrow(values: Values, missing: u32) -> Self {
        let data = match values {
            Values::Codes(codes) => {
                let max = codes.iter().max().copied().unwrap_or(0).max(missing);
                if u8::try_from(max).is_ok() {
                    MapData::Codes8(
                        codes
                            .iter()
                            .map(|c| u8::try_from(*c).unwrap_or(0))
                            .collect(),
                    )
                } else if u16::try_from(max).is_ok() {
                    MapData::Codes16(
                        codes
                            .iter()
                            .map(|c| u16::try_from(*c).unwrap_or(0))
                            .collect(),
                    )
                } else {
                    MapData::Values(Values::Codes(codes))
                }
            }
            other => MapData::Values(other),
        };
        Self { data, missing }
    }
}

/// Decode `rows` of a foreign-key column through `map` into `out`:
/// following the relationship.
fn follow_into(column: &ColumnData, map: &Map, rows: Range<usize>, out: &mut Values) {
    match (&map.data, out) {
        (MapData::Codes8(m), Values::Codes(v)) => column.map_codes(rows, m, map.missing, v),
        (MapData::Codes16(m), Values::Codes(v)) => column.map_codes(rows, m, map.missing, v),
        (MapData::Values(Values::Codes(m)), Values::Codes(v)) => {
            column.map_codes(rows, m, map.missing, v);
        }
        (MapData::Values(values), out) => through_into(column, values, rows, out),
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
        Kind::Text => return None,
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
        (Some(map), Values::Codes(o)) => {
            o.resize(len, 0);
            match &map.data {
                MapData::Codes8(m) => column.gather_map_codes(start, sel, m, map.missing, o),
                MapData::Codes16(m) => column.gather_map_codes(start, sel, m, map.missing, o),
                MapData::Values(Values::Codes(m)) => {
                    column.gather_map_codes(start, sel, m, map.missing, o);
                }
                MapData::Values(_) => {}
            }
        }
        (Some(map), Values::Ints(v, n)) => {
            let MapData::Values(Values::Ints(m, nulls)) = &map.data else {
                return;
            };
            v.resize(len, 0);
            n.resize(len, true);
            column.gather_map_codes(start, sel, m, 0, v);
            column.gather_map_codes(start, sel, nulls, true, n);
        }
        (Some(map), Values::Floats(v, n)) => {
            let MapData::Values(Values::Floats(m, nulls)) = &map.data else {
                return;
            };
            v.resize(len, 0.0);
            n.resize(len, true);
            column.gather_map_codes(start, sel, m, 0.0, v);
            column.gather_map_codes(start, sel, nulls, true, n);
        }
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
            // text is never decoded
            Kind::Codes | Kind::Text => Self::Codes(Vec::new()),
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

/// Room to grow: a kept result is rebuilt when a dictionary outgrows it.
fn padded(space: usize) -> usize {
    space + space / 4 + 8
}

/// An anchor's answer as of some epoch, and what it was built from: kept
/// by the engine to bring the answer up to date with only the rows that
/// changed (plan §6.5). Row ids never change, so "what was counted" is a
/// high-water mark plus the rows that were already deleted.
pub(crate) struct Incremental {
    epoch: u64,
    /// Every row below this row id was counted (or skipped as deleted).
    high: u64,
    /// Row ids below `high` not (or no longer) counted: deleted.
    deleted: RoaringTreemap,
    /// Each segment's delete bitmap as last seen, by (first row id, rows).
    segments: Vec<(u64, u32, Arc<RoaringBitmap>)>,
    dense: Option<Vec<usize>>,
    partial: Partial,
    shared: Vec<Option<Bitset>>,
    maps: Vec<Option<Arc<Map>>>,
    derived: Vec<Option<Arc<DerivedDict>>>,
}

impl Incremental {
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Bytes held, roughly: for the engine's budget.
    pub(crate) fn bytes(&self) -> usize {
        let bits: usize = self.shared.iter().flatten().map(Bitset::bytes).sum();
        bits + self.partial.rows.len() * 8 * (1 + self.partial.states.len())
    }
}

/// Which rows of a morsel a scan counts.
pub(crate) enum Task {
    /// Live rows from this offset in the segment on: everything (0), or
    /// what was appended since a result was last brought up to date.
    Add { from: usize },
    /// These rows (segment offsets, sorted), deleted since they were
    /// counted: taken back out.
    Remove(Vec<u32>),
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
    /// Sums and how many non-NULL values each holds (subtracting can
    /// bring that back to 0, when the sum is NULL again).
    SumInt(Vec<i128>, Vec<i64>),
    SumFloat(Vec<f64>, Vec<i64>),
    AvgInt(Vec<i128>, Vec<i64>),
    AvgFloat(Vec<f64>, Vec<i64>),
    Min(Vec<Option<Cell>>),
    Max(Vec<Option<Cell>>),
    DistinctCodes(Vec<RoaringBitmap>),
    DistinctNumbers(Vec<HashSet<i64>>),
    /// `MEDIAN` and `PERCENTILE_CONT`: raw values (scaled integers for
    /// decimals), counted.
    Quantile(Vec<crate::stats::Counted>),
    /// Standard deviations and variances, of raw values.
    Moments(Vec<crate::stats::Moments>),
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
            (AggFunc::Median | AggFunc::Percentile, ..) => Self::Quantile(Vec::new()),
            // ARG_MAX/ARG_MIN take the general path (`prepare` refuses them)
            (
                AggFunc::StddevSamp
                | AggFunc::StddevPop
                | AggFunc::VarSamp
                | AggFunc::VarPop
                | AggFunc::ArgMax
                | AggFunc::ArgMin,
                ..,
            ) => Self::Moments(Vec::new()),
        }
    }

    fn grow(&mut self, groups: usize) {
        match self {
            Self::Count(v) => v.resize(groups, 0),
            Self::SumInt(s, a) => {
                s.resize(groups, 0);
                a.resize(groups, 0);
            }
            Self::SumFloat(s, a) => {
                s.resize(groups, 0.0);
                a.resize(groups, 0);
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
            Self::Quantile(v) => v.resize_with(groups, crate::stats::Counted::default),
            Self::Moments(v) => v.resize_with(groups, crate::stats::Moments::default),
        }
    }

    /// Feed the rows `sel` (in groups `groups`) of `values`.
    /// Feed the rows `sel` (in groups `groups`) of `values`, counted
    /// `weight` times: 1 to add rows, -1 to take deleted ones back out (only
    /// for subtractable aggregates, see [`Agg::subtractable`]).
    #[allow(clippy::too_many_lines)] // one arm per state, as the others
    fn update(
        &mut self,
        sel: &[u32],
        groups: &[u32],
        values: Option<&Values>,
        ranks: Option<&[u32]>,
        weight: i64,
    ) {
        let rows = sel
            .iter()
            .map(|r| *r as usize)
            .zip(groups.iter().map(|g| *g as usize));
        match (self, values) {
            (Self::Count(counts), None) => {
                for g in groups {
                    counts[*g as usize] += weight;
                }
            }
            (Self::Count(counts), Some(values)) => {
                for (r, g) in rows {
                    counts[g] += i64::from(!values.is_null(r)) * weight;
                }
            }
            (Self::SumInt(sums, any), Some(Values::Ints(v, n))) => {
                for (r, g) in rows {
                    if !n[r] {
                        sums[g] += i128::from(v[r]) * i128::from(weight);
                        any[g] += weight;
                    }
                }
            }
            (Self::SumFloat(sums, any), Some(Values::Floats(v, n))) => {
                #[allow(clippy::cast_precision_loss)]
                let w = weight as f64;
                for (r, g) in rows {
                    if !n[r] {
                        sums[g] += v[r] * w;
                        any[g] += weight;
                    }
                }
            }
            (Self::AvgInt(sums, counts), Some(Values::Ints(v, n))) => {
                for (r, g) in rows {
                    if !n[r] {
                        sums[g] += i128::from(v[r]) * i128::from(weight);
                        counts[g] += weight;
                    }
                }
            }
            (Self::AvgFloat(sums, counts), Some(Values::Floats(v, n))) => {
                #[allow(clippy::cast_precision_loss)]
                let w = weight as f64;
                for (r, g) in rows {
                    if !n[r] {
                        sums[g] += v[r] * w;
                        counts[g] += weight;
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
            #[allow(clippy::cast_precision_loss)]
            (Self::Quantile(counted), Some(Values::Ints(v, n))) => {
                for (r, g) in rows {
                    if !n[r] {
                        counted[g].add(v[r] as f64);
                    }
                }
            }
            (Self::Quantile(counted), Some(Values::Floats(v, n))) => {
                for (r, g) in rows {
                    if !n[r] {
                        counted[g].add(v[r]);
                    }
                }
            }
            #[allow(clippy::cast_precision_loss)]
            (Self::Moments(moments), Some(Values::Ints(v, n))) => {
                for (r, g) in rows {
                    if !n[r] {
                        moments[g].add(v[r] as f64);
                    }
                }
            }
            (Self::Moments(moments), Some(Values::Floats(v, n))) => {
                for (r, g) in rows {
                    if !n[r] {
                        moments[g].add(v[r]);
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
                a[into] += b[from];
            }
            (Self::SumFloat(s, a), Self::SumFloat(t, b)) => {
                s[into] += t[from];
                a[into] += b[from];
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
            (Self::Quantile(a), Self::Quantile(b)) => {
                let taken = std::mem::take(&mut b[from]);
                a[into].merge(taken);
            }
            (Self::Moments(a), Self::Moments(b)) => {
                let taken = std::mem::take(&mut b[from]);
                a[into].merge(taken);
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
            Self::SumInt(_, any) | Self::SumFloat(_, any) if any[group] == 0 => Scalar::Null,
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
            // raw values: a decimal's scaled integers, divided back here
            Self::Quantile(v) => v[group]
                .quantile(agg.fraction.unwrap_or(500_000))
                .map_or(Scalar::Null, |q| {
                    Scalar::Float(q / 10f64.powi(i32::from(scale)))
                }),
            Self::Moments(v) => v[group]
                .scaled(10f64.powi(-i32::from(scale)))
                .finish(agg.func)
                .map_or(Scalar::Null, Scalar::Float),
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
        Cell::Code(code) => read.code_value(code),
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
/// Counts per group are kept as bits go from 0 to 1, so reading them costs
/// nothing, and a result kept across epochs only pays for the bits its new
/// rows set.
///
/// Small bitsets are filled per thread (threads writing one small bitset
/// would fight over its cache lines), in pages allocated as rows touch
/// them and merged into `words` when counts are read. Large ones are shared:
/// a task doesn't set bits as it scans but collects their positions into
/// [`Pending`] buckets by range, and sets them a bucket at a time, each
/// bucket's slice small enough to stay in cache.
struct Bitset {
    space: usize,
    /// Bucket of a bit: `bit >> shift`.
    shift: u32,
    words: Vec<AtomicU64>,
    /// One per bucket: whoever holds it owns that slice of `words`, so
    /// setting bits there needs no atomic read-modify-write.
    locks: Vec<std::sync::Mutex<()>>,
    /// Dimensions the counted code determines, folded out of the group.
    folded: Option<Folded>,
    /// Per worker thread, for small bitsets: pages of private bits.
    private: Option<Vec<std::sync::Mutex<Pages>>>,
    /// Distinct codes per (dense) group so far.
    counts: Vec<AtomicI64>,
}

/// One thread's private bits, in pages allocated as rows touch them.
type Pages = Vec<Option<Box<[u64]>>>;

/// Bytes all private bitsets of one distinct count may take together.
const PRIVATE_BYTES: usize = 64 << 20;
/// Words per private page (32 KiB).
const PAGE: usize = 1 << 12;

/// When a dimension is reached through the very key being counted (region
/// through `customer_id`, for `COUNT(DISTINCT customer_id)`), a code can
/// only ever fall in one value of that dimension. The bitset then leaves
/// those dimensions out of its group index, which makes it many times
/// smaller (it fits in cache where the full one didn't), and puts them back
/// when counting: each set bit's code says which group it belongs to.
struct Folded {
    /// Dense group id → id without the determined dimensions.
    reduce: Vec<u32>,
    /// Reduced id → dense group id, determined dimensions' parts at 0.
    expand: Vec<usize>,
    /// Per determined dimension: its stride in the dense id, and the
    /// relationship map giving its code from the counted code.
    determined: Vec<(usize, Arc<Map>)>,
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
    fn new(groups: usize, space: usize, folded: Option<Folded>) -> Self {
        let rows = folded.as_ref().map_or(groups, |f| f.expand.len());
        let bits = rows * space;
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
            folded,
            private: {
                let threads = rayon::current_num_threads();
                // debug builds (tests) take both ways on small data
                let fits = if cfg!(debug_assertions) {
                    space.is_multiple_of(2)
                } else {
                    threads * bits.div_ceil(64) * 8 <= PRIVATE_BYTES
                };
                fits.then(|| {
                    (0..threads)
                        .map(|_| std::sync::Mutex::new(Vec::new()))
                        .collect()
                })
            },
            counts: (0..groups).map(|_| AtomicI64::new(0)).collect(),
        }
    }

    fn bytes(&self) -> usize {
        let private: usize = self
            .private
            .iter()
            .flatten()
            .map(|p| {
                p.lock()
                    .map_or(0, |p| p.iter().flatten().count() * PAGE * 8)
            })
            .sum();
        self.words.len() * 8 + private + self.counts.len() * 8
    }

    /// The dense group bit `bit` counts for.
    fn group(&self, bit: usize) -> usize {
        let (row, code) = (bit / self.space, bit % self.space);
        match &self.folded {
            None => row,
            Some(folded) => {
                folded.expand[row]
                    + folded
                        .determined
                        .iter()
                        .map(|(stride, map)| stride * map.code(code) as usize)
                        .sum::<usize>()
            }
        }
    }

    /// Set `new` in word `at` (which must hold none of them yet) and count
    /// its bits into `local` (by group). Only one task writes a word at a
    /// time (bucket locks, or one task per page).
    fn set(&self, at: usize, new: u64, local: &mut [i64]) {
        let word = &self.words[at];
        word.store(word.load(Ordering::Relaxed) | new, Ordering::Relaxed);
        let mut bits = new;
        while bits != 0 {
            let bit = at * 64 + bits.trailing_zeros() as usize;
            bits &= bits - 1;
            if let Some(n) = local.get_mut(self.group(bit)) {
                *n += 1;
            }
        }
    }

    /// Add a task's counts to the shared ones: once per task, not per bit,
    /// or every thread would fight over a handful of counters.
    fn add(&self, local: &[i64]) {
        for (n, l) in self.counts.iter().zip(local) {
            if *l != 0 {
                n.fetch_add(*l, Ordering::Relaxed);
            }
        }
    }

    /// Move every private page into `words`, counting what's new.
    fn gather(&self) {
        let Some(private) = &self.private else { return };
        // pages by index: one task per index, so no word has two writers
        let mut pages: HashMap<usize, Vec<Box<[u64]>>> = HashMap::new();
        for mine in private {
            let mut mine = mine
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (index, page) in mine.iter_mut().enumerate() {
                if let Some(page) = page.take() {
                    pages.entry(index).or_default().push(page);
                }
            }
        }
        pages.into_par_iter().for_each(|(index, pages)| {
            let mut local = vec![0i64; self.counts.len()];
            for i in 0..PAGE {
                let at = index * PAGE + i;
                let Some(word) = self.words.get(at) else {
                    break;
                };
                let old = word.load(Ordering::Relaxed);
                let new = pages.iter().fold(0, |w, p| w | p[i]) & !old;
                if new != 0 {
                    self.set(at, new, &mut local);
                }
            }
            self.add(&local);
        });
    }

    /// The distinct count of every dense group.
    fn counts(&self) -> Vec<i64> {
        self.gather();
        self.counts
            .iter()
            .map(|n| n.load(Ordering::Relaxed))
            .collect()
    }

    fn insert(&self, rows: &[u32], groups: &[u32], codes: &[u32], pending: &mut Pending) {
        if let Some(private) = &self.private {
            let mine = &private[rayon::current_thread_index().unwrap_or(0) % private.len()];
            // only this thread takes its lock: it is never contended
            let mut pages = mine
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pages.is_empty() {
                pages.resize_with(self.words.len().div_ceil(PAGE), || None);
            }
            for (r, g) in rows.iter().zip(groups) {
                let code = codes[*r as usize];
                if code == 0 {
                    continue;
                }
                let g = self.folded.as_ref().map_or(*g, |f| f.reduce[*g as usize]);
                let bit = g as usize * self.space + code as usize;
                let (word, offset) = (bit / 64, bit % 64);
                let page =
                    pages[word / PAGE].get_or_insert_with(|| vec![0u64; PAGE].into_boxed_slice());
                page[word % PAGE] |= 1 << offset;
            }
            return;
        }
        if pending.buckets.is_empty() {
            pending.buckets = (0..=BUCKETS).map(|_| Vec::new()).collect();
        }
        for (r, g) in rows.iter().zip(groups) {
            let code = codes[*r as usize];
            if code == 0 {
                continue;
            }
            let g = self.folded.as_ref().map_or(*g, |f| f.reduce[*g as usize]);
            // < BITSET_LIMIT, so it fits
            #[allow(clippy::cast_possible_truncation)]
            let bit = (g as usize * self.space + code as usize) as u32;
            pending.buckets[(bit >> self.shift) as usize].push(bit);
        }
        pending.count += rows.len();
        if pending.count >= PENDING {
            self.flush(pending);
        }
    }

    /// Set every pending bit.
    fn flush(&self, pending: &mut Pending) {
        let mut local = vec![0i64; self.counts.len()];
        for (bucket, lock) in pending.buckets.iter_mut().zip(&self.locks) {
            if bucket.is_empty() {
                continue;
            }
            // nothing guarded can be left half-done: a poisoned lock is fine
            let _owned = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for bit in bucket.drain(..) {
                let (at, mask) = (bit as usize / 64, 1u64 << (bit % 64));
                // a bucket's words are only written under its lock, and a
                // word never straddles two buckets (`shift` >= 6)
                if self.words[at].load(Ordering::Relaxed) & mask == 0 {
                    self.set(at, mask, &mut local);
                }
            }
        }
        self.add(&local);
        pending.count = 0;
    }
}

/// Groups found so far, and every aggregate's values for them.
struct Partial {
    /// Group id → key (sparse groups only; dense ids are their own key).
    keys: Vec<Key>,
    lookup: hashbrown::HashMap<Key, u32>,
    /// Rows counted per group (a group with none left isn't output).
    rows: Vec<i64>,
    states: Vec<State>,
}

const DAY: i64 = 86_400_000_000;

/// Days truncated to a calendar part, memoized.
fn truncate(part: Part, days: i64, memo: &mut hashbrown::HashMap<(Part, i64), i64>) -> i64 {
    *memo.entry((part, days)).or_insert_with(|| {
        i32::try_from(days)
            .ok()
            .and_then(date_from_days)
            .map_or(days, |d| i64::from(days_from_date(trunc_date(part, d))))
    })
}

/// The bucket of a timestamp: in days for a calendar part, in microseconds
/// for a shorter one.
fn bucket(part: Part, micros: i64, memo: &mut hashbrown::HashMap<(Part, i64), i64>) -> i64 {
    if part.is_calendar() {
        truncate(part, micros.div_euclid(DAY), memo)
    } else {
        trunc_micros(part, micros)
    }
}

/// What a plan reads related and derived values through, per read.
pub(crate) type Inputs = (Vec<Option<Arc<Map>>>, Vec<Option<Arc<DerivedDict>>>);

/// Reusable buffers of one scanning task.
#[derive(Default)]
pub(crate) struct Scratch {
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
    text: TextScratch,
}

impl Scratch {
    /// The rows [`Prepared::select`] left, as offsets in its morsel.
    pub(crate) fn selected(&self) -> &[u32] {
        &self.sel
    }
}

/// Buffers of a text search, reused from morsel to morsel.
#[derive(Default)]
pub(crate) struct TextScratch {
    /// A morsel's text, lowercased.
    folded: Vec<u8>,
    bits: Vec<u64>,
}

/// Whether bit `r` is set.
#[inline]
fn bit(bits: &[u64], r: u32) -> bool {
    bits.get(r as usize / 64)
        .is_some_and(|w| w & (1 << (r % 64)) != 0)
}

/// A morsel's hits of `needle` in a string column: bit `r` for its row `r`.
/// With the hits of a needle it narrows built for the morsel, only those
/// rows are searched.
fn morsel_hits(
    column: &ColumnData,
    needle: &Needle,
    wider: Option<&[u64]>,
    rows: Range<usize>,
    scratch: &mut Vec<u8>,
) -> Box<[u64]> {
    let mut bits = vec![0u64; rows.len().div_ceil(64)];
    let ColumnData::Str {
        bytes,
        offsets,
        nulls,
    } = column
    else {
        // a column added since (all NULL) holds nothing to find
        return bits.into();
    };
    match wider {
        Some(wider) => {
            for r in 0..u32::try_from(rows.len()).unwrap_or(u32::MAX) {
                if bit(wider, r) {
                    let at = rows.start + r as usize;
                    let text = &bytes[offsets[at] as usize..offsets[at + 1] as usize];
                    if needle.is_in(text) {
                        bits[r as usize / 64] |= 1 << (r % 64);
                    }
                }
            }
        }
        None => needle.search(bytes, offsets, rows.clone(), scratch, &mut bits),
    }
    // NULLs are stored as empty strings, which only the empty needle is in
    if let Some(nulls) = nulls
        && needle.folded().is_empty()
    {
        for r in 0..rows.len() {
            if nulls.get(rows.start + r) {
                bits[r / 64] &= !(1 << (r % 64));
            }
        }
    }
    bits.into()
}

/// Keep the rows of `sel` (offsets from `rows.start`) whose text in
/// `column` contains `needle`. Cached bits are used when built; otherwise a
/// whole morsel is searched and cached, unless few rows are left, which are
/// then searched one at a time.
fn search(
    column: &ColumnData,
    needle: &Needle,
    hits: Option<&TextHits>,
    rows: Range<usize>,
    sel: &mut Vec<u32>,
    scratch: &mut TextScratch,
) {
    if sel.is_empty() {
        return;
    }
    if let Some(bits) = hits.and_then(|h| h.built(rows.start)) {
        return compact(sel, |r| bit(bits, r));
    }
    // a needle this one narrows: only its hits need searching, one at a
    // time, which beats a search of the morsel only when they are few
    let wider = hits
        .and_then(|h| h.wider.as_ref())
        .and_then(|w| w.built(rows.start))
        .filter(|w| {
            let found: u32 = w.iter().map(|word| word.count_ones()).sum();
            (found as usize) * SPARSE < rows.len()
        });
    let sparse = sel.len() * SPARSE < rows.len();
    if sparse {
        let ColumnData::Str { bytes, offsets, .. } = column else {
            sel.clear();
            return;
        };
        let start = rows.start;
        // NULLs are empty: only the empty needle is in them, and then the
        // bitmap decides
        let nulls = |r: u32| match column {
            ColumnData::Str { nulls: Some(n), .. } => n.get(start + r as usize),
            _ => false,
        };
        return compact(sel, |r| {
            let at = start + r as usize;
            wider.is_none_or(|w| bit(w, r))
                && !nulls(r)
                && needle.is_in(&bytes[offsets[at] as usize..offsets[at + 1] as usize])
        });
    }
    if let Some(hits) = hits {
        if let Some(bits) = hits.chunk(rows.start, || {
            morsel_hits(column, needle, wider, rows.clone(), &mut scratch.folded)
        }) {
            compact(sel, |r| bit(bits, r));
        }
    } else {
        scratch.bits = morsel_hits(column, needle, None, rows, &mut scratch.folded).into_vec();
        let bits = &scratch.bits;
        compact(sel, |r| bit(bits, r));
    }
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
            rows: vec![0; groups],
            states,
        }
    }

    /// Decode read `read` for `rows` of a segment into `out`: every row, or
    /// only the rows in `sel` (relative to `rows.start`) when they are few.
    /// Other rows then hold arbitrary values, and nothing reads them.
    /// [`Prepared::load_codes`], then a derived field's base codes mapped
    /// to its own (related fields' maps are derived already).
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
        self.load_codes(view, segment, read, rows.clone(), sel, out);
        let read = &self.reads[read];
        if let (Some(shift), Values::Ints(values, _)) = (read.shift, &mut *out) {
            let sparse = sel.len() * SPARSE < rows.len();
            move_in_time(shift, read.ty, values.as_mut_slice(), sparse.then_some(sel));
        }
        if let (None, Some(derived), Values::Codes(codes)) = (&read.gathered, &read.derived, out) {
            let to = |code: u32| derived.map.get(code as usize).copied().unwrap_or(0);
            if sel.len() * SPARSE < rows.len() {
                for r in sel {
                    codes[*r as usize] = to(codes[*r as usize]);
                }
            } else {
                for c in codes.iter_mut() {
                    *c = to(*c);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn load_codes(
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

    /// The rows of a morsel `task` counts that pass every condition, as
    /// offsets from `rows.start`, into `scratch.sel`: live rows from the
    /// task's start (or its deleted ones). The reads the conditions needed
    /// are left decoded in `scratch.buffers`.
    pub(crate) fn select(
        &self,
        table: &TableSnapshot,
        segment: usize,
        rows: Range<usize>,
        task: &Task,
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
            text,
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
        if let Task::Remove(offsets) = task {
            // rows deleted since they were counted: deleted now, so the
            // delete bitmap mustn't drop them
            sel.extend(
                offsets
                    .iter()
                    .filter(|o| (start..start + len).contains(o))
                    .map(|o| o - start),
            );
        } else if view.deletes.is_empty() {
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
        if let Task::Add { from } = task
            && *from > rows.start
        {
            let skip = u32::try_from(from - rows.start).unwrap_or(u32::MAX);
            sel.retain(|r| *r >= skip);
        }
        for (read, test) in &self.conditions {
            if let Test::Text { needle, hits } = test {
                let hits = hits.as_ref().and_then(|h| h.get(segment)).map(|h| &**h);
                let column = &view.data.columns[self.reads[*read].column];
                search(column, needle, hits, rows.clone(), sel, text);
                continue;
            }
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
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn scan(
        &self,
        partial: &mut Partial,
        table: &TableSnapshot,
        segment: usize,
        rows: Range<usize>,
        task: &Task,
        ranks: &[Option<Vec<u32>>],
        shared: &[Option<Bitset>],
        scratch: &mut Scratch,
    ) {
        let view = &table.segments[segment];
        let weight = match task {
            Task::Add { .. } => 1,
            Task::Remove(_) => -1,
        };
        self.select(table, segment, rows.clone(), task, scratch);
        let Scratch {
            buffers,
            loaded,
            sel,
            ..
        } = scratch;
        for read in 0..self.reads.len() {
            if !loaded[read] && self.reads[read].kind != Kind::Text {
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
                        partial.rows.push(0);
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
            partial.rows[*g as usize] += weight;
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
                _ => state.update(rows, groups, arg, ranks.as_deref(), weight),
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
            let part_value = match (values[*read].cell(row), part) {
                (Cell::Null, _) => None,
                (Cell::Code(c), None) => Some(i64::from(c)),
                // a dictionary-coded date or timestamp: truncate its value
                (Cell::Code(c), Some(part)) => match self.reads[*read].code_value(c) {
                    Scalar::Date(d) => Some(truncate(*part, i64::from(d), memo)),
                    Scalar::Timestamp(m) | Scalar::TimestampTz(m) => Some(bucket(*part, m, memo)),
                    _ => None,
                },
                (Cell::Int(v), None) => Some(v),
                (Cell::Int(v), Some(part)) => Some(if layout.column_type == ColumnType::Date {
                    truncate(*part, v, memo)
                } else {
                    bucket(*part, v, memo)
                }),
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
            // a group no row of `b` touched has nothing to merge (one pass
            // only adds rows or only removes them, so touched ⇔ rows ≠ 0)
            for (group, rows) in b.rows.iter().enumerate() {
                if *rows != 0 {
                    a.rows[group] += rows;
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
                a.rows.push(0);
                for state in &mut a.states {
                    state.grow(a.keys.len());
                }
                id
            };
            a.rows[target] += b.rows[group];
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
            (Some(grain), _) if grain.is_calendar() => {
                Scalar::Date(i32::try_from(part).unwrap_or(0))
            }
            (Some(_), _) if read.ty == Type::TimestampTz => Scalar::TimestampTz(part),
            (Some(_), _) => Scalar::Timestamp(part),
            (None, Kind::Codes) => read.code_value(u32::try_from(part).unwrap_or(0)),
            (None, Kind::Ints) => scalar(Value::from_i64(read.layout.column_type, part)),
            (None, Kind::Floats) => Scalar::Float(f64::from_ne_bytes(part.to_ne_bytes())),
            // `prepare` refuses text as a dimension
            (None, Kind::Text) => Scalar::Null,
        }
    }

    /// Shared bitsets for `COUNT(DISTINCT code)` over dense groups, per
    /// aggregate, when they are small enough.
    /// `space` is the code space a bitset must cover; `pad` leaves room
    /// for the dictionary to grow (for a result kept across epochs).
    fn bitsets(&self, pad: bool) -> Vec<Option<Bitset>> {
        self.aggs
            .iter()
            .map(|agg| {
                let groups: usize = self.dense.as_ref()?.iter().product();
                let read = &self.reads[agg.arg?];
                let ok = agg.func == AggFunc::Count && agg.distinct && read.kind == Kind::Codes;
                let space = if pad {
                    padded(code_space(read))
                } else {
                    code_space(read)
                };
                let folded = self.folded(agg.arg?);
                let rows = folded.as_ref().map_or(groups, |f| f.expand.len());
                (ok && rows.checked_mul(space)? <= BITSET_LIMIT)
                    .then(|| Bitset::new(groups, space, folded))
            })
            .collect()
    }

    /// What this plan reads related and derived values through: kept with
    /// an answer, to tell whether it still applies ([`Self::same_reads`]).
    pub(crate) fn inputs(&self) -> Inputs {
        (
            self.reads.iter().map(|r| r.gathered.clone()).collect(),
            self.reads.iter().map(|r| r.derived.clone()).collect(),
        )
    }

    /// Whether an answer built through `maps` and `derived` reads the same
    /// values as this plan does: the same relationship maps, and derived
    /// dictionaries that only grew.
    pub(crate) fn same_reads(
        &self,
        maps: &[Option<Arc<Map>>],
        derived: &[Option<Arc<DerivedDict>>],
    ) -> bool {
        let same_maps = maps.len() == self.reads.len()
            && self
                .reads
                .iter()
                .zip(maps)
                .all(|(read, old)| match (&read.gathered, old) {
                    (None, None) => true,
                    (Some(map), Some(old)) => Arc::ptr_eq(map, old),
                    _ => false,
                });
        let same_derived = derived.len() == self.reads.len()
            && self
                .reads
                .iter()
                .zip(derived)
                .all(|(read, old)| match (&read.derived, old) {
                    (None, None) => true,
                    (Some(now), Some(old)) => {
                        now.map.len() >= old.map.len() && now.map[..old.map.len()] == old.map[..]
                    }
                    _ => false,
                });
        same_maps && same_derived
    }

    /// Whether every aggregate can take deleted rows back out.
    fn subtractable(&self) -> bool {
        self.aggs.iter().all(|a| {
            matches!(
                a.func,
                AggFunc::CountStar | AggFunc::Sum | AggFunc::Avg | AggFunc::Count
            ) && !a.distinct
        })
    }

    /// Whether `state` can be brought up to date for this plan: built from
    /// the same relationship maps, derived dictionaries that only grew, and
    /// group and bitset layouts that still have room for every code.
    fn compatible(&self, state: &Incremental, table: &TableSnapshot) -> bool {
        let reads = self.same_reads(&state.maps, &state.derived);
        let dense = match (&self.dense, &state.dense) {
            (None, None) => true,
            (Some(now), Some(old)) => {
                now.len() == old.len() && now.iter().zip(old).all(|(n, o)| n <= o)
            }
            _ => false,
        };
        let bitsets = self.aggs.iter().zip(&state.shared).all(|(agg, old)| {
            let wanted = self.dense.is_some()
                && agg.func == AggFunc::Count
                && agg.distinct
                && agg.arg.is_some_and(|a| self.reads[a].kind == Kind::Codes);
            match old {
                None => !wanted,
                Some(old) => agg
                    .arg
                    .is_some_and(|a| old.space >= code_space(&self.reads[a])),
            }
        });
        let rows: u64 = table
            .segments
            .iter()
            .map(|s| s.data.start + u64::from(s.data.rows))
            .max()
            .unwrap_or(0);
        reads
            && dense
            && bitsets
            && rows >= state.high
            && state.maps.len() == self.reads.len()
            && state.shared.len() == self.aggs.len()
    }

    /// The dimensions a distinct count over read `arg` determines (see
    /// [`Folded`]), if any.
    fn folded(&self, arg: usize) -> Option<Folded> {
        let spaces = self.dense.as_ref()?;
        let counted = &self.reads[arg];
        if counted.gathered.is_some() {
            return None;
        }
        // stride of dimension i in `id = id * space + code`
        let strides: Vec<usize> = (0..spaces.len())
            .map(|i| spaces[i + 1..].iter().product())
            .collect();
        let determined: Vec<Option<Arc<Map>>> = self
            .dims
            .iter()
            .map(|(read, _)| {
                let read = &self.reads[*read];
                read.gathered
                    .as_ref()
                    .filter(|map| read.column == counted.column && map.is_codes())
                    .cloned()
            })
            .collect();
        if determined.iter().all(Option::is_none) {
            return None;
        }
        let kept: Vec<usize> = (0..spaces.len())
            .filter(|i| determined[*i].is_none())
            .collect();
        let groups: usize = spaces.iter().product();
        let reduce = (0..groups)
            .map(|g| {
                let id = kept
                    .iter()
                    .fold(0, |id, i| id * spaces[*i] + g / strides[*i] % spaces[*i]);
                u32::try_from(id).unwrap_or(u32::MAX)
            })
            .collect();
        let reduced: usize = kept.iter().map(|i| spaces[*i]).product();
        let expand = (0..reduced)
            .map(|r| {
                let mut rest = r;
                let mut full = 0;
                for i in kept.iter().rev() {
                    full += rest % spaces[*i] * strides[*i];
                    rest /= spaces[*i];
                }
                full
            })
            .collect();
        Some(Folded {
            reduce,
            expand,
            determined: determined
                .into_iter()
                .zip(strides)
                .filter_map(|(map, stride)| map.map(|m| (stride, m)))
                .collect(),
        })
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
                let sorted = match &read.derived {
                    Some(derived) => {
                        let mut codes: Vec<u32> =
                            (1..u32::try_from(derived.values.len()).unwrap_or(u32::MAX)).collect();
                        codes.sort_by(|a, b| {
                            derived.values[*a as usize].sort_cmp(&derived.values[*b as usize])
                        });
                        codes
                    }
                    None => read
                        .dict?
                        .sorted_codes(read.layout.column_type == ColumnType::Float64),
                };
                let mut ranks = vec![0u32; sorted.iter().max().map_or(0, |m| *m as usize) + 1];
                for (rank, code) in sorted.iter().enumerate() {
                    ranks[*code as usize] = u32::try_from(rank + 1).unwrap_or(u32::MAX);
                }
                Some(ranks)
            })
            .collect()
    }

    /// The answer: (dimension values, aggregate values) per group. With a
    /// `kept` slot (a cached engine), the answer last kept there is brought
    /// up to date when it can be (only rows appended or deleted since are
    /// scanned), and the new one is kept in its place.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn run(
        &mut self,
        table: &TableSnapshot,
        epoch: u64,
        kept: Option<&mut Option<Incremental>>,
    ) -> Vec<(Vec<Scalar>, Vec<Scalar>)> {
        let ranks = self.ranks();
        let keeping = kept.is_some();
        let mut kept = kept;
        let mut state = kept
            .as_mut()
            .and_then(|k| k.take())
            .filter(|s| s.epoch <= epoch && self.compatible(s, table));

        // rows deleted since they were counted, by segment
        let mut removed: Vec<(usize, Vec<u32>)> = Vec::new();
        if let Some(st) = &state {
            for (s, view) in table.segments.iter().enumerate() {
                let start = view.data.start;
                let same = st.segments.iter().any(|(at, rows, deletes)| {
                    *at == start && *rows == view.data.rows && Arc::ptr_eq(deletes, &view.deletes)
                });
                if same {
                    continue;
                }
                let offsets: Vec<u32> = view
                    .deletes
                    .iter()
                    .filter(|d| {
                        let id = start + u64::from(*d);
                        id < st.high && !st.deleted.contains(id)
                    })
                    .collect();
                if !offsets.is_empty() {
                    removed.push((s, offsets));
                }
            }
            if !removed.is_empty() && !self.subtractable() {
                // MIN, MAX and distinct counts can't take rows back out
                state = None;
                removed.clear();
            }
        }
        let mut state = if let Some(st) = state {
            self.dense.clone_from(&st.dense);
            st
        } else {
            {
                if keeping && let Some(spaces) = &self.dense {
                    let padded: Vec<usize> = spaces.iter().map(|s| padded(*s)).collect();
                    if padded
                        .iter()
                        .try_fold(1usize, |acc, s| acc.checked_mul(*s))
                        .is_some_and(|n| n <= DENSE_LIMIT)
                    {
                        self.dense = Some(padded);
                    }
                }
                Incremental {
                    epoch,
                    high: 0,
                    deleted: RoaringTreemap::new(),
                    segments: Vec::new(),
                    dense: self.dense.clone(),
                    partial: self.empty(),
                    shared: self.bitsets(keeping),
                    maps: self.reads.iter().map(|r| r.gathered.clone()).collect(),
                    derived: self.reads.iter().map(|r| r.derived.clone()).collect(),
                }
            }
        };

        // the rows to add: everything from the high-water mark on
        let high = state.high;
        let added: Vec<(usize, Range<usize>, Task)> = table
            .segments
            .iter()
            .enumerate()
            .flat_map(|(s, view)| {
                let rows = view.data.rows as usize;
                let from =
                    usize::try_from(high.saturating_sub(view.data.start)).unwrap_or(usize::MAX);
                (0..rows)
                    .step_by(MORSEL)
                    .map(move |start| (s, start..(start + MORSEL).min(rows)))
                    .filter(move |(_, morsel)| morsel.end > from)
                    .map(move |(s, morsel)| (s, morsel, Task::Add { from }))
            })
            .collect();
        let taken: Vec<(usize, Range<usize>, Task)> = removed
            .iter()
            .flat_map(|(s, offsets)| {
                let rows = table.segments[*s].data.rows as usize;
                (0..rows).step_by(MORSEL).filter_map(move |start| {
                    let morsel = start..(start + MORSEL).min(rows);
                    let here: Vec<u32> = offsets
                        .iter()
                        .copied()
                        .filter(|o| morsel.contains(&(*o as usize)))
                        .collect();
                    (!here.is_empty()).then_some((*s, morsel, Task::Remove(here)))
                })
            })
            .collect();

        let shared = std::mem::take(&mut state.shared);
        let pass = |tasks: Vec<(usize, Range<usize>, Task)>| {
            tasks
                .into_par_iter()
                .fold(
                    || (self.empty(), Scratch::default()),
                    |(mut partial, mut scratch), (segment, rows, task)| {
                        self.scan(
                            &mut partial,
                            table,
                            segment,
                            rows,
                            &task,
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
                .reduce(|| self.empty(), |a, b| self.merge(a, b, &ranks))
        };
        let plus = pass(added);
        let minus = pass(taken);
        let partial = std::mem::replace(&mut state.partial, self.empty());
        let partial = self.merge(self.merge(partial, plus, &ranks), minus, &ranks);

        // what's counted now
        for view in table.segments.iter() {
            let start = view.data.start;
            let same = state.segments.iter().any(|(at, rows, deletes)| {
                *at == start && *rows == view.data.rows && Arc::ptr_eq(deletes, &view.deletes)
            });
            if !same {
                state
                    .deleted
                    .extend(view.deletes.iter().map(|d| start + u64::from(d)));
            }
        }
        state.segments = table
            .segments
            .iter()
            .map(|v| (v.data.start, v.data.rows, Arc::clone(&v.deletes)))
            .collect();
        state.high = table
            .segments
            .iter()
            .map(|v| v.data.start + u64::from(v.data.rows))
            .max()
            .unwrap_or(0)
            .max(high);
        state.epoch = epoch;
        state.partial = partial;
        state.shared = shared;
        let out = self.output(&mut state.partial, &state.shared);
        if let Some(kept) = kept {
            *kept = Some(state);
        }
        out
    }

    /// The answer from a merged partial and distinct-count bitsets.
    fn output(
        &self,
        partial: &mut Partial,
        shared: &[Option<Bitset>],
    ) -> Vec<(Vec<Scalar>, Vec<Scalar>)> {
        // distinct counts per group, counted in parallel
        let groups = partial.rows.len();
        let counts: Vec<Option<Vec<i64>>> = shared
            .iter()
            .map(|bits| bits.as_ref().map(Bitset::counts))
            .collect();
        let mut out = Vec::new();
        for group in 0..groups {
            // without dimensions there is always exactly one row, as in SQL
            if partial.rows[group] == 0 && !self.dims.is_empty() {
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
