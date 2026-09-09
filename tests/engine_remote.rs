//! Acceptance evidence for Task 10: HTTP sources reaching the decode worker
//! itself, through a real `EngineHandle` over `TestOutput`, against the
//! loopback server. Every test binds `127.0.0.1`; no test touches the public
//! network.

mod support;

use std::time::{Duration, Instant};

use continuo::media::capabilities::SeekSupport;
use continuo::playback::command::Admission;
use continuo::playback::event::PlaybackEvent;
use continuo::playback::state::PlaybackState;

use support::server::{Script, TestServer};
use support::{TestEngine, fixture_path};

/// Blocks until `server` has recorded at least one request, or `patience`
/// elapses. The proof that a header-stalled wait was actually entered: the
/// request line and headers are parsed and recorded before the server ever
/// parks on the stall gate, so a nonempty list means the worker's read is now
/// genuinely blocked on that connection.
fn wait_for_request(server: &TestServer, patience: Duration) -> bool {
    let deadline = Instant::now() + patience;
    loop {
        if !server.requests().is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn remote_playback_reaches_the_test_output_before_the_body_completes() {
    // H1: the whole point of finite HTTP media is that playback starts
    // before the transfer finishes. `sine-5s.flac` never has to arrive in
    // full for 200 ms of it to have been heard.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(200));

    assert!(
        engine.progress().position >= Duration::from_millis(150),
        "playback did not reach the test output: {:?}",
        engine.progress().position
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_forward_seek_installs_the_media_position_and_requests_that_byte() {
    // H2: a seek's landing must be both a local fact (the engine's own
    // position) and a remote one (the byte range the server actually saw).
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));

    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(3)),
        Admission::Accepted
    );
    let landed = engine.await_seek_completed(Duration::from_secs(10));
    assert!(
        landed >= Duration::from_secs(2),
        "the seek landed short at {landed:?}"
    );
    assert!(
        engine.progress().position >= Duration::from_secs(2),
        "the engine's own position was not installed at the seek's landing"
    );

    let ranged = server
        .requests()
        .into_iter()
        .rev()
        .find_map(|request| request.range());
    let Some((first, _)) = ranged else {
        panic!("no ranged request for the seek ever reached the server");
    };
    assert!(
        first > 0,
        "the seek's request did not target a nonzero byte offset: {first}"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn stop_closes_the_fetch_and_play_reopens_at_the_preserved_position() {
    // H3: stop preserves the logical position exactly as it does for a local
    // file, even though the decoder over a remote source is dropped rather
    // than merely parked - and the reopen `play` performs lands back there.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(200));
    let preserved = engine.progress().position;

    engine.handle().submit_stop();
    engine.await_state(PlaybackState::Stopped);
    assert_eq!(
        engine.progress().position,
        preserved,
        "stop must not reset the preserved position"
    );

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    assert!(
        engine.progress().position >= preserved,
        "the reopen did not resume at the preserved position: {:?} < {preserved:?}",
        engine.progress().position
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_range_less_server_plays_sequentially_and_refuses_every_seek() {
    // H5: a server with no range support is still finite media - it plays -
    // but every seek attempt against it is refused rather than pretending to
    // land somewhere it never actually reached.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").without_ranges());
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));

    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(2)),
        Admission::Accepted
    );
    let rejection = engine.await_event(|e| matches!(e, PlaybackEvent::SeekRejected { .. }));
    let PlaybackEvent::SeekRejected { reason, .. } = rejection else {
        unreachable!("await_event's predicate already matched SeekRejected")
    };
    assert!(
        reason.to_lowercase().contains("seek"),
        "unexpected rejection reason: {reason:?}"
    );
    // Playback itself is undisturbed by the refused seek.
    assert_eq!(engine.state(), PlaybackState::Playing);

    engine.finish();
    server.shutdown();
}

