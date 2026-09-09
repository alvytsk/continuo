//! The checkpoint policy: what to persist, and when.
//!
//! Pure by construction — no I/O, no threads, no clock of its own. Every
//! decision is a function of the lossless event stream, the single `Progress`
//! sample the application takes per iteration, and an injected `ClockSample`.
//!
//! The application's order is load-bearing: drain events, then sample once.
//! §3 establishes that a command's event reaches the application on the pass
//! *after* the one that applied it, and that the pass publishes progress before
//! it flushes events — so the sample that follows a transition event is
//! strictly newer than the transition it reports.

use std::time::{Duration, Instant};

use crate::clock::ClockSample;
use crate::media::id::MediaId;
use crate::persistence::model::{PersistedCheckpoint, PersistedState};
use crate::persistence::writer::Urgency;
use crate::playback::checkpoint::PlaybackCheckpoint;
use crate::playback::event::{PlaybackEvent, Progress, ShutdownReport, StartDisposition};
use crate::playback::state::PlaybackState;
use crate::resume::ResumeCandidate;
// Re-exported so `src/app.rs` and `tests/resume_contract.rs` keep importing
// these from `session` — the type and the function moved to `src/resume.rs`
// so the playback worker could depend on them too (G3), without dragging
// `crate::persistence` in behind them. The worker is `decide_resume`'s only
// production caller now (Ruling 5); what stays here is the one conversion
// below, from a persisted entry to the persistence-free shape it reasons
// about.
pub use crate::resume::{ResumeDecision, decide_resume};

/// The capture interval §6 requires while playing. With the writer's 2 s
/// coalescing window it bounds worst-case loss at 7 s.
pub const CAPTURE_INTERVAL: Duration = Duration::from_secs(5);

/// Persistence is a `session` concern, not a `resume` one: `resume` must not
/// know `PersistedCheckpoint` exists (G3), so this is the one place a stored
/// entry becomes the persistence-free shape `decide_resume` reasons about.
impl From<&PersistedCheckpoint> for ResumeCandidate {
    fn from(entry: &PersistedCheckpoint) -> Self {
        Self {
            position: entry.position,
            completed: entry.completed,
        }
    }
}

#[derive(Debug)]
pub enum Action {
    None,
    Submit {
        state: PersistedState,
        urgency: Urgency,
    },
}

/// The position the policy would use for a checkpoint it can no longer sample.
struct Sample {
    session_rev: u64,
    media: MediaId,
    position: Duration,
}

pub struct Session {
    /// The authoritative state. `Action::Submit` carries a clone, which becomes
    /// the writer's property; the session never shares a reference into it.
    state: PersistedState,
    session_rev: u64,
    playback: PlaybackState,
    current_media: Option<MediaId>,
    /// Whether `current_media` is complete, so a checkpoint written before any
    /// further event still carries the right completion. The pair moves through
    /// `adopt_media` and nowhere else; every other write here changes the flag
    /// for a media that is not moving.
    completed: bool,
    last_sample: Option<Sample>,
    /// Monotonic anchor for the 5 s rule; set when playback establishes.
    last_capture: Option<Instant>,
    /// Raised by an event that forces a checkpoint but carries no position,
    /// keyed by the revision that event carried (D13).
    pending_force: Option<u64>,
    /// A stopped seek's target, which supersedes `Progress.position` until the
    /// engine resolves it (D17).
    outstanding_target: Option<Duration>,
    /// Whether playback established, or the position changed explicitly, since
    /// the current media was loaded. It gates every checkpoint whose position
    /// comes from `Progress` — a resolved force, the ordinary 5 s capture, the
    /// shutdown snapshot and the outgoing entry a media switch records —
    /// because such a position is one the engine never validated until
    /// something established. D20 gates only the shutdown force and reads the
    /// flag at the current revision; §19 records why the shipped gate is wider
    /// and why the flag latches until the next `Loaded`. The two positions that
    /// arrive on events of their own are exempt, and say so where they are
    /// recorded (D6).
    established: bool,
    /// A positive checkpoint the current run must not overwrite, because
    /// playback fell back to zero on a source that cannot resume (§10).
    ///
    /// Deliberately not a max-position merge: the point is to recover the
    /// *earlier* resume point, and progress heard in a fallback run does not
    /// replace it however far it goes (R4). Set from `Loaded.disposition`
    /// rather than a later warning, so it is in force before any `Playing` or
    /// progress event can be observed (§5). Ends only on an established
    /// `RestartEstablished` (G1), an established `SeekCompleted`, or verified
    /// completion — never on `CapabilitiesChanged` alone (§10).
    protected: Option<Duration>,
}

