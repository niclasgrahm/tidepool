//! Text search (plan §5.7): whether a string contains a needle, ignoring
//! the case of ASCII letters.
//!
//! One value at a time ([`Needle::is_in`]) for dictionaries and the general
//! path, and a morsel at a time ([`Needle::search`]) for plain string
//! columns, where a morsel's strings are one contiguous slice of the
//! segment's arena: lowercased into a scratch buffer (ASCII only, a
//! branch-free pass that vectorizes), searched with `memchr::memmem` (SIMD)
//! and each hit mapped to its row by walking the offsets. After a hit the
//! search jumps to the next row, so a row is found at most once, and a hit
//! that runs into the next row is no hit.

use std::fmt;
use std::ops::Range;

use memchr::memmem::Finder;

/// What a [`crate::Condition::Contains`] looks for.
pub struct Needle {
    text: String,
    /// `text`, ASCII letters lowercased.
    folded: Vec<u8>,
    /// Whether `text` has ASCII letters, so a haystack must be lowercased
    /// before it is searched.
    caseless: bool,
    finder: Finder<'static>,
}

/// `byte` with ASCII letters lowercased, without a branch.
#[inline]
fn fold(byte: u8) -> u8 {
    byte | (u8::from(byte.wrapping_sub(b'A') < 26) << 5)
}

/// `bytes` lowercased (ASCII only) into `out`, replacing what it held.
fn fold_into(bytes: &[u8], out: &mut Vec<u8>) {
    out.clear();
    out.extend(bytes.iter().map(|b| fold(*b)));
}

impl Needle {
    #[must_use]
    pub fn new(text: &str) -> Self {
        let folded: Vec<u8> = text.bytes().map(fold).collect();
        Self {
            text: text.to_string(),
            caseless: text.bytes().any(|b| b.is_ascii_alphabetic()),
            finder: Finder::new(&folded).into_owned(),
            folded,
        }
    }

    /// The text as given.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The text with ASCII letters lowercased: two needles that are equal
    /// here find the same rows.
    #[must_use]
    pub fn folded(&self) -> &[u8] {
        &self.folded
    }

    /// Whether every row this needle finds is also found by `wider`: when
    /// `self` contains `wider`'s text.
    #[must_use]
    pub fn narrows(&self, wider: &Self) -> bool {
        self.folded.len() > wider.folded.len() && wider.finder.find(&self.folded).is_some()
    }

    /// Whether `haystack` contains the needle.
    #[must_use]
    pub fn is_in(&self, haystack: &[u8]) -> bool {
        if !self.caseless {
            return self.finder.find(haystack).is_some();
        }
        // the first byte in either case, then the rest compared folded:
        // no copy of the haystack
        let Some((&first, rest)) = self.folded.split_first() else {
            return true;
        };
        let Some(last) = haystack.len().checked_sub(self.folded.len()) else {
            return false;
        };
        let upper = first.to_ascii_uppercase();
        memchr::memchr2_iter(first, upper, haystack)
            .take_while(|at| *at <= last)
            .any(|at| {
                haystack[at + 1..at + 1 + rest.len()]
                    .iter()
                    .zip(rest)
                    .all(|(h, n)| fold(*h) == *n)
            })
    }

    /// The rows in `rows` of a string column (its `bytes` and `offsets`,
    /// row `r` being `bytes[offsets[r]..offsets[r + 1]]`) that contain the
    /// needle, as bits relative to `rows.start` set in `out`. `scratch` is
    /// reused from call to call. An empty needle is in every row: the
    /// caller leaves out NULLs.
    pub fn search(
        &self,
        bytes: &[u8],
        offsets: &[u32],
        rows: Range<usize>,
        scratch: &mut Vec<u8>,
        out: &mut [u64],
    ) {
        if self.folded.is_empty() {
            for r in 0..rows.len() {
                out[r / 64] |= 1 << (r % 64);
            }
            return;
        }
        let base = offsets[rows.start] as usize;
        let end = offsets[rows.end] as usize;
        let haystack: &[u8] = if self.caseless {
            fold_into(&bytes[base..end], scratch);
            scratch
        } else {
            &bytes[base..end]
        };
        // where each row ends, relative to the slice
        let ends = &offsets[rows.start + 1..=rows.end];
        let mut at = 0;
        let mut row = 0;
        while let Some(found) = self.finder.find(&haystack[at..]) {
            let start = at + found;
            // the row the hit starts in: the first that ends past it
            while (ends[row] as usize - base) <= start {
                row += 1;
            }
            let row_end = ends[row] as usize - base;
            if start + self.folded.len() <= row_end {
                out[row / 64] |= 1 << (row % 64);
            }
            // a row is found once; a hit running into the next row is
            // none, and the next row is searched from its start
            at = row_end;
            row += 1;
            if row >= ends.len() {
                break;
            }
        }
    }
}

