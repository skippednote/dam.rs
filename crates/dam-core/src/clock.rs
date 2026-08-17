//! A clock you can advance.
//!
//! Restore is a *timing* problem, not a state problem. The cases that matter are "the
//! temporary copy expires while a download is in flight" and "minimum duration blocks
//! a re-tier" — and neither is testable against a real clock, because the waits are
//! measured in hours. Nor is either testable against a real S3: no emulator gives you
//! genuine Glacier timing, which is why `FakeS3Store` exists at all (§20.2).
//!
//! So time is a dependency. Production passes [`SystemClock`]; tests pass
//! [`TestClock`] and move it.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::{
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

/// The current time, as far as the caller is concerned.
pub trait Clock: Send + Sync + fmt::Debug {
    fn now(&self) -> DateTime<Utc>;
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that only moves when told to.
///
/// Cloneable and shared, so a store holding one and a test holding one see the same
/// time. Never advances on its own — a test that forgets to advance it sees a stopped
/// clock, which fails loudly rather than passing intermittently.
#[derive(Clone)]
pub struct TestClock(Arc<Mutex<DateTime<Utc>>>);

impl TestClock {
    /// Starts at a fixed, arbitrary instant rather than `Utc::now()`, so a failure
    /// reproduces with the same timestamps tomorrow.
    pub fn new() -> Self {
        Self::at(
            DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        )
    }

    pub fn at(instant: DateTime<Utc>) -> Self {
        Self(Arc::new(Mutex::new(instant)))
    }

    /// Moves time forward.
    pub fn advance(&self, by: Duration) {
        if let Ok(mut t) = self.0.lock() {
            *t += ChronoDuration::from_std(by).unwrap_or_else(|_| ChronoDuration::zero());
        }
    }

    /// Moves time forward by whole hours. Restore waits are measured in hours, and
    /// `advance(Duration::from_secs(13 * 3600))` reads worse than `advance_hours(13)`.
    pub fn advance_hours(&self, hours: i64) {
        self.advance(Duration::from_secs((hours.max(0) as u64) * 3600));
    }

    pub fn set(&self, instant: DateTime<Utc>) {
        if let Ok(mut t) = self.0.lock() {
            *t = instant;
        }
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for TestClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TestClock").field(&self.now()).finish()
    }
}

impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        // A poisoned lock means another thread panicked mid-write. Returning the
        // recovered value beats panicking again and burying the original failure.
        match self.0.lock() {
            Ok(t) => *t,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_clock_does_not_move_on_its_own() {
        let c = TestClock::new();
        let a = c.now();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(c.now(), a, "a stopped clock must stay stopped");
    }

    #[test]
    fn advancing_moves_it_and_clones_share_the_movement() {
        let c = TestClock::new();
        let shared = c.clone();
        let before = c.now();
        c.advance_hours(13);
        assert_eq!(shared.now(), before + ChronoDuration::hours(13));
    }

    #[test]
    fn it_starts_at_a_fixed_instant_so_failures_reproduce() {
        assert_eq!(TestClock::new().now(), TestClock::new().now());
    }
}
