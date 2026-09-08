use std::process::Command;

#[test]
fn bare_invocation_prints_help_and_reports_filter_errors() {
    // A bare invocation now prints help and exits nonzero rather than running
    // the old M0 startup message: `continuo` requires the `play` subcommand.
    let bare = Command::new(env!("CARGO_BIN_EXE_continuo"))
        .env("RUST_LOG", "continuo=info")
        .output()
        .unwrap();
    assert!(!bare.status.success());
    assert!(bare.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&bare.stderr);
    assert!(
        stderr.contains("play"),
        "help must mention the play subcommand: {stderr}"
    );

    // The RUST_LOG filter-error path predates the CLI and still applies: it
    // fires during `telemetry::init`, before argument parsing runs.
    let failure = Command::new(env!("CARGO_BIN_EXE_continuo"))
        .env("RUST_LOG", "continuo=not-a-level")
        .output()
        .unwrap();
    assert!(!failure.status.success());
    let stderr = String::from_utf8_lossy(&failure.stderr);
    assert!(stderr.contains("continuo: invalid tracing filter"));
    assert!(stderr.contains("application startup failed"));
    assert!(stderr.contains("error parsing level filter"));
}
