//! The store: every table's rows, every dictionary, and the epochs that make
//! them visible.
//!
//! Only the writer (one thread, [`crate::writer`]) mutates a [`Store`].
//! Readers never touch it: [`Store::publish`] produces an immutable
//! [`Snapshot`] of everything applied so far, and that is what queries read.
//!
//! A table is a list of segments covering consecutive row ids, plus the rows
//! applied since the last publish (the builder). Publishing freezes the
//! builder into a new segment, a *chunk*. When the chunks hold enough rows (or
//! there are too many of them), they are sealed into one bit-packed segment.
//! Every segment is immutable once published; deletes live beside it in a
//! copy-on-write bitmap, so a snapshot keeps the bitmap it was published
//! with.
//!
//! Row ids never change, which is what lets the primary-key index point at
//! rows across sealing.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use roaring::RoaringBitmap;
use tidepool_config::table::{Encoding, TableMode};
use tidepool_config::types::ColumnType;
use tidepool_model::ir::{ColumnId, DomainId, Model, Table, TableId};

use crate::batch::Batch;
use crate::column::{ColumnBuilder, ColumnData, Raw, seal};
use crate::dict::{DictSnapshot, DictValue, Dictionary, Key, NULL};
use crate::value::{Parsed, Storage, Value};

/// Seal chunks into a segment once they hold this many rows.
pub const DEFAULT_SEAL_ROWS: usize = 1 << 20;
/// Seal chunks however few rows they hold once there are this many: many
/// tiny chunks (a slow trickle of rows) would make every scan slow.
pub const MAX_CHUNKS: usize = 64;

/// Which dictionary a column's codes refer to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DictId {
    Column(ColumnId),
    /// Shared by the columns relationships join.
    Domain(DomainId),
}

/// How one column is stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnLayout {
    pub id: ColumnId,
    pub name: String,
    pub column_type: ColumnType,
    pub storage: Storage,
    pub dict: Option<DictId>,
}

impl ColumnLayout {
    fn new(column: &tidepool_model::ir::Column) -> Self {
        let dict = (column.encoding == Encoding::Dict).then(|| {
            column
                .key_domain
                .map_or(DictId::Column(column.id), DictId::Domain)
        });
        Self {
            id: column.id,
            name: column.name.clone(),
            column_type: column.column_type,
            storage: Storage::of(column.column_type),
            dict,
        }
    }
}

/// The immutable rows of a segment.
#[derive(Debug)]
pub struct SegmentData {
    /// Row id of the first row.
    pub start: u64,
    pub rows: u32,
    /// One per table column, in the table's column order.
    pub columns: Vec<ColumnData>,
    /// Bit-packed (sealed) rather than a fresh chunk.
    pub sealed: bool,
}

impl SegmentData {
    fn end(&self) -> u64 {
        self.start + u64::from(self.rows)
    }

    #[must_use]
    pub fn bytes(&self) -> usize {
        self.columns.iter().map(ColumnData::bytes).sum()
    }
}

/// A segment as published: its rows, which of them are deleted, and a
/// version that changes whenever the deletes do (so anything cached about the
/// segment can tell it is stale).
#[derive(Clone, Debug)]
pub struct SegmentView {
    pub data: Arc<SegmentData>,
    pub deletes: Arc<RoaringBitmap>,
    pub version: u64,
}

/// The primary-key index of an upsert table: key → row id of the live row.
#[derive(Debug)]
enum PkIndex {
    /// One key column: indexed directly by its dictionary code.
    Single(Vec<u64>),
    Composite(HashMap<Box<[u32]>, u64>),
}

const ABSENT: u64 = u64::MAX;

impl PkIndex {
    fn get(&self, key: &[u32]) -> Option<u64> {
        match self {
            Self::Single(rows) => rows.get(key[0] as usize).copied().filter(|r| *r != ABSENT),
            Self::Composite(rows) => rows.get(key).copied(),
        }
    }

