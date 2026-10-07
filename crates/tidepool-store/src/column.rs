//! Column data inside a segment, as built (plain vectors) and as sealed
//! (bit-packed).
//!
//! Dictionary codes are packed to ⌈log₂(max code + 1)⌉ bits. 64-bit integers
//! (which also carry dates, timestamps and small decimals) are stored as an
//! offset from the segment's minimum, packed the same way ("frame of
//! reference"). Floats, wide decimals and plain strings stay as they are.
//! Sealed integer and code columns also carry their min and max, so a reader
//! can skip a whole segment that can't match.

use crate::value::Storage;
use std::ops::Range;

/// A packed array of `len` unsigned values of `width` bits each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packed {
    width: u8,
    len: usize,
    words: Vec<u64>,
}

impl Packed {
    #[must_use]
    pub fn pack(values: impl ExactSizeIterator<Item = u64> + Clone) -> Self {
        let max = values.clone().max().unwrap_or(0);
        let width = u8::try_from(64 - max.leading_zeros()).unwrap_or(64);
        let len = values.len();
        // one spare word, so unpacking can always read the next word
        let mut words = vec![0u64; (len * usize::from(width)).div_ceil(64) + 1];
        if width > 0 {
            for (i, value) in values.enumerate() {
                let bit = i * usize::from(width);
                let (word, shift) = (bit / 64, bit % 64);
                words[word] |= value << shift;
                if shift + usize::from(width) > 64 {
                    words[word + 1] |= value >> (64 - shift);
                }
            }
        }
        Self { width, len, words }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn width(&self) -> u8 {
        self.width
    }

    #[must_use]
    #[inline]
    pub fn get(&self, index: usize) -> u64 {
        if self.width == 0 {
            return 0;
        }
        let width = usize::from(self.width);
        let bit = index * width;
        let (word, shift) = (bit / 64, bit % 64);
        let mask = if width == 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        };
        let mut value = self.words[word] >> shift;
        if shift + width > 64 {
            value |= self.words[word + 1] << (64 - shift);
        }
        value & mask
    }

    /// The values of `rows`, in order. Much faster than [`Packed::get`]
    /// per index.
    #[must_use]
    pub fn iter(&self, rows: Range<usize>) -> PackedIter<'_> {
        let width = usize::from(self.width);
        PackedIter {
            words: &self.words,
            bit: rows.start * width,
            width,
            mask: if width == 64 {
                u64::MAX
            } else {
                (1u64 << width) - 1
            },
            remaining: rows.len(),
        }
    }

    /// `convert` of the values of `rows`, into `out` (as long as `rows`).
    /// When the width divides 64 and `rows` starts on a word, no value
    /// straddles two words, and a word is unpacked by shifting alone.
    pub fn unpack<T>(&self, rows: Range<usize>, out: &mut [T], convert: impl Fn(u64) -> T) {
        let width = usize::from(self.width);
        // whole blocks of 64 values (exactly `width` words) from a block
        // boundary: unpacked by a loop specialized for the width
        if rows.start.is_multiple_of(64) && (1..=32).contains(&width) {
            let blocks = rows.len() / 64;
            let first = rows.start / 64 * width;
            let (head, tail) = out.split_at_mut(blocks * 64);
            let words = &self.words[first..];
            macro_rules! fixed {
                ($($w:literal)*) => {
                    match width {
                        $($w => unpack_blocks::<$w, T>(words, head, &convert),)*
                        _ => {}
                    }
                };
            }
            fixed!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32);
            let rest = rows.start + blocks * 64..rows.end;
            for (slot, v) in tail.iter_mut().zip(self.iter(rest)) {
                *slot = convert(v);
            }
            return;
        }
        for (slot, v) in out.iter_mut().zip(self.iter(rows)) {
            *slot = convert(v);
        }
    }

    fn bytes(&self) -> usize {
        self.words.len() * 8
    }
}

