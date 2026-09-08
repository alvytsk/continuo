//! Contract tests worded directly from the M0 invariant:
//! "Stop and transport recreation preserve position; restoration, media
//! selection, explicit restart, and successful seeks establish a new position."

use std::time::Duration;

use continuo::playback::command::PlaybackCommand;
use continuo::playback::event::PlaybackEvent;
use continuo::playback::state::PlaybackState;
use continuo::playback::volume::Volume;

mod support;
use support::TestEngine;

#[test]
fn stop_preserves_the_logical_position() {
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(200));
    let before = engine.position();
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    assert_eq!(engine.position(), before, "stop must not reset position");
}

#[test]
fn play_from_stopped_resumes_at_the_preserved_position_without_resetting() {
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(200));
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    let preserved = engine.position();
    engine.send(PlaybackCommand::Play);
    engine.await_state(PlaybackState::Playing);
    // Exact, not `>=`: the clock is frozen across the transition, so any
    // movement at all is a bug rather than playback. A `>=` here would accept
    // the forward jump a mis-ordered generation reset produces.
    assert_eq!(
        engine.position(),
        preserved,
        "resume must continue from the preserved position, not rewind or jump"
    );
}

#[test]
fn pause_holds_the_position_still_and_resume_continues_from_it() {
    // Pause and resume are the same transport generation: the callback keeps
    // its cumulative counter and the timeline keeps its floor. If either were
    // voided, resuming would jump the position by everything played since the
    // generation was installed.
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(200));
    engine.send(PlaybackCommand::Pause);
    engine.await_state(PlaybackState::Paused);
    // The frames already handed to the device still play out after the park,
    // so let that settle before taking the reading that must not move.
    engine.let_time_pass(Duration::from_millis(300));
    let paused_at = engine.position();
    engine.let_time_pass(Duration::from_millis(300));
    assert_eq!(
        engine.position(),
        paused_at,
        "a parked transport must not advance the position"
    );
    engine.send(PlaybackCommand::Play);
    engine.await_state(PlaybackState::Playing);
    assert_eq!(
        engine.position(),
        paused_at,
        "resume must continue from the paused position, not jump or rewind"
    );
    engine.play_for(paused_at + Duration::from_millis(100));
    assert!(
        engine.position() > paused_at,
        "playback must actually run again after resuming"
    );
}

#[test]
fn transport_recreation_preserves_position() {
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(200));
    let before = engine.position();
    engine.force_device_loss();
    engine.await_event(|e| matches!(e, PlaybackEvent::DeviceRecovered { .. }));
    assert_eq!(
        engine.position(),
        before,
        "recreation must preserve the position exactly, not merely not reset it"
    );
}

#[test]
fn a_discard_that_times_out_does_not_rebuild_ahead_of_where_playback_was() {
    // Regression. `reinstall` used to move the anchor onto the freshly seeked
    // target *before* asking the callback to discard the old generation's ring.
    // A discard that timed out dropped into `rebuild`, whose capture then ran
    // against a timeline that had never been reset and still held the previous
    // generation's spans: it read back `new anchor + everything played since
    // the last install`, and the transport came back that far ahead of where
    // the media actually was. Nothing anywhere reported it.
    //
    // Only `Discard` goes unanswered, so the recovery still meets a live device
    // and really does capture - which is the whole point. A wholly dead device
    // takes the rescue path instead and never touches the stale timeline.
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(200));
    let played_to = engine.position();
    engine.stop_answering_discards();
    engine.send(PlaybackCommand::SeekTo(Duration::from_millis(50)));
    engine.await_event(|e| matches!(e, PlaybackEvent::DeviceRecovered { .. }));
    engine.answer_discards_again();
    let landed = engine.position();
    assert!(
        landed <= played_to,
        "a rebuild may fall back to where playback was, but never jump past it; \
         playback was at {played_to:?} and the rebuild landed at {landed:?}"
    );
}

