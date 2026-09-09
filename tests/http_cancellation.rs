//! §12's acceptance evidence for H9: every wait a source read can be parked
//! in wakes on retirement, and a stale generation's late-arriving data can
//! never repopulate the one that superseded it. Header waits and empty-
//! buffer reads are already exercised at engine level by
//! `engine_remote.rs` (a stop waking a stalled header wait during a seek;
//! a shutdown waking a stalled body read) and at the channel/fetch unit
//! level by `tests/http_channel.rs` and `tests/http_fetch.rs`; what those do
//! not cover is a plain `Stop` (not a shutdown) waking a body read blocked
//! on an *empty* buffer, a full-buffer *producer* wait, and a stale
//! generation's data actually being let back onto the wire and proven
//! harmless. Every server is `127.0.0.1:<ephemeral>`; every engine runs
//! over `TestOutput`. Each case below proves its target wait was entered
//! before cancelling it.

mod support;

use std::time::{Duration, Instant};

use continuo::playback::command::Admission;
use continuo::playback::event::PlaybackEvent;
use continuo::playback::state::PlaybackState;

use support::TestEngine;
use support::server::{Script, TestServer};

/// A WAV body built by repeating `sine-5s.wav`'s own PCM payload, with the
/// RIFF and `data` chunk sizes patched to match — the same construction
/// `http_playback.rs`'s H13 test uses, duplicated locally rather than
/// shared, matching how every acceptance file here keeps its own rig.
fn large_wav_body(repeats: usize) -> Vec<u8> {
    let source = std::fs::read(support::fixture_path("sine-5s.wav"))
        .unwrap_or_else(|error| panic!("the fixture must exist: {error}"));
    let marker = source
        .windows(4)
        .position(|w| w == b"data")
        .unwrap_or_else(|| panic!("sine-5s.wav has no data chunk"));
    let payload_start = marker + 8;
    let prefix = &source[..payload_start];
    let payload = &source[payload_start..];

    let mut body = Vec::with_capacity(prefix.len() + payload.len() * repeats);
    body.extend_from_slice(prefix);
    for _ in 0..repeats {
        body.extend_from_slice(payload);
    }

    let data_len = u32::try_from(payload.len() * repeats)
        .unwrap_or_else(|error| panic!("synthesized WAV body too large: {error}"));
    body[marker + 4..marker + 8].copy_from_slice(&data_len.to_le_bytes());
    let riff_len = u32::try_from(body.len() - 8)
        .unwrap_or_else(|error| panic!("synthesized WAV body too large: {error}"));
    body[4..8].copy_from_slice(&riff_len.to_le_bytes());
    body
}

