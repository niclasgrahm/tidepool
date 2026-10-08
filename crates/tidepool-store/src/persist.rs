//! Durability (plan §4.7): the WAL plus checkpoints, and recovery.
//!
//! ```text
//! data/
//! ├── wal/{first_lsn}.wal              every applied batch since the last checkpoint
//! ├── dict/{id}.dict                   dictionary values, appended in code order
//! ├── segments/t{table}/{start}-{rows}-{c|s}.seg   immutable segments, written once
//! └── checkpoints/{lsn}.ckpt           what a snapshot holds: segments, deletes, dictionary lengths
//! ```
//!
//! A checkpoint is a published [`Snapshot`] written down, plus the LSN of
//! the last batch it contains. Segments are immutable, so each file is
//! written once and shared by every checkpoint that lists it; dictionaries
//! only grow, so their files are appended to. The checkpoint file itself is
//! written last and atomically: a crash halfway leaves the previous one in
//! charge. Recovery loads the newest checkpoint that reads back whole,
//! rebuilds dictionaries by encoding their values again in code order (so
//! codes come out the same), rebuilds primary-key indexes, and replays the
//! WAL after the checkpoint's LSN.
//!
//! Segments are read into memory rather than memory-mapped: mapping needs
//! `unsafe`, and the store has none.

use std::collections::{BTreeSet, HashMap};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use roaring::RoaringBitmap;
use tidepool_config::server::Durability;
use tidepool_model::Model;
use tidepool_model::ir::{ColumnId, DomainId, TableId};

use crate::batch::Batch;
use crate::codec::{In, Out, corrupt, frame, sync_dir, unframe, write_atomic};
use crate::column::{Bits, ColumnData, Packed};
use crate::dict::{DictSnapshot, DictValue, Dictionary, Key};
use crate::store::{Applied, DictId, Op, SegmentData, SegmentView, Snapshot, Store};
use crate::value::Storage;
use crate::wal::Wal;

const SEGMENT_MAGIC: u32 = 0x5450_5347; // "TPSG"
const CHECKPOINT_MAGIC: u32 = 0x5450_434b; // "TPCK"
/// Checkpoints kept; files only they reference are kept with them.
const KEEP: usize = 2;

fn storage_tag(storage: Storage) -> u8 {
    match storage {
        Storage::Bool => 0,
        Storage::I64 => 1,
        Storage::I128 => 2,
        Storage::F64 => 3,
        Storage::Str => 4,
    }
}

fn storage_of(tag: u8) -> io::Result<Storage> {
    Ok(match tag {
        0 => Storage::Bool,
        1 => Storage::I64,
        2 => Storage::I128,
        3 => Storage::F64,
        4 => Storage::Str,
        _ => return Err(corrupt("storage")),
    })
}

fn dict_name(id: DictId) -> String {
    match id {
        DictId::Column(ColumnId(n)) => format!("c{n}.dict"),
        DictId::Domain(DomainId(ColumnId(n))) => format!("d{n}.dict"),
    }
}

fn put_dict_id(out: &mut Out, id: DictId) {
    match id {
        DictId::Column(ColumnId(n)) => {
            out.u8(0);
            out.u32(n);
        }
        DictId::Domain(DomainId(ColumnId(n))) => {
            out.u8(1);
            out.u32(n);
        }
    }
}

fn get_dict_id(input: &mut In) -> io::Result<DictId> {
    Ok(match input.u8()? {
        0 => DictId::Column(ColumnId(input.u32()?)),
        1 => DictId::Domain(DomainId(ColumnId(input.u32()?))),
        _ => return Err(corrupt("dictionary id")),
    })
}

fn segment_name(data: &SegmentData) -> String {
    format!(
        "{}-{}-{}.seg",
        data.start,
        data.rows,
        if data.sealed { "s" } else { "c" }
    )
}

fn put_bits(out: &mut Out, bits: Option<&Bits>) {
    match bits {
        None => out.u8(0),
        Some(bits) => {
            out.u8(1);
            out.u64s(bits.words());
        }
    }
}

