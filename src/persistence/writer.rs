//! The disk's owner: a keep-latest slot, a coalescing thread, and a bounded
//! shutdown.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};

use crate::clock::Clock;

use super::PersistenceError;
use super::model::PersistedState;
use super::store::StateStore;

/// A maximum age, not a minimum spacing (D5).
pub const COALESCE_WINDOW: Duration = Duration::from_secs(2);
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
const IDLE_WAIT: Duration = Duration::from_millis(100);
const FAILURE_WARNING_THRESHOLD: u32 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Urgency {
    Ordinary,
    Forced,
}

pub trait StateSink: Send {
    fn write(&self, state: &PersistedState) -> Result<(), PersistenceError>;
}

impl StateSink for StateStore {
    fn write(&self, state: &PersistedState) -> Result<(), PersistenceError> {
        StateStore::write(self, state)
    }
}

struct Pending {
    state: PersistedState,
    deadline: Instant,
    submit_seq: u64,
}

/// Replace-on-submit, and the newest snapshot wins whoever asks: keep-latest is
/// a property of the slot rather than of its callers. `submit_seq` is what makes
/// that checkable rather than assumed, and is never persisted (§9).
#[derive(Default)]
struct Slot {
    pending: Option<Pending>,
    last_written: u64,
}

impl Slot {
    fn submit(&mut self, state: PersistedState, urgency: Urgency, now: Instant, submit_seq: u64) {
        // Sequence numbers are handed out before the lock is taken, so two
        // producers can arrive in the wrong order. A submission that lost that
        // race is dropped rather than allowed to regress what is pending: the
        // one already in the slot is the newer snapshot and supersedes it, the
        // same way a newer one supersedes a write that failed (§9).
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.submit_seq > submit_seq)
        {
            return;
        }
        let deadline = match (&self.pending, urgency) {
            (_, Urgency::Forced) => now,
            // Anchored when the first snapshot entered an empty slot;
            // replacement never extends it.
            (Some(pending), Urgency::Ordinary) => pending.deadline,
            (None, Urgency::Ordinary) => now + COALESCE_WINDOW,
        };
        self.pending = Some(Pending {
            state,
            deadline,
            submit_seq,
        });
    }

    fn take_due(&mut self, now: Instant) -> Option<Pending> {
        if self.pending.as_ref().is_some_and(|p| p.deadline <= now) {
            self.pending.take()
        } else {
            None
        }
    }

    fn take_pending(&mut self) -> Option<Pending> {
        self.pending.take()
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|pending| pending.deadline)
    }

    fn is_stale(&self, submit_seq: u64) -> bool {
        submit_seq <= self.last_written
    }

    fn mark_written(&mut self, submit_seq: u64) {
        self.last_written = self.last_written.max(submit_seq);
    }

    /// Only into an empty slot: a newer snapshot that arrived while the write
    /// was in flight supersedes the one that failed.
    fn reinsert_failed(&mut self, pending: Pending, now: Instant) {
        if self.pending.is_none() {
            self.pending = Some(Pending {
                deadline: now + COALESCE_WINDOW,
                ..pending
            });
        }
    }
}

fn should_warn(consecutive_failures: u32) -> bool {
    consecutive_failures == FAILURE_WARNING_THRESHOLD
}

#[derive(Debug)]
pub enum ShutdownOutcome {
    Written,
    Failed(PersistenceError),
    /// The writer did not answer inside the bound; it has been detached.
    Unconfirmed,
}

struct Shared {
    slot: Mutex<Slot>,
    wake: Condvar,
    closing: AtomicBool,
}

pub struct WriterHandle {
    shared: Arc<Shared>,
    ack: Receiver<Result<(), PersistenceError>>,
    thread: Option<JoinHandle<()>>,
    clock: Arc<dyn Clock>,
    next_seq: AtomicU64,
}

