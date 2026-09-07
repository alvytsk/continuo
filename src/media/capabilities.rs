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
