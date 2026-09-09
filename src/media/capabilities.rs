#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Continuity {
    Unresolved,
    Finite,
    Indefinite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekSupport {
    Unknown,
    Native,
    RestartAndDiscard,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeCapability {
    Supported,
    Unsupported,
    Undetermined,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaCapabilities {
    pub continuity: Continuity,
    pub seek: SeekSupport,
}

impl MediaCapabilities {
    pub fn resume_capability(&self) -> ResumeCapability {
        match (self.continuity, self.seek) {
            (Continuity::Indefinite, _) => ResumeCapability::Unsupported,
            (Continuity::Unresolved, _) | (Continuity::Finite, SeekSupport::Unknown) => {
                ResumeCapability::Undetermined
            }
            (Continuity::Finite, SeekSupport::Unsupported) => ResumeCapability::Unsupported,
            (Continuity::Finite, SeekSupport::Native | SeekSupport::RestartAndDiscard) => {
                ResumeCapability::Supported
            }
        }
    }
}

/// What a source has proven about itself, independent of how it was reached.
///
/// Lives beside [`MediaCapabilities`] rather than in `http`, because both a
/// local file and a remote one construct it (`DecodedSource::open` builds
/// local evidence with `demuxer: DemuxerSeek::Proven`), and an HTTP-flavoured
/// type here would make local playback construct a network type to describe
/// itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceEvidence {
    /// A fixed response length or a valid range total.
    pub byte_len: Option<u64>,
    pub byte_seekable: bool,
    /// Explicit live/ICY semantics were seen.
    pub live: bool,
    /// Whether the *demuxer's* ability to seek in media time is established.
    /// Byte access says nothing about it, and `MediaSource` carries no such
    /// evidence, so it can only come from a format this project has already
    /// demonstrated or from a trial seek.
    pub demuxer: DemuxerSeek,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DemuxerSeek {
    /// Demonstrated.
    Proven,
    /// Not yet established. Publishes `Unknown`, verified on demand.
    Unproven,
}
