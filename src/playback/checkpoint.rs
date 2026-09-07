use crate::media::id::MediaId;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use time::OffsetDateTime;

/// Logical resume position, independent of current transport capabilities.
/// `updated_at` is for inspection, never ordering or merging updates.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlaybackCheckpoint {
    pub media: MediaId,
    pub position: Duration,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}
