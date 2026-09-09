//! What a blocked source read services on the worker's behalf (G5, §8, §9).
//!
//! `TransportCore` (in `engine.rs`) is the only mutable state a blocked read
//! can reach: `Handshake` and `Timeline` live there, behind one lock, because
//! `pump_audio -> source.next_planar()` already holds `&mut self.source` when
//! the read blocks, and `unsafe_code = "forbid"` rules out a lifetime-erased
//! slot back onto the rest of `Worker`. `WaitService` is everything else the
//! hook needs, and nothing more: it holds no decoder and no source, so §8's
//! "must not re-enter decoder reads or seeks" is a property of the type
//! rather than a rule in prose.
//!
//! **Lock order, stated once and never varied: `SessionFacts` before
//! `TransportCore`.** `SourceInterrupt::state` is a leaf and is never held
//! while either is taken — `ByteChannel::read` already drops it across the
//! hook call for exactly this reason. Nothing in this module takes
//! `TransportCore` before `SessionFacts`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crossbeam_channel::{Sender, TrySendError};

use crate::http::channel::{SourceInterrupt, WaitHook};
use crate::media::id::MediaId;

use super::engine::{DEADLINE, EVENT_CAPACITY, PUMP_NAP, RESERVED_EVENT_SLOTS, TransportCore};
use super::event::{PlaybackEvent, Progress};
use super::output::Nanos;
use super::state::PlaybackState;
use super::timeline::PositionQuality;

/// A poisoned lock means a thread already panicked while holding it; there is
/// nothing better to do than carry on with the state it left. Same pattern as
/// `http::channel::lock` and `playback::prepare::lock`.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Distinguishes why `service_as` is running.
///
/// Nothing branches on it yet: a later task sets a `buffering` flag only for
/// `BlockedRead`, once the worker's own loop pass is *not* the thing keeping
/// progress alive. The two call sites already pass distinct values so that
/// addition needs no further plumbing (Ruling 4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Servicing {
    /// `WaitHook::service`, called from inside a blocked decoder read.
    BlockedRead,
    /// `Worker::publish_progress`, called once per pass through the main loop.
    WorkerLoop,
}

/// The scalars `service()` needs that change with the session rather than
/// with the transport.
///
/// Deliberately does not carry `quality`: that is derived fresh on every
/// publish from `degraded`, `playing` and (while playing) the transport's own
/// `Timeline::quality`, never stored.
pub struct SessionFacts {
    pub session_rev: u64,
    pub media: Option<MediaId>,
    pub position: Duration,
    pub degraded: bool,
    pub playing: bool,
    /// Set by the hook when it parks for a freeze, cleared when it releases.
    /// The worker reads it to know the transport is parked without having
    /// dispatched the `Pause` itself.
    pub frozen_by_hook: bool,
}

/// What a blocked source read services on the worker's behalf.
///
/// Three jobs, and no others: drain spans into the timeline, publish the
/// keep-latest progress snapshot, and act on the freeze level — park the
/// output when a pause arrives, release it when a play does, and announce
/// each.
pub struct WaitService {
    transport: Arc<Mutex<Option<TransportCore>>>,
    progress: Arc<Mutex<Progress>>,
    facts: Arc<Mutex<SessionFacts>>,
    interrupt: Arc<SourceInterrupt>,
    events: Sender<PlaybackEvent>,
    outbox: Arc<Mutex<VecDeque<PlaybackEvent>>>,
    backlog_empty: Arc<AtomicBool>,
    /// `AudioOutput::now()` needs `&self.output`, which the hook cannot
    /// reach — it runs from inside `next_planar()`, deep under
    /// `&mut self.source`, structurally unable to borrow the rest of
    /// `Worker`. The caller supplies a `Send + Sync` stand-in instead; the
    /// worker's own wiring refreshes it from the real device clock right
    /// before every call this module makes to `service_as`.
    clock: Arc<dyn Fn() -> Nanos + Send + Sync>,
}

