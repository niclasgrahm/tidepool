//! Changing the model under stored data (plan §8.2).
//!
//! [`Store::plan_migration`] works out what a new model does to the data
//! held now. Most changes only re-shape tables (a column added reads as
//! virtual NULLs, a removed one is dropped, unchanged columns are shared by
//! the new segments) and re-key dictionaries, which costs nothing per row.
//! Some rewrite data: columns a new relationship joins get one dictionary,
//! with each source dictionary's codes mapped into it, and a column whose
//! type or encoding changed has its values converted.
//!
//! The rewriting is [`build`]. It reads only an immutable [`Snapshot`], so
//! it runs on a thread of its own while the old model keeps serving and
//! ingest carries on. [`Store::migrate`] then catches up on what arrived
//! meanwhile (new rows, and values new to the source dictionaries) and
//! swaps the new layout in, on the writer thread. Without a build (log
//! replay, or a change with nothing to rewrite) it does all of it at once.
//!
//! A dictionary built here is a new dictionary even when it keeps an old
//! id: it gets the store's new generation, so nothing cached about codes
//! of an earlier dictionary of that id is taken for its codes, and it is
//! written to new files.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tidepool_config::table::TableMode;
use tidepool_config::types::ColumnType;
use tidepool_model::Catalog;
use tidepool_model::ir::{ColumnId, Model, Table, TableId};

use crate::column::{ColumnBuilder, ColumnData, Raw, seal};
use crate::dict::{DictSnapshot, DictValue, Dictionary, Key, NULL};
use crate::store::{ColumnLayout, DictId, SegmentData, Snapshot, decode_int, key_of};
use crate::value::{Parsed, Storage, Value, parse_json};

/// A model with its config version and catalog: what a store holds, and
/// what a model change moves it to.
#[derive(Clone, Debug)]
pub struct ModelVersion {
    pub version: u64,
    pub model: Arc<Model>,
    pub catalog: Arc<Catalog>,
}

impl ModelVersion {
    /// Version 1 of `model`, with an empty catalog: for stores that never
    /// see a config change (tests, benchmarks).
    #[must_use]
    pub fn initial(model: Arc<Model>) -> Self {
        Self {
            version: 1,
            model,
            catalog: Arc::new(Catalog::default()),
        }
    }
}

/// Where a new dictionary's values come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DictPlan {
    /// Old dictionary `.0`, codes as they are.
    Keep(DictId),
    /// Built afresh: `base`'s values first, so its codes stay as they were
    /// when the build started; then every other source's, mapped to new
    /// codes; then whatever converted columns bring.
    Built {
        base: Option<DictId>,
        others: Vec<DictId>,
    },
}

impl DictPlan {
    fn sources(&self) -> impl Iterator<Item = DictId> + '_ {
        let (base, others): (Option<DictId>, &[DictId]) = match self {
            Self::Keep(id) => (Some(*id), &[]),
            Self::Built { base, others } => (*base, others),
        };
        base.into_iter().chain(others.iter().copied())
    }
}

/// How one column of the new layout gets its rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColumnPlan {
    /// Old column `.0` as it is: its codes mean the same in the new
    /// dictionary, or it has none and its values didn't change.
    Same(usize),
    /// Every row NULL: the column is new.
    Null,
    /// Old column `.0`'s codes, through the map of its dictionary `.1`
    /// into the new one.
    Remap(usize, DictId),
    /// Old column `.0`'s values, converted to the new type and encoding.
    Convert(usize),
}

pub(crate) struct TablePlan {
    pub id: TableId,
    pub table: Table,
    pub layouts: Arc<[ColumnLayout]>,
    /// How each column is filled; `None` for a new table.
    pub columns: Option<Vec<ColumnPlan>>,
    /// The primary-key index is rebuilt: the key, the mode or the key's
    /// codes changed.
    pub rekey: bool,
}

impl TablePlan {
    /// Whether segments need new column lists (not just the old ones).
    pub fn reshaped(&self, old_columns: usize) -> bool {
        self.columns.as_ref().is_some_and(|plans| {
            plans.len() != old_columns
                || plans
                    .iter()
                    .enumerate()
                    .any(|(i, p)| *p != ColumnPlan::Same(i))
        })
    }