/// Unpack `out.len() / 64` blocks of 64 `W`-bit values from `words`
/// (block `b` is words `b*W .. (b+1)*W`). With `W` a constant, every shift
/// and index in a block is too, so the loop unrolls into straight code.
fn unpack_blocks<const W: usize, T>(words: &[u64], out: &mut [T], convert: &impl Fn(u64) -> T) {
    let mask = (1u64 << W) - 1;
    for (block, chunk) in out.chunks_exact_mut(64).enumerate() {
        // one spare word: a value may end in the next block's first word
        // only when it doesn't (64 values fill exactly W words), but the
        // read below is simpler with it, and `pack` always leaves one
        let base = &words[block * W..=block * W + W];
        for (i, slot) in chunk.iter_mut().enumerate() {
            let bit = i * W;
            let (word, shift) = (bit / 64, bit % 64);
            let value = (base[word] >> shift) | ((base[word + 1] << 1) << (63 - shift));
            *slot = convert(value & mask);
        }
    }
}

/// Values of a [`Packed`] array in order: see [`Packed::iter`].
pub struct PackedIter<'a> {
    words: &'a [u64],
    bit: usize,
    width: usize,
    mask: u64,
    remaining: usize,
}

impl Iterator for PackedIter<'_> {
    type Item = u64;

    #[inline]
    fn next(&mut self) -> Option<u64> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        if self.width == 0 {
            return Some(0);
        }
        let (word, shift) = (self.bit >> 6, self.bit & 63);
        // `<< 1 << (63 - shift)`: the next word's bits, without a 64-bit
        // shift when `shift` is 0 (there is always a spare last word)
        let value = (self.words[word] >> shift) | ((self.words[word + 1] << 1) << (63 - shift));
        self.bit += self.width;
        Some(value & self.mask)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for PackedIter<'_> {}

/// A set of row offsets (NULLs), one bit per row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bits(Vec<u64>);

impl Bits {
    #[must_use]
    #[inline]
    pub fn get(&self, index: usize) -> bool {
        self.0
            .get(index / 64)
            .is_some_and(|w| w >> (index % 64) & 1 == 1)
    }

    pub fn set(&mut self, index: usize) {
        if self.0.len() <= index / 64 {
            self.0.resize(index / 64 + 1, 0);
        }
        self.0[index / 64] |= 1 << (index % 64);
    }

    fn bytes(&self) -> usize {
        self.0.len() * 8
    }
}

/// One stored value, as a reader sees it before decoding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Raw<'a> {
    Null,
    Code(u32),
    I64(i64),
    I128(i128),
    F64(f64),
    Str(&'a str),
}

/// A column of a frozen segment.
#[derive(Clone, Debug, PartialEq)]
pub enum ColumnData {
    Codes(Vec<u32>),
    PackedCodes {
        packed: Packed,
        min: u32,
        max: u32,
    },
    I64 {
        values: Vec<i64>,
        nulls: Option<Bits>,
    },
    PackedI64 {
        base: i64,
        packed: Packed,
        nulls: Option<Bits>,
        min: i64,
        max: i64,
    },
    I128 {
        values: Vec<i128>,
        nulls: Option<Bits>,
    },
    F64 {
        values: Vec<f64>,
        nulls: Option<Bits>,
    },
    Str {
        bytes: Vec<u8>,
        offsets: Vec<u32>,
        nulls: Option<Bits>,
    },
}

