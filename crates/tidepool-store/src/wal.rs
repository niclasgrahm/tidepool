//! The write-ahead log (plan §4.7): every applied batch, in order, as one
//! CRC-checked record, in rolling files named by their first LSN.
//!
//! Records hold the parsed values (not dictionary codes): replaying them
//! through [`Store::apply`](crate::store::Store::apply) after a checkpoint,
//! whose dictionaries are restored to the checkpoint's lengths, gives the
//! same codes and row ids again.
//!
//! A write sent with an `Idempotency-Key` carries the key, the credential
//! it came with, the time and the response it was acknowledged with
//! ([`Keyed`]), so recovery can answer a retry of it (see [`crate::keys`]).
//!
//! A model change is a record too ([`Entry::Model`]), logged when the new
//! model takes over: batches after it are in its tables' columns, and replay
//! changes the model at the same point.
//!
//! Appends are buffered; [`Wal::sync`] writes them out and (with
//! [`Durability::Fsync`]) fsyncs: the writer calls it once per burst of
//! batches and acknowledges them after (group commit). A crash can tear the
//! last record; recovery stops at the first record that doesn't check, and
//! cuts the file there.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tidepool_config::server::Durability;
use tidepool_model::ir::TableId;

use crate::batch::Batch;
use crate::codec::{In, Out, corrupt, frame, sync_dir, unframe};
use crate::keys::{self, Keyed};
use crate::migrate::{ModelVersion, decode_version, encode_version};
use crate::store::Op;
use crate::value::Parsed;

/// A file is closed and a new one started past this size.
const ROLL: u64 = 64 << 20;

/// One logged change.
#[derive(Clone, Debug)]
pub struct Record {
    pub lsn: u64,
    pub entry: Entry,
}

#[derive(Clone, Debug)]
pub enum Entry {
    /// A batch, in the table's columns as of when it was logged.
    Write {
        table: TableId,
        op: Op,
        batch: Batch,
        /// Its idempotency key and response, when it was sent with one.
        key: Option<Keyed>,
    },
    /// The model changed to this version.
    Model(ModelVersion),
}

fn encode(lsn: u64, entry: &Entry) -> Vec<u8> {
    let mut out = Out::default();
    out.u64(lsn);
    let (table, op, batch, key) = match entry {
        Entry::Model(version) => {
            out.u32(0);
            out.u8(2);
            out.bytes(&encode_version(version));
            return out.0;
        }
        Entry::Write {
            table,
            op,
            batch,
            key,
        } => (table, op, batch, key),
    };
    out.u32(table.0);
    // 0/1: a write/delete; 5: a replace; 7/8/9: a write/delete/replace
    // with a key and its credential. 3/4/6 (the same with a key but no
    // credential, before plan §9.2) are no longer written, and still read.
    out.u8(match (op, key) {
        (Op::Write, None) => 0,
        (Op::Delete, None) => 1,
        (Op::Replace, None) => 5,
        (Op::Write, Some(_)) => 7,
        (Op::Delete, Some(_)) => 8,
        (Op::Replace, Some(_)) => 9,
    });
    if let Some(key) = key {
        keys::put(&mut out, key);
    }
    out.len(batch.rows);
    out.len(batch.columns.len());
    for column in &batch.columns {
        for value in column {
            match value {
                None => out.u8(0),
                Some(Parsed::Bool(b)) => {
                    out.u8(1);
                    out.u8(u8::from(*b));
                }
                Some(Parsed::I64(v)) => {
                    out.u8(2);
                    out.i64(*v);
                }
                Some(Parsed::I128(v)) => {
                    out.u8(3);
                    out.i128(*v);
                }
                Some(Parsed::F64(v)) => {
                    out.u8(4);
                    out.u64(v.to_bits());
                }
                Some(Parsed::Str(s)) => {
                    out.u8(5);
                    out.bytes(s.as_bytes());
                }
            }
        }
    }
    out.0
}