    /// Whether a segment's rows have to be read to fill the new columns.
    pub fn rewrites(&self) -> bool {
        self.columns.as_ref().is_some_and(|plans| {
            plans
                .iter()
                .any(|p| matches!(p, ColumnPlan::Remap(..) | ColumnPlan::Convert(_)))
        })
    }
}

/// What a model change does to the data a store holds.
pub struct Migration {
    pub(crate) target: ModelVersion,
    /// The generation built dictionaries and rewritten segments get.
    pub(crate) generation: u32,
    /// New dictionaries that start from old data, by new id. A new id
    /// without an entry starts empty.
    pub(crate) dicts: BTreeMap<DictId, DictPlan>,
    pub(crate) tables: Vec<TablePlan>,
    /// Rows whose values have to be read to rewrite them (progress).
    pub(crate) rows: u64,
}

impl Migration {
    #[must_use]
    pub fn target(&self) -> &ModelVersion {
        &self.target
    }

    /// Rows a [`build`] reads: 0 when the change only re-shapes and
    /// re-keys, which [`Store::migrate`](crate::Store::migrate) does at
    /// once.
    #[must_use]
    pub fn rows(&self) -> u64 {
        self.rows
    }
}

/// Whether a column's stored form can be read as the new column's
/// unchanged: same storage and the same meaning of each stored value.
fn same_form(old: &ColumnLayout, new: &ColumnLayout) -> bool {
    if old.storage != new.storage || old.dict.is_some() != new.dict.is_some() {
        return false;
    }
    match (old.column_type, new.column_type) {
        (a, b) if a == b => true,
        (ColumnType::Int32, ColumnType::Int64)
        | (ColumnType::Timestamp, ColumnType::TimestampTz)
        | (ColumnType::TimestampTz, ColumnType::Timestamp) => true,
        (
            ColumnType::Decimal { scale: s1, .. },
            ColumnType::Decimal {
                scale: s2,
                precision,
            },
        ) => {
            // a narrower precision may not hold every value: convert (and
            // check) instead
            let ColumnType::Decimal { precision: p1, .. } = old.column_type else {
                return false;
            };
            s1 == s2 && precision >= p1
        }
        _ => false,
    }
}

/// The old state a plan is made from: per table its layouts, mode and key.
pub(crate) struct Old<'a> {
    pub tables: HashMap<TableId, (&'a Table, &'a [ColumnLayout])>,
    /// Old dictionaries and their lengths.
    pub dicts: HashMap<DictId, u32>,
}

pub(crate) fn plan(old: &Old, target: ModelVersion, generation: u32) -> Migration {
    let model = Arc::clone(&target.model);
    let layouts: Vec<(&Table, Arc<[ColumnLayout]>)> = model
        .tables
        .iter()
        .map(|t| (t, t.columns.iter().map(ColumnLayout::new).collect()))
        .collect();

    // per new dictionary: the old dictionaries its columns' codes come
    // from, and whether some column has to be converted into it
    let mut sources: BTreeMap<DictId, (BTreeSet<DictId>, bool)> = BTreeMap::new();
    for (table, new) in &layouts {
        let old_layouts = old.tables.get(&table.id).map(|(_, l)| *l);
        for layout in new.iter() {
            let Some(dict) = layout.dict else { continue };
            let entry = sources.entry(dict).or_default();
            let Some(before) = old_layouts.and_then(|l| l.iter().find(|o| o.id == layout.id))
            else {
                continue;
            };
            match before.dict {
                Some(source) if same_form(before, layout) => {
                    entry.0.insert(source);
                }
                _ => entry.1 = true,
            }
        }
    }
    let mut dicts = BTreeMap::new();
    for (id, (from, converts)) in sources {
        let from: Vec<DictId> = from.into_iter().collect();
        match (from.as_slice(), converts) {
            ([], false) => {}
            ([only], false) => {
                dicts.insert(id, DictPlan::Keep(*only));
            }
            _ => {
                // the biggest source keeps its codes: the fewest rows remapped
                let base = from.iter().copied().max_by_key(|d| {
                    (
                        old.dicts.get(d).copied().unwrap_or(0),
                        std::cmp::Reverse(*d),
                    )
                });
                let others = from.iter().copied().filter(|d| Some(*d) != base).collect();
                dicts.insert(id, DictPlan::Built { base, others });
            }
        }
    }

    let mut tables = Vec::new();
    for (table, new) in layouts {
        let Some((before, old_layouts)) = old.tables.get(&table.id) else {
            tables.push(TablePlan {
                id: table.id,
                table: table.clone(),
                layouts: new,
                columns: None,
                rekey: false,
            });
            continue;
        };
        let columns: Vec<ColumnPlan> = new
            .iter()
            .map(|layout| {
                let Some(i) = old_layouts.iter().position(|o| o.id == layout.id) else {
                    return ColumnPlan::Null;
                };
                let was = &old_layouts[i];
                if !same_form(was, layout) {
                    return ColumnPlan::Convert(i);
                }
                match (was.dict, layout.dict.and_then(|d| dicts.get(&d))) {
                    (None, _) | (Some(_), Some(DictPlan::Keep(_))) => ColumnPlan::Same(i),
                    (Some(source), _) => ColumnPlan::Remap(i, source),
                }
            })
            .collect();
        let key_changed = table.primary_key.iter().any(|key| {
            new.iter()
                .position(|l| l.id == *key)
                .is_none_or(|j| !matches!(columns[j], ColumnPlan::Same(_)))
        });
        let rekey =
            before.mode != table.mode || before.primary_key != table.primary_key || key_changed;
        tables.push(TablePlan {
            id: table.id,
            table: table.clone(),
            layouts: new,
            columns: Some(columns),
            rekey,
        });
    }
    Migration {
        target,
        generation,
        dicts,
        tables,
        rows: 0,
    }
}

