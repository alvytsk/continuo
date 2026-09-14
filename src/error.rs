use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("cannot identify path {path:?}: {reason}")]
    InvalidPath { path: PathBuf, reason: &'static str },
    #[error("cannot normalize identity URL {input:?}: {source}")]
    InvalidUrl {
        input: String,
        #[source]
        source: url::ParseError,
    },
    #[error("cannot normalize identity URL {input:?}: expected HTTP(S) with a host")]
    UnsupportedUrl { input: String },
    #[error("cannot construct feed identity: empty identifier")]
    EmptyFeedId,
    #[error("cannot resolve episode identity: no GUID, enclosure URL, or item link")]
    MissingEpisodeIdentity,
    #[error("cannot parse media identity {input:?}: {reason}")]
    InvalidMediaId { input: String, reason: &'static str },
}

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("invalid tracing filter")]
    Filter(#[from] tracing_subscriber::filter::ParseError),
    #[error("RUST_LOG is not valid Unicode")]
    Environment(#[source] std::env::VarError),
    #[error("cannot initialize tracing")]
    Install(#[from] tracing::subscriber::SetGlobalDefaultError),
}

/// What [`crate::app::run`] returns (design doc §6.5). Both arms are
/// `transparent`, so `main.rs`'s `{error}` and `?error` keep printing the
/// concrete failure rather than a wrapper that says nothing.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Playback(#[from] crate::playback::error::PlaybackError),
    #[error(transparent)]
    Feed(#[from] crate::feed::error::FeedError),
}
