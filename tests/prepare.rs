mod support;

use std::sync::Arc;
use std::time::Duration;

use continuo::http::channel::{SourceInterrupt, WaitHook};
use continuo::http::error::RemoteFailure;
use continuo::http::limits::Limits;
use continuo::http::service::HttpService;
use continuo::media::capabilities::{Continuity, SeekSupport};
use continuo::media::source::SourceLocation;
use continuo::playback::error::PlaybackError;
use continuo::playback::prepare::{PrepareContext, prepare};
use support::server::{Script, TestServer};
use url::Url;

struct NoHook;
impl WaitHook for NoHook {
    fn service(&self) {}
}

#[allow(clippy::unwrap_used)] // A URL built from a bound loopback port.
fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

fn context() -> PrepareContext {
    let http = match HttpService::spawn(Limits::default()) {
        Ok(service) => service,
        Err(error) => panic!("the HTTP service must start: {error}"),
    };
    PrepareContext {
        http: Some(http),
        interrupt: SourceInterrupt::new(Limits::default().buffer_bytes),
        hook: Arc::new(NoHook),
        limits: Limits::default(),
    }
}

fn remote(server: &TestServer) -> SourceLocation {
    SourceLocation::Http(url(&server.url("/audio.flac")))
}

#[test]
fn a_range_capable_recording_is_finite_with_seek_support_still_unproven() {
    // §6: "Until conclusive evidence exists, publish Unknown, and verify on
    // demand." Byte-range support is not evidence that *this container* can
    // seek in media time, so preparation stops at Unknown. The promotion to
    // Native is a separate fact, asserted in the next test.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let prepared = match prepare(&remote(&server), &context()) {
        Ok(prepared) => prepared,
        Err(error) => panic!("preparation must succeed: {error}"),
    };
    assert_eq!(prepared.capabilities.continuity, Continuity::Finite);
    assert_eq!(
        prepared.capabilities.seek,
        SeekSupport::Unknown,
        "a byte-seekable source was advertised as media-seekable before anything demonstrated it"
    );
    assert_eq!(prepared.source.sample_rate(), 44_100);
    server.shutdown();
}

#[test]
fn a_trial_seek_promotes_unknown_to_native() {
    // The other half: Unknown is a starting point, not a dead end. This is the
    // transition Task 10's `verify_seek_support` performs and publishes as
    // `CapabilitiesChanged`; here it is exercised directly on the decoder.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut prepared = match prepare(&remote(&server), &context()) {
        Ok(prepared) => prepared,
        Err(error) => panic!("preparation must succeed: {error}"),
    };
    assert_eq!(prepared.capabilities.seek, SeekSupport::Unknown);

    let outcome = prepared
        .source
        .seek_refined(Duration::from_secs(2), None, &mut || false);
    match outcome {
        Ok(landed) => assert!(landed.actual >= Duration::from_secs(2), "{landed:?}"),
        Err(error) => panic!("a range-capable FLAC must seek: {error}"),
    }
    prepared.source.note_demuxer_proven();
    assert_eq!(prepared.source.capabilities().seek, SeekSupport::Native);
    server.shutdown();
}

#[test]
fn a_range_ignoring_recording_is_finite_but_unseekable() {
    // §1.1 R1: no RestartAndDiscard. Sequential playback, seek refused.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").without_ranges());
    let prepared = match prepare(&remote(&server), &context()) {
        Ok(prepared) => prepared,
        Err(error) => panic!("preparation must succeed: {error}"),
    };
    assert_eq!(prepared.capabilities.continuity, Continuity::Finite);
    assert_eq!(prepared.capabilities.seek, SeekSupport::Unsupported);
    assert_ne!(prepared.capabilities.seek, SeekSupport::RestartAndDiscard);
    server.shutdown();
}

#[test]
fn chunked_media_with_decoder_evidence_is_finite() {
    // H12: no Content-Length, but the decoder establishes a recording length.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").chunked());
    let prepared = match prepare(&remote(&server), &context()) {
        Ok(prepared) => prepared,
        Err(error) => panic!("preparation must succeed: {error}"),
    };
    assert_eq!(prepared.capabilities.continuity, Continuity::Finite);
    assert!(prepared.source.metadata().duration.is_some());
    server.shutdown();
}

#[test]
fn an_explicit_live_source_is_refused_as_live() {
    // `TestServer` always receives a `Range` header (`HttpMediaSource::open`
    // sends one unconditionally), so a script that still advertises ranges is
    // answered 206 — and only the 200 path emits the icy headers `is_live`
    // looks for (see `tests/http_source.rs`, which hits this same seam).
    // `.without_ranges()` is what makes this scenario actually exercise a
    // live response, matching how a real icecast origin usually has no range
    // support to begin with.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").live().without_ranges());
    let error = match prepare(&remote(&server), &context()) {
        Err(error) => error,
        Ok(_) => panic!("a live stream must be refused"),
    };
    assert!(
        matches!(
            error,
            PlaybackError::Remote(RemoteFailure::UnsupportedLiveMedia)
        ),
        "{error}"
    );
    server.shutdown();
}