fn decode(bytes: &[u8]) -> io::Result<Record> {
    let mut input = In::new(bytes);
    let lsn = input.u64()?;
    let table = TableId(input.u32()?);
    // (op, keyed, the key has a credential)
    let (op, keyed, scoped) = match input.u8()? {
        0 => (Op::Write, false, false),
        1 => (Op::Delete, false, false),
        3 => (Op::Write, true, false),
        4 => (Op::Delete, true, false),
        5 => (Op::Replace, false, false),
        6 => (Op::Replace, true, false),
        7 => (Op::Write, true, true),
        8 => (Op::Delete, true, true),
        9 => (Op::Replace, true, true),
        2 => {
            return Ok(Record {
                lsn,
                entry: Entry::Model(decode_version(input.bytes()?)?),
            });
        }
        _ => return Err(corrupt("record")),
    };
    let key = if keyed {
        Some(keys::get(&mut input, table, scoped)?)
    } else {
        None
    };
    let rows = input.len()?;
    let count = input.len()?;
    let mut columns = Vec::with_capacity(count);
    for _ in 0..count {
        let mut column = Vec::with_capacity(rows);
        for _ in 0..rows {
            column.push(match input.u8()? {
                0 => None,
                1 => Some(Parsed::Bool(input.u8()? != 0)),
                2 => Some(Parsed::I64(input.i64()?)),
                3 => Some(Parsed::I128(input.i128()?)),
                4 => Some(Parsed::F64(f64::from_bits(input.u64()?))),
                5 => Some(Parsed::Str(
                    String::from_utf8(input.bytes()?.to_vec()).map_err(|_| corrupt("record"))?,
                )),
                _ => return Err(corrupt("record")),
            });
        }
        columns.push(column);
    }
    Ok(Record {
        lsn,
        entry: Entry::Write {
            table,
            op,
            batch: Batch {
                rows,
                columns,
                shape: Vec::new(),
            },
            key,
        },
    })
}

fn file_name(first: u64) -> String {
    format!("{first:020}.wal")
}

/// The WAL files in `dir`, by first LSN.
fn files(dir: &Path) -> io::Result<Vec<(u64, PathBuf)>> {
    let mut files = Vec::new();
    if !dir.exists() {
        return Ok(files);
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let first = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".wal"))
            .and_then(|n| n.parse::<u64>().ok());
        if let Some(first) = first {
            files.push((first, path));
        }
    }
    files.sort();
    Ok(files)
}

pub struct Wal {
    dir: PathBuf,
    durability: Durability,
    file: File,
    /// First LSN of the open file, and its length on disk.
    first: u64,
    size: u64,
    /// The LSN the next record gets.
    next: u64,
    /// Encoded records not written yet.
    pending: Vec<u8>,
    /// The open file's length as of the last fsync: what survives a
    /// power loss (for crash tests).
    synced: u64,
    /// Files roll past this size.
    roll: u64,
}

impl Wal {
    /// Open the log in `dir`: every intact record after `after`, in order,
    /// and a log ready to append. A torn tail is cut off.
    ///
    /// # Errors
    /// The directory or a file can't be read or written.
    pub fn open(dir: &Path, after: u64, durability: Durability) -> io::Result<(Self, Vec<Record>)> {
        std::fs::create_dir_all(dir)?;
        let mut records = Vec::new();
        let mut next = after + 1;
        let mut broken = false;
        let files = files(dir)?;
        let mut last: Option<(u64, PathBuf, u64)> = None;
        for (first, path) in files {
            if broken {
                // past a torn record nothing can be trusted: an older crash
                std::fs::remove_file(&path)?;
                continue;
            }
            let bytes = std::fs::read(&path)?;
            let mut at = 0;
            while let Some((payload, end)) = unframe(&bytes, at) {
                let Ok(record) = decode(payload) else { break };
                if record.lsn >= next {
                    next = record.lsn + 1;
                }
                if record.lsn > after {
                    records.push(record);
                }
                at = end;
            }
            if at < bytes.len() {
                // cut the torn tail, so new records follow intact ones
                OpenOptions::new()
                    .write(true)
                    .open(&path)?
                    .set_len(at as u64)?;
                broken = true;
            }
            last = Some((first, path, at as u64));
        }
        let (first, path, size) = match last {
            Some((first, path, size)) if size < ROLL => (first, path, size),
            _ => {
                let path = dir.join(file_name(next));
                (next, path, 0)
            }
        };
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        file.sync_all()?;
        sync_dir(dir)?;
        Ok((
            Self {
                dir: dir.to_path_buf(),
                durability,
                file,
                first,
                size,
                next,
                pending: Vec::new(),
                synced: size,
                roll: ROLL,
            },
            records,
        ))
    }

    /// Log a write; durable after the next [`Wal::sync`]. Returns its LSN.
    pub fn append(&mut self, table: TableId, op: Op, batch: &Batch, key: Option<Keyed>) -> u64 {
        self.log(&Entry::Write {
            table,
            op,
            batch: batch.clone(),
            key,
        })
    }

    /// Log any entry; durable after the next [`Wal::sync`]. Returns its LSN.
    pub fn log(&mut self, entry: &Entry) -> u64 {
        let lsn = self.next;
        self.next += 1;
        frame(&encode(lsn, entry), &mut self.pending);
        lsn
    }

    /// Roll files past `bytes` instead of 64 MiB (tests use small files).
    pub fn roll_at(&mut self, bytes: u64) {
        self.roll = bytes;
    }

    /// The last LSN handed out (0 before the first record).
    #[must_use]
    pub fn last(&self) -> u64 {
        self.next - 1
    }

