//! The clock the session and the writer read time from.
//!
//! Two hands, deliberately (D16): `monotonic` orders deadlines and intervals,
//! `wall` stamps `updated_at`. A wall clock that steps backwards must not stall
//! the 5 s capture or the 2 s coalesce, and a monotonic instant cannot be
//! written into an RFC 3339 field.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

#[derive(Clone, Copy, Debug)]
pub struct ClockSample {
    pub monotonic: Instant,
    pub wall: OffsetDateTime,
}

pub trait Clock: Send + Sync {
    fn sample(&self) -> ClockSample;
}

/// The real clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn sample(&self) -> ClockSample {
        ClockSample {
            monotonic: Instant::now(),
            wall: OffsetDateTime::now_utc(),
        }
    }
}

/// A clock the tests drive. Both hands are settable on their own, which is what
/// makes "a wall-clock jump does not disturb the 5 s interval" a test rather
/// than a claim.
#[derive(Debug)]
pub struct FakeClock {
    inner: Mutex<ClockSample>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ClockSample {
                monotonic: Instant::now(),
                wall: OffsetDateTime::UNIX_EPOCH,
            }),
        }
    }

    pub fn advance(&self, span: Duration) {
        self.advance_monotonic(span);
        let mut inner = self.lock();
        inner.wall += span;
    }

    pub fn advance_monotonic(&self, span: Duration) {
        let mut inner = self.lock();
        inner.monotonic += span;
    }

    pub fn set_wall(&self, wall: OffsetDateTime) {
        self.lock().wall = wall;
    }

    /// A poisoned fake clock means a test thread already panicked; the sample
    /// inside is still the truth, so recover rather than compound the panic.
    fn lock(&self) -> std::sync::MutexGuard<'_, ClockSample> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn sample(&self) -> ClockSample {
        *self.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_two_hands_move_independently() {
        let clock = FakeClock::new();
        let start = clock.sample();

        clock.advance_monotonic(Duration::from_secs(5));
        let stepped = clock.sample();
        assert_eq!(
            stepped.monotonic.duration_since(start.monotonic),
            Duration::from_secs(5)
        );
        assert_eq!(
            stepped.wall, start.wall,
            "monotonic advance must not move the wall clock"
        );

        // A wall clock that jumps backwards must leave deadlines alone.
        clock.set_wall(start.wall - Duration::from_secs(3600));
        let jumped = clock.sample();
        assert!(jumped.wall < start.wall);
        assert_eq!(jumped.monotonic, stepped.monotonic);
    }

    #[test]
    fn advance_moves_both_hands_together() {
        let clock = FakeClock::new();
        let start = clock.sample();
        clock.advance(Duration::from_secs(2));
        let after = clock.sample();
        assert_eq!(
            after.monotonic.duration_since(start.monotonic),
            Duration::from_secs(2)
        );
        assert_eq!(after.wall - start.wall, time::Duration::seconds(2));
    }

    #[test]
    fn the_system_clock_reports_a_plausible_wall_time() {
        let sample = SystemClock.sample();
        assert!(sample.wall.year() >= 2024);
    }
}