impl ColumnData {
    #[must_use]
    pub fn get(&self, row: usize) -> Raw<'_> {
        let null = |nulls: &Option<Bits>| nulls.as_ref().is_some_and(|n| n.get(row));
        match self {
            Self::Codes(codes) => Raw::Code(codes[row]),
            Self::PackedCodes { packed, .. } => {
                Raw::Code(u32::try_from(packed.get(row)).unwrap_or(0))
            }
            Self::I64 { nulls, .. }
            | Self::PackedI64 { nulls, .. }
            | Self::I128 { nulls, .. }
            | Self::F64 { nulls, .. }
            | Self::Str { nulls, .. }
                if null(nulls) =>
            {
                Raw::Null
            }
            Self::I64 { values, .. } => Raw::I64(values[row]),
            #[allow(clippy::cast_possible_wrap)]
            Self::PackedI64 { base, packed, .. } => {
                Raw::I64(base.wrapping_add(packed.get(row) as i64))
            }
            Self::I128 { values, .. } => Raw::I128(values[row]),
            Self::F64 { values, .. } => Raw::F64(values[row]),
            Self::Str { bytes, offsets, .. } => {
                let (start, end) = (offsets[row] as usize, offsets[row + 1] as usize);
                Raw::Str(std::str::from_utf8(&bytes[start..end]).unwrap_or_default())
            }
        }
    }

    /// Approximate memory held.
    #[must_use]
    pub fn bytes(&self) -> usize {
        let nulls = |n: &Option<Bits>| n.as_ref().map_or(0, Bits::bytes);
        match self {
            Self::Codes(codes) => codes.len() * 4,
            Self::PackedCodes { packed, .. } => packed.bytes(),
            Self::I64 { values, nulls: n } => values.len() * 8 + nulls(n),
            Self::PackedI64 {
                packed, nulls: n, ..
            } => packed.bytes() + nulls(n),
            Self::I128 { values, nulls: n } => values.len() * 16 + nulls(n),
            Self::F64 { values, nulls: n } => values.len() * 8 + nulls(n),
            Self::Str {
                bytes,
                offsets,
                nulls: n,
            } => bytes.len() + offsets.len() * 4 + nulls(n),
        }
    }

    /// The dictionary codes of `rows` of a code column, into `out` (cleared
    /// first). Other columns leave `out` empty.
    pub fn decode_codes(&self, rows: Range<usize>, out: &mut Vec<u32>) {
        out.clear();
        match self {
            Self::Codes(codes) => out.extend_from_slice(&codes[rows]),
            Self::PackedCodes { packed, .. } => {
                out.resize(rows.len(), 0);
                #[allow(clippy::cast_possible_truncation)]
                packed.unpack(rows, out, |v| v as u32);
            }
            _ => {}
        }
    }

    /// The codes at rows `start + r` for each `r` of `at`, into `out[r]`.
    /// `out` is sized to `len` first; its other entries keep what they
    /// held. For reading a few scattered rows without decoding them all.
    pub fn gather_codes(&self, start: usize, at: &[u32], len: usize, out: &mut Vec<u32>) {
        out.resize(len, 0);
        match self {
            Self::Codes(codes) => {
                for r in at {
                    out[*r as usize] = codes[start + *r as usize];
                }
            }
            Self::PackedCodes { packed, .. } =>
            {
                #[allow(clippy::cast_possible_truncation)]
                for r in at {
                    out[*r as usize] = packed.get(start + *r as usize) as u32;
                }
            }
            _ => {}
        }
    }

    /// [`ColumnData::gather_codes`] through `map`, as
    /// [`ColumnData::map_codes`] does for every row.
    pub fn gather_map_codes<T: Copy, U: Copy + From<T>>(
        &self,
        start: usize,
        at: &[u32],
        map: &[T],
        missing: U,
        out: &mut [U],
    ) {
        let lookup = |code: u64| {
            usize::try_from(code)
                .ok()
                .and_then(|c| map.get(c))
                .map_or(missing, |v| U::from(*v))
        };
        match self {
            Self::Codes(codes) => {
                for r in at {
                    out[*r as usize] = lookup(u64::from(codes[start + *r as usize]));
                }
            }
            Self::PackedCodes { packed, .. } => {
                for r in at {
                    out[*r as usize] = lookup(packed.get(start + *r as usize));
                }
            }
            _ => {}
        }
    }

    /// [`ColumnData::gather_codes`] for a 64-bit integer column, with NULLs.
    pub fn gather_i64(
        &self,
        start: usize,
        at: &[u32],
        len: usize,
        values: &mut Vec<i64>,
        nulls: &mut Vec<bool>,
    ) {
        values.resize(len, 0);
        nulls.resize(len, false);
        let null_bits = match self {
            Self::I64 { values: v, nulls } => {
                for r in at {
                    values[*r as usize] = v[start + *r as usize];
                }
                nulls
            }
            Self::PackedI64 {
                base,
                packed,
                nulls,
                ..
            } => {
                #[allow(clippy::cast_possible_wrap)]
                for r in at {
                    values[*r as usize] = base.wrapping_add(packed.get(start + *r as usize) as i64);
                }
                nulls
            }
            _ => return,
        };
        for r in at {
            nulls[*r as usize] = null_bits
                .as_ref()
                .is_some_and(|b| b.get(start + *r as usize));
        }
    }

    /// [`ColumnData::gather_i64`] for a float column.
    pub fn gather_f64(
        &self,
        start: usize,
        at: &[u32],
        len: usize,
        values: &mut Vec<f64>,
        nulls: &mut Vec<bool>,
    ) {
        values.resize(len, 0.0);
        nulls.resize(len, false);
        if let Self::F64 {
            values: v,
            nulls: bits,
        } = self
        {
            for r in at {
                values[*r as usize] = v[start + *r as usize];
                nulls[*r as usize] = bits.as_ref().is_some_and(|b| b.get(start + *r as usize));
            }
        }
    }

    /// `map[code]` for the code of every row of `rows` of a code column
    /// (`missing` past the end of `map`), into `out` (cleared first).
    /// Following a relationship is decoding its foreign key through a map.
    pub fn map_codes<T: Copy, U: Copy + From<T>>(
        &self,
        rows: Range<usize>,
        map: &[T],
        missing: U,
        out: &mut Vec<U>,
    ) {
        out.clear();
        out.resize(rows.len(), missing);
        let at = |code: u64| {
            usize::try_from(code)
                .ok()
                .and_then(|c| map.get(c))
                .map_or(missing, |v| U::from(*v))
        };
        match self {
            Self::Codes(codes) => {
                for (slot, code) in out.iter_mut().zip(&codes[rows]) {
                    *slot = at(u64::from(*code));
                }
            }
            Self::PackedCodes { packed, .. } => {
                packed.unpack(rows, out, at);
            }
            _ => {}
        }
    }

    /// The values of `rows` of a 64-bit integer column into `values`, and
    /// whether each is NULL into `nulls` (both cleared first). Other columns
    /// leave them empty.
    pub fn decode_i64(&self, rows: Range<usize>, values: &mut Vec<i64>, nulls: &mut Vec<bool>) {
        values.clear();
        nulls.clear();
        let null_bits = match self {
            Self::I64 {
                values: v,
                nulls: n,
            } => {
                values.extend_from_slice(&v[rows.clone()]);
                n
            }
            Self::PackedI64 {
                base,
                packed,
                nulls: n,
                ..
            } => {
                values.resize(rows.len(), 0);
                #[allow(clippy::cast_possible_wrap)]
                packed.unpack(rows.clone(), values, |v| base.wrapping_add(v as i64));
                n
            }
            _ => return,
        };
        match null_bits {
            Some(bits) => nulls.extend(rows.map(|i| bits.get(i))),
            None => nulls.resize(rows.len(), false),
        }
    }

    /// The values and NULLs of `rows` of a float column, as
    /// [`ColumnData::decode_i64`].
    pub fn decode_f64(&self, rows: Range<usize>, values: &mut Vec<f64>, nulls: &mut Vec<bool>) {
        values.clear();
        nulls.clear();
        if let Self::F64 {
            values: v,
            nulls: n,
        } = self
        {
            values.extend_from_slice(&v[rows.clone()]);
            match n {
                Some(bits) => nulls.extend(rows.map(|i| bits.get(i))),
                None => nulls.resize(rows.len(), false),
            }
        }
    }

    /// Min and max of a sealed integer or code column, for pruning.
    #[must_use]
    pub fn range(&self) -> Option<(i64, i64)> {
        match self {
            Self::PackedCodes { min, max, .. } => Some((i64::from(*min), i64::from(*max))),
            Self::PackedI64 { min, max, .. } => Some((*min, *max)),
            _ => None,
        }
    }
}