/// Dictionary values by code: the live dictionaries (the writer's) or a
/// snapshot's (a build's).
pub(crate) trait Values {
    fn value(&self, dict: DictId, code: u32) -> DictValue<'_>;
    fn count(&self, dict: DictId) -> u32;
}

impl Values for HashMap<DictId, Dictionary> {
    fn value(&self, dict: DictId, code: u32) -> DictValue<'_> {
        self.get(&dict).map_or(DictValue::Null, |d| d.get(code))
    }
    fn count(&self, dict: DictId) -> u32 {
        self.get(&dict).map_or(0, Dictionary::len)
    }
}

impl Values for HashMap<DictId, DictSnapshot> {
    fn value(&self, dict: DictId, code: u32) -> DictValue<'_> {
        self.get(&dict).map_or(DictValue::Null, |d| d.get(code))
    }
    fn count(&self, dict: DictId) -> u32 {
        self.get(&dict).map_or(0, DictSnapshot::len)
    }
}

fn key_of_value(value: DictValue<'_>) -> Option<Key<'_>> {
    match value {
        DictValue::Null => None,
        DictValue::Int(v) => Some(Key::Int(v)),
        DictValue::Str(s) => Some(Key::Str(s)),
    }
}

/// A dictionary being built, and how each source's codes map into it.
#[derive(Clone, Debug)]
pub(crate) struct Built {
    pub dict: Dictionary,
    /// Per source: new code by old code (index 0 is NULL → NULL).
    pub remaps: HashMap<DictId, Vec<u32>>,
    /// The source whose codes were kept, and up to which code they are.
    pub base: Option<(DictId, u32)>,
}

impl Built {
    fn new(storage: Storage, generation: u32) -> Self {
        Self {
            dict: Dictionary::new(storage).with_generation(generation),
            remaps: HashMap::new(),
            base: None,
        }
    }

    /// Take in every value of the sources not taken in yet: the base first
    /// (so, into an empty dictionary, its codes stay the same), then the
    /// others in order.
    fn extend(&mut self, plan: &DictPlan, values: &impl Values) {
        let first = self.dict.is_empty();
        for (n, source) in plan.sources().enumerate() {
            let remap = self.remaps.entry(source).or_insert_with(|| vec![NULL]);
            let done = u32::try_from(remap.len() - 1).unwrap_or(u32::MAX);
            for code in done + 1..=values.count(source) {
                let new = key_of_value(values.value(source, code))
                    .map_or(NULL, |key| self.dict.encode(&key));
                remap.push(new);
            }
            if first
                && n == 0
                && let DictPlan::Built {
                    base: Some(base), ..
                } = plan
            {
                self.base = Some((*base, self.dict.len()));
            }
        }
    }
}

/// What a build produced: the new dictionaries so far, and segments
/// already rewritten (by table and the address of the old segment data).
#[derive(Default)]
pub struct Rebuilt {
    pub(crate) dicts: HashMap<DictId, Built>,
    pub(crate) segments: HashMap<(TableId, usize), Arc<SegmentData>>,
}

pub(crate) fn address(data: &Arc<SegmentData>) -> usize {
    Arc::as_ptr(data) as usize
}

/// The logical value of one stored value of an old column.
fn logical(values: &impl Values, layout: &ColumnLayout, raw: Raw) -> Value {
    match raw {
        Raw::Null => Value::Null,
        Raw::Code(code) => match layout.dict.map(|d| values.value(d, code)) {
            Some(DictValue::Str(s)) => Value::String(s.to_string()),
            Some(DictValue::Int(v)) => decode_int(layout.column_type, v),
            Some(DictValue::Null) | None => Value::Null,
        },
        Raw::I64(v) => Value::from_i64(layout.column_type, v),
        Raw::I128(v) => decode_int(layout.column_type, v),
        Raw::F64(v) => Value::Float(v),
        Raw::Str(s) => Value::String(s.to_string()),
    }
}

/// 10^n, if it fits.
fn power(n: u8) -> Option<i128> {
    10i128.checked_pow(u32::from(n))
}

/// `value` as a value of type `to`: exactly, or `None` (stored as NULL)
/// when it doesn't fit or doesn't read as one.
pub(crate) fn cast(value: &Value, to: ColumnType) -> Option<Parsed> {
    const DAY: i64 = 86_400_000_000;
    let scaled = match value {
        Value::Int(v) => Some((i128::from(*v), 0u8)),
        Value::Decimal { value, scale } => Some((*value, *scale)),
        _ => None,
    };
    match (value, to, scaled) {
        (Value::Null, _, _) => return None,
        (_, ColumnType::Int32 | ColumnType::Int64, Some((v, scale))) => {
            let unit = power(scale)?;
            if v % unit != 0 {
                return None;
            }
            let v = i64::try_from(v / unit).ok()?;
            if to == ColumnType::Int32 && i32::try_from(v).is_err() {
                return None;
            }
            return Some(Parsed::I64(v));
        }
        (_, ColumnType::Decimal { precision, scale }, Some((v, from))) => {
            let v = if scale >= from {
                v.checked_mul(power(scale - from)?)?
            } else {
                let unit = power(from - scale)?;
                if v % unit != 0 {
                    return None;
                }
                v / unit
            };
            if v.unsigned_abs() >= power(precision)?.unsigned_abs() {
                return None;
            }
            return Some(if precision <= 18 {
                Parsed::I64(i64::try_from(v).ok()?)
            } else {
                Parsed::I128(v)
            });
        }
        (Value::Date(d), ColumnType::Timestamp | ColumnType::TimestampTz, _) => {
            return i64::from(*d).checked_mul(DAY).map(Parsed::I64);
        }
        (
            Value::Timestamp(m) | Value::TimestampTz(m),
            ColumnType::Timestamp | ColumnType::TimestampTz,
            _,
        ) => return Some(Parsed::I64(*m)),
        (Value::Timestamp(m) | Value::TimestampTz(m), ColumnType::Date, _) => {
            return Some(Parsed::I64(m.div_euclid(DAY)));
        }
        _ => {}
    }
    // everything else through the value's text, as ingest would read it
    let json = value.to_json();
    if let Ok(parsed) = parse_json(to, &json.to_string()) {
        return Some(parsed);
    }
    if let Value::String(text) = value
        && let Ok(parsed) = parse_json(to, text)
    {
        return Some(parsed);
    }
    (to == ColumnType::String).then(|| {
        Parsed::Str(match json {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        })
    })
}

/// A parsed value as stored in a plain (not dictionary) column.
fn plain(parsed: &Parsed) -> Raw<'_> {
    match parsed {
        Parsed::Bool(b) => Raw::I64(i64::from(*b)),
        Parsed::I64(v) => Raw::I64(*v),
        Parsed::I128(v) => Raw::I128(*v),
        Parsed::F64(v) => Raw::F64(*v),
        Parsed::Str(s) => Raw::Str(s),
    }
}

