//! What §11's table says a resume should do, decided from a bare position and
//! completion flag rather than from anything persistence owns.
//!
//! Deliberately persistence-free: nothing here imports `PersistedCheckpoint`
//! or anything else from `crate::persistence`. That is what lets
//! `crate::playback` — the worker, resolving a candidate against the
//! duration only its own decode probe can report — depend on this module
//! without the playback engine ever learning that persistence exists (G3).
//! `crate::session` depends on it too, but only to convert a stored entry
//! into a `ResumeCandidate` and to re-export the type and the function so
//! `app::run` and the tests that predate this split keep their import paths
//! (Ruling 4); the decision itself is made wherever a duration actually is,
//! which since Ruling 5 is the worker alone. Whoever calls `decide_resume`
//! gets the same answer for the same inputs, which is what keeps a
//! worker-resolved resume and an application-resolved one from ever landing
//! somewhere the checkpoint never said.

use std::time::Duration;

use crate::playback::provenance::PositionProvenance;

/// A duration `decide_resume` reasons against, carrying whether it was
/// derived from a real index/container header or extrapolated from a
/// byte-rate estimate (§5.5). The distinction matters because an
/// under-estimated duration is not evidence that a stored position is stale:
/// `decide_resume` treats `Estimated` exactly as it treats an absent
/// duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KnownDuration {
    pub value: Duration,
    pub provenance: PositionProvenance,
}

impl From<Duration> for KnownDuration {
    /// For the callers this milestone must not change: an M1/M2-style
    /// duration a decoder actually reported. Do not reach for this to
    /// describe a value that came from `estimate_num_mpeg_frames` — construct
    /// `KnownDuration` explicitly with `PositionProvenance::Estimated` there.
    fn from(value: Duration) -> Self {
        Self {
            value,
            provenance: PositionProvenance::Established,
        }
    }
}

/// The two facts §11's table is a function of, however they were learned. A
/// caller with a `PersistedCheckpoint` in hand converts it into one of these
/// (`Session`'s `From` impl does exactly that); a caller with only a
/// worker-reported target builds one directly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResumeCandidate {
    pub position: Duration,
    pub completed: bool,
}

/// Builds the candidate `decide_resume` reasons about from a checkpoint's raw
/// fields, without this module ever importing `PersistedCheckpoint` (G3): the
/// caller — `open_persistence`, resolving whatever a freshly loaded file
/// contains — hands over `position` and `completed` directly instead.
///
/// An absent established position (design doc §4.2 — an entry that exists
/// only to carry an estimate, with nothing ever established) is its own
/// case, `None`, rather than a fabricated `Some(ResumeCandidate { position:
/// Duration::ZERO, .. })`: `decide_resume` would read that as
/// `ResumeDecision::AtStart`, an established position of zero, which is not
/// what an absent position means. Resuming a listener at the start because
/// only an estimate was ever stored for them is exactly the loss this
/// construction exists to prevent.
///
/// A completed entry is still reported even with no established position
/// (an estimated timeline can complete a track without ever establishing its
/// anchor, design doc §4.2): `decide_resume` decides `Completed` before it
/// ever looks at `position` (see below), so the filler `Duration::ZERO` built
/// here for that case is provably never read.
pub fn resume_candidate(position: Option<Duration>, completed: bool) -> Option<ResumeCandidate> {
    if completed {
        return Some(ResumeCandidate {
            position: position.unwrap_or(Duration::ZERO),
            completed: true,
        });
    }
    position.map(|position| ResumeCandidate {
        position,
        completed: false,
    })
}

/// What §11's table says about one resume candidate, and why.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeDecision {
    /// No candidate at all.
    NoEntry,
    /// The entry is complete. D1 retains its position; the resume declines it.
    Completed,
    /// An entry that never got anywhere.
    AtStart,
    Resume(Duration),
    /// `position == duration`: preserved in storage, not usable as a start.
    DegenerateEnd,
    /// `position > duration`: the file no longer describes this media.
    StalePastEnd,
    /// The duration is unknown, so the position is retained unvalidated.
    Unvalidated(Duration),
}

impl ResumeDecision {
    pub fn start_at(&self) -> Duration {
        match self {
            Self::Resume(position) | Self::Unvalidated(position) => *position,
            Self::NoEntry
            | Self::Completed
            | Self::AtStart
            | Self::DegenerateEnd
            | Self::StalePastEnd => Duration::ZERO,
        }
    }
}

/// Applied against whatever duration the caller already has in hand — the
/// worker's own decode probe when resolving a `ResumeIntent::Candidate`, a
/// literal in a test — so deciding never opens a second file on its own
/// account. Completion is never inferred from `position >= duration`, and
/// there is no near-end heuristic anywhere.
///
/// An **estimated** duration is treated exactly as an absent one (§5.5):
/// `StalePastEnd` requires an established duration, because declaring a
/// listener's checkpoint stale is destructive and an estimate — which the
/// spike measured 40% short on a genuinely VBR file — is not evidence enough
/// to do it.
pub fn decide_resume(
    candidate: Option<ResumeCandidate>,
    duration: Option<KnownDuration>,
) -> ResumeDecision {
    let Some(candidate) = candidate else {
        return ResumeDecision::NoEntry;
    };
    if candidate.completed {
        return ResumeDecision::Completed;
    }
    if candidate.position.is_zero() {
        return ResumeDecision::AtStart;
    }
    let Some(duration) = duration else {
        return ResumeDecision::Unvalidated(candidate.position);
    };
    if duration.provenance == PositionProvenance::Estimated {
        return ResumeDecision::Unvalidated(candidate.position);
    }
    match candidate.position.cmp(&duration.value) {
        std::cmp::Ordering::Less => ResumeDecision::Resume(candidate.position),
        std::cmp::Ordering::Equal => ResumeDecision::DegenerateEnd,
        std::cmp::Ordering::Greater => ResumeDecision::StalePastEnd,
    }
}
