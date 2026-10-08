//! Dictionaries: every distinct value of a column (or of a key domain) gets a
//! small integer code, in arrival order. Code 0 is NULL.
//!
//! Codes never change while a dictionary lives, so a code stored in a segment
//! means the same value forever. Values are kept in fixed-size blocks: full
//! blocks are immutable and shared by `Arc`; only the last, partial block is
//! copied when a snapshot is taken. Readers therefore get a [`DictSnapshot`]
//! that needs no locks, while the writer keeps appending. The value → code
//! index is the writer's alone.

use std::sync::Arc;

use hashbrown::HashMap;
use hashbrown::HashTable;

use crate::value::Storage;

/// Values per block. Small enough that copying the partial block at each
/// snapshot is cheap; large enough that full blocks are few.
pub const BLOCK: usize = 4096;

/// The NULL code.
pub const NULL: u32 = 0;

/// A dictionary key: strings as themselves, everything else as an integer
/// (bools as 0/1, the i64 kinds widened, floats by their bit pattern).
#[derive(Clone, Debug, PartialEq)]
pub enum Key<'a> {
    Int(i128),
    Str(&'a str),
}

/// A value looked up by code.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DictValue<'a> {
    Null,
    Int(i128),
    Str(&'a str),
}

#[derive(Clone, Debug, Default)]
pub struct Block {
    ints: Vec<i128>,
    /// String bytes, back to back; `offsets[i]..offsets[i + 1]` is value `i`.
    bytes: Vec<u8>,
    offsets: Vec<u32>,
}

impl Block {
    fn len(&self, storage: Storage) -> usize {
        match storage {
            Storage::Str => self.offsets.len().saturating_sub(1),
            _ => self.ints.len(),
        }
    }

    fn str(&self, index: usize) -> &str {
        let (start, end) = (
            self.offsets[index] as usize,
            self.offsets[index + 1] as usize,
        );
        // only ever filled from `&str`s, so always valid UTF-8
        std::str::from_utf8(&self.bytes[start..end]).unwrap_or_default()
    }

    fn get(&self, storage: Storage, index: usize) -> DictValue<'_> {
        match storage {
            Storage::Str => DictValue::Str(self.str(index)),
            _ => DictValue::Int(self.ints[index]),
        }
    }

    fn push(&mut self, key: &Key) {
        match key {
            Key::Int(value) => self.ints.push(*value),
            Key::Str(value) => {
                if self.offsets.is_empty() {
                    self.offsets.push(0);
                }
                self.bytes.extend_from_slice(value.as_bytes());
                let end = u32::try_from(self.bytes.len()).unwrap_or(u32::MAX);
                self.offsets.push(end);
            }
        }
    }

    fn bytes(&self) -> usize {
        self.ints.len() * 16 + self.bytes.len() + self.offsets.len() * 4
    }
}

/// The writer's dictionary.
#[derive(Clone, Debug)]
pub struct Dictionary {
    storage: Storage,
    /// Which dictionary of this id it is: a model change that builds a
    /// dictionary afresh gives it a new generation, so codes cached against
    /// an earlier one of the same id are never taken for its codes.
    generation: u32,
    full: Vec<Arc<Block>>,
    current: Block,
    len: u32,
    ints: HashMap<i128, u32>,
    /// Codes of string values, hashed by their string.
    strs: HashTable<u32>,
    hasher: hashbrown::DefaultHashBuilder,
    /// The partial block as of the last snapshot, reused while unchanged.
    published: Option<(u32, Arc<Block>)>,
}

impl Dictionary {
    #[must_use]
    pub fn new(storage: Storage) -> Self {
        Self {
            storage,
            generation: 0,
            full: Vec::new(),
            current: Block::default(),
            len: 0,
            ints: HashMap::new(),
            strs: HashTable::new(),
            hasher: hashbrown::DefaultHashBuilder::default(),
            published: None,
        }
    }

    #[must_use]
    pub fn storage(&self) -> Storage {
        self.storage
    }