/// A column being filled: the writer's side of a chunk.
#[derive(Clone, Debug)]
pub enum ColumnBuilder {
    Codes(Vec<u32>),
    I64 {
        values: Vec<i64>,
        nulls: Option<Bits>,
    },
    I128 {
        values: Vec<i128>,
        nulls: Option<Bits>,
    },
    F64 {
        values: Vec<f64>,
        nulls: Option<Bits>,
    },
    Str {
        bytes: Vec<u8>,
        offsets: Vec<u32>,
        nulls: Option<Bits>,
    },
}

impl ColumnBuilder {
    /// An empty builder for a column stored as `storage`, dictionary-encoded or not.
    #[must_use]
    pub fn new(storage: Storage, dict: bool) -> Self {
        match (dict, storage) {
            (true, _) => Self::Codes(Vec::new()),
            (false, Storage::Bool | Storage::I64) => Self::I64 {
                values: Vec::new(),
                nulls: None,
            },
            (false, Storage::I128) => Self::I128 {
                values: Vec::new(),
                nulls: None,
            },
            (false, Storage::F64) => Self::F64 {
                values: Vec::new(),
                nulls: None,
            },
            (false, Storage::Str) => Self::Str {
                bytes: Vec::new(),
                offsets: vec![0],
                nulls: None,
            },
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Codes(codes) => codes.len(),
            Self::I64 { values, .. } => values.len(),
            Self::I128 { values, .. } => values.len(),
            Self::F64 { values, .. } => values.len(),
            Self::Str { offsets, .. } => offsets.len() - 1,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn mark_null(nulls: &mut Option<Bits>, row: usize) {
        nulls.get_or_insert_with(Bits::default).set(row);
    }

    /// Append one value. The value must match the builder's kind; a mismatch
    /// is a bug in the caller and stores NULL.
    pub fn push(&mut self, value: Raw) {
        let row = self.len();
        match (self, value) {
            (Self::Codes(codes), Raw::Code(code)) => codes.push(code),
            (Self::Codes(codes), _) => codes.push(0),
            (Self::I64 { values, .. }, Raw::I64(v)) => values.push(v),
            (Self::I64 { values, nulls }, _) => {
                values.push(0);
                Self::mark_null(nulls, row);
            }
            (Self::I128 { values, .. }, Raw::I128(v)) => values.push(v),
            (Self::I128 { values, nulls }, _) => {
                values.push(0);
                Self::mark_null(nulls, row);
            }
            (Self::F64 { values, .. }, Raw::F64(v)) => values.push(v),
            (Self::F64 { values, nulls }, _) => {
                values.push(0.0);
                Self::mark_null(nulls, row);
            }
            (Self::Str { bytes, offsets, .. }, Raw::Str(s)) => {
                bytes.extend_from_slice(s.as_bytes());
                offsets.push(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
            }
            (
                Self::Str {
                    bytes,
                    offsets,
                    nulls,
                },
                _,
            ) => {
                offsets.push(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
                Self::mark_null(nulls, row);
            }
        }
    }

    /// Freeze into column data as is (a tail chunk).
    #[must_use]
    pub fn freeze(self) -> ColumnData {
        match self {
            Self::Codes(codes) => ColumnData::Codes(codes),
            Self::I64 { values, nulls } => ColumnData::I64 { values, nulls },
            Self::I128 { values, nulls } => ColumnData::I128 { values, nulls },
            Self::F64 { values, nulls } => ColumnData::F64 { values, nulls },
            Self::Str {
                bytes,
                offsets,
                nulls,
            } => ColumnData::Str {
                bytes,
                offsets,
                nulls,
            },
        }
    }
}

/// Concatenate the same column of several chunks and compress it: the
/// column of a sealed segment.
#[must_use]
pub fn seal(parts: &[&ColumnData], storage: Storage, dict: bool) -> ColumnData {
    let mut builder = ColumnBuilder::new(storage, dict);
    for part in parts {
        let rows = match part {
            ColumnData::Codes(c) => c.len(),
            ColumnData::I64 { values, .. } => values.len(),
            ColumnData::I128 { values, .. } => values.len(),
            ColumnData::F64 { values, .. } => values.len(),
            ColumnData::Str { offsets, .. } => offsets.len() - 1,
            ColumnData::PackedCodes { packed, .. } | ColumnData::PackedI64 { packed, .. } => {
                packed.len()
            }
        };
        for row in 0..rows {
            builder.push(part.get(row));
        }
    }
    match builder {
        ColumnBuilder::Codes(codes) => {
            let min = codes.iter().copied().min().unwrap_or(0);
            let max = codes.iter().copied().max().unwrap_or(0);
            ColumnData::PackedCodes {
                packed: Packed::pack(codes.iter().map(|c| u64::from(*c))),
                min,
                max,
            }
        }
        ColumnBuilder::I64 { values, nulls } => {
            let live = |i: &usize| !nulls.as_ref().is_some_and(|n| n.get(*i));
            let min = (0..values.len())
                .filter(live)
                .map(|i| values[i])
                .min()
                .unwrap_or(0);
            let max = (0..values.len())
                .filter(live)
                .map(|i| values[i])
                .max()
                .unwrap_or(0);
            // NULL slots hold 0 in the builder; store them at the base
            #[allow(clippy::cast_sign_loss)]
            let offsets = values.iter().enumerate().map(|(i, v)| {
                if live(&i) {
                    v.wrapping_sub(min) as u64
                } else {
                    0
                }
            });
            ColumnData::PackedI64 {
                base: min,
                packed: Packed::pack(offsets.collect::<Vec<_>>().into_iter()),
                nulls,
                min,
                max,
            }
        }
        other => other.freeze(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpacking_a_range_matches_reading_each_value() {
        for width in 0u32..=64 {
            // values that use the full width, so a shift error shows
            let values: Vec<u64> = (0..300u64)
                .map(|i| match width {
                    0 => 0,
                    64 => i.wrapping_mul(0x9E37_79B9_7F4A_7C15),
                    w => i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - w),
                })
                .collect();
            let packed = Packed::pack(values.iter().copied());
            for rows in [0..300, 64..192, 128..300, 1..299, 37..38, 0..0] {
                let mut out = vec![0u64; rows.len()];
                packed.unpack(rows.clone(), &mut out, |v| v);
                assert_eq!(out, values[rows.clone()], "width {width}, rows {rows:?}");
            }
        }
    }

    #[test]
    fn packing_round_trips_at_every_width() {
        for width in [0u32, 1, 3, 7, 13, 31, 33, 63, 64] {
            let max = if width == 64 {
                u64::MAX
            } else {
                (1u64 << width).saturating_sub(1)
            };
            let values: Vec<u64> = (0..200u64)
                .map(|i| {
                    if max == 0 {
                        0
                    } else {
                        i.wrapping_mul(2_654_435_761) % max.max(1)
                    }
                })
                .chain([max])
                .collect();
            let packed = Packed::pack(values.iter().copied());
            assert_eq!(u32::from(packed.width()), width, "width for max {max}");
            for (i, v) in values.iter().enumerate() {
                assert_eq!(packed.get(i), *v, "width {width}, index {i}");
            }
        }
    }

    #[test]
    fn sealing_concatenates_and_keeps_every_value() {
        let mut a = ColumnBuilder::new(Storage::I64, false);
        let mut b = ColumnBuilder::new(Storage::I64, false);
        for v in [100, 105, 103] {
            a.push(Raw::I64(v));
        }
        b.push(Raw::Null);
        b.push(Raw::I64(-3));
        let (a, b) = (a.freeze(), b.freeze());
        let sealed = seal(&[&a, &b], Storage::I64, false);
        let got: Vec<Raw> = (0..5).map(|i| sealed.get(i)).collect();
        assert_eq!(
            got,
            [
                Raw::I64(100),
                Raw::I64(105),
                Raw::I64(103),
                Raw::Null,
                Raw::I64(-3)
            ]
        );
        assert_eq!(sealed.range(), Some((-3, 105)));
    }

    #[test]
    fn sealed_codes_are_packed_narrowly() {
        let mut builder = ColumnBuilder::new(Storage::Str, true);
        for code in [1, 2, 3, 0, 2] {
            builder.push(Raw::Code(code));
        }
        let column = builder.freeze();
        let sealed = seal(&[&column], Storage::Str, true);
        assert!(matches!(&sealed, ColumnData::PackedCodes { packed, .. } if packed.width() == 2));
        assert_eq!(sealed.get(3), Raw::Code(0));
        assert_eq!(sealed.get(4), Raw::Code(2));
    }

    #[test]
    fn plain_strings_and_nulls() {
        let mut builder = ColumnBuilder::new(Storage::Str, false);
        builder.push(Raw::Str("hello"));
        builder.push(Raw::Null);
        builder.push(Raw::Str(""));
        let column = builder.freeze();
        assert_eq!(column.get(0), Raw::Str("hello"));
        assert_eq!(column.get(1), Raw::Null);
        assert_eq!(column.get(2), Raw::Str(""));
    }
}