fn get_bits(input: &mut In) -> io::Result<Option<Bits>> {
    Ok(match input.u8()? {
        0 => None,
        _ => Some(Bits::from_words(input.u64s()?)),
    })
}

fn put_packed(out: &mut Out, packed: &Packed) {
    let (width, len, words) = packed.parts();
    out.u8(width);
    out.len(len);
    out.u64s(words);
}

fn get_packed(input: &mut In) -> io::Result<Packed> {
    let width = input.u8()?;
    let len = usize::try_from(input.u64()?).map_err(|_| corrupt("segment"))?;
    let words = input.u64s()?;
    Packed::from_parts(width, len, words).ok_or_else(|| corrupt("segment"))
}

/// A segment as bytes.
fn encode_segment(data: &SegmentData) -> Vec<u8> {
    let mut out = Out::default();
    out.u32(SEGMENT_MAGIC);
    out.u64(data.start);
    out.u32(data.rows);
    out.u8(u8::from(data.sealed));
    out.len(data.columns.len());
    for column in &data.columns {
        match column {
            ColumnData::Codes(codes) => {
                out.u8(0);
                out.u32s(codes);
            }
            ColumnData::PackedCodes { packed, min, max } => {
                out.u8(1);
                put_packed(&mut out, packed);
                out.u32(*min);
                out.u32(*max);
            }
            ColumnData::I64 { values, nulls } => {
                out.u8(2);
                out.i64s(values);
                put_bits(&mut out, nulls.as_ref());
            }
            ColumnData::PackedI64 {
                base,
                packed,
                nulls,
                min,
                max,
            } => {
                out.u8(3);
                out.i64(*base);
                put_packed(&mut out, packed);
                put_bits(&mut out, nulls.as_ref());
                out.i64(*min);
                out.i64(*max);
            }
            ColumnData::I128 { values, nulls } => {
                out.u8(4);
                out.len(values.len());
                for v in values {
                    out.i128(*v);
                }
                put_bits(&mut out, nulls.as_ref());
            }
            ColumnData::F64 { values, nulls } => {
                out.u8(5);
                out.u64s(&values.iter().map(|v| v.to_bits()).collect::<Vec<_>>());
                put_bits(&mut out, nulls.as_ref());
            }
            ColumnData::Str {
                bytes,
                offsets,
                nulls,
            } => {
                out.u8(6);
                out.bytes(bytes);
                out.u32s(offsets);
                put_bits(&mut out, nulls.as_ref());
            }
        }
    }
    let mut framed = Vec::with_capacity(out.0.len() + 8);
    frame(&out.0, &mut framed);
    framed
}

fn decode_segment(bytes: &[u8]) -> io::Result<SegmentData> {
    let (payload, _) = unframe(bytes, 0).ok_or_else(|| corrupt("segment"))?;
    let mut input = In::new(payload);
    if input.u32()? != SEGMENT_MAGIC {
        return Err(corrupt("segment"));
    }
    let start = input.u64()?;
    let rows = input.u32()?;
    let sealed = input.u8()? != 0;
    let count = input.len()?;
    let mut columns = Vec::with_capacity(count);
    for _ in 0..count {
        columns.push(match input.u8()? {
            0 => ColumnData::Codes(input.u32s()?),
            1 => {
                let packed = get_packed(&mut input)?;
                ColumnData::PackedCodes {
                    packed,
                    min: input.u32()?,
                    max: input.u32()?,
                }
            }
            2 => ColumnData::I64 {
                values: input.i64s()?,
                nulls: get_bits(&mut input)?,
            },
            3 => {
                let base = input.i64()?;
                let packed = get_packed(&mut input)?;
                let nulls = get_bits(&mut input)?;
                ColumnData::PackedI64 {
                    base,
                    packed,
                    nulls,
                    min: input.i64()?,
                    max: input.i64()?,
                }
            }
            4 => {
                let n = input.len()?;
                let values = (0..n).map(|_| input.i128()).collect::<io::Result<_>>()?;
                ColumnData::I128 {
                    values,
                    nulls: get_bits(&mut input)?,
                }
            }
            5 => ColumnData::F64 {
                values: input.u64s()?.into_iter().map(f64::from_bits).collect(),
                nulls: get_bits(&mut input)?,
            },
            6 => ColumnData::Str {
                bytes: input.bytes()?.to_vec(),
                offsets: input.u32s()?,
                nulls: get_bits(&mut input)?,
            },
            _ => return Err(corrupt("segment")),
        });
    }
    Ok(SegmentData {
        start,
        rows,
        columns,
        sealed,
    })
}

