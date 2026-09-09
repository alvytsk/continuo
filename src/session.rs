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
    /// Not read until Task 7's pending-force resolution reconciles a stale
    /// sample against the revision it was taken under.
    #[allow(dead_code)]
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
    /// Tracked for the current media, so a checkpoint written before any
    /// further event still carries the right completion.
    completed: bool,
    last_sample: Option<Sample>,
    /// Monotonic anchor for the 5 s rule; set when playback establishes.
    last_capture: Option<Instant>,
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
        }
    }

    pub fn observe(&mut self, event: &PlaybackEvent, now: ClockSample) -> Action {
        self.session_rev = event.session_rev();

        match event {
            PlaybackEvent::Loaded {
                media, position, ..
            } => self.on_loaded(media, *position, now),
            PlaybackEvent::StateChanged { state, .. } => self.on_state(*state, now),
            PlaybackEvent::VolumeChanged { volume, .. } => {
                self.state.set_volume(*volume);
                self.submit(Urgency::Ordinary)
            }
            PlaybackEvent::EndOfTrack { position, .. } => {
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

        if self.playback != PlaybackState::Playing {
            return Action::None;
        }
        let due = self
            .last_capture
            .is_none_or(|last| now.monotonic.duration_since(last) >= CAPTURE_INTERVAL);
        if !due {
            return Action::None;
        }
        self.last_capture = Some(now.monotonic);
        self.record_current(progress.position, now);
        self.submit(Urgency::Ordinary)
    }

    fn on_state(&mut self, state: PlaybackState, now: ClockSample) -> Action {
        self.playback = state;
        if state == PlaybackState::Playing {
            // §12: a successful establishment after a completed state clears
            // it. Persistence restoration alone does not.
            self.completed = false;
            self.last_capture = Some(now.monotonic);
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
        if !switching {
            self.last_sample = Some(Sample {
                session_rev: self.session_rev,
                media: media.clone(),
                position,
            });
            return Action::None;
        }

        // §3: `load()` overwrites the engine's position with `start_at` before
        // anything publishes, so the outgoing media's final position is only
        // reachable from what the session retained.
        if let Some(previous) = self.last_sample.take() {
            let completed = self.completed;
            self.state.record(
                &PlaybackCheckpoint {
                    media: previous.media,
                    position: previous.position,
                    updated_at: now.wall,
                },
                completed,
            );
        }

        self.current_media = Some(media.clone());
        self.state.current_media = Some(media.clone());
        self.completed = self.state.completed_for(media);
        self.last_sample = Some(Sample {
            session_rev: self.session_rev,
            media: media.clone(),
            position,
        });
        self.last_capture = None;
        self.submit(Urgency::Forced)
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
