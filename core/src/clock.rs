use chrono::{DateTime, Utc, Duration};
use std::sync::atomic::{AtomicI64, Ordering};

/// Controllable clock: lets tests advance time without waiting (Prompt 1).
/// When TEST_CLOCK=1 the offset can be advanced via the test endpoint.
pub struct Clock {
    offset_secs: AtomicI64,
    pub test_mode: bool,
}

impl Clock {
    pub fn new(test_mode: bool) -> Self {
        Self { offset_secs: AtomicI64::new(0), test_mode }
    }
    pub fn now(&self) -> DateTime<Utc> {
        Utc::now() + Duration::seconds(self.offset_secs.load(Ordering::Relaxed))
    }
    pub fn advance(&self, secs: i64) {
        self.offset_secs.fetch_add(secs, Ordering::Relaxed);
    }
}
