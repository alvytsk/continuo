use std::time::Duration;

use crate::http::error::RemoteFailure;
use crate::media::capabilities::MediaCapabilities;
use crate::media::id::MediaId;
use crate::media::metadata::MediaMetadata;

use super::state::PlaybackState;
use super::timeline::PositionQuality;
use super::volume::Volume;

/// What a load actually did with its resume intent (§8). Carried on `Loaded`
/// rather than announced as a separate event, so a policy that must act on it
/// — lifting checkpoint protection, say — can do so before any `Playing` or
/// progress event has been observed (§5), and so the reserve arithmetic in
/// `engine.rs` never has to budget a fifth event for the widest command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartDisposition {
    /// No candidate, or one that resolved to zero.
    Fresh,
    /// The candidate's position was established by the decoder.
    Resumed,
    /// The entry was complete; M2's replay policy starts it over.
    CompletedReplay,
    /// A positive candidate could not be established, because this source
    /// cannot seek. Playback starts at zero and the entry is protected (§10).
    ResumeUnavailable { retained: Duration },
}

#[derive(Clone, Debug)]
pub enum PlaybackEvent {
    Loaded {
        session_rev: u64,
        media: MediaId,
        metadata: MediaMetadata,
        capabilities: MediaCapabilities,
        /// Where the load actually landed after its refined seek to `start_at`.
        /// Without it the application cannot report where a resume landed and
        /// would render 00:00:00 after one (D8).
        position: Duration,
        /// What the load did with its resume intent, and why. See
        /// `StartDisposition`.
        disposition: StartDisposition,
    },
    StateChanged {
        session_rev: u64,
        state: PlaybackState,
    },
    SeekCompleted {
        session_rev: u64,
        requested: Duration,
        actual: Duration,
        refinement_truncated: bool,
    },
    /// A seek accepted while stopped. Deliberately not `SeekCompleted`: the
    /// target is unvalidated until the decoder opens.
    SeekTargetStored {
        session_rev: u64,
        target: Duration,
    },
    SeekRejected {
        session_rev: u64,
        reason: String,
    },
    /// An accepted seek that a stop or shutdown cancelled before it committed.
    /// Terminal, so §8's "every accepted seek receives an outcome" survives a
    /// shutdown backlog: nothing that follows implies it the way an ordinary
    /// event can be inferred from a later one.
    SeekCancelled {
        session_rev: u64,
        requested: Duration,
    },
    VolumeChanged {
        session_rev: u64,
        volume: Volume,
    },
    EndOfTrack {
        session_rev: u64,
        position: Duration,
    },
    DeviceRecovered {
        session_rev: u64,
    },
    /// Capability evidence that arrived after `Loaded` — an on-demand seek
    /// probe resolving `Unknown`. Ordered, revision-keyed, consumed like
    /// `Loaded`. It never establishes or clears checkpoint protection (§5).
    CapabilitiesChanged {
        session_rev: u64,
        capabilities: MediaCapabilities,
    },
    /// An explicit restart that actually landed.
    ///
    /// `SeekCompleted` deliberately does not cover this (D17): `restart()`
    /// discards a stored target and seeks to zero without emitting one, so a
    /// policy that must tell an explicit restart from any other establishment
    /// has nothing else to key on (G1).
    RestartEstablished {
        session_rev: u64,
        position: Duration,
    },
    Warning {
        session_rev: u64,
        message: String,
    },
    Failed {
        session_rev: u64,
        message: String,
        /// A typed cause for a remote failure, so a policy can act on what
        /// went wrong rather than only read a string a human wrote. M1's
        /// local failures have none; `message` stays the field every existing
        /// test asserts on (Ruling 1) — the remaining stringiness there is
        /// known debt for a later task, not something to fix here.
        cause: Option<RemoteFailure>,
    },
}

impl PlaybackEvent {
    /// Every event carries the revision it was emitted under. A reader that
    /// adopts it from **every** event — not only the ones it acts on — bounds
    /// its exposure to a dropped `DeviceRecovered` to "until the next event of
    /// any kind" (§7).
    pub fn session_rev(&self) -> u64 {
        match self {
            Self::Loaded { session_rev, .. }
            | Self::StateChanged { session_rev, .. }
            | Self::SeekCompleted { session_rev, .. }
            | Self::SeekTargetStored { session_rev, .. }
            | Self::SeekRejected { session_rev, .. }
            | Self::SeekCancelled { session_rev, .. }
            | Self::VolumeChanged { session_rev, .. }
            | Self::EndOfTrack { session_rev, .. }
            | Self::DeviceRecovered { session_rev }
            | Self::CapabilitiesChanged { session_rev, .. }
            | Self::RestartEstablished { session_rev, .. }
            | Self::Warning { session_rev, .. }
            | Self::Failed { session_rev, .. } => *session_rev,
        }
    }

    /// Terminal outcomes may occupy the reserved tail of the event channel;
    /// ordinary events may not. An outcome the application cannot infer from
    /// anything later — the run ended, the device died, the session stopped,
    /// an accepted seek was cancelled rather than completed — is terminal.
    /// `RestartEstablished` and `CapabilitiesChanged` are not: both are
    /// implied by whatever ordinary event comes next in the same way any
    /// other establishment is.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Failed { .. } | Self::EndOfTrack { .. } | Self::SeekCancelled { .. } => true,
            Self::StateChanged { state, .. } => matches!(
                state,
                PlaybackState::Stopped | PlaybackState::Ended | PlaybackState::Failed
            ),
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub session_rev: u64,
    pub media: Option<MediaId>,
    pub position: Duration,
    pub quality: PositionQuality,
    /// True exactly while a source read is blocked on the network and the
    /// hook, not the worker's own loop pass, is what is keeping progress
    /// alive (Ruling 4). Distinct from `quality == Degraded`, which reports a
    /// timing base that jumped - an unrelated fact this field must never be
    /// derived from.
    pub buffering: bool,
}

/// What a shutdown hands back: the position the worker captured on its way out,
/// and every event that never reached the application — whether it was still in
/// the worker's backlog or already in the channel when the interrupt landed
/// (D14, D19).
#[derive(Debug)]
pub struct ShutdownReport {
    pub progress: Progress,
    pub events: Vec<PlaybackEvent>,
}
