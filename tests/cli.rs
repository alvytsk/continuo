#![cfg(target_os = "linux")]

#[path = "support/process.rs"]
mod process;

#[test]
fn bare_invocation_prints_help_and_reports_filter_errors() {
    // A bare invocation now prints help and exits nonzero rather than running
    // the old M0 startup message: `continuo` requires the `play` subcommand.
    let profile = process::Profile::new().unwrap();
    let bare = profile
        .command()
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
    let profile = process::Profile::new().unwrap();
    let failure = profile
        .command()
        .env("RUST_LOG", "continuo=not-a-level")
        .output()
        .unwrap();
    assert!(!failure.status.success());
    let stderr = String::from_utf8_lossy(&failure.stderr);
    assert!(stderr.contains("continuo: invalid tracing filter"));
    assert!(stderr.contains("application startup failed"));
    assert!(stderr.contains("error parsing level filter"));
}

#[test]
fn a_bare_invocation_is_a_usage_error_with_status_two() -> Result<(), Box<dyn std::error::Error>> {
    let profile = process::Profile::new()?;
    let output = profile.command().output()?;
    assert_eq!(output.status.code(), Some(2), "§4: usage errors exit 2");
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("play"));
    Ok(())
}

#[test]
fn help_is_a_successful_entry_point_on_stdout() -> Result<(), Box<dyn std::error::Error>> {
    let profile = process::Profile::new()?;
    let output = profile.command().arg("--help").output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("play"));
    Ok(())
}
