//! A small binary codec for what the store writes to disk: little-endian
//! integers, length-prefixed bytes, and frames checked by CRC32. Reading
//! never panics on bad input: a short or corrupt file is an `Err`, which is
//! what recovery needs to stop at a torn write.

use std::io;

/// Bytes being written.
#[derive(Default)]
pub(crate) struct Out(pub Vec<u8>);

impl Out {
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i128(&mut self, v: i128) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn len(&mut self, n: usize) {
        self.u64(n as u64);
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.0.extend_from_slice(b);
    }
    pub fn u32s(&mut self, values: &[u32]) {
        self.len(values.len());
        self.0.reserve(values.len() * 4);
        for v in values {
            self.0.extend_from_slice(&v.to_le_bytes());
        }
    }
    pub fn u64s(&mut self, values: &[u64]) {
        self.len(values.len());
        self.0.reserve(values.len() * 8);
        for v in values {
            self.0.extend_from_slice(&v.to_le_bytes());
        }
    }
    pub fn i64s(&mut self, values: &[i64]) {
        self.len(values.len());
        self.0.reserve(values.len() * 8);
        for v in values {
            self.0.extend_from_slice(&v.to_le_bytes());
        }
    }
}

pub(crate) fn corrupt(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("corrupt {what}"))
}

/// Bytes being read.
pub(crate) struct In<'a> {
    pub bytes: &'a [u8],
    pub at: usize,
}

impl<'a> In<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .filter(|e| *e <= self.bytes.len())
            .ok_or_else(|| corrupt("data (too short)"))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }

    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    pub fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    pub fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_le_bytes(self.array()?))
    }
    pub fn i128(&mut self) -> io::Result<i128> {
        Ok(i128::from_le_bytes(self.array()?))
    }
    pub fn len(&mut self) -> io::Result<usize> {
        let n = self.u64()?;
        let n = usize::try_from(n).map_err(|_| corrupt("length"))?;
        // no length can exceed what's left: refuse before allocating
        if n > self.bytes.len() - self.at {
            return Err(corrupt("length"));
        }
        Ok(n)
    }
    pub fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let n = self.len()?;
        self.take(n)
    }
    pub fn u32s(&mut self) -> io::Result<Vec<u32>> {
        let n = self.len()?;
        Ok(self
            .take(n.checked_mul(4).ok_or_else(|| corrupt("length"))?)?
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect())
    }
    pub fn u64s(&mut self) -> io::Result<Vec<u64>> {
        let n = self.len()?;
        Ok(self
            .take(n.checked_mul(8).ok_or_else(|| corrupt("length"))?)?
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_le_bytes(*c))
            .collect())
    }
    pub fn i64s(&mut self) -> io::Result<Vec<i64>> {
        Ok(self
            .u64s()?
            .into_iter()
            .map(|v| i64::from_le_bytes(v.to_le_bytes()))
            .collect())
    }
}

/// `payload` framed: length, CRC32 of the payload, the payload.
pub(crate) fn frame(payload: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(
        &u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    out.extend_from_slice(&crc32fast::hash(payload).to_le_bytes());
    out.extend_from_slice(payload);
}

/// The next frame of `bytes` from `at`: its payload and where the next one
/// starts, or `None` at the end or at the first torn or corrupt frame.
pub(crate) fn unframe(bytes: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let header = bytes.get(at..at + 8)?;
    let len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let payload = bytes.get(at + 8..at + 8 + len)?;
    (crc32fast::hash(payload) == crc).then_some((payload, at + 8 + len))
}

/// Write `bytes` to `path` atomically: a temporary file, fsynced, renamed
/// over it, and the directory fsynced so the rename itself survives.
pub(crate) fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| corrupt("path"))?;
    std::fs::create_dir_all(dir)?;
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)?;
    sync_dir(dir)
}

/// fsync a directory, so files created or renamed in it survive a crash.
pub(crate) fn sync_dir(dir: &std::path::Path) -> io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_a_torn_one_ends_the_stream() {
        let mut out = Vec::new();
        frame(b"first", &mut out);
        frame(b"second", &mut out);
        let whole = out.len();
        let Some((one, next)) = unframe(&out, 0) else {
            panic!("the first frame should read")
        };
        assert_eq!(one, b"first");
        let Some((two, end)) = unframe(&out, next) else {
            panic!("the second frame should read")
        };
        assert_eq!((two, end), (&b"second"[..], whole));
        assert!(unframe(&out, end).is_none());
        // torn: half the second frame
        assert!(unframe(&out[..whole - 3], next).is_none());
        // corrupt: a flipped bit
        out[whole - 1] ^= 1;
        assert!(unframe(&out, next).is_none());
    }

    #[test]
    fn reading_past_the_end_is_an_error_not_a_panic() {
        let mut out = Out::default();
        out.u32s(&[1, 2, 3]);
        let bytes = &out.0[..out.0.len() - 1];
        assert!(In::new(bytes).u32s().is_err());
        assert!(In::new(&[255; 8]).bytes().is_err());
    }
}
