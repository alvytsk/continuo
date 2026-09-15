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