    fn insert(&mut self, key: &[u32], row: u64) {
        match self {
            Self::Single(rows) => {
                let code = key[0] as usize;
                if rows.len() <= code {
                    rows.resize(code + 1, ABSENT);
                }
                rows[code] = row;
            }
            Self::Composite(rows) => {
                rows.insert(key.into(), row);
            }
        }
    }

    fn remove(&mut self, key: &[u32]) {
        match self {
            Self::Single(rows) => {
                if let Some(slot) = rows.get_mut(key[0] as usize) {
                    *slot = ABSENT;
                }
            }
            Self::Composite(rows) => {
                rows.remove(key);
            }
        }
    }
}

/// What a write does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Insert rows; for an upsert table, a row with an existing key replaces it.
    Write,
    /// Delete rows by key (upsert tables only). The batch holds key columns only.
    Delete,
}

/// What applying a batch did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Rows written.
    pub written: usize,
    /// Rows that replaced or deleted an existing row.
    pub replaced: usize,
    /// Keys that matched no row (deletes only).
    pub missing: usize,
}

struct TableStore {
    table: Table,
    layouts: Arc<[ColumnLayout]>,
    /// Positions of the primary-key columns in `layouts`.
    key_columns: Vec<usize>,
    segments: Vec<SegmentView>,
    builder: Vec<ColumnBuilder>,
    builder_start: u64,
    /// Deleted rows among the builder's, by offset.
    builder_deletes: RoaringBitmap,
    next_row: u64,
    live_rows: u64,
    pk: Option<PkIndex>,
    /// Columns whose value changed for some existing row since the last
    /// publish (an upsert that changed `region`, say).
    changed: BTreeSet<ColumnId>,
    dirty: bool,
    seal_rows: usize,
    written_total: u64,
    rate_samples: VecDeque<(Instant, u64)>,
    published: Option<Arc<TableSnapshot>>,
}

impl TableStore {
    fn new(table: &Table, seal_rows: usize) -> Self {
        let layouts: Vec<ColumnLayout> = table.columns.iter().map(ColumnLayout::new).collect();
        let key_columns = table
            .primary_key
            .iter()
            .filter_map(|id| layouts.iter().position(|l| l.id == *id))
            .collect::<Vec<_>>();
        let pk = (table.mode == TableMode::Upsert).then(|| {
            if key_columns.len() == 1 {
                PkIndex::Single(Vec::new())
            } else {
                PkIndex::Composite(HashMap::new())
            }
        });
        let builder = layouts
            .iter()
            .map(|l| ColumnBuilder::new(l.storage, l.dict.is_some()))
            .collect();
        Self {
            table: table.clone(),
            layouts: layouts.into(),
            key_columns,
            segments: Vec::new(),
            builder,
            builder_start: 0,
            builder_deletes: RoaringBitmap::new(),
            next_row: 0,
            live_rows: 0,
            pk,
            changed: BTreeSet::new(),
            dirty: false,
            seal_rows,
            written_total: 0,
            rate_samples: VecDeque::new(),
            published: None,
        }
    }

