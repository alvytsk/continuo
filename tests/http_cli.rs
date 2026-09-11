mod support;

use std::process::Command;

use support::server::{Script, TestServer};

#[allow(clippy::unwrap_used)] // Spawning a fixed test binary.
fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_continuo"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn probe_only_reports_a_remote_recordings_capabilities() {
    // H15: no terminal, no device, no persistence.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let output = run(&["play", &server.url("/audio.flac"), "--probe-only"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("44100"), "{text}");
    assert!(text.contains("Finite"), "{text}");
    // §11 requires the probe to distinguish unresolved capability from
    // unsupported, so `--probe-only` performs the trial seek before printing.
    // A range-capable source therefore reports Native here even though
    // preparation alone stops at Unknown: a probe that printed Unknown would
    // be reporting its own incuriosity as a property of the recording.
    assert!(text.contains("Native"), "{text}");
    server.shutdown();
}

#[test]
fn probe_only_on_a_range_less_server_reports_no_seek_support() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").without_ranges());
    let output = run(&["play", &server.url("/audio.flac"), "--probe-only"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Finite"), "{text}");
    assert!(text.contains("Unsupported"), "{text}");
    server.shutdown();
}

#[test]
fn a_live_stream_is_refused_legibly_and_not_played() {
    // `TestServer` always receives a `Range` header (`HttpMediaSource::open`
    // sends one unconditionally), so a script that still advertises ranges is
    // answered 206 - and only the 200 path emits the icy headers `is_live`
    // looks for (see `tests/prepare.rs`'s `an_explicit_live_source_is_refused_
    // as_live`, which hits this exact seam). `.without_ranges()` is what
    // makes this actually exercise a live response.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").live().without_ranges());
    let output = run(&["play", &server.url("/audio.flac"), "--probe-only"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("live"), "{text}");
    server.shutdown();
}

#[test]
fn a_malformed_url_is_reported_as_a_url_not_as_a_missing_file() {
    let output = run(&["play", "https://", "--probe-only"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("URL"), "expected a URL diagnostic: {text}");
    assert!(!text.contains("No such file"), "{text}");
}

#[test]
fn a_url_with_credentials_is_refused_rather_than_used() {
    // §5: no implicit credential feature, and the password must not echo.
    let output = run(&[
        "play",
        "https://alice:hunter2@example.com/a.mp3",
        "--probe-only",
    ]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("credentials"), "{text}");
    assert!(!text.contains("hunter2"), "the password leaked: {text}");
}

#[test]
fn a_signed_query_is_redacted_from_diagnostics() {
    // H15's redaction half.
    let output = run(&[
        "play",
        "https://nonexistent.invalid/a.mp3?token=SECRETVALUE",
        "--probe-only",
    ]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        !text.contains("SECRETVALUE"),
        "the signed query leaked: {text}"
    );
    assert!(
        text.contains("nonexistent.invalid"),
        "the diagnostic lost its host: {text}"
    );
}

#[test]
fn a_local_spelling_that_looks_like_a_url_stays_a_path() {
    // §5: `./https:...` is an unambiguous local spelling.
    let output = run(&["play", "./https:not-a-url", "--probe-only"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("https:not-a-url"),
        "expected a path diagnostic: {text}"
    );
    assert!(
        !text.contains("URL"),
        "a local spelling was parsed as a URL: {text}"
    );
}