#[test]
fn an_unsupported_stopped_seek_emits_no_seek_target_stored() {
    // H14: a target M2 would treat as durable must never be stored for a
    // source that has already demonstrated it cannot honour one - the
    // capability gate has to run before `SeekTargetStored`, not after.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").without_ranges());
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    engine.handle().submit_stop();
    engine.await_state(PlaybackState::Stopped);

    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(2)),
        Admission::Accepted
    );
    let rejection = engine.await_event(|e| matches!(e, PlaybackEvent::SeekRejected { .. }));
    assert!(matches!(rejection, PlaybackEvent::SeekRejected { .. }));
    assert_eq!(
        engine.count_events(|e| matches!(e, PlaybackEvent::SeekTargetStored { .. })),
        0,
        "an unsupported source stored a seek target it can never honour"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_retired_by_a_stop_reports_cancelled_and_commits_no_target() {
    // §8: an accepted seek always receives an outcome. A stop that retires
    // one in flight must report it as `SeekCancelled`, never as a silent
    // drop and never as `SeekRejected` (which would misreport a
    // cancellation as a validation failure), and the position it preserves
    // must be exactly what playback had already reached.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    let before = engine.progress().position;
    let port = server.port();
    server.shutdown();

    // A second server on the same port - the URL, and so the `MediaId`,
    // never changes - but every request from here on stalls before it is
    // ever answered, which is what lets this test prove the seek's wait was
    // entered before it interrupts.
    let stalling = TestServer::start_on(port, Script::from_fixture("sine-5s.flac").stall_headers());

    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(3)),
        Admission::Accepted
    );
    assert!(
        wait_for_request(&stalling, Duration::from_secs(5)),
        "the seek's request never reached the server"
    );

    engine.handle().submit_stop();
    engine.await_state(PlaybackState::Stopped);

    let cancelled = engine.await_event(|e| matches!(e, PlaybackEvent::SeekCancelled { .. }));
    let PlaybackEvent::SeekCancelled { requested, .. } = cancelled else {
        unreachable!("await_event's predicate already matched SeekCancelled")
    };
    assert_eq!(requested, Duration::from_secs(3));
    assert_eq!(
        engine.count_events(|e| matches!(e, PlaybackEvent::SeekCompleted { .. })),
        0,
        "a cancelled seek must never also report completion"
    );
    assert_eq!(
        engine.progress().position,
        before,
        "the cancelled seek moved the preserved position"
    );

    engine.finish();
    stalling.shutdown();
}

#[test]
fn pause_during_a_stalled_read_freezes_output_and_resume_continues() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").stall_body_after(32 << 10));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    assert!(
        server.wait_until_stalled(Duration::from_secs(5)),
        "the read never blocked"
    );

    // The read is pending inside the decoder. Pause must reach it without
    // returning a destructive error to the demuxer.
    assert_eq!(engine.handle().submit_pause(), Admission::Accepted);
    engine.await_state(PlaybackState::Paused);
    // The frames already handed to the device still play out after the
    // park, so let that settle before taking the reading that must not
    // move (matches `pause_holds_the_position_still_and_resume_continues_
    // from_it` in engine_contract.rs).
    engine.let_time_pass(Duration::from_millis(300));
    let frozen = engine.position();
    engine.let_time_pass(Duration::from_millis(200));
    assert_eq!(
        engine.position(),
        frozen,
        "output kept running while paused"
    );

    assert!(server.release());
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    // `play_for`'s argument is an absolute target position, not a relative
    // step - `frozen` is already past 100ms from the play span before the
    // pause, so the target has to be stated relative to it or this call is
    // a no-op that proves nothing.
    engine.play_for(frozen + Duration::from_millis(200));
    assert!(
        engine.progress().position > frozen,
        "playback did not continue"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn quit_while_paused_wakes_every_source_wait() {
    // H10's other half: a shutdown must reach a read blocked inside the byte
    // channel, not only one waiting on the command queue or the tick.
    // `finish` blocks on the worker's real thread join with no timeout of
    // its own, so returning here at all is the proof that it woke.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").stall_body_after(32 << 10));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    assert!(
        server.wait_until_stalled(Duration::from_secs(5)),
        "the read never blocked"
    );

    assert_eq!(engine.handle().submit_pause(), Admission::Accepted);
    engine.await_state(PlaybackState::Paused);

    engine.finish();
    server.shutdown();
}