/// A poisoned slot means the writer thread panicked mid-write. The snapshot
/// inside is still a valid snapshot, so recover rather than take the app down
/// with it — D11 says persistence is never fatal.
fn lock(slot: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl WriterHandle {
    pub fn spawn(sink: Box<dyn StateSink>, clock: Arc<dyn Clock>) -> Self {
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::default()),
            wake: Condvar::new(),
            closing: AtomicBool::new(false),
        });
        let (ack_tx, ack_rx) = bounded(1);
        let thread = {
            let shared = Arc::clone(&shared);
            let clock = Arc::clone(&clock);
            match std::thread::Builder::new()
                .name("continuo-state".into())
                .spawn(move || run(shared, sink, clock, ack_tx))
            {
                Ok(thread) => Some(thread),
                // Never fatal and never self-disabling (D11): submissions keep
                // being accepted, they simply never reach the disk.
                Err(error) => {
                    tracing::error!(
                        %error,
                        "cannot start the state writer; playback state will not be saved"
                    );
                    None
                }
            }
        };
        Self {
            shared,
            ack: ack_rx,
            thread,
            clock,
            next_seq: AtomicU64::new(1),
        }
    }

    /// Never blocks and never fails: the slot is keep-latest, so the worst a
    /// submission can do is replace one that had not been written yet.
    pub fn submit(&self, state: PersistedState, urgency: Urgency) {
        if self.shared.closing.load(Ordering::Acquire) {
            return;
        }
        let submit_seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let now = self.clock.sample().monotonic;
        lock(&self.shared.slot).submit(state, urgency, now, submit_seq);
        self.shared.wake.notify_all();
    }

    /// Bounded by design (D10): an unconditional join after the timeout would
    /// defeat the bound it exists to enforce.
    pub fn shutdown(&mut self) -> ShutdownOutcome {
        self.shared.closing.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        // With no writer there is nobody to acknowledge, so spending the bound
        // waiting for an answer that cannot arrive is pure delay at quit time.
        let Some(thread) = self.thread.take() else {
            return ShutdownOutcome::Unconfirmed;
        };
        match self.ack.recv_timeout(ACK_TIMEOUT) {
            Ok(Ok(())) => {
                let _ = thread.join();
                ShutdownOutcome::Written
            }
            Ok(Err(error)) => {
                let _ = thread.join();
                ShutdownOutcome::Failed(error)
            }
            // Detach: dropping the handle abandons the thread without waiting.
            Err(_) => {
                drop(thread);
                ShutdownOutcome::Unconfirmed
            }
        }
    }
}

impl Drop for WriterHandle {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        self.shared.wake.notify_all();
    }
}

