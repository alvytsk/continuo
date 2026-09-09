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
use crate::persistence::model::PersistedState;
use crate::persistence::writer::Urgency;
use crate::playback::checkpoint::PlaybackCheckpoint;
use crate::playback::event::{PlaybackEvent, Progress};
use crate::playback::state::PlaybackState;

/// The capture interval §6 requires while playing. With the writer's 2 s
/// coalescing window it bounds worst-case loss at 7 s.
pub const CAPTURE_INTERVAL: Duration = Duration::from_secs(5);

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
    /// comes from `Progress` — a resolved force and the shutdown snapshot —
    /// because such a position is one the engine never validated until
    /// something established (D20). The two positions that arrive on events of
    /// their own are exempt, and say so where they are recorded (D6).
    established: bool,
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
        }
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
                media, position, ..
            } => self.on_loaded(media, *position, now),
            PlaybackEvent::StateChanged { state, .. } => self.on_state(*state, now),
            PlaybackEvent::SeekCompleted { .. } => {
                self.resolve_target();
                self.established = true;
                self.completed = false;
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
                self.record_current(*position, now);
                self.submit(Urgency::Forced)
            }
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
    fn on_loaded(&mut self, media: &MediaId, position: Duration, now: ClockSample) -> Action {
        let switching = self.current_media.as_ref() != Some(media);

        // Recorded before anything resets, and through `position_for`: a
        // stopped seek's target is the outgoing media's real position, and
        // clearing it first would write the pre-seek sample back over it. A
        // completed entry is left alone entirely — `EndOfTrack` already
        // recorded the position D1 retains, and `last_sample` can only be
        // behind it.
        let outgoing = if switching {
            self.last_sample.take()
        } else {
            None
        };
        if let Some(previous) = outgoing
            && !self.completed
        {
            // §3: `load()` overwrites the engine's own position with `start_at`
            // before anything publishes, so this is the only place the outgoing
            // media's final position still exists.
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

        // Only now: a force raised against the previous media cannot answer for
        // this one, and none of these carry across a load.
        self.pending_force = None;
        self.resolve_target();
        self.established = false;
        self.last_capture = None;

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

    /// The only write path for the current media and its completion. Nothing in
    /// the type system keeps two sibling fields in step, so this method exists
    /// to make sure no edit can move `current_media` without deciding
    /// `completed` in the same breath.
    fn adopt_media(&mut self, media: MediaId, completed: bool) {
        self.current_media = Some(media.clone());
        self.state.current_media = Some(media);
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

    fn record_current(&mut self, position: Duration, now: ClockSample) {
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
