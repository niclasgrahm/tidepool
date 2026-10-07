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
        let mut words = vec![0u64; (len * usize::from(width)).div_ceil(64)];
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

    fn bytes(&self) -> usize {
        self.words.len() * 8
    }
}

/// A set of row offsets (NULLs), one bit per row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bits(Vec<u64>);

impl Bits {
    #[must_use]
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
