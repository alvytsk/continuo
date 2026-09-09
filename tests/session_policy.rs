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
    // Whatever the pause itself is worth, it is worth it once: this first tick
    // resolves the pause's forced checkpoint, and the claim here is only that
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

// --------------------------------------------------- pending forces (D13)

#[test]
fn a_pause_from_playing_is_resolved_by_the_same_iterations_tick() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(2));

    // The application drains events first...
    assert!(is_none(&session.observe(
        &state_changed(1, PlaybackState::Paused),
        clock.sample()
    )));
    // ...then samples once, and that sample is newer than the transition.
    let (state, urgency) = submitted(session.tick(&progress(1, "a", 2), clock.sample()));

    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(2)
    );
}

#[test]
fn a_pause_that_interrupts_no_playback_raises_nothing() {
    // Every launch emits a StateChanged{Paused} nobody asked for, before the
    // queued Play is dispatched. Ungated, that would checkpoint the resume
    // landing on every launch — and zero it for a completed entry.
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let _ = session.observe(&loaded(1, "a", Duration::ZERO), clock.sample());

    assert!(is_none(&session.observe(
        &state_changed(1, PlaybackState::Paused),
        clock.sample()
    )));
    assert!(is_none(&session.tick(&progress(1, "a", 0), clock.sample())));
}

#[test]
fn a_stop_raises_a_force_that_the_tick_resolves() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(2));
    assert!(is_none(&session.observe(
        &state_changed(2, PlaybackState::Stopped),
        clock.sample()
    )));

    let (state, urgency) = submitted(session.tick(&progress(2, "a", 93), clock.sample()));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(93)
    );
}

#[test]
fn a_seek_persists_the_canonical_position_never_the_events_actual() {
    let (mut session, clock) = playing("a");
    let seek = PlaybackEvent::SeekCompleted {
        session_rev: 1,
        requested: Duration::from_secs(60),
        // A landing the M1 debt entry says can disagree with the position.
        actual: Duration::from_secs(59),
        refinement_truncated: false,
    };
    assert!(is_none(&session.observe(&seek, clock.sample())));

    let (state, urgency) = submitted(session.tick(&progress(1, "a", 60), clock.sample()));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(60),
        "D6 sidesteps the debt by never persisting SeekCompleted.actual"
    );
}

#[test]
fn a_force_is_rekeyed_across_a_device_recovery_and_still_resolves() {
    let (mut session, clock) = playing("a");
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    // rebuild bumps the revision, with the position continuous across it.
    let _ = session.observe(
        &PlaybackEvent::DeviceRecovered { session_rev: 2 },
        clock.sample(),
    );

    let (state, urgency) = submitted(session.tick(&progress(2, "a", 40), clock.sample()));
    assert_eq!(
        urgency,
        Urgency::Forced,
        "a real pause must not be lost to an unrelated fault"
    );
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(40)
    );
}

#[test]
fn a_load_retires_a_force_raised_against_the_previous_media() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());

    // The load replaces the media; its own handling already recorded `a`.
    let (state, _) = submitted(session.observe(&loaded(2, "b", Duration::ZERO), clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(93)
    );

    assert!(
        is_none(&session.tick(&progress(2, "b", 1), clock.sample())),
        "the retired force must not fire against the new media"
    );
}

// --------------------------------------------- the outstanding target (D17)

/// play → 93 s → stop → seek to 30 s → quit. The sequence D7 exists for.
#[test]
fn a_stopped_seek_target_survives_the_shutdown_force() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.tick(&progress(2, "a", 93), clock.sample());

    let (state, urgency) = submitted(session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    ));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(30)
    );

    // The engine's canonical position still reads the pre-seek value, because a
    // stopped seek deliberately does not move it.
    let final_state = session.shutdown_snapshot(&progress(2, "a", 93), clock.sample());
    assert_eq!(
        final_state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(30),
        "the target supersedes Progress.position until the engine resolves it"
    );
}

/// stop → seek to 30 s → the same iteration's tick. Both the stop's force and
/// the target are outstanding, and the target is what the checkpoint uses.
#[test]
fn a_force_that_resolves_under_an_outstanding_target_records_the_target() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    );

    // The stop raised its force before the seek stored anything, and the seek
    // re-keyed rather than retired it.
    let (state, urgency) = submitted(session.tick(&progress(2, "a", 93), clock.sample()));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(30),
        "the pre-seek sample the engine still reports must not win over the target"
    );
}

