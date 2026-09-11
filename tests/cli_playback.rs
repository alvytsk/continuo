mod support;

use std::process::Command;
use std::time::Duration;

#[allow(clippy::unwrap_used)] // Fallible spawn of a fixed test binary.
fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_continuo"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn no_arguments_prints_help_and_exits_nonzero() {
    let output = run(&[]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("play"),
        "help must mention the play subcommand: {text}"
    );
}

#[test]
fn an_absent_file_exits_nonzero_with_a_concise_message() {
    let output = run(&["play", "/nonexistent/definitely-not-here.flac"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("continuo:"),
        "expected a prefixed message: {text}"
    );
    assert!(
        text.contains("definitely-not-here.flac"),
        "expected the path: {text}"
    );
}

#[test]
fn a_directory_is_rejected_as_not_a_regular_file() {
    let output = run(&["play", env!("CARGO_MANIFEST_DIR")]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("regular file"),
        "expected the regular-file rule: {text}"
    );
}

#[test]
fn a_relative_path_is_accepted_and_canonicalized() {
    // Canonicalization happens at the worker's source-opening boundary, so a
    // relative path must not be rejected by argument parsing.
    let output = Command::new(env!("CARGO_BIN_EXE_continuo"))
        .args(["play", "tests/fixtures/sine.flac", "--probe-only"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("44100"),
        "expected the negotiated rate: {text}"
    );
}

#[test]
fn a_stalled_remote_open_is_quittable_before_anything_loads() {
    // §5: "remains able to stop or quit". With no tty this exercises the
    // no-raw-mode path, where the loop still runs and still shuts down
    // cleanly when the open deadline fires; with a tty it is one keypress.
    let server = support::server::TestServer::start(
        support::server::Script::serving(b"x".to_vec()).stall_headers(),
    );
    let started = std::time::Instant::now();
    let output = run(&["play", &server.url("/audio.mp3")]);
    assert!(!output.status.success());
    assert!(
        started.elapsed() < Duration::from_secs(45),
        "the process hung on a stalled open: {:?}",
        started.elapsed()
    );
    server.shutdown();
}

#[test]
fn a_rejected_file_still_reports_without_a_device_or_a_terminal() {
    // R5/G4. Preparation moved to the worker; this is the M1 property that
    // move must not cost. CI has neither an audio device nor a controlling
    // terminal, so a passing run here is the evidence.
    let output = run(&["play", env!("CARGO_MANIFEST_DIR")]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("regular file"),
        "expected the regular-file rule: {text}"
    );
}
