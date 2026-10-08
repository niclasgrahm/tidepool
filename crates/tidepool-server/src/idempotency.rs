//! `Idempotency-Key`: a retried write with the same key gets the first
//! response again and writes nothing.
//!
//! Keys are per table and remembered for a day. Retries are answered from
//! this map; with a data directory the writer also logs each key with the
//! batch it came with and the response it got, and keeps live keys in
//! checkpoints (`tidepool_store::keys`), so the map is rebuilt on startup
//! ([`Keys::restore`]) and a retry of a write acknowledged just before a
//! restart still writes nothing. Without one, keys last as long as the
//! process (and so does the data). A key whose first request is still
//! running answers `409` rather than waiting, which keeps this a map and not
//! a queue.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use tidepool_model::ir::TableId;
use tidepool_protocol::IngestResponse;
use tidepool_store::keys::{CAPACITY, Keyed, TTL};

#[derive(Clone, Debug)]
enum Slot {
    InFlight,
    Done(Instant, IngestResponse),
}

/// What a request with a key should do.
#[derive(Debug, PartialEq, Eq)]
pub enum Begin {
    /// First time: go ahead, then [`Keys::finish`] or [`Keys::abandon`].
    Fresh,
    /// Seen and finished: answer this.
    Done(IngestResponse),
    /// Seen and still running.
    InFlight,
}

#[derive(Debug, Default)]
pub struct Keys(Mutex<HashMap<(TableId, String), Slot>>);

impl Keys {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(TableId, String), Slot>> {
        // a panic while holding this lock leaves the map consistent (every
        // operation is a single insert or remove), so poisoning is ignorable
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn begin(&self, table: TableId, key: &str, now: Instant) -> Begin {
        let mut keys = self.lock();
        match keys.get(&(table, key.to_string())) {
            Some(Slot::InFlight) => return Begin::InFlight,
            Some(Slot::Done(at, response)) if now.duration_since(*at) < TTL => {
                return Begin::Done(response.clone());
            }
            _ => {}
        }
        if keys.len() >= CAPACITY {
            keys.retain(|_, slot| {
                matches!(slot, Slot::Done(at, _) if now.duration_since(*at) < TTL)
                    || matches!(slot, Slot::InFlight)
            });
            while keys.len() >= CAPACITY {
                let oldest = keys
                    .iter()
                    .filter_map(|(k, slot)| match slot {
                        Slot::Done(at, _) => Some((*at, k.clone())),
                        Slot::InFlight => None,
                    })
                    .min_by_key(|(at, _)| *at)
                    .map(|(_, k)| k);
                match oldest {
                    Some(oldest) => {
                        keys.remove(&oldest);
                    }
                    None => break,
                }
            }
        }
        keys.insert((table, key.to_string()), Slot::InFlight);
        Begin::Fresh
    }

    pub fn finish(&self, table: TableId, key: &str, response: IngestResponse, now: Instant) {
        self.lock()
            .insert((table, key.to_string()), Slot::Done(now, response));
    }

    /// The request failed before writing: forget the key so a retry runs.
    pub fn abandon(&self, table: TableId, key: &str) {
        self.lock().remove(&(table, key.to_string()));
    }

    /// Remember keys the data directory held (oldest first), each as old
    /// as its record says, as of `now` (an `Instant`, and milliseconds
    /// since the Unix epoch).
    pub fn restore(&self, keyed: Vec<Keyed>, now: Instant, now_ms: u64) {
        let mut keys = self.lock();
        for k in keyed {
            let age = k.age(now_ms);
            let Some(at) = now.checked_sub(age).filter(|_| age < TTL) else {
                continue;
            };
            let response = response_of(&k);
            keys.insert((k.table, k.key), Slot::Done(at, response));
        }
    }
}

/// The response a logged write was acknowledged with.
pub fn response_of(keyed: &Keyed) -> IngestResponse {
    IngestResponse {
        written: keyed.applied.written as u64,
        replaced: keyed.applied.replaced as u64,
        missing: keyed.applied.missing as u64,
        epoch: keyed.epoch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(epoch: u64) -> IngestResponse {
        IngestResponse {
            written: 1,
            replaced: 0,
            missing: 0,
            epoch,
        }
    }

    #[test]
    fn a_finished_key_returns_the_first_response() {
        let keys = Keys::default();
        let now = Instant::now();
        assert_eq!(keys.begin(TableId(1), "k", now), Begin::Fresh);
        assert_eq!(keys.begin(TableId(1), "k", now), Begin::InFlight);
        keys.finish(TableId(1), "k", response(7), now);
        assert_eq!(keys.begin(TableId(1), "k", now), Begin::Done(response(7)));
        // keys are per table
        assert_eq!(keys.begin(TableId(2), "k", now), Begin::Fresh);
    }

    #[test]
    fn an_abandoned_key_can_run_again_and_old_keys_expire() {
        let keys = Keys::default();
        let now = Instant::now();
        assert_eq!(keys.begin(TableId(1), "k", now), Begin::Fresh);
        keys.abandon(TableId(1), "k");
        assert_eq!(keys.begin(TableId(1), "k", now), Begin::Fresh);
        keys.finish(TableId(1), "k", response(1), now);
        assert_eq!(keys.begin(TableId(1), "k", now + TTL), Begin::Fresh);
    }

    #[test]
    fn restored_keys_keep_their_age() {
        let keys = Keys::default();
        let now = Instant::now() + TTL; // room to go back a day
        let now_ms = 10 * 86_400_000;
        let logged = |key: &str, hours_ago: u64| Keyed {
            table: TableId(1),
            key: key.into(),
            at: now_ms - hours_ago * 3_600_000,
            applied: tidepool_store::Applied {
                written: 1,
                ..tidepool_store::Applied::default()
            },
            epoch: 7,
        };
        keys.restore(vec![logged("old", 25), logged("new", 23)], now, now_ms);
        assert_eq!(keys.begin(TableId(1), "old", now), Begin::Fresh);
        assert_eq!(keys.begin(TableId(1), "new", now), Begin::Done(response(7)));
        // and it expires a day after it was written, not after the restart
        let later = now + std::time::Duration::from_hours(1);
        assert_eq!(keys.begin(TableId(1), "new", later), Begin::Fresh);
    }
}