#[test]
fn a_truncated_tail_cannot_become_end_of_track() {
    // H8/H9: a body that ends early must fail the attempt, never drain to a
    // clean `EndOfTrack` that would mark the episode complete on less audio
    // than was actually recorded.
    let server =
        TestServer::start(Script::from_fixture("sine-5s.flac").truncate_body_after(16 << 10));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.play_until_terminal(Duration::from_secs(10));

    assert_eq!(
        engine.state(),
        PlaybackState::Failed,
        "a truncated body was reported as {:?}",
        engine.state()
    );
    assert!(
        !engine.saw_end_of_track(),
        "EndOfTrack was emitted for a body that never fully arrived"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_capability_change_carries_the_current_session_rev() {
    // H14: any demonstrated seek promotes `Unknown` to `Native`, not only
    // one taken through the stopped-seek verification path - and the event
    // that announces it must carry the session revision current at the
    // moment it fires, not the one current when the source was opened.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    let session_rev = engine.progress().session_rev;

    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(2)),
        Admission::Accepted
    );
    engine.await_seek_completed(Duration::from_secs(10));

    let event = engine.await_event(|e| matches!(e, PlaybackEvent::CapabilitiesChanged { .. }));
    let PlaybackEvent::CapabilitiesChanged {
        session_rev: event_rev,
        capabilities,
    } = event
    else {
        unreachable!("await_event's predicate already matched CapabilitiesChanged")
    };
    assert_eq!(
        event_rev, session_rev,
        "a capability change must carry the session's current revision"
    );
    assert_eq!(
        capabilities.seek,
        SeekSupport::Native,
        "a successful seek must promote Unknown to Native"
    );

    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_after_a_stop_reopens_rather_than_reporting_nothing_is_loaded() {
    // #7: a stopped seek's `source.is_none()` guard must not reject a seek
    // on a source whose identity, descriptor and position the worker is
    // still holding - it has to reopen first.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));

    engine.handle().submit_stop();
    engine.await_state(PlaybackState::Stopped);

    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(3)),
        Admission::Accepted
    );
    let event = engine.await_event(|e| {
        matches!(
            e,
            PlaybackEvent::SeekTargetStored { .. } | PlaybackEvent::SeekRejected { .. }
        )
    });
    match event {
        PlaybackEvent::SeekTargetStored { target, .. } => {
            assert_eq!(target, Duration::from_secs(3));
        }
        PlaybackEvent::SeekRejected { reason, .. } => {
            panic!("a seek after a stop was rejected as {reason:?} instead of reopening")
        }
        _ => unreachable!("await_event's predicate matched neither expected event"),
    }

    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_after_a_retired_seek_reopens_and_lands() {
    // #7's other caller: after a stop retires a seek in flight - dropping
    // the decoder along with it (§8) - the very next seek must still reopen
    // and, once played, land where it asked, not fail on a source that is
    // very much loaded.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(100));
    let port = server.port();
    server.shutdown();

    let stalling = TestServer::start_on(port, Script::from_fixture("sine-5s.flac").stall_headers());
    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(3)),
        Admission::Accepted
    );
    assert!(
        wait_for_request(&stalling, Duration::from_secs(5)),
        "the seek's request never reached the server"
    );
    engine.handle().submit_stop();
    engine.await_state(PlaybackState::Stopped);
    let _ = engine.await_event(|e| matches!(e, PlaybackEvent::SeekCancelled { .. }));
    stalling.shutdown();

    // A healthy server, same port, so the retired seek's fetch left nothing
    // behind that a healthy fetch could stumble over.
    let healed = TestServer::start_on(port, Script::from_fixture("sine-5s.flac"));

    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(2)),
        Admission::Accepted
    );
    let stored = engine.await_event(|e| matches!(e, PlaybackEvent::SeekTargetStored { .. }));
    let PlaybackEvent::SeekTargetStored { target, .. } = stored else {
        unreachable!("await_event's predicate already matched SeekTargetStored")
    };
    assert_eq!(target, Duration::from_secs(2));

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    let landed = engine.await_seek_completed(Duration::from_secs(10));
    assert!(
        landed >= Duration::from_secs(1),
        "the reopened seek landed short at {landed:?}"
    );

    engine.finish();
    healed.shutdown();
}

