use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum PlaybackError {
    #[error("cannot open media {path:?}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot play {path:?}: {reason}")]
    UnsupportedInput { path: PathBuf, reason: String },
    #[error("cannot decode media")]
    Decode(#[source] symphonia::core::errors::Error),
    #[error("cannot seek to {target:?}")]
    SeekFailed {
        target: std::time::Duration,
        #[source]
        source: symphonia::core::errors::Error,
    },
    #[error("the audio output did not respond within the deadline")]
    Timeout,
    #[error("the operation was cancelled")]
    Cancelled,
    #[error("audio output failure")]
    Output(#[source] cpal::Error),
    #[error("terminal I/O error")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Failed(String),
}