fn finish(builder: ColumnBuilder, sealed: bool, layout: &ColumnLayout) -> ColumnData {
    let column = builder.freeze();
    if sealed {
        seal(&[&column], layout.storage, layout.dict.is_some())
    } else {
        column
    }
}

/// The largest code a code column holds.
fn max_code(column: &ColumnData) -> u32 {
    match column {
        ColumnData::PackedCodes { max, .. } => *max,
        ColumnData::Codes(codes) => codes.iter().copied().max().unwrap_or(0),
        _ => 0,
    }
}

/// `data` (written in `old` layouts) in the `new` layouts.
pub(crate) fn convert_segment(
    data: &SegmentData,
    plans: &[ColumnPlan],
    old: &[ColumnLayout],
    new: &[ColumnLayout],
    values: &impl Values,
    built: &mut HashMap<DictId, Built>,
    generation: u32,
) -> SegmentData {
    let rows = data.rows as usize;
    let columns = plans
        .iter()
        .zip(new)
        .map(|(plan, layout)| match *plan {
            ColumnPlan::Same(i) => Arc::clone(&data.columns[i]),
            ColumnPlan::Null => Arc::new(ColumnData::Null {
                rows: data.rows,
                dict: layout.dict.is_some(),
            }),
            ColumnPlan::Remap(i, source) => {
                let column = &data.columns[i];
                let target = layout.dict.and_then(|d| built.get(&d));
                let remap = target.and_then(|b| b.remaps.get(&source).map(|r| (b, r)));
                match remap {
                    // the base's codes, all still where they were
                    Some((b, _))
                        if b.base.is_some_and(|(base, upto)| {
                            base == source && max_code(column) <= upto
                        }) =>
                    {
                        Arc::clone(column)
                    }
                    Some((_, remap)) => {
                        let mut builder = ColumnBuilder::new(layout.storage, true);
                        for row in 0..rows {
                            let code = match column.get(row) {
                                Raw::Code(code) => {
                                    remap.get(code as usize).copied().unwrap_or(NULL)
                                }
                                _ => NULL,
                            };
                            builder.push(Raw::Code(code));
                        }
                        Arc::new(finish(builder, data.sealed, layout))
                    }
                    None => Arc::clone(column),
                }
            }
            ColumnPlan::Convert(i) => {
                let column = &data.columns[i];
                let mut builder = ColumnBuilder::new(layout.storage, layout.dict.is_some());
                for row in 0..rows {
                    let value = logical(values, &old[i], column.get(row));
                    let parsed = cast(&value, layout.column_type);
                    let raw = match (&parsed, layout.dict) {
                        (None, Some(_)) => Raw::Code(NULL),
                        (None, None) => Raw::Null,
                        (Some(parsed), Some(dict)) => {
                            let b = built
                                .entry(dict)
                                .or_insert_with(|| Built::new(layout.storage, generation));
                            Raw::Code(b.dict.encode(&key_of(parsed)))
                        }
                        (Some(parsed), None) => plain(parsed),
                    };
                    builder.push(raw);
                }
                Arc::new(finish(builder, data.sealed, layout))
            }
        })
        .collect();
    SegmentData {
        start: data.start,
        rows: data.rows,
        columns,
        sealed: data.sealed,
        generation,
    }
}

