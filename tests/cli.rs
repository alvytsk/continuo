use std::process::Command;

#[test]
fn startup_is_minimal_and_reports_filter_errors() {
    let success = Command::new(env!("CARGO_BIN_EXE_continuo"))
        .env("RUST_LOG", "continuo=info")
        .output()
        .unwrap();
    assert!(success.status.success());
    assert!(success.stdout.is_empty());
    assert!(String::from_utf8_lossy(&success.stderr).contains("Continuo foundation initialized"));
    let failure = Command::new(env!("CARGO_BIN_EXE_continuo"))
        .env("RUST_LOG", "continuo=not-a-level")
        .output()
        .unwrap();
    assert!(!failure.status.success());
    let stderr = String::from_utf8_lossy(&failure.stderr);
    assert!(stderr.contains("continuo: invalid tracing filter"));
    assert!(stderr.contains("application startup failed"));
}