/// One dictionary value as bytes.
fn put_value(out: &mut Vec<u8>, value: DictValue) {
    match value {
        DictValue::Int(v) => out.extend_from_slice(&v.to_le_bytes()),
        DictValue::Str(s) => {
            out.extend_from_slice(&u32::try_from(s.len()).unwrap_or(u32::MAX).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        DictValue::Null => {}
    }
}

/// What a checkpoint records about one segment.
struct SegmentEntry {
    start: u64,
    rows: u32,
    sealed: bool,
    version: u64,
    deletes: RoaringBitmap,
}

struct TableEntry {
    id: TableId,
    /// The table's columns as stored: a changed config doesn't fit.
    layout: String,
    live_rows: u64,
    written_total: u64,
    segments: Vec<SegmentEntry>,
}

/// One checkpoint file's content.
struct Manifest {
    lsn: u64,
    epoch: u64,
    /// Dictionaries: storage, values, and the file's length in bytes.
    dicts: Vec<(DictId, Storage, u32, u64)>,
    tables: Vec<TableEntry>,
}

fn layout_of(snapshot: &Snapshot, table: TableId) -> String {
    snapshot
        .table(table)
        .map(|t| format!("{:?}", t.layouts))
        .unwrap_or_default()
}

fn encode_manifest(manifest: &Manifest) -> io::Result<Vec<u8>> {
    let mut out = Out::default();
    out.u32(CHECKPOINT_MAGIC);
    out.u64(manifest.lsn);
    out.u64(manifest.epoch);
    out.len(manifest.dicts.len());
    for (id, storage, len, bytes) in &manifest.dicts {
        put_dict_id(&mut out, *id);
        out.u8(storage_tag(*storage));
        out.u32(*len);
        out.u64(*bytes);
    }
    out.len(manifest.tables.len());
    for table in &manifest.tables {
        out.u32(table.id.0);
        out.bytes(table.layout.as_bytes());
        out.u64(table.live_rows);
        out.u64(table.written_total);
        out.len(table.segments.len());
        for segment in &table.segments {
            out.u64(segment.start);
            out.u32(segment.rows);
            out.u8(u8::from(segment.sealed));
            out.u64(segment.version);
            let mut deletes = Vec::new();
            segment.deletes.serialize_into(&mut deletes)?;
            out.bytes(&deletes);
        }
    }
    let mut framed = Vec::new();
    frame(&out.0, &mut framed);
    Ok(framed)
}

fn decode_manifest(bytes: &[u8]) -> io::Result<Manifest> {
    let (payload, _) = unframe(bytes, 0).ok_or_else(|| corrupt("checkpoint"))?;
    let mut input = In::new(payload);
    if input.u32()? != CHECKPOINT_MAGIC {
        return Err(corrupt("checkpoint"));
    }
    let lsn = input.u64()?;
    let epoch = input.u64()?;
    let count = input.len()?;
    let mut dicts = Vec::with_capacity(count);
    for _ in 0..count {
        let id = get_dict_id(&mut input)?;
        let storage = storage_of(input.u8()?)?;
        dicts.push((id, storage, input.u32()?, input.u64()?));
    }
    let count = input.len()?;
    let mut tables = Vec::with_capacity(count);
    for _ in 0..count {
        let id = TableId(input.u32()?);
        let layout =
            String::from_utf8(input.bytes()?.to_vec()).map_err(|_| corrupt("checkpoint"))?;
        let live_rows = input.u64()?;
        let written_total = input.u64()?;
        let n = input.len()?;
        let mut segments = Vec::with_capacity(n);
        for _ in 0..n {
            let start = input.u64()?;
            let rows = input.u32()?;
            let sealed = input.u8()? != 0;
            let version = input.u64()?;
            let deletes = RoaringBitmap::deserialize_from(input.bytes()?)?;
            segments.push(SegmentEntry {
                start,
                rows,
                sealed,
                version,
                deletes,
            });
        }
        tables.push(TableEntry {
            id,
            layout,
            live_rows,
            written_total,
            segments,
        });
    }
    Ok(Manifest {
        lsn,
        epoch,
        dicts,
        tables,
    })
}

/// Checkpoint files, newest first.
fn checkpoints(dir: &Path) -> io::Result<Vec<(u64, PathBuf)>> {
    let dir = dir.join("checkpoints");
    let mut found = Vec::new();
    if !dir.exists() {
        return Ok(found);
    }
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        let lsn = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".ckpt"))
            .and_then(|n| n.parse::<u64>().ok());
        if let Some(lsn) = lsn {
            found.push((lsn, path));
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    Ok(found)
}

/// Writes checkpoints, and remembers how far each dictionary file goes.
pub struct Checkpoints {
    dir: PathBuf,
    /// Per dictionary: values written, and bytes.
    written: HashMap<DictId, (u32, u64)>,
    /// Test hook: fail (as if the process died) after this many files.
    crash_after: Option<usize>,
}

impl Checkpoints {
    fn new(dir: &Path, manifest: Option<&Manifest>) -> Self {
        Self {
            dir: dir.to_path_buf(),
            written: manifest
                .map(|m| {
                    m.dicts
                        .iter()
                        .map(|(id, _, len, bytes)| (*id, (*len, *bytes)))
                        .collect()
                })
                .unwrap_or_default(),
            crash_after: None,
        }
    }

    /// Make the next checkpoint die after writing `files` files.
    pub fn crash_after(&mut self, files: usize) {
        self.crash_after = Some(files);
    }

    fn wrote(&mut self) -> io::Result<()> {
        if let Some(n) = &mut self.crash_after {
            if *n == 0 {
                self.crash_after = None;
                return Err(io::Error::other("crash (test)"));
            }
            *n -= 1;
        }
        Ok(())
    }

    /// Write `snapshot`, which holds every batch up to `lsn`, then forget
    /// what no kept checkpoint needs.
    ///
    /// # Errors
    /// A file can't be written; the previous checkpoint stays in charge.
    pub fn write(&mut self, snapshot: &Snapshot, lsn: u64) -> io::Result<()> {
        // dictionaries: the values added since the last checkpoint
        let dict_dir = self.dir.join("dict");
        std::fs::create_dir_all(&dict_dir)?;
        let mut dicts: Vec<(DictId, Storage, u32, u64)> = Vec::new();
        let mut ids: Vec<&DictId> = snapshot.dicts.keys().collect();
        ids.sort();
        for id in ids {
            let dict: &DictSnapshot = &snapshot.dicts[id];
            let (done, bytes) = self.written.get(id).copied().unwrap_or((0, 0));
            let mut size = bytes;
            if dict.len() > done {
                let mut out = Vec::new();
                for code in done + 1..=dict.len() {
                    put_value(&mut out, dict.get(code));
                }
                let path = dict_dir.join(dict_name(*id));
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?;
                // a crash after an earlier append may have left more: cut back
                file.set_len(bytes)?;
                file.write_all(&out)?;
                file.sync_all()?;
                size = bytes + out.len() as u64;
                self.wrote()?;
            }
            dicts.push((*id, dict.storage(), dict.len(), size));
        }
        sync_dir(&dict_dir)?;

        // segments: each written once
        let mut tables = Vec::new();
        let mut ids: Vec<&TableId> = snapshot.tables.keys().collect();
        ids.sort();
        for id in ids {
            let table = &snapshot.tables[id];
            let seg_dir = self.dir.join("segments").join(format!("t{}", id.0));
            std::fs::create_dir_all(&seg_dir)?;
            let mut segments = Vec::new();
            for view in table.segments.iter() {
                let path = seg_dir.join(segment_name(&view.data));
                if !path.exists() {
                    write_atomic(&path, &encode_segment(&view.data))?;
                    self.wrote()?;
                }
                segments.push(SegmentEntry {
                    start: view.data.start,
                    rows: view.data.rows,
                    sealed: view.data.sealed,
                    version: view.version,
                    deletes: (*view.deletes).clone(),
                });
            }
            tables.push(TableEntry {
                id: *id,
                layout: layout_of(snapshot, *id),
                live_rows: table.live_rows,
                written_total: table.written_total,
                segments,
            });
        }

        let manifest = Manifest {
            lsn,
            epoch: snapshot.epoch,
            dicts,
            tables,
        };
        let path = self.dir.join("checkpoints").join(format!("{lsn:020}.ckpt"));
        write_atomic(&path, &encode_manifest(&manifest)?)?;
        for (id, _, len, bytes) in &manifest.dicts {
            self.written.insert(*id, (*len, *bytes));
        }
        // the hook was for this checkpoint only
        self.crash_after = None;
        self.collect()
    }

    /// Remove checkpoints past the newest `KEEP`, and segment files none of
    /// the kept ones list.
    fn collect(&self) -> io::Result<()> {
        let all = checkpoints(&self.dir)?;
        let mut keep: BTreeSet<PathBuf> = BTreeSet::new();
        for (_, path) in all.iter().take(KEEP) {
            let Ok(manifest) = std::fs::read(path).and_then(|b| decode_manifest(&b)) else {
                continue;
            };
            for table in &manifest.tables {
                let dir = self.dir.join("segments").join(format!("t{}", table.id.0));
                for s in &table.segments {
                    let name = format!(
                        "{}-{}-{}.seg",
                        s.start,
                        s.rows,
                        if s.sealed { "s" } else { "c" }
                    );
                    keep.insert(dir.join(name));
                }
            }
        }
        for (_, path) in all.iter().skip(KEEP) {
            std::fs::remove_file(path)?;
        }
        let segments = self.dir.join("segments");
        if segments.exists() {
            for table in std::fs::read_dir(&segments)? {
                for file in std::fs::read_dir(table?.path())? {
                    let path = file?.path();
                    if path.extension().is_some_and(|e| e == "seg") && !keep.contains(&path) {
                        std::fs::remove_file(&path)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// What recovery found.
struct Recovered {
    manifest: Manifest,
    dicts: HashMap<DictId, Dictionary>,
    segments: HashMap<TableId, Vec<SegmentView>>,
}

/// Load the newest checkpoint that reads back whole.
fn load(dir: &Path) -> io::Result<Option<Recovered>> {
    let mut last_error = None;
    for (_, path) in checkpoints(dir)? {
        match load_one(dir, &path) {
            Ok(recovered) => return Ok(Some(recovered)),
            Err(error) => {
                tracing::warn!("checkpoint {} unusable: {error}", path.display());
                last_error = Some(error);
            }
        }
    }
    match last_error {
        // there were checkpoints, and none loads: not something to ignore
        Some(error) => Err(error),
        None => Ok(None),
    }
}

fn load_one(dir: &Path, path: &Path) -> io::Result<Recovered> {
    let manifest = decode_manifest(&std::fs::read(path)?)?;
    let mut dicts = HashMap::new();
    for (id, storage, len, bytes) in &manifest.dicts {
        let raw = std::fs::read(dir.join("dict").join(dict_name(*id)))?;
        let raw = raw
            .get(..usize::try_from(*bytes).map_err(|_| corrupt("dictionary"))?)
            .ok_or_else(|| corrupt("dictionary (short)"))?;
        let mut dict = Dictionary::new(*storage);
        let mut input = In::new(raw);
        for _ in 0..*len {
            match storage {
                Storage::Str => {
                    let n = input.u32()? as usize;
                    let bytes = raw
                        .get(input.at..input.at + n)
                        .ok_or_else(|| corrupt("dictionary"))?;
                    input.at += n;
                    let value = std::str::from_utf8(bytes).map_err(|_| corrupt("dictionary"))?;
                    dict.encode(&Key::Str(value));
                }
                _ => {
                    dict.encode(&Key::Int(input.i128()?));
                }
            }
        }
        if dict.len() != *len {
            return Err(corrupt("dictionary (repeated values)"));
        }
        dicts.insert(*id, dict);
    }
    let mut segments = HashMap::new();
    for table in &manifest.tables {
        let mut views = Vec::with_capacity(table.segments.len());
        let seg_dir = dir.join("segments").join(format!("t{}", table.id.0));
        for entry in &table.segments {
            let name = format!(
                "{}-{}-{}.seg",
                entry.start,
                entry.rows,
                if entry.sealed { "s" } else { "c" }
            );
            let data = decode_segment(&std::fs::read(seg_dir.join(name))?)?;
            if data.start != entry.start || data.rows != entry.rows {
                return Err(corrupt("segment (not the one listed)"));
            }
            views.push(SegmentView {
                data: Arc::new(data),
                deletes: Arc::new(entry.deletes.clone()),
                version: entry.version,
            });
        }
        segments.insert(table.id, views);
    }
    Ok(Recovered {
        manifest,
        dicts,
        segments,
    })
}

/// The store with its log: what the writer thread owns when there is a
/// data directory. Every write is logged before it's applied; [`commit`]
/// makes everything so far durable.
///
/// [`commit`]: Durable::commit
pub struct Durable {
    store: Store,
    wal: Wal,
    /// The LSN the checkpoint recovered from holds (0: none).
    recovered: u64,
}

/// A recovered store, its first snapshot, and the checkpoint writer.
pub type Opened = (Durable, Snapshot, Checkpoints);

impl Durable {
    /// Open (or create) the data directory `dir` for `model`: recover the
    /// newest checkpoint and replay the log after it.
    ///
    /// # Errors
    /// Files can't be read or written, the checkpoint is corrupt, or the
    /// config no longer fits the data (a table's columns changed).
    pub fn open(
        model: Arc<Model>,
        dir: &Path,
        seal_rows: usize,
        durability: Durability,
    ) -> io::Result<Opened> {
        std::fs::create_dir_all(dir)?;
        let recovered = load(dir)?;
        let (mut store, after, checkpoints) = match recovered {
            None => (
                Store::with_seal_rows(Arc::clone(&model), seal_rows),
                0,
                Checkpoints::new(dir, None),
            ),
            Some(recovered) => {
                let probe = Snapshot::empty(Arc::clone(&model));
                let empty = Store::with_seal_rows(Arc::clone(&model), seal_rows)
                    .publish()
                    .unwrap_or(probe);
                for table in &recovered.manifest.tables {
                    let now = layout_of(&empty, table.id);
                    if now != table.layout {
                        return Err(io::Error::other(format!(
                            "table {} in {} was written with other columns than the config now has; \
                             changing a table's columns over existing data needs the config pipeline (M7)",
                            table.id.0,
                            dir.display()
                        )));
                    }
                }
                let checkpoints = Checkpoints::new(dir, Some(&recovered.manifest));
                let lsn = recovered.manifest.lsn;
                let tables = recovered
                    .manifest
                    .tables
                    .iter()
                    .map(|t| (t.id, (t.live_rows, t.written_total)))
                    .collect();
                let store = Store::restore(
                    model,
                    seal_rows,
                    recovered.manifest.epoch,
                    recovered.dicts,
                    recovered.segments,
                    &tables,
                );
                (store, lsn, checkpoints)
            }
        };
        let (wal, records) = Wal::open(&dir.join("wal"), after, durability)?;
        for record in &records {
            store.apply(record.table, record.op, &record.batch);
        }
        let snapshot = store
            .publish()
            .unwrap_or_else(|| Snapshot::empty(Arc::clone(store.model())));
        Ok((
            Self {
                store,
                wal,
                recovered: after,
            },
            snapshot,
            checkpoints,
        ))
    }

    /// Log and apply a batch. Durable after the next [`Durable::commit`].
    pub fn apply(&mut self, table: TableId, op: Op, batch: &Batch) -> Applied {
        if batch.rows > 0 {
            self.wal.append(table, op, batch);
        }
        self.store.apply(table, op, batch)
    }

    /// Make everything applied so far durable.
    ///
    /// # Errors
    /// The log can't be written: nothing since the last commit is safe.
    pub fn commit(&mut self) -> io::Result<()> {
        self.wal.sync()
    }

    /// fsync the log (async durability does this on a clock).
    ///
    /// # Errors
    /// The log can't be synced.
    pub fn flush(&mut self) -> io::Result<()> {
        self.wal.flush()
    }

    /// Publish, with the LSN of the last batch the snapshot holds.
    pub fn publish(&mut self) -> Option<(Snapshot, u64)> {
        let lsn = self.wal.last();
        self.store.publish().map(|s| (s, lsn))
    }

    /// The LSN the checkpoint recovered from holds (0: none).
    #[must_use]
    pub fn recovered_lsn(&self) -> u64 {
        self.recovered
    }

    /// The last LSN logged.
    #[must_use]
    pub fn last_lsn(&self) -> u64 {
        self.wal.last()
    }

    /// Drop log files a checkpoint at `lsn` made unnecessary.
    ///
    /// # Errors
    /// A file can't be removed.
    pub fn forget(&mut self, lsn: u64) -> io::Result<()> {
        self.wal.forget(lsn)
    }

    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// Roll log files past `bytes` (tests use small ones).
    pub fn roll_log_at(&mut self, bytes: u64) {
        self.wal.roll_at(bytes);
    }

    /// The open log file and its length as of the last fsync (crash tests).
    #[must_use]
    pub fn durable_log(&self) -> (PathBuf, u64) {
        self.wal.durable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Parsed;

    #[test]
    fn segments_round_trip_in_every_encoding() {
        use crate::column::Raw;
        use crate::column::{ColumnBuilder, seal};
        let mut parts = Vec::new();
        for (storage, dict, values) in [
            (
                Storage::Str,
                true,
                vec![Raw::Code(1), Raw::Code(2), Raw::Code(0)],
            ),
            (
                Storage::I64,
                false,
                vec![Raw::I64(5), Raw::Null, Raw::I64(-3)],
            ),
            (
                Storage::I128,
                false,
                vec![Raw::I128(5), Raw::I128(1 << 90), Raw::Null],
            ),
            (
                Storage::F64,
                false,
                vec![Raw::F64(0.5), Raw::Null, Raw::F64(-1.0)],
            ),
            (
                Storage::Str,
                false,
                vec![Raw::Str("a"), Raw::Null, Raw::Str("ccc")],
            ),
        ] {
            let mut builder = ColumnBuilder::new(storage, dict);
            for v in values {
                builder.push(v);
            }
            let frozen = builder.freeze();
            parts.push(frozen.clone());
            parts.push(seal(&[&frozen, &frozen], storage, dict));
        }
        let data = SegmentData {
            start: 40,
            rows: 3,
            columns: parts,
            sealed: true,
        };
        let back = decode_segment(&encode_segment(&data)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back.columns, data.columns);
        assert_eq!((back.start, back.rows, back.sealed), (40, 3, true));
        let _ = Parsed::Bool(true);
    }
}
