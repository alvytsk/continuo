use std::time::Duration;

use crate::media::id::MediaId;
use crate::media::source::SourceLocation;
use crate::resume::ResumeCandidate;

use super::volume::Volume;

/// How a load's start is decided. `Load` carries one of these rather than a
/// bare `Duration` because a `Candidate` cannot be resolved into a position
/// until a decoder has reported a duration to resolve it against — and only
/// the worker has one (Ruling 5): hoisting the decision into `app::run` would
/// mean opening the media twice, once to decide and once to play.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeIntent {
    /// The application has already decided — there was no candidate to
    /// resolve, or a test wants a known start. `Restart` is unaffected: it is
    /// its own command, always to zero, and never goes through `Load`.
    StartAt(Duration),
    /// Resolved by the worker after its single probe, using the same rules the
    /// application would apply if it had a duration to apply them to.
    Candidate(ResumeCandidate),
}

#[derive(Clone, Debug)]
pub enum PlaybackCommand {
    Load {
        media: MediaId,
        source: SourceLocation,
        resume: ResumeIntent,
    },
    Play,
    Pause,
    TogglePause,
    SeekTo(Duration),
    /// Signed seconds; the engine clamps at zero and at a known duration.
    SeekBy(i64),
    /// Validate first, start output only on success. One command rather than an
    /// app-sequenced `SeekTo(0)` + `Play`, because a stopped seek never emits
    /// `SeekCompleted` for the app to sequence on.
    Restart,
    SetVolume(Volume),
    Stop,
    Shutdown,
}

/// Whether a submission entered the queue. §8: saturation is visible, never a
/// block on the application thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    Accepted,
    Busy,
    Gone,
}
