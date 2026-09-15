//! Task 17 (design doc M5 §11): per-session TUI log files, retention of the
//! five most recent prior logs, and (Unix) fd 2 redirection.

use continuo::lifecycle::stderr::{KEPT_PRIOR_LOGS, log_dir, open_session_log, retain_recent_logs};
use time::OffsetDateTime;

#[test]
fn retention_keeps_the_five_most_recent_prior_logs_and_ignores_other_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    for day in 1..=8 {
        std::fs::write(
            dir.path()
                .join(format!("continuo-tui-2026090{day}T000000Z-1.log")),
            b"x",
        )
        .expect("seed");
    }
    std::fs::write(dir.path().join("notes.txt"), b"keep").expect("seed");
    let removed = retain_recent_logs(dir.path(), KEPT_PRIOR_LOGS).expect("retain");
    assert_eq!(removed.len(), 3);
    let mut left: Vec<_> = std::fs::read_dir(dir.path())
        .expect("dir")
        .map(|e| e.expect("entry").file_name().into_string().expect("utf8"))
        .collect();
    left.sort();
    assert_eq!(left.len(), 6);
    assert!(left.contains(&"notes.txt".to_string()));
    assert!(left.contains(&"continuo-tui-20260908T000000Z-1.log".to_string()));
    assert!(!left.contains(&"continuo-tui-20260903T000000Z-1.log".to_string()));
}

#[test]
fn each_session_gets_a_new_unique_log_under_the_profile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("continuo").join("state.json");
    // 2026-09-14T12:00:00Z. `time`'s `macros` feature is not enabled, so this
    // is built from a verified Unix timestamp rather than `datetime!`.
    let wall = OffsetDateTime::from_unix_timestamp(1_789_387_200).expect("timestamp");
    let (_a, first) = open_session_log(&state, wall).expect("first");
    let (_b, second) = open_session_log(&state, wall).expect("second");
    assert_ne!(first, second);
    assert!(first.starts_with(log_dir(&state)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&first)
                .expect("meta")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
