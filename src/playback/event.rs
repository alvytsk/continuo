use std::time::Duration;

use crate::media::capabilities::MediaCapabilities;
use crate::media::id::MediaId;
use crate::media::metadata::MediaMetadata;

use super::state::PlaybackState;
use super::timeline::PositionQuality;
use super::volume::Volume;

#[derive(Clone, Debug)]
pub enum PlaybackEvent {
    Loaded {
        session_rev: u64,
        media: MediaId,
        metadata: MediaMetadata,
        capabilities: MediaCapabilities,
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
    Warning {
        session_rev: u64,
        message: String,
    },
    Failed {
        session_rev: u64,
        message: String,
    },
}

impl PlaybackEvent {
    /// Terminal outcomes may occupy the reserved tail of the event channel;
    /// ordinary events may not. An outcome the application cannot infer from
    /// anything later — the run ended, the device died, the session stopped —
    /// is terminal.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Failed { .. } | Self::EndOfTrack { .. } => true,
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
}
