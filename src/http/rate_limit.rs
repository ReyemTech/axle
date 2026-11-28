//! Request rate limits (spec §7): an in-memory token bucket per key — per bearer token for
//! the API, per client IP for UI logins. The MVP runs one replica, so process memory is the
//! whole picture.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;

use time::OffsetDateTime;

use crate::domain::TokenId;

/// Above this many buckets, `check` first drops the ones that have fully refilled (they are
/// indistinguishable from a fresh bucket), so unauthenticated keys cannot grow the map forever.
const SWEEP_ABOVE: usize = 1024;

struct Bucket {
    tokens: f64,
    at: OffsetDateTime,
}

pub struct RateLimiter<K = TokenId> {
    per_sec: f64,
    burst: f64,
    buckets: Mutex<HashMap<K, Bucket>>,
}

impl<K: Eq + Hash> RateLimiter<K> {
    /// `per_sec` requests per second sustained, up to `burst` at once (both at least 1).
    pub fn new(per_sec: u32, burst: u32) -> Self {
        Self::with_rate(f64::from(per_sec.max(1)), burst)
    }

    /// `per_min` requests per minute sustained, up to `per_min` at once (at least 1).
    pub fn per_minute(per_min: u32) -> Self {
        let per_min = per_min.max(1);
        Self::with_rate(f64::from(per_min) / 60.0, per_min)
    }

    fn with_rate(per_sec: f64, burst: u32) -> Self {
        Self {
            per_sec,
            burst: f64::from(burst.max(1)),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one request from `key`'s bucket at `now`. `Err(secs)`: over the limit, retry
    /// after `secs` whole seconds (at least 1).
    pub fn check(&self, key: K, now: OffsetDateTime) -> Result<(), u64> {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if buckets.len() > SWEEP_ABOVE {
            let (per_sec, burst) = (self.per_sec, self.burst);
            buckets.retain(|_, b| {
                let elapsed = (now - b.at).as_seconds_f64().max(0.0);
                b.tokens + elapsed * per_sec < burst
            });
        }
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: self.burst,
            at: now,
        });
        // A clock that steps backwards refills nothing.
        let elapsed = (now - bucket.at).as_seconds_f64().max(0.0);
        bucket.tokens = (bucket.tokens + elapsed * self.per_sec).min(self.burst);
        bucket.at = bucket.at.max(now);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }
        let wait = (1.0 - bucket.tokens) / self.per_sec;
        Err((wait.ceil() as u64).max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;
    use time::macros::datetime;

    #[test]
    fn bucket_refills_at_rate_up_to_burst() {
        let t0 = datetime!(2026-11-27 9:00 UTC);
        let limiter: RateLimiter = RateLimiter::new(2, 3);
        let token = TokenId::new();
        for _ in 0..3 {
            assert_eq!(limiter.check(token, t0), Ok(()));
        }
        assert_eq!(limiter.check(token, t0), Err(1));
        assert_eq!(
            limiter.check(token, t0 + Duration::milliseconds(500)),
            Ok(())
        );
        assert!(
            limiter
                .check(token, t0 + Duration::milliseconds(500))
                .is_err()
        );
        // A long idle refills only to the burst.
        let later = t0 + Duration::hours(1);
        for _ in 0..3 {
            assert_eq!(limiter.check(token, later), Ok(()));
        }
        assert!(limiter.check(token, later).is_err());
        // Going back in time does not refill.
        assert!(limiter.check(token, t0).is_err());
    }

    #[test]
    fn refilled_buckets_are_evicted_once_the_map_is_large() {
        let t0 = datetime!(2026-11-27 9:00 UTC);
        let limiter = RateLimiter::per_minute(10);
        for key in 0..(SWEEP_ABOVE as u32 + 50) {
            assert_eq!(limiter.check(key, t0), Ok(()));
        }
        let size = |l: &RateLimiter<u32>| l.buckets.lock().unwrap().len();
        assert_eq!(size(&limiter), SWEEP_ABOVE + 50);
        // Each key spent one token; a minute later all are full again and get dropped.
        assert_eq!(limiter.check(u32::MAX, t0 + Duration::minutes(2)), Ok(()));
        assert_eq!(size(&limiter), 1);
        // A bucket that is still drained survives a sweep.
        let limiter = RateLimiter::per_minute(1);
        assert_eq!(limiter.check(0u32, t0), Ok(()));
        for key in 1..(SWEEP_ABOVE as u32 + 2) {
            let _ = limiter.check(key, t0);
        }
        assert!(limiter.check(0, t0 + Duration::seconds(1)).is_err());
    }

    #[test]
    fn per_minute_allows_a_minute_of_burst_then_one_every_interval() {
        let t0 = datetime!(2026-11-27 9:00 UTC);
        let limiter = RateLimiter::per_minute(10);
        for _ in 0..10 {
            assert_eq!(limiter.check("10.0.0.1", t0), Ok(()));
        }
        assert_eq!(limiter.check("10.0.0.1", t0), Err(6));
        // Keys are independent.
        assert_eq!(limiter.check("10.0.0.2", t0), Ok(()));
        assert_eq!(limiter.check("10.0.0.1", t0 + Duration::seconds(6)), Ok(()));
    }
}