    /// The stored form of one parsed value of column `index`.
    fn encode<'a>(
        &self,
        dicts: &mut HashMap<DictId, Dictionary>,
        index: usize,
        value: Option<&'a Parsed>,
    ) -> Raw<'a> {
        let layout = &self.layouts[index];
        let Some(value) = value else {
            return if layout.dict.is_some() {
                Raw::Code(NULL)
            } else {
                Raw::Null
            };
        };
        if let Some(dict) = layout.dict {
            let dictionary = dicts
                .entry(dict)
                .or_insert_with(|| Dictionary::new(layout.storage));
            let code = dictionary.encode(&key_of(value));
            return Raw::Code(code);
        }
        match value {
            Parsed::Bool(b) => Raw::I64(i64::from(*b)),
            Parsed::I64(v) => Raw::I64(*v),
            Parsed::I128(v) => Raw::I128(*v),
            Parsed::F64(v) => Raw::F64(*v),
            Parsed::Str(s) => Raw::Str(s),
        }
    }

    /// The value of column `index` of `row`, as stored; `None` if the row
    /// isn't stored here.
    fn stored(&self, row: u64, index: usize) -> Option<Raw<'_>> {
        if row >= self.builder_start {
            let offset = usize::try_from(row - self.builder_start).ok()?;
            return Some(builder_get(&self.builder[index], offset));
        }
        let segment = self.segment_of(row)?;
        let offset = usize::try_from(row - segment.data.start).ok()?;
        Some(segment.data.columns[index].get(offset))
    }

    fn segment_of(&self, row: u64) -> Option<&SegmentView> {
        let at = self.segments.partition_point(|s| s.data.end() <= row);
        self.segments.get(at).filter(|s| s.data.start <= row)
    }

    /// Mark `row` deleted. Returns the version-bump target, if a published
    /// segment changed.
    fn delete_row(&mut self, row: u64, versions: &mut u64) {
        if row >= self.builder_start {
            if let Ok(offset) = u32::try_from(row - self.builder_start) {
                self.builder_deletes.insert(offset);
            }
        } else {
            let at = self.segments.partition_point(|s| s.data.end() <= row);
            if let Some(segment) = self.segments.get_mut(at) {
                let offset = u32::try_from(row - segment.data.start).unwrap_or(u32::MAX);
                // copy-on-write: the published snapshot keeps its bitmap
                Arc::make_mut(&mut segment.deletes).insert(offset);
                *versions += 1;
                segment.version = *versions;
            }
        }
        self.live_rows = self.live_rows.saturating_sub(1);
    }

    fn apply(
        &mut self,
        dicts: &mut HashMap<DictId, Dictionary>,
        op: Op,
        batch: &Batch,
        versions: &mut u64,
    ) -> Applied {
        let mut applied = Applied::default();
        if batch.rows == 0 {
            return applied;
        }
        self.dirty = true;
        let mut key = vec![0u32; self.key_columns.len()];
        // A delete batch holds the key columns only, in *table* order; the
        // index is keyed in *primary key* order. Batch column j is table
        // column `deleted[j]`, at key position `slot[j]`.
        let deleted: Vec<usize> = (0..self.layouts.len())
            .filter(|i| self.key_columns.contains(i))
            .collect();
        let slot: Vec<usize> = deleted
            .iter()
            .map(|i| self.key_columns.iter().position(|k| k == i).unwrap_or(0))
            .collect();
        for row in 0..batch.rows {
            if op == Op::Delete {
                for (j, column) in batch.columns.iter().enumerate() {
                    let index = deleted[j];
                    key[slot[j]] = match self.layouts[index].dict {
                        Some(dict) => column[row]
                            .as_ref()
                            .and_then(|v| dicts.get(&dict).and_then(|d| d.lookup(&key_of(v))))
                            .unwrap_or(NULL),
                        None => NULL,
                    };
                }
                match self.pk.as_ref().and_then(|pk| pk.get(&key)) {
                    Some(existing) => {
                        self.delete_row(existing, versions);
                        if let Some(pk) = &mut self.pk {
                            pk.remove(&key);
                        }
                        applied.replaced += 1;
                    }
                    None => applied.missing += 1,
                }
                continue;
            }

            let raws: Vec<Raw> = (0..self.layouts.len())
                .map(|index| self.encode(dicts, index, batch.columns[index][row].as_ref()))
                .collect();
            let new_row = self.next_row;
            if self.pk.is_some() {
                for (k, index) in self.key_columns.iter().enumerate() {
                    key[k] = match raws[*index] {
                        Raw::Code(code) => code,
                        _ => NULL,
                    };
                }
                if let Some(existing) = self.pk.as_ref().and_then(|pk| pk.get(&key)) {
                    for (index, raw) in raws.iter().enumerate() {
                        if self.stored(existing, index).is_some_and(|old| old != *raw) {
                            self.changed.insert(self.layouts[index].id);
                        }
                    }
                    self.delete_row(existing, versions);
                    applied.replaced += 1;
                }
                if let Some(pk) = &mut self.pk {
                    pk.insert(&key, new_row);
                }
            }
            for (builder, raw) in self.builder.iter_mut().zip(&raws) {
                builder.push(*raw);
            }
            self.next_row += 1;
            self.live_rows += 1;
            applied.written += 1;
        }
        self.written_total += applied.written as u64;
        applied
    }

    /// Freeze the builder into a chunk, and seal chunks if it's time.
    fn freeze(&mut self, versions: &mut u64) {
        let rows = self.builder.first().map_or(0, ColumnBuilder::len);
        if rows > 0 {
            let builder = self
                .layouts
                .iter()
                .map(|l| ColumnBuilder::new(l.storage, l.dict.is_some()))
                .collect();
            let columns = std::mem::replace(&mut self.builder, builder)
                .into_iter()
                .map(ColumnBuilder::freeze)
                .collect();
            *versions += 1;
            self.segments.push(SegmentView {
                data: Arc::new(SegmentData {
                    start: self.builder_start,
                    rows: u32::try_from(rows).unwrap_or(u32::MAX),
                    columns,
                    sealed: false,
                }),
                deletes: Arc::new(std::mem::take(&mut self.builder_deletes)),
                version: *versions,
            });
            self.builder_start = self.next_row;
        }

        let chunks = self
            .segments
            .iter()
            .rev()
            .take_while(|s| !s.data.sealed)
            .count();
        let chunk_rows: u64 = self
            .segments
            .iter()
            .rev()
            .take(chunks)
            .map(|s| u64::from(s.data.rows))
            .sum();
        if chunks > 0 && (chunk_rows >= self.seal_rows as u64 || chunks > MAX_CHUNKS) {
            self.seal_chunks(chunks, versions);
        }
    }

    /// Replace the last `chunks` segments by one sealed segment.
    fn seal_chunks(&mut self, chunks: usize, versions: &mut u64) {
        let parts = self.segments.split_off(self.segments.len() - chunks);
        let start = parts[0].data.start;
        let mut deletes = RoaringBitmap::new();
        for part in &parts {
            let shift = u32::try_from(part.data.start - start).unwrap_or(u32::MAX);
            deletes.extend(part.deletes.iter().map(|offset| offset + shift));
        }
        let columns = (0..self.layouts.len())
            .map(|index| {
                let layout = &self.layouts[index];
                let pieces: Vec<&ColumnData> =
                    parts.iter().map(|p| &p.data.columns[index]).collect();
                seal(&pieces, layout.storage, layout.dict.is_some())
            })
            .collect();
        let rows: u32 = parts.iter().map(|p| p.data.rows).sum();
        *versions += 1;
        self.segments.push(SegmentView {
            data: Arc::new(SegmentData {
                start,
                rows,
                columns,
                sealed: true,
            }),
            deletes: Arc::new(deletes),
            version: *versions,
        });
    }

    fn snapshot(&mut self, now: Instant) -> Arc<TableSnapshot> {
        if !self.dirty
            && let Some(published) = &self.published
        {
            return Arc::clone(published);
        }
        self.rate_samples.push_back((now, self.written_total));
        while self
            .rate_samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t).as_secs_f64() > 10.0)
        {
            self.rate_samples.pop_front();
        }
        let rate = match (self.rate_samples.front(), self.rate_samples.back()) {
            (Some((t0, n0)), Some((t1, n1))) if t1 > t0 => {
                #[allow(clippy::cast_precision_loss)]
                let rate = (n1 - n0) as f64 / t1.duration_since(*t0).as_secs_f64();
                rate
            }
            _ => 0.0,
        };
        let snapshot = Arc::new(TableSnapshot {
            id: self.table.id,
            layouts: Arc::clone(&self.layouts),
            segments: self.segments.clone().into(),
            live_rows: self.live_rows,
            changed: std::mem::take(&mut self.changed).into_iter().collect(),
            written_total: self.written_total,
            rows_per_second: rate,
        });
        self.dirty = false;
        self.published = Some(Arc::clone(&snapshot));
        snapshot
    }
}

