//! `Idempotency-Key`: a retried write with the same key gets the first
//! response again and writes nothing.
//!
//! Keys are per table and remembered for a day, in memory (they move into the
//! write-ahead log with durability, M5, so they survive a restart). A key
//! whose first request is still running answers `409` rather than waiting,
//! which keeps this a map and not a queue.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tidepool_model::ir::TableId;
use tidepool_protocol::IngestResponse;

/// How long a key is remembered.
pub const TTL: Duration = Duration::from_hours(24);
/// Keys remembered at most; the oldest finished ones go first.
pub const CAPACITY: usize = 100_000;

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
}