    #[must_use]
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// The same values under a new generation (see [`Dictionary::generation`]).
    #[must_use]
    pub fn with_generation(mut self, generation: u32) -> Self {
        self.generation = generation;
        self
    }

    /// Number of distinct values (not counting NULL).
    #[must_use]
    pub fn len(&self) -> u32 {
        self.len
    }

    /// The value of `code` (NULL for code 0 and past the end).
    #[must_use]
    pub fn get(&self, code: u32) -> DictValue<'_> {
        self.value(code)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn value(&self, code: u32) -> DictValue<'_> {
        if code == NULL || code > self.len {
            return DictValue::Null;
        }
        let index = (code - 1) as usize;
        let (block, offset) = (index / BLOCK, index % BLOCK);
        match self.full.get(block) {
            Some(full) => full.get(self.storage, offset),
            None => self.current.get(self.storage, offset),
        }
    }

    fn hash_str(&self, value: &str) -> u64 {
        use std::hash::BuildHasher;
        self.hasher.hash_one(value)
    }

    /// The code of `key`, if it is in the dictionary.
    #[must_use]
    pub fn lookup(&self, key: &Key) -> Option<u32> {
        match key {
            Key::Int(value) => self.ints.get(value).copied(),
            Key::Str(value) => {
                let hash = self.hash_str(value);
                self.strs
                    .find(hash, |code| self.value(*code) == DictValue::Str(value))
                    .copied()
            }
        }
    }

    /// The code of `key`, adding it if it is new.
    pub fn encode(&mut self, key: &Key) -> u32 {
        if let Some(code) = self.lookup(key) {
            return code;
        }
        self.len += 1;
        let code = self.len;
        self.current.push(key);
        match key {
            Key::Int(value) => {
                self.ints.insert(*value, code);
            }
            Key::Str(value) => {
                let hash = self.hash_str(value);
                let Self {
                    strs,
                    hasher,
                    full,
                    current,
                    storage,
                    ..
                } = self;
                strs.insert_unique(hash, code, |code| {
                    use std::hash::BuildHasher;
                    let index = (*code - 1) as usize;
                    let (block, offset) = (index / BLOCK, index % BLOCK);
                    let text = match full.get(block) {
                        Some(full) => full.get(*storage, offset),
                        None => current.get(*storage, offset),
                    };
                    match text {
                        DictValue::Str(text) => hasher.hash_one(text),
                        _ => 0,
                    }
                });
            }
        }
        if self.current.len(self.storage) == BLOCK {
            self.full.push(Arc::new(std::mem::take(&mut self.current)));
        }
        code
    }

    /// An immutable view of the dictionary as it is now.
    pub fn snapshot(&mut self) -> DictSnapshot {
        let partial = match &self.published {
            Some((len, block)) if *len == self.len => Arc::clone(block),
            _ => {
                let block = Arc::new(self.current.clone());
                self.published = Some((self.len, Arc::clone(&block)));
                block
            }
        };
        DictSnapshot {
            storage: self.storage,
            generation: self.generation,
            len: self.len,
            full: Arc::from(self.full.as_slice()),
            partial,
        }
    }

    /// Approximate memory held by the values (not the index).
    #[must_use]
    pub fn value_bytes(&self) -> usize {
        self.full.iter().map(|b| b.bytes()).sum::<usize>() + self.current.bytes()
    }
}

/// A reader's view of a dictionary at one epoch.
#[derive(Clone, Debug)]
pub struct DictSnapshot {
    storage: Storage,
    generation: u32,
    len: u32,
    full: Arc<[Arc<Block>]>,
    partial: Arc<Block>,
}

impl DictSnapshot {
    #[must_use]
    pub fn len(&self) -> u32 {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn storage(&self) -> Storage {
        self.storage
    }

    /// See [`Dictionary::generation`].
    #[must_use]
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// The value of `code`; NULL for code 0 and for codes newer than this
    /// snapshot.
    #[must_use]
    pub fn get(&self, code: u32) -> DictValue<'_> {
        if code == NULL || code > self.len {
            return DictValue::Null;
        }
        let index = (code - 1) as usize;
        let (block, offset) = (index / BLOCK, index % BLOCK);
        match self.full.get(block) {
            Some(full) => full.get(self.storage, offset),
            None => self.partial.get(self.storage, offset),
        }
    }