#[test]
#[ignore = "unreachable as specified: sine-5s.flac's STREAMINFO always carries \
    a total-sample count, so `metadata.duration` is `Some(..)` the instant the \
    probe succeeds — regardless of `.unresolved()`, `.chunked()` or \
    `.without_ranges()` — and §6's table (byte_len.is_none() but \
    decoder_duration.is_some() => Finite, the same row `chunked_media_with_\
    decoder_evidence_is_finite` exercises) then makes this Finite, never \
    Unresolved. Verified empirically: evidence.byte_len is None and \
    metadata.duration is Some(5s) either way. `ContinuityUndetermined` needs a \
    fixture whose container does not self-declare a sample count (no such \
    fixture exists in tests/fixtures today), or a design decision on how an \
    open-ended transport should override decoder evidence the way `evidence.\
    live` already does for live media (Ruling 4) — flagged for the controller \
    rather than resolved unilaterally, since neither SourceEvidence (Task 6) \
    nor §6's table as given carries that signal."]
fn an_unresolved_source_is_refused_distinctly_from_a_live_one() {
    // H12, R3. Two different refusals, because they are two different facts.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").unresolved());
    let error = match prepare(&remote(&server), &context()) {
        Err(error) => error,
        Ok(_) => panic!("an unresolved source must be refused"),
    };
    assert!(
        matches!(
            error,
            PlaybackError::Remote(RemoteFailure::ContinuityUndetermined)
        ),
        "an unresolved source must not be reported as live: {error}"
    );
    server.shutdown();
}

#[test]
fn a_local_file_prepares_through_the_same_path_with_no_http_service() {
    // R5: one preparation path. A local session builds no runtime at all.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sine.flac");
    let canonical = match path.canonicalize() {
        Ok(canonical) => canonical,
        Err(error) => panic!("the fixture must exist: {error}"),
    };
    let context = PrepareContext {
        http: None,
        interrupt: SourceInterrupt::new(Limits::default().buffer_bytes),
        hook: Arc::new(NoHook),
        limits: Limits::default(),
    };
    let prepared = match prepare(&SourceLocation::LocalPath(canonical), &context) {
        Ok(prepared) => prepared,
        Err(error) => panic!("a local file must prepare: {error}"),
    };
    assert_eq!(prepared.capabilities.continuity, Continuity::Finite);
    assert_eq!(prepared.capabilities.seek, SeekSupport::Native);
}

#[test]
fn an_http_url_without_a_service_is_refused_rather_than_silently_ignored() {
    let context = PrepareContext {
        http: None,
        interrupt: SourceInterrupt::new(Limits::default().buffer_bytes),
        hook: Arc::new(NoHook),
        limits: Limits::default(),
    };
    let error = match prepare(
        &SourceLocation::Http(url("https://example.com/a.mp3")),
        &context,
    ) {
        Err(error) => error,
        Ok(_) => panic!("an HTTP source needs a service"),
    };
    assert!(
        matches!(
            error,
            PlaybackError::Remote(RemoteFailure::InvalidSource { .. })
        ),
        "{error}"
    );
}

#[test]
fn opening_that_exceeds_the_probe_byte_cap_fails_cancellably() {
    // H17's second half.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut context = context();
    context.limits.probe_bytes = 1024;
    let error = match prepare(&remote(&server), &context) {
        Err(error) => error,
        Ok(_) => panic!("the probe cap was ignored"),
    };
    assert!(
        matches!(
            error,
            PlaybackError::Remote(RemoteFailure::ProbeLimitExceeded { limit: 1024 })
        ),
        "{error}"
    );
    server.shutdown();
}

#[test]
fn a_slow_trickle_cannot_outlast_the_opening_deadline() {
    // Every individual read stays inside `stall`, so a deadline checked only
    // after probing returns never fires and opening runs indefinitely. The
    // deadline has to be inside each wait, not around all of them.
    let server = TestServer::start(
        Script::from_fixture("sine-5s.flac").trickle(1, Duration::from_millis(50)),
    );
    let mut context = context();
    context.limits.open = Duration::from_secs(1);
    context.limits.stall = Duration::from_secs(30);
    let started = std::time::Instant::now();
    let error = match prepare(&remote(&server), &context) {
        Err(error) => error,
        Ok(_) => panic!("a trickling server opened despite the deadline"),
    };
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(
        matches!(
            error,
            PlaybackError::Remote(RemoteFailure::Timeout {
                phase: continuo::http::error::Phase::Open
            })
        ),
        "{error}"
    );
    server.shutdown();
}

#[test]
fn a_probe_that_outlives_its_deadline_is_refused() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").stall_body_after(64));
    let mut context = context();
    context.limits = Limits::brisk();
    let started = std::time::Instant::now();
    let error = match prepare(&remote(&server), &context) {
        Err(error) => error,
        Ok(_) => panic!("a stalled probe must not succeed"),
    };
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(
        matches!(error, PlaybackError::Remote(RemoteFailure::Timeout { .. })),
        "{error}"
    );
    server.shutdown();
}