impl WaitService {
    #[allow(clippy::too_many_arguments)] // one field per constructor argument, exactly `Worker::new`'s own precedent for a struct this shape; a config struct would only rename these nine fields, not reduce them.
    pub fn new(
        transport: Arc<Mutex<Option<TransportCore>>>,
        progress: Arc<Mutex<Progress>>,
        facts: Arc<Mutex<SessionFacts>>,
        interrupt: Arc<SourceInterrupt>,
        events: Sender<PlaybackEvent>,
        outbox: Arc<Mutex<VecDeque<PlaybackEvent>>>,
        backlog_empty: Arc<AtomicBool>,
        clock: Arc<dyn Fn() -> Nanos + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            transport,
            progress,
            facts,
            interrupt,
            events,
            outbox,
            backlog_empty,
            clock,
        })
    }

    /// Drained by the worker at the top of every loop pass, into
    /// `pending_events`, before anything else can emit (Ruling 5).
    pub fn take_outbox(&self) -> Vec<PlaybackEvent> {
        lock(&self.outbox).drain(..).collect()
    }

    /// Park the callback for a freeze. Returns whether the park is considered
    /// acknowledged: trivially `true` with no transport open, mirroring
    /// `Worker::pause`'s own `None => Ok(())` — there is nothing to park, so
    /// there is nothing to fail.
    ///
    /// A park that times out leaves `frozen_by_hook` false (the caller checks
    /// this return value before setting it), so the worker's own `pause()`
    /// handles the recovery once the blocked read finally returns.
    fn park(&self) -> bool {
        let mut guard = lock(&self.transport);
        match guard.as_mut() {
            Some(core) => {
                let mut pump = || std::thread::sleep(PUMP_NAP);
                core.park(&mut pump, DEADLINE)
            }
            None => true,
        }
    }

    /// Release a parked callback. A no-op with no transport open — there is
    /// nothing to release, and nothing downstream treats that as a failure.
    fn release(&self) {
        let mut guard = lock(&self.transport);
        if let Some(core) = guard.as_mut() {
            core.release();
        }
    }

    /// Announce one event, respecting the worker's own event ordering.
    ///
    /// `try_send` may not simply run first: the worker's `pending_events`
    /// backlog might be non-empty, and jumping it would deliver this event
    /// ahead of ones emitted before it. `backlog_empty` is the worker's own
    /// signal of that — set whenever `pending_events` is empty, cleared
    /// whenever it is not.
    fn announce(&self, event: PlaybackEvent) {
        // `EVENT_CAPACITY` and `RESERVED_EVENT_SLOTS` are `pub(crate)` in
        // `engine.rs` for this line (Ruling 7): the hook is a second emitter,
        // and the reserve exists so a terminal outcome always has room. It
        // never occupies it.
        let ordered = self.backlog_empty.load(Ordering::Acquire) && lock(&self.outbox).is_empty();
        let room = self.events.len() + RESERVED_EVENT_SLOTS < EVENT_CAPACITY;
        if !ordered || !room {
            lock(&self.outbox).push_back(event);
            return;
        }
        // `try_send` moves the event, and hands it back inside
        // `TrySendError::Full` — the only way to keep it after a failed send.
        // Writing this as `if try_send(event).is_ok() { return } ...
        // push_back(event)` does not compile: E0382, use of moved value
        // (Ruling 6).
        match self.events.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(event)) => lock(&self.outbox).push_back(event),
            // The application is gone. The worker learns this from its own
            // send and shuts down; dropping it here is not this function's
            // decision to report, and the outbox would never be drained.
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Update the keep-latest `Progress` snapshot from `SessionFacts`,
    /// recomputing the live position (and its quality) from the transport
    /// while playing — the same guard `Worker::publish_progress` used to
    /// apply inline, before this recompute moved here so the hook could
    /// share it.
    ///
    /// Paused counts as well as Playing: parking silences the callback, but
    /// the frames it already handed to the device still play out, so the
    /// position goes on rising for one output latency after the park and
    /// only then settles. Freezing the number at the instant of the park
    /// would report a position slightly behind what the listener actually
    /// heard.
    fn publish_progress(&self) {
        let snapshot = {
            let mut facts = lock(&self.facts);
            let mut quality = if facts.degraded {
                PositionQuality::Degraded
            } else if facts.playing {
                PositionQuality::Estimated
            } else {
                PositionQuality::Exact
            };
            if facts.playing {
                let now = (self.clock)();
                let mut transport = lock(&self.transport);
                if let Some(core) = transport.as_mut() {
                    facts.position = core.observed_position(now);
                    if !facts.degraded {
                        quality = core.quality();
                    }
                }
            }
            Progress {
                session_rev: facts.session_rev,
                media: facts.media.clone(),
                position: facts.position,
                quality,
            }
        };
        // Keep-latest: nothing but the assignment happens under the lock.
        match self.progress.lock() {
            Ok(mut slot) => *slot = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        }
    }

    /// One implementation, two callers (Ruling 4): `WaitHook::service`
    /// delegates here with `Servicing::BlockedRead`, and
    /// `Worker::publish_progress` calls this directly with
    /// `Servicing::WorkerLoop` — which is what makes "the hook does exactly
    /// what the loop does" a fact rather than a comment, including the
    /// freeze arm below, so a pause that arrives while the worker is *not*
    /// blocked is handled by the same code.
    pub(crate) fn service_as(&self, servicing: Servicing) {
        tracing::trace!(?servicing, "servicing the transport");
        let frozen = self.interrupt.is_frozen();
        let mut facts = lock(&self.facts);
        if frozen && !facts.frozen_by_hook {
            // Park the callback. Everything else — the ring, the decoder, the
            // pending read — is left exactly as it is, which is what makes
            // resuming a single release (§9).
            if self.park() {
                facts.frozen_by_hook = true;
                facts.playing = false;
                self.announce(PlaybackEvent::StateChanged {
                    session_rev: facts.session_rev,
                    state: PlaybackState::Paused,
                });
            }
        } else if !frozen && facts.frozen_by_hook {
            self.release();
            facts.frozen_by_hook = false;
            facts.playing = true;
            self.announce(PlaybackEvent::StateChanged {
                session_rev: facts.session_rev,
                state: PlaybackState::Playing,
            });
        }
        // Ruling 2: dropped before `publish_progress`, which takes this same
        // lock first thing. `std::sync::Mutex` is not reentrant, so holding
        // this across the call deadlocks on the very first pass.
        drop(facts);
        self.publish_progress();
    }
}

impl WaitHook for WaitService {
    fn service(&self) {
        self.service_as(Servicing::BlockedRead);
    }
}
