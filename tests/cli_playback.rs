use std::process::Command;

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