#[test]
fn a_recovery_that_never_completes_fails_once_and_keeps_the_position() {
    // The device stops answering, so every handshake in the recovery runs to
    // its deadline: the capture times out, and so does the install that would
    // have brought the transport back. This is the milestone's invariant on its
    // worst path - the pipeline is gone and cannot be rebuilt, and the logical
    // position still has to survive it.
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(200));
    let before = engine.position();
    engine.silence_the_device();
    engine.force_device_loss();
    engine.await_state(PlaybackState::Failed);
    assert_eq!(
        engine.position(),
        before,
        "a failed recovery must still preserve the position"
    );
    assert_eq!(
        engine.count_events(|e| matches!(e, PlaybackEvent::Failed { .. })),
        1,
        "a failure is reported once, not on every pass of the loop"
    );
}

#[test]
fn a_recovery_cancelled_by_stop_ends_stopped_rather_than_failed() {
    // Regression: a stop arriving during a recovery's re-seek used to come back
    // as a rebuild failure, which turned the stop the user asked for into a
    // spurious `Failed` that `do_stop` then refused to correct.
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(200));
    let before = engine.position();
    engine.silence_the_device();
    engine.force_device_loss();
    engine.await_recovery_capture();
    engine.interrupt_stop();
    engine.await_state(PlaybackState::Stopped);
    assert_eq!(
        engine.count_events(|e| matches!(e, PlaybackEvent::Failed { .. })),
        0,
        "a cancelled recovery is not a failure"
    );
    assert_eq!(
        engine.position(),
        before,
        "a cancelled recovery preserves the position too"
    );
}

#[test]
fn a_successful_seek_establishes_a_new_position() {
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(100));
    engine.send(PlaybackCommand::SeekTo(Duration::from_millis(300)));
    let event = engine.await_event(|e| matches!(e, PlaybackEvent::SeekCompleted { .. }));
    let PlaybackEvent::SeekCompleted { actual, .. } = event else {
        unreachable!()
    };
    assert!(actual.as_millis().abs_diff(300) <= 5);
    // The event alone only shows what the decoder did. The engine must also
    // have adopted it as the position it reports.
    assert!(
        engine.position() >= Duration::from_millis(295),
        "a successful seek must establish the new position, got {:?}",
        engine.position()
    );
}

#[test]
fn a_seek_while_stopped_stores_a_target_and_does_not_claim_completion() {
    // Regression: emitting SeekCompleted only after Play made Restart stall.
    let mut engine = TestEngine::start("sine.flac");
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    engine.send(PlaybackCommand::SeekTo(Duration::from_millis(300)));
    let event = engine.await_event(|e| {
        matches!(
            e,
            PlaybackEvent::SeekTargetStored { .. } | PlaybackEvent::SeekCompleted { .. }
        )
    });
    assert!(matches!(event, PlaybackEvent::SeekTargetStored { .. }));
}

#[test]
fn restart_works_from_stopped_and_from_ended() {
    for setup in [PlaybackState::Stopped, PlaybackState::Ended] {
        let mut engine = TestEngine::start("sine.flac");
        match setup {
            PlaybackState::Stopped => {
                engine.send(PlaybackCommand::Stop);
                engine.await_state(PlaybackState::Stopped);
            }
            _ => engine.play_to_end(),
        }
        engine.send(PlaybackCommand::Restart);
        engine.await_state(PlaybackState::Playing);
        assert!(engine.position() < Duration::from_millis(100));
    }
}

#[test]
fn play_from_ended_does_not_restart_implicitly() {
    let mut engine = TestEngine::start("sine.flac");
    engine.play_to_end();
    engine.send(PlaybackCommand::Play);
    engine.await_event(|e| matches!(e, PlaybackEvent::Warning { .. }));
    assert_eq!(engine.state(), PlaybackState::Ended);
}

#[test]
fn end_of_track_waits_for_the_final_frames_predicted_play_time() {
    let mut engine = TestEngine::start("sine.flac");
    engine.drain_ring_without_advancing_clock();
    assert!(
        engine.try_event().is_none(),
        "EOF must not fire when the ring merely empties"
    );
    engine.advance_past_output_latency();
    engine.await_event(|e| matches!(e, PlaybackEvent::EndOfTrack { .. }));
}

#[test]
fn progress_carries_the_session_revision_it_belongs_to() {
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(100));
    let first = engine.progress();
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    assert_ne!(engine.progress().session_rev, first.session_rev);
}