fn builder_get(builder: &ColumnBuilder, offset: usize) -> Raw<'_> {
    match builder {
        ColumnBuilder::Codes(codes) => Raw::Code(codes[offset]),
        ColumnBuilder::I64 { values, nulls } if !nulls.as_ref().is_some_and(|n| n.get(offset)) => {
            Raw::I64(values[offset])
        }
        ColumnBuilder::I128 { values, nulls } if !nulls.as_ref().is_some_and(|n| n.get(offset)) => {
            Raw::I128(values[offset])
        }
        ColumnBuilder::F64 { values, nulls } if !nulls.as_ref().is_some_and(|n| n.get(offset)) => {
            Raw::F64(values[offset])
        }
        ColumnBuilder::Str {
            bytes,
            offsets,
            nulls,
        } if !nulls.as_ref().is_some_and(|n| n.get(offset)) => {
            let (start, end) = (offsets[offset] as usize, offsets[offset + 1] as usize);
            Raw::Str(std::str::from_utf8(&bytes[start..end]).unwrap_or_default())
        }
        _ => Raw::Null,
    }
}

/// The dictionary key of a parsed value.
fn key_of(value: &Parsed) -> Key<'_> {
    match value {
        Parsed::Bool(b) => Key::Int(i128::from(*b)),
        Parsed::I64(v) => Key::Int(i128::from(*v)),
        Parsed::I128(v) => Key::Int(*v),
        Parsed::F64(v) => Key::Int(i128::from(v.to_bits())),
        Parsed::Str(s) => Key::Str(s),
    }
}

