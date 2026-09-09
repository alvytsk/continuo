//! The checkpoint policy, driven synchronously.

use std::time::Duration;

use continuo::clock::{Clock, FakeClock};
use continuo::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use continuo::media::metadata::MediaMetadata;
use continuo::persistence::model::PersistedState;
use continuo::persistence::writer::Urgency;
use continuo::playback::event::{PlaybackEvent, Progress};
use continuo::playback::state::PlaybackState;
use continuo::playback::timeline::PositionQuality;
use continuo::playback::volume::Volume;
use continuo::session::{Action, Session};

mod support;
use support::media;

fn loaded(session_rev: u64, name: &str, position: Duration) -> PlaybackEvent {
    PlaybackEvent::Loaded {
        session_rev,
        media: media(name),
        metadata: MediaMetadata::default(),
        capabilities: MediaCapabilities {
            continuity: Continuity::Finite,
            seek: SeekSupport::Native,
        },
        position,
    }
}

fn state_changed(session_rev: u64, state: PlaybackState) -> PlaybackEvent {
    PlaybackEvent::StateChanged { session_rev, state }
}

fn progress(session_rev: u64, name: &str, secs: u64) -> Progress {
    Progress {
        session_rev,
        media: Some(media(name)),
        position: Duration::from_secs(secs),
        quality: PositionQuality::Exact,
    }
}

fn submitted(action: Action) -> (PersistedState, Urgency) {
    match action {
        Action::Submit { state, urgency } => (state, urgency),
        Action::None => panic!("expected a submission"),
    }
}

fn is_none(action: &Action) -> bool {
    matches!(action, Action::None)
}

/// A session already playing `a`, with the clock parked at the moment playback
/// started. Returns the session and the clock that drives it.
fn playing(name: &str) -> (Session, FakeClock) {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let _ = session.observe(&loaded(1, name, Duration::ZERO), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());
    (session, clock)
}

#[test]
fn five_seconds_of_playback_becomes_an_ordinary_submission() {
    let (mut session, clock) = playing("a");

    clock.advance(Duration::from_millis(4_900));
    assert!(
        is_none(&session.tick(&progress(1, "a", 4), clock.sample())),
        "4.9 s is not yet due"
    );

    clock.advance(Duration::from_millis(100));
    let (state, urgency) = submitted(session.tick(&progress(1, "a", 5), clock.sample()));
    assert_eq!(urgency, Urgency::Ordinary);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(5),
        "the position is the tick's own sample"
    );
}

#[test]
fn the_interval_restarts_after_each_capture() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 5), clock.sample()));

    clock.advance(Duration::from_secs(4));
    assert!(is_none(&session.tick(&progress(1, "a", 9), clock.sample())));
    clock.advance(Duration::from_secs(1));
    let _ = submitted(session.tick(&progress(1, "a", 10), clock.sample()));
}

#[test]
fn a_wall_clock_that_jumps_backwards_does_not_disturb_the_interval() {
    let (mut session, clock) = playing("a");
    clock.advance_monotonic(Duration::from_secs(5));
    // An hour backwards on the wall, mid-interval.
    clock.set_wall(time::OffsetDateTime::UNIX_EPOCH - Duration::from_secs(3600));

    let (state, _) = submitted(session.tick(&progress(1, "a", 5), clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(5),
        "deadlines read the monotonic hand; only updated_at reads the wall"
    );
}

#[test]
fn no_ordinary_capture_happens_while_paused() {
    let (mut session, clock) = playing("a");
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    // Whatever the pause itself is worth, it is worth it once. Task 7 makes
    // this first tick resolve a forced checkpoint; the claim here is only that
    // nothing keeps firing behind it.
    let _ = session.tick(&progress(1, "a", 5), clock.sample());

    clock.advance(Duration::from_secs(30));
    assert!(is_none(&session.tick(&progress(1, "a", 5), clock.sample())));
}

#[test]
fn a_sample_from_a_session_the_policy_is_not_tracking_is_ignored() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(10));
    assert!(
        is_none(&session.tick(&progress(7, "a", 10), clock.sample())),
        "a stale revision must not move a checkpoint"
    );
}

#[test]
fn a_revision_is_adopted_from_an_event_the_policy_otherwise_ignores() {
    // §7: a DeviceRecovered can be dropped when the backlog is full, so the
    // revision must be adopted from every event, not only the acted-on ones.
    let (mut session, clock) = playing("a");
    let _ = session.observe(
        &PlaybackEvent::Warning {
            session_rev: 9,
            message: "a device warning".into(),
        },
        clock.sample(),
    );
    clock.advance(Duration::from_secs(5));
    let (state, _) = submitted(session.tick(&progress(9, "a", 5), clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(5)
    );
}

#[test]
fn a_volume_change_submits_at_ordinary_urgency_and_touches_no_checkpoint() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(2));
    let _ = session.tick(&progress(1, "a", 2), clock.sample());

    let (state, urgency) = submitted(session.observe(
        &PlaybackEvent::VolumeChanged {
            session_rev: 1,
            volume: Volume::new(0.25),
        },
        clock.sample(),
    ));
    assert_eq!(urgency, Urgency::Ordinary);
    assert_eq!(state.volume(), Volume::new(0.25));
    assert!(
        state.entry_for(&media("a")).is_none(),
        "volume is not a position"
    );
}

#[test]
fn end_of_track_records_the_events_own_position_and_marks_completion() {
    let (mut session, clock) = playing("a");
    let (state, urgency) = submitted(session.observe(
        &PlaybackEvent::EndOfTrack {
            session_rev: 1,
            position: Duration::from_secs(240),
        },
        clock.sample(),
    ));
    assert_eq!(urgency, Urgency::Forced);
    assert!(state.completed_for(&media("a")));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(240),
        "D1 retains the position"
    );
}

#[test]
fn a_media_switch_produces_one_snapshot_carrying_both_halves() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));

    let (state, urgency) =
        submitted(session.observe(&loaded(2, "b", Duration::ZERO), clock.sample()));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(93),
        "the outgoing entry comes from last_sample; load() has already overwritten the engine's position"
    );
    assert_eq!(
        state.current_media,
        Some(media("b")),
        "and the move of current_media is the same mutation"
    );
}

#[test]
fn reloading_the_same_media_submits_nothing() {
    let (mut session, clock) = playing("a");
    assert!(is_none(&session.observe(
        &loaded(2, "a", Duration::from_secs(30)),
        clock.sample()
    )));
}

#[test]
fn playing_clears_a_completed_flag_carried_in_from_the_file() {
    let mut opening = PersistedState::default();
    opening.record(
        &continuo::playback::checkpoint::PlaybackCheckpoint {
            media: media("a"),
            position: Duration::from_secs(240),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        true,
    );

    let clock = FakeClock::new();
    let mut session = Session::new(opening);
    let _ = session.observe(&loaded(1, "a", Duration::ZERO), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());

    clock.advance(Duration::from_secs(5));
    let (state, _) = submitted(session.tick(&progress(1, "a", 5), clock.sample()));
    assert!(
        !state.completed_for(&media("a")),
        "§12: a successful establishment after a completed state clears it"
    );
}