fn run(
    shared: Arc<Shared>,
    sink: Box<dyn StateSink>,
    clock: Arc<dyn Clock>,
    ack: Sender<Result<(), PersistenceError>>,
) {
    let mut consecutive_failures: u32 = 0;
    loop {
        let closing = shared.closing.load(Ordering::Acquire);
        let now = clock.sample().monotonic;
        let taken = {
            let mut slot = lock(&shared.slot);
            // Closing takes whatever is there: the final write does not wait
            // out a coalescing window nobody is left to fill.
            if closing {
                slot.take_pending()
            } else {
                slot.take_due(now)
            }
        };

        let Some(pending) = taken else {
            if closing {
                let _ = ack.send(Ok(()));
                return;
            }
            // Sampled before the slot is locked: the slot lock is never held
            // while calling out to the clock or to the sink.
            let idle_at = clock.sample().monotonic;
            let slot = lock(&shared.slot);
            let wait = slot
                .next_deadline()
                .map(|deadline| deadline.saturating_duration_since(idle_at))
                .unwrap_or(IDLE_WAIT)
                .clamp(Duration::from_millis(1), IDLE_WAIT);
            let _ = shared.wake.wait_timeout(slot, wait);
            continue;
        };

        if lock(&shared.slot).is_stale(pending.submit_seq) {
            continue;
        }

        // The snapshot is held outside the slot while the I/O is in flight, so
        // the producer can fill the emptied slot meanwhile (§9).
        match sink.write(&pending.state) {
            Ok(()) => {
                consecutive_failures = 0;
                lock(&shared.slot).mark_written(pending.submit_seq);
                if closing {
                    let _ = ack.send(Ok(()));
                    return;
                }
            }
            Err(error) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                if should_warn(consecutive_failures) {
                    tracing::warn!(
                        %error,
                        failures = consecutive_failures,
                        "the playback state file has failed to write three times running"
                    );
                }
                if closing {
                    let _ = ack.send(Err(error));
                    return;
                }
                tracing::debug!(%error, "state write failed; retrying at the coalescing cadence");
                let retry_from = clock.sample().monotonic;
                lock(&shared.slot).reinsert_failed(pending, retry_from);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    #[test]
    fn an_ordinary_replacement_does_not_extend_the_deadline() {
        let base = Instant::now();
        let mut slot = Slot::default();
        slot.submit(PersistedState::default(), Urgency::Ordinary, base, 1);
        slot.submit(
            PersistedState::default(),
            Urgency::Ordinary,
            at(base, 1_500),
            2,
        );

        assert_eq!(slot.next_deadline(), Some(base + COALESCE_WINDOW));
        assert!(slot.take_due(at(base, 1_999)).is_none());
        assert!(slot.take_due(at(base, 2_000)).is_some());
    }

    #[test]
    fn a_forced_submit_is_due_immediately_even_over_a_waiting_one() {
        let base = Instant::now();
        let mut slot = Slot::default();
        slot.submit(PersistedState::default(), Urgency::Ordinary, base, 1);
        slot.submit(PersistedState::default(), Urgency::Forced, at(base, 10), 2);

        let due = slot.take_due(at(base, 10)).expect("forced is due at once");
        assert_eq!(due.submit_seq, 2);
    }

    #[test]
    fn a_submission_that_lost_the_race_never_displaces_a_newer_one() {
        let base = Instant::now();
        let mut slot = Slot::default();
        // Two producers: the sequence number is taken before the lock, so the
        // lower one can reach the slot second.
        slot.submit(PersistedState::default(), Urgency::Forced, base, 6);
        slot.submit(PersistedState::default(), Urgency::Forced, at(base, 1), 5);

        assert_eq!(
            slot.take_pending().map(|pending| pending.submit_seq),
            Some(6),
            "keep-latest holds however the producers interleave"
        );
    }

    #[test]
    fn an_older_submission_is_recognized_as_stale() {
        let mut slot = Slot::default();
        slot.mark_written(7);
        assert!(slot.is_stale(7));
        assert!(slot.is_stale(6));
        assert!(!slot.is_stale(8));
    }

    #[test]
    fn a_failed_snapshot_never_displaces_a_newer_one() {
        let base = Instant::now();
        let mut slot = Slot::default();
        slot.submit(PersistedState::default(), Urgency::Forced, base, 1);
        let failed = slot.take_pending().expect("in flight");

        // A newer snapshot arrives while the write is failing.
        slot.submit(PersistedState::default(), Urgency::Forced, at(base, 5), 2);
        slot.reinsert_failed(failed, at(base, 10));

        assert_eq!(
            slot.take_pending().map(|pending| pending.submit_seq),
            Some(2),
            "keep-latest is never violated by a retry"
        );
    }

    #[test]
    fn a_failed_snapshot_returns_to_an_empty_slot_with_a_fresh_deadline() {
        let base = Instant::now();
        let mut slot = Slot::default();
        slot.submit(PersistedState::default(), Urgency::Forced, base, 1);
        let failed = slot.take_pending().expect("in flight");
        slot.reinsert_failed(failed, at(base, 10));

        assert_eq!(slot.next_deadline(), Some(at(base, 10) + COALESCE_WINDOW));
    }

    #[test]
    fn the_warning_fires_once_at_three_consecutive_failures() {
        assert!(!should_warn(1));
        assert!(!should_warn(2));
        assert!(should_warn(3));
        assert!(!should_warn(4), "one warning, not one per failure");
    }
}
