#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackState {
    Idle,
    Loading,
    Playing,
    Paused,
    Reconnecting,
    Stopped,
    Ended,
    Failed,
}

impl PlaybackState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Loading => "loading",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Reconnecting => "reconnecting",
            Self::Stopped => "stopped",
            Self::Ended => "ended",
            Self::Failed => "failed",
        }
    }
}