impl Session {
    pub fn new(state: PersistedState) -> Self {
        Self {
            state,
            session_rev: 0,
            playback: PlaybackState::Idle,
            // Learned from `Loaded`, never from the file: what was current last
            // run says nothing about what this run is playing.
            current_media: None,
            completed: false,
            last_sample: None,
            last_capture: None,
            pending_force: None,
            outstanding_target: None,
            established: false,
            protected: None,
        }
    }

    /// The state as it currently stands, for a reader that needs it directly
    /// rather than through whichever `Action` happens to submit next —
    /// `Action::Submit` only ever carries a clone of exactly this. The test
    /// suite is the one caller today: a protected capture can legitimately
    /// leave the state unchanged, so asserting on it this way is what lets a
    /// test tell "no submission happened" apart from "a submission happened
    /// and changed nothing."
    pub fn state(&self) -> &PersistedState {
        &self.state
    }

    pub fn observe(&mut self, event: &PlaybackEvent, now: ClockSample) -> Action {
        let session_rev = event.session_rev();
        self.session_rev = session_rev;
        // A newer revision re-keys a pending force rather than dropping it:
        // `rebuild` bumps the revision on device recovery with the position
        // continuous across it, so the force is still answerable — and dropping
        // it would lose a real pause for good, since no ordinary trigger fires
        // while paused. `Loaded` is the one exception, retired in `on_loaded`.
        if self.pending_force.is_some() {
            self.pending_force = Some(session_rev);
        }

        match event {
            PlaybackEvent::Loaded {
                media,
                position,
                disposition,
                ..
            } => self.on_loaded(media, *position, disposition, now),
            PlaybackEvent::StateChanged { state, .. } => self.on_state(*state, now),
            PlaybackEvent::SeekCompleted { .. } => {
                self.resolve_target();
                self.established = true;
                self.completed = false;
                // An established user seek (§10): the listener steered the
                // position themselves, so whatever fallback zero was protected
                // no longer needs protecting.
                self.protected = None;
                self.pending_force = Some(session_rev);
                Action::None
            }
            // This event exists only because nothing else lets the policy tell
            // an explicit restart from any other establishment (G1) — which is
            // exactly the distinction clearing protection needs, so it clears
            // it and otherwise behaves like `SeekCompleted`.
            PlaybackEvent::RestartEstablished { .. } => {
                self.resolve_target();
                self.established = true;
                self.completed = false;
                self.protected = None;
                self.pending_force = Some(session_rev);
                Action::None
            }
            // One of the two positions that never come from `Progress` (D6),
            // so the establishment gate does not apply: the target is the
            // listener's own, not a number the engine has yet to validate.
            PlaybackEvent::SeekTargetStored { target, .. } => {
                self.outstanding_target = Some(*target);
                self.established = true;
                // §12 has a seek clear completion, and a stopped seek is the
                // same listener intent one step earlier: a target persisted
                // beside `completed` would be thrown away by the resume it
                // exists to steer.
                self.completed = false;
                self.record_current(*target, now);
                self.submit(Urgency::Forced)
            }
            PlaybackEvent::VolumeChanged { volume, .. } => {
                self.state.set_volume(*volume);
                self.submit(Urgency::Ordinary)
            }
            // The other one (D6): the end of the track is a position the engine
            // reached, carried by the event that reports it.
            PlaybackEvent::EndOfTrack { position, .. } => {
                self.resolve_target();
                self.established = true;
                self.completed = true;
                // Cleared before `record_current`, not after: verified
                // completion is itself the thing worth writing, and clearing
                // afterwards would have gated that very write (§10).
                self.protected = None;
                self.record_current(*position, now);
                self.submit(Urgency::Forced)
            }
            // §10: "Capability changes alone never delete, clear or replace
            // checkpoints." A server that starts advertising ranges mid-session
            // must not be able to discard a protected entry by saying so —
            // this is `Action::None` and falls through to the catch-all below
            // for exactly that reason: there is nothing here to touch.
            PlaybackEvent::CapabilitiesChanged { .. } => Action::None,
            // A cancelled seek commits no target — `resolve_target()` is
            // deliberately not called here, because the stored target it
            // would discard belongs to a stopped seek that is still
            // outstanding, not to this one.
            PlaybackEvent::SeekCancelled { .. } => Action::None,
            _ => Action::None,
        }
    }