/// Start the built dictionaries of `migration` and bring them up to
/// `values`.
pub(crate) fn extend_dicts(
    migration: &Migration,
    built: &mut HashMap<DictId, Built>,
    values: &impl Values,
    storage_of: impl Fn(DictId) -> Storage,
) {
    for (id, plan) in &migration.dicts {
        if let DictPlan::Built { .. } = plan {
            built
                .entry(*id)
                .or_insert_with(|| Built::new(storage_of(*id), migration.generation))
                .extend(plan, values);
        }
    }
}

/// The storage of new dictionary `id`, from the layouts that use it.
pub(crate) fn dict_storage(migration: &Migration, id: DictId) -> Storage {
    migration
        .tables
        .iter()
        .flat_map(|t| t.layouts.iter())
        .find(|l| l.dict == Some(id))
        .map_or(Storage::Str, |l| l.storage)
}

/// The rewriting part of a migration, from an immutable snapshot of the
/// store as it was: built dictionaries and rewritten segments, for
/// [`Store::migrate`](crate::Store::migrate) to finish. Counts rows into
/// `progress` as it goes.
#[must_use]
pub fn build(migration: &Migration, snapshot: &Snapshot, progress: &AtomicU64) -> Rebuilt {
    let mut rebuilt = Rebuilt::default();
    extend_dicts(migration, &mut rebuilt.dicts, &snapshot.dicts, |id| {
        dict_storage(migration, id)
    });
    for table in migration.tables.iter().filter(|t| t.rewrites()) {
        let (Some(plans), Some(old)) = (&table.columns, snapshot.table(table.id)) else {
            continue;
        };
        for view in old.segments.iter() {
            let data = convert_segment(
                &view.data,
                plans,
                &old.layouts,
                &table.layouts,
                &snapshot.dicts,
                &mut rebuilt.dicts,
                migration.generation,
            );
            rebuilt
                .segments
                .insert((table.id, address(&view.data)), Arc::new(data));
            progress.fetch_add(u64::from(view.data.rows), Ordering::Relaxed);
        }
    }
    rebuilt
}

