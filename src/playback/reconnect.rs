//! M7 §7: reconnect timing, as pure data. No I/O, no clock of its own — the
//! worker passes `Instant`s and listening time in, so every rule here is an
//! assertion rather than a sleep.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReconnectPolicy {
    /// Delay before attempt n; the last entry repeats.
    pub backoff: [Duration; 5],
    /// Wall time from the outage's first failure, evaluated only when
    /// something fails. It never cuts an in-flight open short and never stops
    /// playback that is succeeding.
    pub budget: Duration,
    /// Listening time that must advance after a reconnect before the outage
    /// is over. Played audio only: bytes and decoded frames do not count.
    pub stable_after: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            backoff: [1, 2, 4, 8, 15].map(Duration::from_secs),
            budget: Duration::from_secs(5 * 60),
            stable_after: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Next {
    AttemptAt(Instant),
    GiveUp,
}

#[derive(Clone, Debug)]
pub struct Outage {
    started: Instant,
    failures: usize,
    next_attempt_at: Instant,
    /// Listening time when the latest reconnect started playing.
    playing_from: Option<Duration>,
}

impl Outage {
    pub fn begin(now: Instant) -> Self {
        Self {
            started: now,
            failures: 0,
            next_attempt_at: now,
            playing_from: None,
        }
    }

    /// A playing connection or an attempt failed.
    pub fn failed(&mut self, now: Instant, policy: &ReconnectPolicy) -> Next {
        self.playing_from = None;
        if now.duration_since(self.started) >= policy.budget {
            return Next::GiveUp;
        }
        let step = self.failures.min(policy.backoff.len() - 1);
        self.failures += 1;
        self.next_attempt_at = now + policy.backoff[step];
        Next::AttemptAt(self.next_attempt_at)
    }

    pub fn due(&self, now: Instant) -> bool {
        self.playing_from.is_none() && now >= self.next_attempt_at
    }

    pub fn playing_from(&mut self, position: Duration) {
        self.playing_from = Some(position);
    }

    pub fn is_over(&self, position: Duration, policy: &ReconnectPolicy) -> bool {
        self.playing_from
            .is_some_and(|from| position.saturating_sub(from) >= policy.stable_after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ReconnectPolicy {
        ReconnectPolicy::default()
    }

    #[test]
    fn backoff_steps_then_repeats_its_last_entry() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        let delays: Vec<u64> = (0..7)
            .map(|_| match outage.failed(t0, &policy()) {
                Next::AttemptAt(at) => at.duration_since(t0).as_secs(),
                Next::GiveUp => panic!("inside the budget"),
            })
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 15, 15, 15]);
    }

    #[test]
    fn the_budget_is_judged_only_when_something_fails() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        assert!(matches!(outage.failed(t0, &policy()), Next::AttemptAt(_)));
        let late = t0 + Duration::from_secs(301);
        assert!(
            !outage.is_over(Duration::ZERO, &policy()),
            "time alone ends nothing"
        );
        assert_eq!(outage.failed(late, &policy()), Next::GiveUp);
    }

    #[test]
    fn an_attempt_is_due_at_its_scheduled_instant_and_not_a_moment_before() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        outage.failed(t0, &policy());
        assert!(!outage.due(t0 + Duration::from_millis(999)));
        assert!(outage.due(t0 + Duration::from_secs(1)));
    }

    #[test]
    fn short_connections_stay_one_outage_and_thirty_played_seconds_end_it() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        outage.failed(t0, &policy());
        outage.playing_from(Duration::from_secs(100));
        assert!(
            !outage.due(t0 + Duration::from_secs(60)),
            "no attempt while playing"
        );
        assert!(!outage.is_over(Duration::from_secs(102), &policy()));
        // It closed after two seconds: same outage, next backoff step.
        assert_eq!(
            outage.failed(t0 + Duration::from_secs(3), &policy()),
            Next::AttemptAt(t0 + Duration::from_secs(5))
        );
        outage.playing_from(Duration::from_secs(102));
        assert!(outage.is_over(Duration::from_secs(132), &policy()));
    }
}