    pub fn tick(&mut self, progress: &Progress, now: ClockSample) -> Action {
        // The same guard `Mirror` already applies: never persist a position
        // from a session the policy was not tracking.
        if progress.session_rev != self.session_rev {
            return Action::None;
        }
        let Some(media) = self.current_media.clone() else {
            return Action::None;
        };
        self.last_sample = Some(Sample {
            session_rev: progress.session_rev,
            media,
            position: progress.position,
        });

        // The sample the pending force has been waiting for. §3's pass ordering
        // makes it newer than the transition that raised the force, so this is
        // a resolution rather than a delay (D13).
        if self.pending_force == Some(progress.session_rev) {
            self.pending_force = None;
            // The force is answered by recording nothing when the gate is shut:
            // there is no validated position for it to checkpoint.
            if !self.checkpoint_from_progress(progress.position, now) {
                return Action::None;
            }
            self.last_capture = Some(now.monotonic);
            return self.submit(Urgency::Forced);
        }

        if self.playback != PlaybackState::Playing {
            return Action::None;
        }
        let due = self
            .last_capture
            .is_none_or(|last| now.monotonic.duration_since(last) >= CAPTURE_INTERVAL);
        if !due {
            return Action::None;
        }
        // `playback` can still read `Playing` here across a media switch, since
        // the `StateChanged{Loading}` that precedes `Loaded` is an ordinary
        // event the engine may drop under backlog. The gate is what makes the
        // interval harmless in that window: nothing has established the
        // incoming media, so its sampled position is not a checkpoint.
        if !self.checkpoint_from_progress(progress.position, now) {
            return Action::None;
        }
        self.last_capture = Some(now.monotonic);
        self.submit(Urgency::Ordinary)
    }

    /// Checkpoint a position that came from `Progress`, and report whether one
    /// was recorded. Every such position goes through here, which is what makes
    /// the two rules that qualify them impossible to forget: an outstanding
    /// stopped-seek target supersedes the sample (D17), and a sample taken
    /// before anything established is one the engine never validated (D20) —
    /// §11 resumes a completed entry at zero, and `load()` reports `Loaded`
    /// before it opens the device, so an ungated sample writes that zero over
    /// the position D1 retains. The two positions that arrive on events of their
    /// own do not come through here and are not gated (D6).
    fn checkpoint_from_progress(&mut self, sampled: Duration, now: ClockSample) -> bool {
        if !self.established {
            return false;
        }
        let position = self.position_for(sampled);
        self.record_current(position, now);
        true
    }

