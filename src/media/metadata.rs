use std::time::Duration;

use crate::playback::provenance::PositionProvenance;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MediaMetadata {
    pub title: Option<String>,
    pub duration: Option<Duration>,
    /// Whether `duration` came from a real index/container header (or is
    /// simply absent) versus `estimate_num_mpeg_frames`'s ~16-frame
    /// extrapolation (§5.5). The spike measured that estimate 40% short on a
    /// genuinely VBR file with no Xing/VBRI tag — exact for CBR, which is why
    /// nothing had noticed. Defaults to `Established`: nothing in this
    /// milestone's decode path yet distinguishes the two, so every existing
    /// caller keeps the meaning it always had.
    pub duration_provenance: PositionProvenance,
}