/// Blocks until `server` has recorded at least one request, or `patience`
/// elapses — the same proof `engine_remote.rs` uses that a header-stalled
/// wait was genuinely entered before cancelling it.
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
fn every_wait_wakes_and_stale_responses_cannot_repopulate() {
    // Case 1: a plain `Stop` — not a shutdown — wakes a body read blocked on
    // an empty buffer. `engine_remote.rs`'s own stalled-read tests only ever
    // release the gate or quit through it; nothing there stops mid-stall
    // and keeps running.
    {
        let server =
            TestServer::start(Script::from_fixture("sine-5s.flac").stall_body_after(32 << 10));
        let mut engine = TestEngine::start_idle();
        engine.load_remote(&server.url("/audio.flac"));
        assert_eq!(engine.handle().submit_play(), Admission::Accepted);
        engine.await_state(PlaybackState::Playing);
        engine.play_for(Duration::from_millis(100));
        assert!(
            server.wait_until_stalled(Duration::from_secs(5)),
            "case 1: the read never blocked, so there was no wait to cancel"
        );

        engine.handle().submit_stop();
        // `await_state`'s own bounded deadline is the proof: a worker still
        // blocked on the network could never answer this.
        engine.await_state(PlaybackState::Stopped);
        // And the connection was woken by cancellation, not by data: nothing
        // released it, so the gate still finds it parked.
        assert!(
            server.release(),
            "case 1: the connection was not still parked; something else must have unblocked it"
        );

        engine.finish();
        server.shutdown();
    }

    // Case 2: a full buffer's producer wait. A body far larger than the
    // byte channel's real capacity, from a server that never stalls, fills
    // it until the producer's own push blocks — observed the same way
    // H13's occupancy test observes it, from the server's write counter
    // plateauing — and `Stop` reaches that wait too.
    {
        let body = large_wav_body(20);
        let server = TestServer::start(Script::serving(body));
        let mut engine = TestEngine::start_idle();
        engine.load_remote(&server.url("/audio.wav"));

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut last = server.bytes_written();
        let mut stable_since = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(20));
            let now = server.bytes_written();
            if now != last {
                last = now;
                stable_since = Instant::now();
            }
            if stable_since.elapsed() >= Duration::from_millis(500) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "case 2: bytes_written never settled, so the producer never actually blocked"
            );
        }

        engine.handle().submit_stop();
        engine.await_state(PlaybackState::Stopped);

        engine.finish();
        server.shutdown();
    }

    // Case 3: a stale generation cannot repopulate the one that superseded
    // it. `engine_remote.rs`'s `a_seek_retired_by_a_stop_reports_cancelled_
    // and_commits_no_target` proves the cancellation itself; this extends
    // it by actually releasing the retired connection's stalled headers —
    // letting its data onto the wire for real — and then proving a fresh
    // operation afterward is unaffected.
    {
        let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
        let mut engine = TestEngine::start_idle();
        engine.load_remote(&server.url("/audio.flac"));
        assert_eq!(engine.handle().submit_play(), Admission::Accepted);
        engine.await_state(PlaybackState::Playing);
        engine.play_for(Duration::from_millis(100));
        let before = engine.progress().position;
        let port = server.port();
        server.shutdown();

        let stalling =
            TestServer::start_on(port, Script::from_fixture("sine-5s.flac").stall_headers());
        assert_eq!(
            engine.handle().submit_seek(Duration::from_secs(3)),
            Admission::Accepted
        );
        assert!(
            wait_for_request(&stalling, Duration::from_secs(5)),
            "case 3: the seek's request never reached the server"
        );

        engine.handle().submit_stop();
        engine.await_state(PlaybackState::Stopped);
        let cancelled = engine.await_event(|e| matches!(e, PlaybackEvent::SeekCancelled { .. }));
        assert!(matches!(cancelled, PlaybackEvent::SeekCancelled { .. }));
        assert_eq!(
            engine.progress().position,
            before,
            "case 3: the retired seek moved the position before its stale response even arrived"
        );

        // Now let the stale generation's headers actually arrive, late.
        assert!(
            stalling.release(),
            "case 3: the stale connection was not parked where it should have been"
        );
        // Nothing here reads from `engine` for a moment, so this proves the
        // late arrival landed on a retired generation that discards it
        // rather than one this test happened not to look at yet.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            engine.progress().position,
            before,
            "case 3: the stale generation's late response repopulated the position"
        );
        assert_eq!(engine.state(), PlaybackState::Stopped);
        stalling.shutdown();

        // A fresh, healthy seek on the same URL must still work cleanly —
        // proof the stale generation's late arrival left nothing behind for
        // a real one to stumble over.
        let healed = TestServer::start_on(port, Script::from_fixture("sine-5s.flac"));
        assert_eq!(
            engine.handle().submit_seek(Duration::from_secs(2)),
            Admission::Accepted
        );
        let stored = engine.await_event(|e| matches!(e, PlaybackEvent::SeekTargetStored { .. }));
        assert!(matches!(stored, PlaybackEvent::SeekTargetStored { .. }));
        assert_eq!(engine.handle().submit_play(), Admission::Accepted);
        engine.await_state(PlaybackState::Playing);
        let landed = engine.await_seek_completed(Duration::from_secs(10));
        assert!(
            landed >= Duration::from_secs(1),
            "case 3: the fresh seek after the stale release landed short at {landed:?}"
        );

        engine.finish();
        healed.shutdown();
    }
}