/// Everything, for the writer.
pub struct Store {
    model: Arc<Model>,
    tables: HashMap<TableId, TableStore>,
    dicts: HashMap<DictId, Dictionary>,
    epoch: u64,
    /// Store-wide counter for segment versions.
    versions: u64,
}

impl Store {
    #[must_use]
    pub fn new(model: Arc<Model>) -> Self {
        Self::with_seal_rows(model, DEFAULT_SEAL_ROWS)
    }

    /// A store that seals chunks at `seal_rows` rows (small values for tests).
    #[must_use]
    pub fn with_seal_rows(model: Arc<Model>, seal_rows: usize) -> Self {
        let tables = model
            .tables
            .iter()
            .map(|t| (t.id, TableStore::new(t, seal_rows)))
            .collect();
        Self {
            model,
            tables,
            dicts: HashMap::new(),
            epoch: 0,
            versions: 0,
        }
    }

    /// A store holding recovered data: `segments` per table (with their
    /// delete bitmaps), dictionaries with every code those segments use,
    /// and per table (live rows, rows written in total). Primary-key
    /// indexes are rebuilt from the key columns. Recovery only.
    #[must_use]
    pub fn restore(
        model: Arc<Model>,
        seal_rows: usize,
        epoch: u64,
        dicts: HashMap<DictId, Dictionary>,
        segments: HashMap<TableId, Vec<SegmentView>>,
        counts: &HashMap<TableId, (u64, u64)>,
    ) -> Self {
        let mut store = Self::with_seal_rows(model, seal_rows);
        store.epoch = epoch;
        store.dicts = dicts;
        for (id, views) in segments {
            let Some(table) = store.tables.get_mut(&id) else {
                continue;
            };
            store.versions = views
                .iter()
                .map(|v| v.version)
                .fold(store.versions, u64::max);
            let end = views.last().map_or(0, |v| v.data.end());
            for view in &views {
                let Some(pk) = table.pk.as_mut() else { break };
                let mut key = vec![0u32; table.key_columns.len()];
                for offset in 0..view.data.rows {
                    if view.deletes.contains(offset) {
                        continue;
                    }
                    for (slot, column) in table.key_columns.iter().enumerate() {
                        key[slot] = match view.data.columns[*column].get(offset as usize) {
                            Raw::Code(code) => code,
                            _ => NULL,
                        };
                    }
                    pk.insert(&key, view.data.start + u64::from(offset));
                }
            }
            table.segments = views;
            table.next_row = end;
            table.builder_start = end;
            let (live, written) = counts.get(&id).copied().unwrap_or((0, 0));
            table.live_rows = live;
            table.written_total = written;
            table.dirty = true;
        }
        store
    }

    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Retention (plan §4.6): delete every row of sealed segments whose
    /// event time is entirely older than the table's retention as of `now`.
    /// Returns how many rows went. Expiry isn't logged: after a restart,
    /// rows expired since the last checkpoint come back until it runs again.
    pub fn expire(&mut self, now: std::time::SystemTime) -> usize {
        let now = now
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX));
        let mut expired = 0;
        let versions = &mut self.versions;
        for table in self.tables.values_mut() {
            let (Some(event), Some(retention)) = (table.table.event_time, table.table.retention)
            else {
                continue;
            };
            let Some(column) = table.layouts.iter().position(|l| l.id == event) else {
                continue;
            };
            let keep = i64::try_from(retention.0.as_micros()).unwrap_or(i64::MAX);
            let cutoff = match table.layouts[column].column_type {
                ColumnType::Date => (now - keep).div_euclid(86_400_000_000),
                _ => now - keep,
            };
            let old: Vec<(u64, u32)> = table
                .segments
                .iter()
                .filter(|s| s.data.sealed && table.layouts[column].dict.is_none())
                .filter(|s| {
                    s.data.columns[column]
                        .range()
                        .is_some_and(|(_, max)| max < cutoff)
                })
                .filter(|s| s.deletes.len() < u64::from(s.data.rows))
                .map(|s| (s.data.start, s.data.rows))
                .collect();
            for (start, rows) in old {
                for offset in 0..rows {
                    let row = start + u64::from(offset);
                    let deleted = table
                        .segment_of(row)
                        .is_none_or(|s| s.deletes.contains(offset));
                    if deleted {
                        continue;
                    }
                    if table.pk.is_some() {
                        let key: Vec<u32> = table
                            .key_columns
                            .iter()
                            .map(|c| match table.stored(row, *c) {
                                Some(Raw::Code(code)) => code,
                                _ => NULL,
                            })
                            .collect();
                        if let Some(pk) = table.pk.as_mut()
                            && pk.get(&key) == Some(row)
                        {
                            pk.remove(&key);
                        }
                    }
                    table.delete_row(row, versions);
                    table.dirty = true;
                    expired += 1;
                }
            }
        }
        expired
    }

    #[must_use]
    pub fn model(&self) -> &Arc<Model> {
        &self.model
    }

    /// Apply a decoded batch. Visible to readers at the next publish.
    ///
    /// # Panics
    /// Never on a batch decoded for this table; the decoders guarantee the
    /// shape.
    pub fn apply(&mut self, table: TableId, op: Op, batch: &Batch) -> Applied {
        let Some(store) = self.tables.get_mut(&table) else {
            return Applied::default();
        };
        store.apply(&mut self.dicts, op, batch, &mut self.versions)
    }

    /// Make everything applied so far visible: a new epoch, if anything
    /// changed. `None` if nothing did.
    pub fn publish(&mut self) -> Option<Snapshot> {
        if !self.tables.values().any(|t| t.dirty) && self.epoch > 0 {
            return None;
        }
        self.epoch += 1;
        let now = Instant::now();
        let versions = &mut self.versions;
        let tables = self
            .tables
            .iter_mut()
            .map(|(id, table)| {
                if table.dirty {
                    table.freeze(versions);
                }
                (*id, table.snapshot(now))
            })
            .collect();
        let dicts = self
            .dicts
            .iter_mut()
            .map(|(id, dict)| (*id, dict.snapshot()))
            .collect();
        Some(Snapshot {
            epoch: self.epoch,
            model: Arc::clone(&self.model),
            tables,
            dicts,
        })
    }
}

