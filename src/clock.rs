use std::sync::Mutex;

use time::{Duration, OffsetDateTime};

pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

pub struct TestClock(Mutex<OffsetDateTime>);

impl TestClock {
    pub fn new(t: OffsetDateTime) -> Self {
        Self(Mutex::new(t))
    }

    pub fn advance(&self, d: Duration) {
        *self.0.lock().unwrap() += d;
    }
}

impl Clock for TestClock {
    fn now(&self) -> OffsetDateTime {
        *self.0.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Duration, macros::datetime};

    #[test]
    fn test_clock_advances() {
        let c = TestClock::new(datetime!(2026-01-01 0:00 UTC));
        c.advance(Duration::minutes(5));
        assert_eq!(c.now(), datetime!(2026-01-01 0:05 UTC));
    }
}