    fn on_state(&mut self, state: PlaybackState, now: ClockSample) -> Action {
        let previous = self.playback;
        self.playback = state;
        match state {
            PlaybackState::Playing => {
                // The engine can resolve a stored target by *discarding* it:
                // `restart()` clears it, seeks to zero and lands here with no
                // SeekCompleted ever emitted (D17).
                self.resolve_target();
                self.established = true;
                // §12: a successful establishment after a completed state
                // clears it. Persistence restoration alone does not.
                self.completed = false;
                self.last_capture = Some(now.monotonic);
            }
            // A pause that interrupts no playback is not a checkpoint: every
            // launch emits one before the queued Play is dispatched.
            PlaybackState::Paused if previous == PlaybackState::Playing => {
                self.pending_force = Some(self.session_rev);
            }
            // `do_stop` returns early from Idle, Stopped and Failed, so this
            // event only exists when something was actually running.
            PlaybackState::Stopped => {
                self.pending_force = Some(self.session_rev);
            }
            _ => {}
        }
        Action::None
    }

    /// A `Loaded` for a different media is **one** snapshot: the outgoing entry
    /// is recorded from `last_sample` and `current_media` moves in a single
    /// mutation. A keep-latest slot cannot promise that an intermediate
    /// submission reaches disk, so "flush, then move" is unenforceable — and
    /// unnecessary, since the snapshot is the whole state.
    fn on_loaded(
        &mut self,
        media: &MediaId,
        position: Duration,
        disposition: &StartDisposition,
        now: ClockSample,
    ) -> Action {
        let switching = self.current_media.as_ref() != Some(media);

        // First, while every per-media field still describes the media on its
        // way out — `protected` among them, so this reads the outgoing media's
        // protection, not the incoming one's.
        if switching {
            self.record_outgoing(now);
        }

        // Only now: a force raised against the previous media cannot answer for
        // this one, and none of these carry across a load.
        self.pending_force = None;
        self.resolve_target();
        self.established = false;
        self.last_capture = None;
        // Set from the disposition this `Loaded` carries, not from a later
        // warning: this is what puts protection in force before any `Playing`
        // or progress event for the incoming media can be observed (§5).
        // `ResumeUnavailable` is the only disposition that sets it — every
        // other one, including a plain reload of the same media, clears it.
        self.protected = match disposition {
            StartDisposition::ResumeUnavailable { retained } => Some(*retained),
            _ => None,
        };

        let completed = self.state.completed_for(media);
        self.adopt_media(media.clone(), completed);
        self.last_sample = Some(Sample {
            session_rev: self.session_rev,
            media: media.clone(),
            position,
        });

        if switching {
            self.submit(Urgency::Forced)
        } else {
            Action::None
        }
    }

    /// The entry for the media on its way out, written from the position the
    /// session retained for it. §3: `load()` overwrites the engine's own
    /// position with `start_at` before anything publishes, so `last_sample` is
    /// the only place that position still exists.
    ///
    /// **Call this before the per-media fields reset.** `completed`,
    /// `established`, `outstanding_target` and `protected` are all read here,
    /// and all four describe the outgoing media only until `on_loaded` resets
    /// them — read afterwards they describe the incoming one, and the mistake
    /// would be silent. They are read nowhere else in `on_loaded`.
    ///
    /// Three things make a retained sample not worth writing. A completed
    /// entry's position is the one `EndOfTrack` recorded and the sample can
    /// only be behind it (D1). A sample for a media nothing established is the
    /// zero §11 resumes at, not a position the engine ever validated — `load()`
    /// reports `Loaded` before it opens the device, so switching away from a
    /// launch that failed would otherwise carry that zero out as the media's
    /// final word (D20). And a protected entry (§10) must not be overwritten
    /// by this path either: this is *not* `record_current`, it writes the
    /// previous media's entry straight from `last_sample`, so a gate placed
    /// only in `record_current` would leave a media switch free to overwrite
    /// the very checkpoint protection exists to keep.
    fn record_outgoing(&mut self, now: ClockSample) {
        let Some(previous) = self.last_sample.take() else {
            return;
        };
        if self.protected.is_some() {
            return;
        }
        if self.completed || !self.established {
            return;
        }
        // A stopped seek's target is the outgoing media's real position, and
        // resolving it first would write the pre-seek sample back over it (D17).
        let position = self.position_for(previous.position);
        self.state.record(
            &PlaybackCheckpoint {
                media: previous.media,
                position,
                updated_at: now.wall,
            },
            false,
        );
    }