#[test]
fn stop_travels_out_of_band_past_a_pending_backlog() {
    // Stop travels on the interrupt flag plus the wake channel, so neither a
    // queue of commands nor a backlog of undelivered events can delay it.
    // Note that this does not exercise the admission limit: each TogglePause
    // costs two handshakes, so Stop arrives long before enough events pile up
    // to saturate the channel. The test below covers admission.
    let mut engine = TestEngine::start("sine.flac");
    engine.stop_draining_events();
    for _ in 0..256 {
        engine.send(PlaybackCommand::TogglePause);
    }
    engine.interrupt_stop();
    engine.resume_draining_events();
    engine.await_state(PlaybackState::Stopped);
}

#[test]
fn a_backlog_closes_command_admission_until_it_drains() {
    // The test above cannot reach saturation on its own: each TogglePause
    // costs two handshakes, so Stop arrives long before 64 events pile up.
    // This one uses a command whose whole cost is the event it emits, which
    // is what makes the admission limit observable - the command channel
    // still holds most of the batch because the worker stopped taking from it.
    let mut engine = TestEngine::start("sine.flac");
    engine.stop_draining_events();
    for step in 0..256u32 {
        engine.send(PlaybackCommand::SetVolume(Volume::new(step as f32 / 256.0)));
    }
    std::thread::sleep(Duration::from_millis(300));
    let pending = engine.pending_commands();
    assert!(
        pending > 128,
        "admission must halt while a backlog exists; only {pending} of 256 commands were left unread"
    );
    engine.interrupt_stop();
    engine.resume_draining_events();
    engine.await_state(PlaybackState::Stopped);
}

/// The underrun count an aggregated diagnostic warning reports, or zero if the
/// message is not one.
fn reported_underruns(message: &str) -> usize {
    let words: Vec<&str> = message.split_whitespace().collect();
    words
        .windows(2)
        .find(|pair| pair[1] == "underruns,")
        .and_then(|pair| pair[0].parse().ok())
        .unwrap_or(0)
}

#[test]
fn diagnostics_aggregate_rather_than_accumulating_events() {
    // A bare upper bound on the warning count proves nothing: the event channel
    // holds 64 slots with 8 held back, so no more than 56 warnings can ever be
    // emitted even with coalescing deleted outright. What distinguishes
    // coalescing from its absence is that a handful of events account for every
    // single injected xrun - so assert both halves.
    const XRUNS: usize = 10_000;
    let mut engine = TestEngine::start("sine.flac");
    engine.stop_draining_events();
    engine.inject_xruns(XRUNS);
    engine.resume_draining_events();
    let aggregate = engine.await_event(|e| match e {
        PlaybackEvent::Warning { message, .. } => reported_underruns(message) >= XRUNS,
        _ => false,
    });
    let PlaybackEvent::Warning { message, .. } = &aggregate else {
        panic!("await_event returned {aggregate:?}, which is not a warning");
    };
    assert!(
        reported_underruns(message) >= XRUNS,
        "one warning must account for all {XRUNS} xruns, got {message:?}"
    );
    // `await_event` took the aggregate out of the inbox; the rest stayed.
    let others = engine.count_events(|e| matches!(e, PlaybackEvent::Warning { .. }));
    let total = others + 1;
    assert!(
        total < 10,
        "{XRUNS} xruns must coalesce into a handful of events, got {total} warnings"
    );
}

#[test]
fn a_device_with_more_than_two_channels_is_refused_rather_than_played_wrong() {
    // M1 is scoped to mono and stereo. A 6-channel default is routine on HDMI
    // and PipeWire, and the converter would fill only two of every six slots:
    // playback three times too fast, position inflated by the same factor, and
    // not a word of it anywhere. Refusing is the contract until downmixing
    // lands.
    let message = support::load_failure_on_device("sine.flac", 6);
    assert!(
        message.contains('6') && message.contains("channels"),
        "the refusal must name the negotiated channel count, got {message:?}"
    );
}

#[test]
fn a_disconnected_event_receiver_terminates_the_worker() {
    let engine = TestEngine::start("sine.flac");
    engine.drop_event_receiver();
    assert!(engine.join_within(Duration::from_secs(2)));
}