/// Whether a table's mode keeps a primary-key index.
pub(crate) fn keyed(table: &Table) -> bool {
    table.mode == TableMode::Upsert
}

/// Ids of a table's columns, in order: the shape a batch decoded for it has.
#[must_use]
pub fn shape(table: &Table) -> Vec<ColumnId> {
    table.columns.iter().map(|c| c.id).collect()
}

/// A model version as bytes, for the log and checkpoints.
pub(crate) fn encode_version(version: &ModelVersion) -> Vec<u8> {
    serde_json::json!({
        "version": version.version,
        "model": &*version.model,
        "catalog": &*version.catalog,
    })
    .to_string()
    .into_bytes()
}

pub(crate) fn decode_version(bytes: &[u8]) -> std::io::Result<ModelVersion> {
    #[derive(serde::Deserialize)]
    struct Stored {
        version: u64,
        model: Model,
        catalog: Catalog,
    }
    let stored: Stored = serde_json::from_slice(bytes)
        .map_err(|e| std::io::Error::other(format!("a stored model doesn't read back: {e}")))?;
    Ok(ModelVersion {
        version: stored.version,
        model: Arc::new(stored.model),
        catalog: Arc::new(stored.catalog),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_convert_exactly_or_not_at_all() {
        let dec = |precision, scale| ColumnType::Decimal { precision, scale };
        assert_eq!(cast(&Value::Int(12), dec(10, 2)), Some(Parsed::I64(1200)));
        assert_eq!(
            cast(
                &Value::Decimal {
                    value: 1234,
                    scale: 2
                },
                dec(30, 4)
            ),
            Some(Parsed::I128(123_400))
        );
        // narrowing that would lose a digit, or overflow
        assert_eq!(
            cast(
                &Value::Decimal {
                    value: 1234,
                    scale: 2
                },
                dec(10, 1)
            ),
            None
        );
        assert_eq!(cast(&Value::Int(1000), dec(3, 0)), None);
        assert_eq!(cast(&Value::Int(1 << 40), ColumnType::Int32), None);
        assert_eq!(
            cast(
                &Value::Decimal {
                    value: 500,
                    scale: 2
                },
                ColumnType::Int64
            ),
            Some(Parsed::I64(5))
        );
    }

    #[test]
    fn text_and_times_convert_through_what_ingest_accepts() {
        assert_eq!(
            cast(&Value::String("42".into()), ColumnType::Int64),
            Some(Parsed::I64(42))
        );
        assert_eq!(
            cast(&Value::String("forty".into()), ColumnType::Int64),
            None
        );
        assert_eq!(
            cast(&Value::Int(7), ColumnType::String),
            Some(Parsed::Str("7".into()))
        );
        assert_eq!(
            cast(
                &Value::Decimal {
                    value: 705,
                    scale: 2
                },
                ColumnType::String
            ),
            Some(Parsed::Str("7.05".into()))
        );
        assert_eq!(
            cast(&Value::Date(1), ColumnType::Timestamp),
            Some(Parsed::I64(86_400_000_000))
        );
        assert_eq!(
            cast(&Value::Timestamp(86_400_000_001), ColumnType::Date),
            Some(Parsed::I64(1))
        );
        assert_eq!(cast(&Value::Null, ColumnType::String), None);
    }
}