    /// Approximate memory held by the values.
    #[must_use]
    pub fn value_bytes(&self) -> usize {
        self.full.iter().map(|b| b.bytes()).sum::<usize>() + self.partial.bytes()
    }

    /// Every code, ordered by value (strings by bytes, floats by total order,
    /// everything else numerically). For ordered filter lists and sorted
    /// output.
    #[must_use]
    pub fn sorted_codes(&self, float: bool) -> Vec<u32> {
        let mut codes: Vec<u32> = (1..=self.len).collect();
        match self.storage {
            Storage::Str => codes.sort_by(|a, b| match (self.get(*a), self.get(*b)) {
                (DictValue::Str(x), DictValue::Str(y)) => x.cmp(y),
                _ => std::cmp::Ordering::Equal,
            }),
            _ if float => codes.sort_by(|a, b| {
                let as_float = |code: u32| match self.get(code) {
                    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                    DictValue::Int(bits) => f64::from_bits(bits as u64),
                    _ => f64::NAN,
                };
                as_float(*a).total_cmp(&as_float(*b))
            }),
            _ => codes.sort_by_key(|code| match self.get(*code) {
                DictValue::Int(value) => value,
                _ => i128::MIN,
            }),
        }
        codes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_start_at_one_and_repeat_for_equal_values() {
        let mut dict = Dictionary::new(Storage::Str);
        let a = dict.encode(&Key::Str("north"));
        let b = dict.encode(&Key::Str("south"));
        assert_eq!((a, b), (1, 2));
        assert_eq!(dict.encode(&Key::Str("north")), 1);
        assert_eq!(dict.lookup(&Key::Str("east")), None);
        assert_eq!(dict.len(), 2);
    }

    #[test]
    fn a_snapshot_is_unaffected_by_later_appends() {
        let mut dict = Dictionary::new(Storage::I64);
        dict.encode(&Key::Int(10));
        let before = dict.snapshot();
        dict.encode(&Key::Int(20));
        let after = dict.snapshot();
        assert_eq!(before.len(), 1);
        assert_eq!(before.get(2), DictValue::Null);
        assert_eq!(after.get(2), DictValue::Int(20));
        assert_eq!(after.get(NULL), DictValue::Null);
    }

    #[test]
    fn values_survive_crossing_block_boundaries() {
        let mut dict = Dictionary::new(Storage::Str);
        let values: Vec<String> = (0..BLOCK * 2 + 10).map(|i| format!("v{i}")).collect();
        for value in &values {
            dict.encode(&Key::Str(value));
        }
        let snapshot = dict.snapshot();
        for (i, value) in values.iter().enumerate() {
            let code = u32::try_from(i + 1).unwrap_or(0);
            assert_eq!(snapshot.get(code), DictValue::Str(value));
            assert_eq!(dict.lookup(&Key::Str(value)), Some(code));
        }
    }

    #[test]
    fn an_unchanged_dictionary_reuses_its_published_block() {
        let mut dict = Dictionary::new(Storage::Str);
        dict.encode(&Key::Str("a"));
        let first = dict.snapshot();
        let second = dict.snapshot();
        assert!(Arc::ptr_eq(&first.partial, &second.partial));
    }

    #[test]
    fn sorted_codes_order_by_value() {
        let mut dict = Dictionary::new(Storage::Str);
        for value in ["pear", "apple", "fig"] {
            dict.encode(&Key::Str(value));
        }
        assert_eq!(dict.snapshot().sorted_codes(false), [2, 3, 1]);

        let mut floats = Dictionary::new(Storage::F64);
        for value in [2.5_f64, -1.0, 0.5] {
            floats.encode(&Key::Int(i128::from(value.to_bits())));
        }
        assert_eq!(floats.snapshot().sorted_codes(true), [2, 3, 1]);
    }
}