    /// Write out what's pending, and fsync it unless durability is async.
    ///
    /// # Errors
    /// Writing or syncing failed: nothing written since the last sync can
    /// be acknowledged.
    pub fn sync(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.file.write_all(&self.pending)?;
        self.size += self.pending.len() as u64;
        self.pending.clear();
        if self.durability == Durability::Fsync {
            self.file.sync_data()?;
            self.synced = self.size;
        }
        if self.size >= self.roll {
            self.roll()?;
        }
        Ok(())
    }

    /// fsync whatever was written (async durability does this on a clock).
    ///
    /// # Errors
    /// Syncing failed.
    pub fn flush(&mut self) -> io::Result<()> {
        self.sync()?;
        self.file.sync_data()?;
        self.synced = self.size;
        Ok(())
    }

    fn roll(&mut self) -> io::Result<()> {
        self.file.sync_all()?;
        let path = self.dir.join(file_name(self.next));
        self.file = OpenOptions::new().create(true).append(true).open(&path)?;
        sync_dir(&self.dir)?;
        self.first = self.next;
        self.size = 0;
        self.synced = 0;
        Ok(())
    }

    /// Remove files holding only records at or below `lsn` (a checkpoint
    /// covers them).
    ///
    /// # Errors
    /// A file can't be removed.
    pub fn forget(&mut self, lsn: u64) -> io::Result<()> {
        let files = files(&self.dir)?;
        for pair in files.windows(2) {
            let ((_, path), (next_first, _)) = (&pair[0], &pair[1]);
            // every record of this file is below the next file's first
            if *next_first <= lsn + 1 && *next_first <= self.first {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    /// The open file and its length as of the last fsync: what a power
    /// loss would leave (crash tests cut the file there).
    #[must_use]
    pub fn durable(&self) -> (PathBuf, u64) {
        (self.dir.join(file_name(self.first)), self.synced)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(n: i64) -> Batch {
        Batch {
            rows: 2,
            columns: vec![
                vec![Some(Parsed::Str(format!("k{n}"))), None],
                vec![Some(Parsed::I64(n)), Some(Parsed::F64(0.5))],
            ],
            shape: Vec::new(),
        }
    }

    #[test]
    fn records_come_back_after_reopening_and_a_torn_tail_is_cut() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let (mut wal, old) =
            Wal::open(dir.path(), 0, Durability::Fsync).unwrap_or_else(|e| panic!("{e}"));
        assert!(old.is_empty());
        let keyed = Keyed {
            credential: "kayak".into(),
            table: TableId(7),
            key: "retry-me".into(),
            at: 1_700_000_000_000,
            applied: crate::store::Applied {
                written: 2,
                ..crate::store::Applied::default()
            },
            epoch: 5,
        };
        for n in 0..3 {
            let key = (n == 1).then(|| keyed.clone());
            wal.append(TableId(7), Op::Write, &batch(n), key);
        }
        wal.sync().unwrap_or_else(|e| panic!("{e}"));
        let (path, synced) = wal.durable();
        drop(wal);
        // a torn record after the synced ones
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap_or_else(|e| panic!("{e}"));
        file.write_all(&[9, 0, 0, 0, 1, 2])
            .unwrap_or_else(|e| panic!("{e}"));
        drop(file);

        let (mut wal, records) =
            Wal::open(dir.path(), 1, Durability::Fsync).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            records.iter().map(|r| r.lsn).collect::<Vec<_>>(),
            vec![2, 3]
        );
        match &records[0].entry {
            Entry::Write { batch: b, key, .. } => {
                assert_eq!(*b, batch(1));
                assert_eq!(key.as_ref(), Some(&keyed));
            }
            Entry::Model(_) => panic!("expected the second write"),
        }
        match &records[1].entry {
            Entry::Write { key, .. } => assert_eq!(*key, None),
            Entry::Model(_) => panic!("expected the third write"),
        }
        assert_eq!(std::fs::metadata(&path).map_or(0, |m| m.len()), synced);
        // appends continue the sequence
        assert_eq!(wal.append(TableId(7), Op::Delete, &batch(9), None), 4);
    }

    #[test]
    fn a_keyed_record_from_before_credentials_is_still_read() {
        // lsn, table, op 3 (a keyed write), the key without a credential
        // (key, time, three counts, epoch), then an empty batch
        let mut out = Out::default();
        out.u64(12);
        out.u32(7);
        out.u8(3);
        out.bytes(b"retry-me");
        for n in [1_700_000_000_000, 2, 0, 0, 5] {
            out.u64(n);
        }
        out.len(0);
        out.len(0);
        let record = decode(&out.0).unwrap_or_else(|e| panic!("{e}"));
        match record.entry {
            Entry::Write { op, key, .. } => {
                assert_eq!(op, Op::Write);
                let key = key.unwrap_or_else(|| panic!("expected the key"));
                assert_eq!(key.credential, keys::LEGACY);
                assert_eq!(key.key, "retry-me");
            }
            Entry::Model(_) => panic!("expected a write"),
        }
    }
}