/// stop → seek to 30 s → Home → play → quit. `restart()` discards the target
/// and announces Playing with no SeekCompleted, so Playing has to clear it.
#[test]
fn a_restart_clears_the_target_it_discarded() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.tick(&progress(2, "a", 93), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    );

    // Home: restart() seeks to zero, clears its own target and announces
    // Playing. No SeekCompleted is emitted for it, ever.
    let _ = session.observe(&state_changed(2, PlaybackState::Playing), clock.sample());
    clock.advance(Duration::from_secs(5));
    let (state, _) = submitted(session.tick(&progress(2, "a", 5), clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(5),
        "an ordinary checkpoint after a restart uses the sample, not the discarded target"
    );

    let final_state = session.shutdown_snapshot(&progress(2, "a", 7), clock.sample());
    assert_eq!(
        final_state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(7)
    );
}

#[test]
fn a_resumed_stopped_seek_clears_the_target_through_its_seek_completed() {
    let (mut session, clock) = playing("a");
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    );
    // `restore()` validates the stored target and emits the SeekCompleted the
    // caller has been waiting for.
    let _ = session.observe(
        &PlaybackEvent::SeekCompleted {
            session_rev: 2,
            requested: Duration::from_secs(30),
            actual: Duration::from_secs(30),
            refinement_truncated: false,
        },
        clock.sample(),
    );
    let (state, _) = submitted(session.tick(&progress(2, "a", 31), clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(31)
    );
}

/// A stopped seek is the same listener intent as a completed one, only earlier:
/// §12 has `SeekCompleted` clear completion, and a target persisted alongside
/// `completed: true` would be discarded by the resume decision that reads it.
#[test]
fn a_stopped_seek_clears_the_completion_its_target_supersedes() {
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
    let (state, urgency) = submitted(session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 1,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    ));

    assert_eq!(urgency, Urgency::Forced);
    let entry = state.entry_for(&media("a")).unwrap();
    assert_eq!(entry.position, Duration::from_secs(30));
    assert!(
        !entry.completed,
        "the listener asked for 30 s; a retained completion would throw the target away"
    );

    let final_state = session.shutdown_snapshot(&progress(1, "a", 0), clock.sample());
    let entry = final_state.entry_for(&media("a")).unwrap();
    assert_eq!(entry.position, Duration::from_secs(30));
    assert!(!entry.completed);
}

// ------------------------------------------- the establishment gate (D20)

#[test]
fn a_launch_that_never_establishes_writes_no_checkpoint() {
    // A completed entry: §11 resumes it at 0, load() emits Loaded before
    // opening the device, and the device refuses to open.
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
    let _ = session.observe(
        &PlaybackEvent::Failed {
            session_rev: 1,
            message: "cannot open the audio device".into(),
        },
        clock.sample(),
    );
    let _ = session.tick(&progress(1, "a", 0), clock.sample());

    let final_state = session.shutdown_snapshot(&progress(1, "a", 0), clock.sample());
    assert_eq!(
        final_state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(240),
        "a failed establishment must not overwrite the position D1 retains"
    );
    assert!(final_state.completed_for(&media("a")));
}

/// §11 maps `position == duration` to a start of zero while retaining the
/// position, with `completed` false — so R16's completed guard is not what
/// covers this one. `Loaded` reports that zero before the device is opened, and
/// a switch would otherwise carry it out as the outgoing media's final word.
#[test]
fn a_switch_away_from_a_media_that_never_established_records_nothing_for_it() {
    let mut opening = PersistedState::default();
    opening.record(
        &continuo::playback::checkpoint::PlaybackCheckpoint {
            media: media("a"),
            position: Duration::from_secs(300),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        false,
    );

    let clock = FakeClock::new();
    let mut session = Session::new(opening);
    let _ = session.observe(&loaded(1, "a", Duration::ZERO), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::Failed {
            session_rev: 1,
            message: "cannot open the audio device".into(),
        },
        clock.sample(),
    );

    // The listener gives up on `a` and picks another track.
    let (state, _) = submitted(session.observe(&loaded(2, "b", Duration::ZERO), clock.sample()));
    let entry = state.entry_for(&media("a")).unwrap();
    assert_eq!(
        entry.position,
        Duration::from_secs(300),
        "nothing validated the zero the load reported, so the retained position stands"
    );
    assert!(!entry.completed);
}

/// A switch onto a completed entry, in an iteration where the engine's
/// `StateChanged{Loading}` never arrived — `emit` drops non-terminal events at
/// the backlog cap, which is exactly the pressure a switch is most likely
/// under. The session therefore sees `Loaded` while it still believes playback
/// is running, and the ordinary capture that follows would be a `Progress`
/// position for a media nothing has established.
#[test]
fn a_switch_onto_a_completed_entry_does_not_capture_its_unvalidated_zero() {
    let mut opening = PersistedState::default();
    opening.record(
        &continuo::playback::checkpoint::PlaybackCheckpoint {
            media: media("b"),
            position: Duration::from_secs(240),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        true,
    );

    let clock = FakeClock::new();
    let mut session = Session::new(opening);
    let _ = session.observe(&loaded(1, "a", Duration::ZERO), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));

    // The switch, with no StateChanged in front of it.
    let _ = submitted(session.observe(&loaded(2, "b", Duration::ZERO), clock.sample()));
    assert!(
        is_none(&session.tick(&progress(2, "b", 0), clock.sample())),
        "nothing has established `b`, so the sample the engine reports for it is not a checkpoint"
    );

    let final_state = session.shutdown_snapshot(&progress(2, "b", 0), clock.sample());
    let entry = final_state.entry_for(&media("b")).unwrap();
    assert_eq!(
        entry.position,
        Duration::from_secs(240),
        "§11 resumes a completed entry at zero while retaining the position D1 keeps"
    );
    assert!(entry.completed);
}

/// The same failure as the shutdown force, one trigger earlier: §11 resumes a
/// completed entry at zero, `load()` reports `Loaded` before it opens the
/// device, and `do_stop` runs from the launch pause. The stop's force would
/// otherwise write that unvalidated zero over the position D1 retains.
#[test]
fn a_stop_before_anything_establishes_writes_no_checkpoint() {
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
    // The launch pause nobody asked for, then a stop before the queued Play is
    // dispatched.
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Stopped), clock.sample());

    assert!(
        is_none(&session.tick(&progress(1, "a", 0), clock.sample())),
        "the force is answered by recording nothing, not by writing a position nothing validated"
    );

    let final_state = session.shutdown_snapshot(&progress(1, "a", 0), clock.sample());
    let entry = final_state.entry_for(&media("a")).unwrap();
    assert_eq!(entry.position, Duration::from_secs(240));
    assert!(entry.completed);
}