    /// The only write path for the current media and its completion. Nothing in
    /// the type system keeps two sibling fields in step, so this method exists
    /// to make sure no edit can move `current_media` without deciding
    /// `completed` in the same breath.
    fn adopt_media(&mut self, media: MediaId, completed: bool) {
        self.current_media = Some(media.clone());
        self.state.set_current_media(media);
        self.completed = completed;
    }

    /// The engine has taken the stopped seek's target somewhere the sampled
    /// position can be trusted again — by adopting it, or by discarding it.
    fn resolve_target(&mut self) {
        self.outstanding_target = None;
    }

    /// A stopped seek stores a target and leaves the engine's position where it
    /// was, so any position-derived checkpoint that follows would write the
    /// pre-seek value back over it (D17).
    fn position_for(&self, sampled: Duration) -> Duration {
        self.outstanding_target.unwrap_or(sampled)
    }

    /// The final snapshot. `volume` and `current_media` are written whatever
    /// happened; the position goes through the same gate every other sampled
    /// position does, and is never taken from a session the policy was not
    /// tracking.
    pub fn shutdown_snapshot(&mut self, progress: &Progress, now: ClockSample) -> PersistedState {
        let Some(media) = self.current_media.clone() else {
            return self.state.clone();
        };
        let sampled = if progress.session_rev == self.session_rev {
            Some(progress.position)
        } else {
            self.last_sample
                .as_ref()
                .filter(|sample| sample.session_rev == self.session_rev && sample.media == media)
                .map(|sample| sample.position)
        };
        if let Some(sampled) = sampled {
            self.checkpoint_from_progress(sampled, now);
        }
        self.state.clone()
    }

    /// The whole of the shutdown handoff that belongs to the policy: replay the
    /// events the application never drained, then take the forced snapshot
    /// (D19). Both halves in one place, because each without the other is a
    /// lost checkpoint — the replay is what makes the snapshot one taken from a
    /// session that has seen everything the run produced.
    ///
    /// The replays are reconciliation rather than submission: whatever they
    /// would have submitted on their own is superseded by the snapshot this
    /// returns.
    ///
    /// The engine-facing half — the shutdown interrupt and the `join` that
    /// produces the report — stays at the call site: it touches the handle,
    /// not the policy.
    pub fn reconcile_shutdown(
        &mut self,
        report: &ShutdownReport,
        now: ClockSample,
    ) -> PersistedState {
        for event in &report.events {
            let _ = self.observe(event, now);
        }
        self.shutdown_snapshot(&report.progress, now)
    }

    /// The write path for the current media's checkpoint: the periodic 5 s
    /// capture, the pause and stop forces, the resolved shutdown snapshot and
    /// `SeekTargetStored` all reach the stored state through here, so gating
    /// here alone covers all of them at once. It does **not** cover
    /// `record_outgoing`: that path writes the *previous* media's entry from
    /// `last_sample` on a media switch, never through this function, so it
    /// carries the identical gate on its own (§10).
    fn record_current(&mut self, position: Duration, now: ClockSample) {
        if self.protected.is_some() {
            return;
        }
        let Some(media) = self.current_media.clone() else {
            return;
        };
        let completed = self.completed;
        self.state.record(
            &PlaybackCheckpoint {
                media,
                position,
                updated_at: now.wall,
            },
            completed,
        );
    }

    fn submit(&self, urgency: Urgency) -> Action {
        Action::Submit {
            state: self.state.clone(),
            urgency,
        }
    }
}
