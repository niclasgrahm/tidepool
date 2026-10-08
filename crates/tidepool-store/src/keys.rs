//! Idempotency keys as the data directory keeps them (plan §4.5): a keyed
//! write's log record carries its key and the response it was acknowledged
//! with, and checkpoints carry the keys still live, so a retry after a
//! restart gets the first response instead of writing the batch again.
//!
//! The server answers retries from its own map; this is what that map is
//! rebuilt from on startup. Keys are kept for [`TTL`] from when they were
//! written, by the wall clock (the time is in the record, so it runs on
//! across restarts), and at most [`CAPACITY`] of them, oldest dropped first.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tidepool_model::ir::TableId;

use crate::codec::{In, Out, corrupt};
use crate::store::Applied;

/// How long a key is remembered.
pub const TTL: Duration = Duration::from_hours(24);
/// Keys remembered at most; the oldest go first.
pub const CAPACITY: usize = 100_000;

/// A keyed write and what it was acknowledged with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keyed {
    pub table: TableId,
    pub key: String,
    /// When it was written, in milliseconds since the Unix epoch.
    pub at: u64,
    pub applied: Applied,
    /// The epoch it was acknowledged as visible from.
    pub epoch: u64,
}

impl Keyed {
    /// How long ago it was written, as of `now` (ms since the Unix epoch).
    #[must_use]
    pub fn age(&self, now: u64) -> Duration {
        Duration::from_millis(now.saturating_sub(self.at))
    }
}

/// Now, in milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The key, time and response; the table is written by the caller.
pub(crate) fn put(out: &mut Out, keyed: &Keyed) {
    out.bytes(keyed.key.as_bytes());
    out.u64(keyed.at);
    // counts, not lengths: `In::len` would bound them by the bytes left
    for n in [
        keyed.applied.written,
        keyed.applied.replaced,
        keyed.applied.missing,
    ] {
        out.u64(n as u64);
    }
    out.u64(keyed.epoch);
}

pub(crate) fn get(input: &mut In, table: TableId) -> io::Result<Keyed> {
    let key = String::from_utf8(input.bytes()?.to_vec()).map_err(|_| corrupt("key"))?;
    let at = input.u64()?;
    let mut count = || usize::try_from(input.u64()?).map_err(|_| corrupt("key"));
    let (written, replaced, missing) = (count()?, count()?, count()?);
    Ok(Keyed {
        table,
        key,
        at,
        applied: Applied {
            written,
            replaced,
            missing,
            table_gone: false,
        },
        epoch: input.u64()?,
    })
}

/// The keys written in the last [`TTL`], oldest first.
#[derive(Debug, Default)]
pub(crate) struct Recent(VecDeque<Keyed>);

impl Recent {
    pub fn push(&mut self, keyed: Keyed, now: u64) {
        self.0.push_back(keyed);
        self.trim(now);
    }

    /// Drop what has expired as of `now`, and the oldest past the capacity.
    pub fn trim(&mut self, now: u64) {
        while self
            .0
            .front()
            .is_some_and(|k| k.age(now) >= TTL || self.0.len() > CAPACITY)
        {
            self.0.pop_front();
        }
    }

    pub fn list(&self) -> Vec<Keyed> {
        self.0.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyed(key: &str, at: u64) -> Keyed {
        Keyed {
            table: TableId(1),
            key: key.into(),
            at,
            applied: Applied::default(),
            epoch: 1,
        }
    }

    #[test]
    fn keys_expire_by_the_time_they_were_written() {
        let ttl = u64::try_from(TTL.as_millis()).unwrap_or(u64::MAX);
        let mut recent = Recent::default();
        recent.push(keyed("a", 1_000), 1_000);
        recent.push(keyed("b", 2_000), 2_000);
        recent.trim(1_000 + ttl - 1);
        assert_eq!(recent.list().len(), 2);
        recent.trim(1_000 + ttl);
        assert_eq!(recent.list(), vec![keyed("b", 2_000)]);
    }

    #[test]
    fn a_key_round_trips() {
        let mut k = keyed("kayak:run-1:42", 1_234);
        k.applied = Applied {
            written: 3,
            replaced: 2,
            missing: 1,
            table_gone: false,
        };
        let mut out = Out::default();
        put(&mut out, &k);
        let back = get(&mut In::new(&out.0), TableId(1)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back, k);
    }
}
