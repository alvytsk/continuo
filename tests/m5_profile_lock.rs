use continuo::lifecycle::lock::{LockError, ProfileLock};

#[test]
fn a_second_acquisition_is_contended_until_the_first_is_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("continuo").join("state.json");
    let first = ProfileLock::acquire(&state).expect("first");
    assert!(first.path().ends_with("state.lock"));
    assert!(matches!(
        ProfileLock::acquire(&state),
        Err(LockError::Contended)
    ));
    drop(first);
    let again = ProfileLock::acquire(&state).expect("released on drop");
    assert!(again.path().exists(), "the lock file is never unlinked");
    assert!(
        !state.exists(),
        "acquiring never creates or reads state.json"
    );
}

#[test]
fn a_failed_initialization_after_acquisition_releases_the_profile() {
    fn start_then_fail(state: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        let _lock = ProfileLock::acquire(state)?;
        Err("initialization failed after locking".into())
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("state.json");
    assert!(start_then_fail(&state).is_err());
    assert!(ProfileLock::acquire(&state).is_ok());
}

#[test]
fn the_contention_message_is_exact() {
    assert_eq!(
        LockError::Contended.to_string(),
        "Another Continuo player is using this state profile"
    );
}

#[cfg(unix)]
#[test]
fn a_new_lock_file_is_private_and_an_existing_one_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;

    let mode = |path: &std::path::Path| {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("fresh").join("state.json");
    let lock = ProfileLock::acquire(&state).expect("lock");
    assert_eq!(mode(lock.path()), 0o600);
    drop(lock);

    let existing = dir.path().join("existing");
    std::fs::create_dir(&existing).expect("dir");
    let path = existing.join("state.lock");
    std::fs::write(&path, b"").expect("lock file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let lock = ProfileLock::acquire(&existing.join("state.json")).expect("lock");
    assert_eq!(mode(lock.path()), 0o644, "left as it was");
}