#[test]
fn a_launch_that_never_establishes_still_writes_volume_and_current_media() {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let _ = session.observe(&loaded(1, "a", Duration::ZERO), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::VolumeChanged {
            session_rev: 1,
            volume: Volume::new(0.25),
        },
        clock.sample(),
    );

    let final_state = session.shutdown_snapshot(&progress(1, "a", 0), clock.sample());
    assert_eq!(final_state.volume(), Volume::new(0.25));
    assert_eq!(final_state.current_media, Some(media("a")));
    assert!(
        final_state.entry_for(&media("a")).is_none(),
        "neither of those is a position claim"
    );
}

#[test]
fn a_second_load_cannot_inherit_the_first_ones_establishment() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));

    // A new media that fails to open after Loaded.
    let _ = session.observe(&loaded(2, "b", Duration::ZERO), clock.sample());
    let final_state = session.shutdown_snapshot(&progress(2, "b", 0), clock.sample());
    assert!(final_state.entry_for(&media("b")).is_none());
    assert_eq!(
        final_state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(93),
        "and the outgoing entry the load recorded stands"
    );
}

/// The gate asks whether anything established, not whether it established under
/// the revision now current: `rebuild` bumps the revision on device recovery
/// with the position continuous across it.
#[test]
fn a_device_recovery_does_not_re_gate_an_established_session() {
    let (mut session, clock) = playing("a");
    let _ = session.observe(
        &PlaybackEvent::DeviceRecovered { session_rev: 2 },
        clock.sample(),
    );

    let final_state = session.shutdown_snapshot(&progress(2, "a", 40), clock.sample());
    assert_eq!(
        final_state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(40),
        "a revision bump the position is continuous across must not close the gate again"
    );
}

#[test]
fn a_media_switch_carries_the_outgoing_stopped_seek_target_out_with_it() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.tick(&progress(2, "a", 93), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    );

    let (state, _) = submitted(session.observe(&loaded(3, "b", Duration::ZERO), clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(30),
        "the outgoing entry is recorded from the effective position, not the pre-seek sample"
    );
}

#[test]
fn a_media_switch_does_not_walk_a_completed_entry_backwards() {
    let (mut session, clock) = playing("a");
    let _ = submitted(session.observe(
        &PlaybackEvent::EndOfTrack {
            session_rev: 1,
            position: Duration::from_secs(240),
        },
        clock.sample(),
    ));
    // A tick after the end can only report a position at or behind the one
    // EndOfTrack already recorded.
    let _ = session.tick(&progress(1, "a", 239), clock.sample());

    let (state, _) = submitted(session.observe(&loaded(2, "b", Duration::ZERO), clock.sample()));
    let entry = state.entry_for(&media("a")).unwrap();
    assert_eq!(
        entry.position,
        Duration::from_secs(240),
        "D1 retains what it retained"
    );
    assert!(entry.completed, "and the switch does not clear it either");
}

#[test]
fn the_shutdown_snapshot_refuses_a_position_from_a_session_it_was_not_tracking() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));

    // A final Progress carrying a revision the policy never learned.
    let final_state = session.shutdown_snapshot(&progress(99, "a", 5), clock.sample());
    assert_eq!(
        final_state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(93),
        "it falls back to last_sample rather than trusting the stranger"
    );
}