/// One table as of one epoch.
#[derive(Debug)]
pub struct TableSnapshot {
    pub id: TableId,
    pub layouts: Arc<[ColumnLayout]>,
    pub segments: Arc<[SegmentView]>,
    /// Rows not deleted.
    pub live_rows: u64,
    /// Columns whose value changed for an existing row in this epoch.
    pub changed: Vec<ColumnId>,
    /// Rows written since the server started.
    pub written_total: u64,
    /// Write rate over the last ten seconds.
    pub rows_per_second: f64,
}

impl TableSnapshot {
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.segments.iter().map(|s| s.data.bytes()).sum()
    }
}

/// Everything as of one epoch: what queries read.
#[derive(Debug)]
pub struct Snapshot {
    pub epoch: u64,
    pub model: Arc<Model>,
    pub tables: HashMap<TableId, Arc<TableSnapshot>>,
    pub dicts: HashMap<DictId, DictSnapshot>,
}

impl Snapshot {
    /// An empty snapshot of a model, before anything is written.
    #[must_use]
    pub fn empty(model: Arc<Model>) -> Self {
        let mut store = Store::new(model);
        store.publish().unwrap_or_else(|| Self {
            epoch: 0,
            model: Arc::clone(&store.model),
            tables: HashMap::new(),
            dicts: HashMap::new(),
        })
    }

