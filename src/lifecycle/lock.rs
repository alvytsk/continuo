//! The exclusive profile lock: two `continuo` players on one state profile
//! would overwrite each other's listening history, so `play` takes an
//! OS-level lock on a sibling `state.lock` file before it ever opens
//! `state.json` (design doc M5 §6).

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use crate::persistence::PersistenceError;
use crate::persistence::atomic::prepare_directory;

/// Held for the lifetime of a `play` session. Dropping it releases the OS
/// lock; the lock file itself is never unlinked, so a later `acquire` on the
/// same profile simply reopens and relocks it.
pub struct ProfileLock {
    /// Kept alive only so the open file description — and the OS lock tied
    /// to it — outlives the guard; dropping this is what releases the lock.
    /// Never read.
    #[allow(dead_code)]
    file: std::fs::File,
    path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("Another Continuo player is using this state profile")]
    Contended,
    #[error("no platform state directory is available")]
    NoStateDirectory,
    #[error("cannot prepare the state directory")]
    Directory(#[source] PersistenceError),
    #[error("cannot {op} {path:?}")]
    Io {
        path: PathBuf,
        op: &'static str,
        #[source]
        source: std::io::Error,
    },
}

impl ProfileLock {
    /// Acquires the exclusive lock for the profile whose durable state lives
    /// at `state_file` (i.e. the path `state.json` would occupy). Resolves
    /// `state_file`'s parent directory, prepares it under the existing
    /// private-directory policy, then opens and locks the sibling
    /// `state.lock` — never `state.json` itself, which this neither creates
    /// nor reads.
    pub fn acquire(state_file: &Path) -> Result<Self, LockError> {
        let dir = state_file.parent().unwrap_or_else(|| Path::new("."));
        prepare_directory(dir).map_err(LockError::Directory)?;

        let path = dir.join("state.lock");
        let file = lock_options().open(&path).map_err(|source| LockError::Io {
            path: path.clone(),
            op: "open",
            source,
        })?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => LockError::Contended,
            std::fs::TryLockError::Error(source) => LockError::Io {
                path: path.clone(),
                op: "lock",
                source,
            },
        })?;

        Ok(Self { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Opens the lock file without truncating it, creating it private to the
/// user like the profile's other files. The mode applies only on creation:
/// an existing lock file keeps whatever permissions it already has.
fn lock_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}
