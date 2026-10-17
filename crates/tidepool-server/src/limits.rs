//! How much a credential may send (plan §9.2): requests per unit of time
//! and values new to the dictionaries, each a token bucket per credential,
//! and the share of the writer queue public credentials may hold together.
//!
//! A bucket holds up to one period's worth (`200/s` lets a burst of 200
//! through) and refills continuously. The limits are per process and
//! approximate under concurrency: requests of one credential arriving
//! together may each see the same budget, so a burst can overshoot by what
//! is in flight. That is enough to keep a public producer from filling
//! memory or the queue, which is what they are for.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use tidepool_config::types::Rate;
use tidepool_store::writer::QUEUE;

/// Batches public credentials may have in the writer queue together: a
/// quarter of it, so a flood from the internet can't keep private producers
/// out.
pub const PUBLIC_QUEUE: usize = QUEUE / 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    Requests,
    NewValues,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    /// Refilled to `now` at `rate`, holding at most one period's worth.
    fn refill(&mut self, rate: Rate, now: Instant) {
        #[allow(clippy::cast_precision_loss)]
        let capacity = rate.count as f64;
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate.per_second()).min(capacity);
        self.at = now;
    }
}

/// How long until `missing` more tokens have come in at `rate`.
fn wait(missing: f64, rate: Rate) -> Duration {
    Duration::from_secs_f64((missing / rate.per_second()).max(0.0))
}

#[derive(Debug, Default)]
pub struct Limiter {
    buckets: Mutex<HashMap<(String, Kind), Bucket>>,
    public_in_flight: AtomicUsize,
}

impl Limiter {
    fn with<T>(
        &self,
        credential: &str,
        kind: Kind,
        rate: Rate,
        now: Instant,
        f: impl FnOnce(&mut Bucket) -> T,
    ) -> T {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        #[allow(clippy::cast_precision_loss)]
        let bucket = buckets
            .entry((credential.to_string(), kind))
            .or_insert_with(|| Bucket {
                tokens: rate.count as f64,
                at: now,
            });
        bucket.refill(rate, now);
        f(bucket)
    }

    /// Take one request from `credential`'s budget.
    ///
    /// # Errors
    /// Over the rate: how long until the next request may come.
    pub fn request(&self, credential: &str, rate: Rate, now: Instant) -> Result<(), Duration> {
        self.with(credential, Kind::Requests, rate, now, |bucket| {
            if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
                Ok(())
            } else {
                Err(wait(1.0 - bucket.tokens, rate))
            }
        })
    }

    /// New values `credential` may add now.
    pub fn new_values_left(&self, credential: &str, rate: Rate, now: Instant) -> u64 {
        self.with(credential, Kind::NewValues, rate, now, |bucket| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let left = bucket.tokens.max(0.0).floor() as u64;
            left
        })
    }

    /// `n` new values were added: take them from the budget.
    pub fn spend_new_values(&self, credential: &str, rate: Rate, n: u64, now: Instant) {
        self.with(credential, Kind::NewValues, rate, now, |bucket| {
            #[allow(clippy::cast_precision_loss)]
            let n = n as f64;
            bucket.tokens -= n;
        });
    }

    /// How long until `n` new values fit in `credential`'s budget.
    pub fn new_values_wait(&self, credential: &str, rate: Rate, n: u64, now: Instant) -> Duration {
        self.with(credential, Kind::NewValues, rate, now, |bucket| {
            #[allow(clippy::cast_precision_loss)]
            let missing = n as f64 - bucket.tokens;
            wait(missing, rate)
        })
    }

    /// A place in the public share of the writer queue, held until the
    /// guard is dropped; `None` when the share is full.
    pub fn public_slot(&self) -> Option<PublicSlot<'_>> {
        let before = self.public_in_flight.fetch_add(1, Ordering::AcqRel);
        if before >= PUBLIC_QUEUE {
            self.public_in_flight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(PublicSlot(&self.public_in_flight))
    }
}

/// A public batch on its way through the writer.
pub struct PublicSlot<'a>(&'a AtomicUsize);

impl Drop for PublicSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(text: &str) -> Rate {
        text.parse().unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn requests_burst_to_one_period_then_refill() {
        let limiter = Limiter::default();
        let now = Instant::now();
        let per_second = rate("2/s");
        assert_eq!(limiter.request("a", per_second, now), Ok(()));
        assert_eq!(limiter.request("a", per_second, now), Ok(()));
        let Err(wait) = limiter.request("a", per_second, now) else {
            panic!("a third request in the same instant is over 2/s");
        };
        assert_eq!(wait, Duration::from_millis(500));
        // another credential has its own budget
        assert_eq!(limiter.request("b", per_second, now), Ok(()));
        // half a second later one more fits
        let later = now + Duration::from_millis(500);
        assert_eq!(limiter.request("a", per_second, later), Ok(()));
    }

    #[test]
    fn new_values_are_spent_after_they_are_counted() {
        let limiter = Limiter::default();
        let now = Instant::now();
        let per_minute = rate("60/min");
        assert_eq!(limiter.new_values_left("a", per_minute, now), 60);
        limiter.spend_new_values("a", per_minute, 50, now);
        assert_eq!(limiter.new_values_left("a", per_minute, now), 10);
        assert_eq!(
            limiter.new_values_wait("a", per_minute, 15, now),
            Duration::from_secs(5)
        );
        let later = now + Duration::from_secs(5);
        assert_eq!(limiter.new_values_left("a", per_minute, later), 15);
    }

    #[test]
    fn the_public_share_of_the_queue_is_a_quarter() {
        let limiter = Limiter::default();
        let slots: Vec<PublicSlot> = (0..PUBLIC_QUEUE)
            .filter_map(|_| limiter.public_slot())
            .collect();
        assert_eq!(slots.len(), PUBLIC_QUEUE);
        assert!(limiter.public_slot().is_none());
        drop(slots);
        assert!(limiter.public_slot().is_some());
    }
}
