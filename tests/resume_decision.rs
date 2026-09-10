//! §11's table, tested against the persistence-free shape `decide_resume`
//! actually reasons about. Moved here from `src/session.rs` (Task 8): the
//! function itself moved to `src/resume.rs` so the playback worker can
//! resolve a candidate without depending on `crate::persistence` (G3).

use std::time::Duration;

use continuo::playback::provenance::PositionProvenance;
use continuo::resume::{KnownDuration, ResumeCandidate, ResumeDecision, decide_resume};

fn stored(secs: u64, completed: bool) -> ResumeCandidate {
    ResumeCandidate {
        position: Duration::from_secs(secs),
        completed,
    }
}

/// Every existing test in this file predates provenance and means an
/// established duration - an M1/M2-style value a decoder actually reported.
fn secs(value: u64) -> Option<KnownDuration> {
    established(Duration::from_secs(value))
}

/// A duration extrapolated from a byte-rate estimate rather than observed
/// (§5.5) - `decide_resume` must treat this exactly as it treats `None`.
fn estimated(value: Duration) -> Option<KnownDuration> {
    Some(KnownDuration {
        value,
        provenance: PositionProvenance::Estimated,
    })
}

/// A duration from a real index, container header or seek table.
fn established(value: Duration) -> Option<KnownDuration> {
    Some(KnownDuration {
        value,
        provenance: PositionProvenance::Established,
    })
}

#[test]
fn no_entry_starts_at_the_beginning() {
    assert_eq!(decide_resume(None, secs(300)), ResumeDecision::NoEntry);
    assert_eq!(decide_resume(None, secs(300)).start_at(), Duration::ZERO);
}

#[test]
fn a_completed_entry_declines_the_resume_without_losing_its_position() {
    let entry = stored(300, true);
    assert_eq!(
        decide_resume(Some(entry), secs(300)),
        ResumeDecision::Completed
    );
    assert_eq!(
        decide_resume(Some(entry), secs(300)).start_at(),
        Duration::ZERO
    );
    assert_eq!(entry.position, Duration::from_secs(300));
    let short = stored(120, true);
    assert_eq!(
        decide_resume(Some(short), secs(300)),
        ResumeDecision::Completed
    );
    assert_eq!(short.position, Duration::from_secs(120));
}

#[test]
fn an_ordinary_position_inside_the_media_is_the_start() {
    assert_eq!(
        decide_resume(Some(stored(93, false)), secs(300)),
        ResumeDecision::Resume(Duration::from_secs(93))
    );
}

#[test]
fn a_position_of_zero_is_a_start_rather_than_a_resume() {
    assert_eq!(
        decide_resume(Some(stored(0, false)), secs(300)),
        ResumeDecision::AtStart
    );
    assert_eq!(
        decide_resume(Some(stored(0, false)), None),
        ResumeDecision::AtStart
    );
}

#[test]
fn a_position_exactly_at_the_end_is_degenerate_not_a_start() {
    assert_eq!(
        decide_resume(Some(stored(300, false)), secs(300)),
        ResumeDecision::DegenerateEnd
    );
    assert_eq!(
        decide_resume(Some(stored(300, false)), secs(300)).start_at(),
        Duration::ZERO
    );
}

#[test]
fn a_position_past_the_end_is_stale_state() {
    assert_eq!(
        decide_resume(Some(stored(400, false)), secs(300)),
        ResumeDecision::StalePastEnd
    );
    assert_eq!(
        decide_resume(Some(stored(400, false)), secs(300)).start_at(),
        Duration::ZERO
    );
}

#[test]
fn an_unknown_duration_keeps_the_position_unvalidated() {
    assert_eq!(
        decide_resume(Some(stored(93, false)), None),
        ResumeDecision::Unvalidated(Duration::from_secs(93))
    );
    assert_eq!(
        decide_resume(Some(stored(93, false)), None).start_at(),
        Duration::from_secs(93)
    );
}

#[test]
fn completion_outranks_every_position_rule() {
    assert_eq!(
        decide_resume(Some(stored(400, true)), secs(300)),
        ResumeDecision::Completed
    );
    assert_eq!(
        decide_resume(Some(stored(0, true)), secs(300)),
        ResumeDecision::Completed
    );
}

#[test]
fn the_decision_is_the_same_whoever_applies_it() {
    // G3: the worker resolves a Candidate with these rules and the application
    // resolves a duration-known one with the same function. A divergence here
    // is a resume that lands somewhere the checkpoint never said.
    for secs_stored in [0u64, 93, 300, 400] {
        for completed in [false, true] {
            for duration in [None, secs(300)] {
                let candidate = stored(secs_stored, completed);
                assert_eq!(
                    decide_resume(Some(candidate), duration),
                    decide_resume(Some(candidate), duration),
                );
            }
        }
    }
}

#[test]
fn a_checkpoint_past_an_estimated_duration_is_retained_rather_than_declared_stale() {
    // The spike measured a 361 s estimate for a 600 s VBR file. Under the old
    // rule a listener 70 % in resumes at zero and their entry is discarded as
    // stale — silent loss, from a number nothing ever measured.
    let candidate = ResumeCandidate {
        position: Duration::from_secs(420),
        completed: false,
    };
    assert_eq!(
        decide_resume(Some(candidate), estimated(Duration::from_secs(361))),
        ResumeDecision::Unvalidated(Duration::from_secs(420))
    );
}

#[test]
fn a_checkpoint_past_an_established_duration_is_still_stale() {
    // The M2 rule is unchanged where the duration was actually observed:
    // a position past a known end really is a file that changed underneath us.
    let candidate = ResumeCandidate {
        position: Duration::from_secs(420),
        completed: false,
    };
    assert_eq!(
        decide_resume(Some(candidate), established(Duration::from_secs(361))),
        ResumeDecision::StalePastEnd
    );
}
