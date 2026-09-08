use std::time::Duration;

use crate::media::id::MediaId;
use crate::media::source::SourceLocation;

use super::volume::Volume;

#[derive(Clone, Debug)]
pub enum PlaybackCommand {
    Load {
        media: MediaId,
        source: SourceLocation,
        start_at: Duration,
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