#[test]
fn corrupt_audio_over_a_complete_body_fails_and_never_ends() {
    // H8: "malformed audio cannot become successful completion". The
    // transfer is perfect - full Content-Length, clean EOF, valid ETag -
    // and the bytes are garbage. Keying the failure on a `RemoteFailure`
    // alone lets this drain to `EndOfTrack` and mark the episode complete,
    // destroying the checkpoint.
    let mut body = match std::fs::read(fixture_path("sine-5s.flac")) {
        Ok(body) => body,
        Err(error) => panic!("the fixture must exist: {error}"),
    };
    // Corrupt from the middle to the end, leaving the header intact so it
    // still opens. A small corrupted window is not reliable here: FLAC's
    // frame sync search treats a handful of garbage bytes as noise to skip
    // past rather than a fault, and decoding drains cleanly through it to
    // `EndOfTrack` - exactly the outcome H8 forbids. Corrupting through to
    // the end removes every remaining valid frame sync, which is what
    // actually forces a decode error rather than a lucky resync.
    let middle = body.len() / 2;
    for byte in &mut body[middle..] {
        *byte = 0xFF;
    }
    let server = TestServer::start(Script::serving(body));

    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.play_until_terminal(Duration::from_secs(10));

    assert_eq!(
        engine.state(),
        PlaybackState::Failed,
        "corrupt audio was reported as {:?}",
        engine.state()
    );
    assert!(
        !engine.saw_end_of_track(),
        "EndOfTrack was emitted for a recording that never decoded through"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn play_after_a_remote_failure_reopens_once_at_the_preserved_position() {
    // §9's last transition. The first attempt dies mid-body; the position
    // the listener actually reached survives, and one explicit Play - not
    // an automatic retry - brings it back there.
    let server =
        TestServer::start(Script::from_fixture("sine-5s.flac").truncate_body_after(16 << 10));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(40));
    // Drive the clock further, tolerating a mid-way failure: `play_for`
    // assumes success and panics if the position stalls, but the ring the
    // priming read already staged means nothing further is even attempted
    // from the network until draining opens room again - `let_time_pass`
    // does not care whether the engine is still progressing, so it is what
    // gives the worker's own pump loop the chance to reach the truncation.
    engine.let_time_pass(Duration::from_secs(2));
    engine.await_state(PlaybackState::Failed);
    let heard = engine.progress().position;
    assert!(
        heard > Duration::ZERO,
        "the failure was reported before any audio was heard"
    );
    let port = server.port();
    server.shutdown();

    // A healthy server for the retry, at the same URL.
    let healed = TestServer::start_on(port, Script::from_fixture("sine-5s.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    assert!(
        engine.progress().position >= heard,
        "the reopen restarted from zero: {:?} < {heard:?}",
        engine.progress().position
    );
    engine.finish();
    healed.shutdown();
}

#[test]
fn play_after_a_remote_failure_on_an_unseekable_source_fails_honestly() {
    // "fail honestly if restoration is unavailable" - never a silent
    // restart from zero, which is the reset the invariant forbids.
    let server = TestServer::start(
        Script::from_fixture("sine-5s.flac")
            .without_ranges()
            .truncate_body_after(16 << 10),
    );
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(40));
    // See the sibling test's comment: `let_time_pass` drives the clock
    // without assuming the engine keeps progressing, which is what gives
    // the worker's pump loop room to reach the truncation.
    engine.let_time_pass(Duration::from_secs(2));
    engine.await_state(PlaybackState::Failed);
    let heard = engine.progress().position;
    assert!(
        heard > Duration::ZERO,
        "the failure was reported before any audio was heard"
    );

    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Stopped);
    assert_eq!(
        engine.progress().position,
        heard,
        "an unseekable retry moved the position"
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn a_seek_taken_while_paused_completes_rather_than_hanging() {
    // Reopening and seek refinement both need bytes, and both are legitimate
    // while playback is paused. A freeze that gated delivery would hang here
    // forever with no error to report.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/audio.flac"));
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(200));
    assert_eq!(engine.handle().submit_pause(), Admission::Accepted);
    engine.await_state(PlaybackState::Paused);

    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(3)),
        Admission::Accepted
    );
    let landed = engine.await_seek_completed(Duration::from_secs(10));
    assert!(
        landed >= Duration::from_secs(3),
        "landed short at {landed:?}"
    );
    // And it is still paused: a seek does not resume playback.
    assert_eq!(engine.state(), PlaybackState::Paused);

    engine.finish();
    server.shutdown();
}