    #[must_use]
    pub fn table(&self, id: TableId) -> Option<&TableSnapshot> {
        self.tables.get(&id).map(AsRef::as_ref)
    }

    /// Decode one stored value of a column.
    #[must_use]
    pub fn decode(&self, layout: &ColumnLayout, raw: Raw) -> Value {
        match raw {
            Raw::Null => Value::Null,
            Raw::Code(code) => {
                let Some(dict) = layout.dict.and_then(|d| self.dicts.get(&d)) else {
                    return Value::Null;
                };
                match dict.get(code) {
                    DictValue::Null => Value::Null,
                    DictValue::Str(s) => Value::String(s.to_string()),
                    DictValue::Int(v) => decode_int(layout.column_type, v),
                }
            }
            Raw::I64(v) => Value::from_i64(layout.column_type, v),
            Raw::I128(v) => decode_int(layout.column_type, v),
            Raw::F64(v) => Value::Float(v),
            Raw::Str(s) => Value::String(s.to_string()),
        }
    }

    /// The newest `limit` live rows of a table, newest first, decoded.
    #[must_use]
    pub fn newest_rows(&self, table: TableId, limit: usize) -> Vec<Vec<Value>> {
        let Some(table) = self.table(table) else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for segment in table.segments.iter().rev() {
            for offset in (0..segment.data.rows).rev() {
                if rows.len() == limit {
                    return rows;
                }
                if segment.deletes.contains(offset) {
                    continue;
                }
                let offset = offset as usize;
                rows.push(
                    table
                        .layouts
                        .iter()
                        .zip(&segment.data.columns)
                        .map(|(layout, column)| self.decode(layout, column.get(offset)))
                        .collect(),
                );
            }
        }
        rows
    }
}

/// Decode an integer-stored dictionary value by column type.
fn decode_int(column_type: ColumnType, value: i128) -> Value {
    match column_type {
        ColumnType::Float64 => {
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let bits = value as u64;
            Value::Float(f64::from_bits(bits))
        }
        ColumnType::Decimal { scale, .. } => Value::Decimal { value, scale },
        _ => Value::from_i64(column_type, i64::try_from(value).unwrap_or(i64::MAX)),
    }
}
