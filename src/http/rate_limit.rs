//! Per-token request rate limit (spec §7): an in-memory token bucket per bearer token. The
//! MVP runs one replica, so process memory is the whole picture.

use std::collections::HashMap;
use std::sync::Mutex;

use time::OffsetDateTime;

use crate::domain::TokenId;

struct Bucket {
    tokens: f64,
    at: OffsetDateTime,
}

pub struct RateLimiter {
    per_sec: f64,
    burst: f64,
    buckets: Mutex<HashMap<TokenId, Bucket>>,
}

impl RateLimiter {
    /// `per_sec` requests per second sustained, up to `burst` at once (both at least 1).
    pub fn new(per_sec: u32, burst: u32) -> Self {
        Self {
            per_sec: f64::from(per_sec.max(1)),
            burst: f64::from(burst.max(1)),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one request from `token`'s bucket at `now`. `Err(secs)`: over the limit, retry
    /// after `secs` whole seconds (at least 1).
    pub fn check(&self, token: TokenId, now: OffsetDateTime) -> Result<(), u64> {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = buckets.entry(token).or_insert(Bucket {
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
        let limiter = RateLimiter::new(2, 3);
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
}