impl fmt::Debug for Needle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Needle({:?})", self.text)
    }
}

impl PartialEq for Needle {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A column of `rows`, as a segment stores it.
    fn column(rows: &[&str]) -> (Vec<u8>, Vec<u32>) {
        let mut bytes = Vec::new();
        let mut offsets = vec![0];
        for row in rows {
            bytes.extend_from_slice(row.as_bytes());
            offsets.push(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
        }
        (bytes, offsets)
    }

    /// The rows in `range` the needle finds, by morsel search.
    fn found(needle: &str, rows: &[&str], range: Range<usize>) -> Vec<usize> {
        let (bytes, offsets) = column(rows);
        let mut bits = vec![0u64; range.len().div_ceil(64)];
        Needle::new(needle).search(&bytes, &offsets, range.clone(), &mut Vec::new(), &mut bits);
        (0..range.len())
            .filter(|r| bits[r / 64] & (1 << (r % 64)) != 0)
            .map(|r| r + range.start)
            .collect()
    }

    #[test]
    fn case_is_ignored_for_ascii_letters_only() {
        let needle = Needle::new("Lock Timeout");
        assert!(needle.is_in(b"canceling statement due to lock timeout"));
        assert!(needle.is_in(b"LOCK TIMEOUT"));
        assert!(!needle.is_in(b"lock  timeout"));
        // other letters match exactly
        assert!(Needle::new("é").is_in("café".as_bytes()));
        assert!(!Needle::new("É").is_in("café".as_bytes()));
        // no letters: searched as bytes
        assert!(Needle::new("pid=12").is_in(b"x pid=12345"));
        assert!(Needle::new("").is_in(b""));
        assert!(!Needle::new("abc").is_in(b"ab"));
    }

    #[test]
    fn a_morsel_finds_each_matching_row_once() {
        let rows = ["error: disk", "ok", "ERROR error", "", "xerro", "rror"];
        assert_eq!(found("error", &rows, 0..rows.len()), vec![0, 2]);
        // a hit across two rows ("xerro" + "rror" hides "errorr") is none
        assert_eq!(found("rorr", &rows, 0..rows.len()), Vec::<usize>::new());
        // a morsel in the middle of the column
        assert_eq!(found("e", &rows, 1..5), vec![2, 4]);
        assert_eq!(found("", &rows, 2..4), vec![2, 3]);
        assert_eq!(found("rror", &rows, 0..rows.len()), vec![0, 2, 5]);
    }

    #[test]
    fn a_longer_needle_narrows_the_one_it_contains() {
        let (time, timeout) = (Needle::new("Time"), Needle::new("timeout"));
        assert!(timeout.narrows(&time));
        assert!(!time.narrows(&timeout));
        assert!(!time.narrows(&Needle::new("time")));
        assert!(!Needle::new("tim eout").narrows(&Needle::new("meo")));
    }

    #[test]
    fn the_morsel_search_agrees_with_one_value_at_a_time() {
        let words = [
            "Lock", "lock", "timeout", "TIME", "out", "", "é", "x=1", " ",
        ];
        let mut seed = 7u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let rows: Vec<String> = (0..300)
            .map(|_| {
                (0..next() % 5)
                    .map(|_| words[usize::try_from(next() % 9).unwrap_or(0)])
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();
        let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
        for needle in ["lock", "timeout", "Lockl", "e", "x=1", "é", " ", "outLock"] {
            let n = Needle::new(needle);
            let one: Vec<usize> = (0..rows.len())
                .filter(|r| n.is_in(rows[*r].as_bytes()))
                .collect();
            assert_eq!(found(needle, &rows, 0..rows.len()), one, "{needle}");
            let some: Vec<usize> = one
                .iter()
                .copied()
                .filter(|r| (40..170).contains(r))
                .collect();
            assert_eq!(found(needle, &rows, 40..170), some, "{needle} in a morsel");
        }
    }
}
