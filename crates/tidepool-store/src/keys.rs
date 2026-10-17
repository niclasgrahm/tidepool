//! Idempotency keys as the data directory keeps them (plan §4.5): a keyed
//! write's log record carries its key and the response it was acknowledged
//! with, and checkpoints carry the keys still live, so a retry after a
//! restart gets the first response instead of writing the batch again.
//!
//! The server answers retries from its own map; this is what that map is
//! rebuilt from on startup. Keys are kept for [`TTL`] from when they were
//! written, by the wall clock (the time is in the record, so it runs on
//! across restarts), and at most [`CAPACITY`] of them per credential,
//! oldest dropped first.
//!
//! A key belongs to the credential it was sent with (plan §9.2): two
//! producers using the same key on the same table are two keys, so one
//! can neither claim nor evict the other's. Records and checkpoints
//! written before keys had a credential are read as [`LEGACY`]'s.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tidepool_model::ir::TableId;

use crate::codec::{In, Out, corrupt};
use crate::store::Applied;

/// How long a key is remembered.
pub const TTL: Duration = Duration::from_hours(24);
/// Keys remembered at most per credential; its oldest go first.
pub const CAPACITY: usize = 100_000;
/// The credential of keys logged before keys had one: the single ingest
/// token there was then.
pub const LEGACY: &str = "ingest";

/// An idempotency key as sent: by which credential, and the key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Scoped {
    pub credential: String,
    pub key: String,
}

/// A keyed write and what it was acknowledged with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keyed {
    /// The credential it was sent with.
    pub credential: String,
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

/// The key, credential, time and response; the table is written by the
/// caller.
pub(crate) fn put(out: &mut Out, keyed: &Keyed) {
    out.bytes(keyed.key.as_bytes());
    out.bytes(keyed.credential.as_bytes());
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

/// What [`put`] wrote; `scoped` is false for the format without the
/// credential, whose keys are [`LEGACY`]'s.
pub(crate) fn get(input: &mut In, table: TableId, scoped: bool) -> io::Result<Keyed> {
    let mut text = || String::from_utf8(input.bytes()?.to_vec()).map_err(|_| corrupt("key"));
    let key = text()?;
    let credential = if scoped { text()? } else { LEGACY.to_string() };
    let at = input.u64()?;
    let mut count = || usize::try_from(input.u64()?).map_err(|_| corrupt("key"));
    let (written, replaced, missing) = (count()?, count()?, count()?);
    Ok(Keyed {
        credential,
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

/// The keys written in the last [`TTL`], per credential, each numbered in
/// the order it came so the list is oldest first across credentials.
#[derive(Debug, Default)]
pub(crate) struct Recent {
    by_credential: HashMap<String, VecDeque<(u64, Keyed)>>,
    next: u64,
}

impl Recent {
    pub fn push(&mut self, keyed: Keyed, now: u64) {
        let serial = self.next;
        self.next += 1;
        let keys = self
            .by_credential
            .entry(keyed.credential.clone())
            .or_default();
        keys.push_back((serial, keyed));
        trim(keys, now);
    }

    /// Drop what has expired as of `now`, and each credential's oldest
    /// past the capacity.
    pub fn trim(&mut self, now: u64) {
        self.by_credential.retain(|_, keys| {
            trim(keys, now);
            !keys.is_empty()
        });
    }

    pub fn list(&self) -> Vec<Keyed> {
        let mut all: Vec<&(u64, Keyed)> = self.by_credential.values().flatten().collect();
        all.sort_unstable_by_key(|(serial, _)| *serial);
        all.into_iter().map(|(_, keyed)| keyed.clone()).collect()
    }
}

fn trim(keys: &mut VecDeque<(u64, Keyed)>, now: u64) {
    while keys
        .front()
        .is_some_and(|(_, k)| k.age(now) >= TTL || keys.len() > CAPACITY)
    {
        keys.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyed(key: &str, at: u64) -> Keyed {
        Keyed {
            credential: "kayak".into(),
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
        let back = get(&mut In::new(&out.0), TableId(1), true).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back, k);
    }

    #[test]
    fn a_key_without_a_credential_is_the_legacy_one() {
        // the format before keys had a credential: key, time, counts, epoch
        let mut out = Out::default();
        out.bytes(b"k");
        for n in [1_234, 1, 0, 0, 7] {
            out.u64(n);
        }
        let back = get(&mut In::new(&out.0), TableId(1), false).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back.credential, LEGACY);
        assert_eq!((back.key.as_str(), back.at, back.epoch), ("k", 1_234, 7));
    }

    #[test]
    fn each_credential_keeps_its_own_keys() {
        let mut recent = Recent::default();
        let mut other = keyed("b", 2);
        other.credential = "sensors".into();
        recent.push(keyed("a", 1), 2);
        recent.push(other.clone(), 2);
        // a flood from one credential past the capacity
        for i in 0..=CAPACITY {
            let mut flood = keyed(&format!("f{i}"), 3);
            flood.credential = "sensors".into();
            recent.push(flood, 3);
        }
        let list = recent.list();
        // the other credential's key is still there, and first: oldest first
        assert_eq!(list.first(), Some(&keyed("a", 1)));
        assert!(!list.contains(&other), "the flood's own oldest key goes");
        assert_eq!(list.len(), 1 + CAPACITY);
    }
}
