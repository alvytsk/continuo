//! The file on disk: read it, classify it, replace it atomically.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use time::OffsetDateTime;

use crate::clock::Clock;

use super::PersistenceError;
use super::model::{PersistedState, SCHEMA_VERSION};

/// How many `-2`, `-3`, … candidates a quarantine will try before giving up.
pub const MAX_QUARANTINE_CANDIDATES: u32 = 100;

/// The oldest schema version `load` still knows how to bring forward to
/// [`SCHEMA_VERSION`] (design doc §4.5). A version below this, like one
/// above [`SCHEMA_VERSION`], is genuinely unreadable rather than migratable:
/// `UnsupportedVersion` and a preserved file either way.
const OLDEST_MIGRATABLE_VERSION: u32 = 1;

/// Only the field every future version is obliged to keep. Read before the
/// model, so that a valid newer file is never misclassified as garbage (D3).
#[derive(Deserialize)]
struct VersionEnvelope {
    schema_version: u32,
}

#[derive(Debug)]
pub enum LoadReason {
    Loaded,
    Missing,
    /// The file was garbage and has been moved aside; writing continues.
    Quarantined {
        moved_to: PathBuf,
    },
    /// The file was garbage and could not be moved aside; writing is disabled
    /// so that the next checkpoint does not overwrite what §6 requires be kept.
    QuarantineFailed,
    /// A version this build does not support; preserved in place.
    UnsupportedVersion {
        found: u32,
    },
    /// Present but unreadable. Preserved in place for the same reason.
    Unreadable,
}

pub struct LoadOutcome {
    pub state: PersistedState,
    pub writable: bool,
    pub reason: LoadReason,
}

pub struct StateStore {
    path: PathBuf,
    clock: Arc<dyn Clock>,
    temp_seq: AtomicU64,
}

impl StateStore {
    pub fn new(path: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self {
            path,
            clock,
            temp_seq: AtomicU64::new(0),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The platform state path. Only `app` calls this, which is what keeps the
    /// tests off `$HOME` (§13).
    pub fn platform_path() -> Result<PathBuf, PersistenceError> {
        let dirs = directories::ProjectDirs::from("", "", "continuo")
            .ok_or(PersistenceError::NoStateDirectory)?;
        // `state_dir` honors XDG_STATE_HOME on Linux and is None elsewhere.
        let base = dirs
            .state_dir()
            .unwrap_or_else(|| dirs.data_local_dir())
            .to_path_buf();
        Ok(base.join("state.json"))
    }

    pub fn load(&self) -> LoadOutcome {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Self::fresh(true, LoadReason::Missing);
            }
            Err(error) => {
                tracing::warn!(
                    path = ?self.path,
                    %error,
                    "cannot read the state file; leaving it in place and not writing this session"
                );
                return Self::fresh(false, LoadReason::Unreadable);
            }
        };

        let envelope = match serde_json::from_slice::<VersionEnvelope>(&bytes) {
            Ok(envelope) => envelope,
            Err(error) => {
                tracing::warn!(path = ?self.path, %error, "the state file has no readable version");
                return self.reject_malformed();
            }
        };
        // Not `> SCHEMA_VERSION` alone: a file from a build that renumbered
        // downward is just as unreadable as one from a build ahead of this
        // one. `OLDEST_MIGRATABLE_VERSION` widens the accepted band by
        // exactly the versions this build knows how to bring forward — today
        // only v1 — everything else, above or below, is rejected the same
        // way it always was.
        if envelope.schema_version > SCHEMA_VERSION
            || envelope.schema_version < OLDEST_MIGRATABLE_VERSION
        {
            tracing::warn!(
                path = ?self.path,
                found = envelope.schema_version,
                supported = SCHEMA_VERSION,
                "unsupported state schema; preserving the file and not writing this session"
            );
            return Self::fresh(
                false,
                LoadReason::UnsupportedVersion {
                    found: envelope.schema_version,
                },
            );
        }

        match serde_json::from_slice::<PersistedState>(&bytes) {
            Ok(mut state) => {
                // A v1 file's shape already deserialises cleanly into the
                // current `PersistedState` (§4.5): `position` lands in
                // `Some`, and the absent `estimated` defaults to `None`. What
                // is missing is the label — without this, the next write
                // would serialise that v2-shaped data back out under a v1
                // envelope, which the next v1 build would read and quietly
                // discard.
                if envelope.schema_version != SCHEMA_VERSION {
                    tracing::info!(
                        path = ?self.path,
                        from = envelope.schema_version,
                        to = SCHEMA_VERSION,
                        "migrating the state file to the current schema"
                    );
                    state.migrate_to_current_schema();
                }
                LoadOutcome {
                    state,
                    writable: true,
                    reason: LoadReason::Loaded,
                }
            }
            Err(error) => {
                tracing::warn!(
                    path = ?self.path,
                    found = envelope.schema_version,
                    %error,
                    "the state file's version is supported but its shape is unreadable"
                );
                self.reject_malformed()
            }
        }
    }

    pub fn write(&self, state: &PersistedState) -> Result<(), PersistenceError> {
        // The version the file claims is this build's, asserted here rather than
        // taken from the snapshot on trust: a file stamped with a version this
        // build cannot read would be quarantined or preserved by its own next
        // load (D3). Only a defect inside this crate could get one here — the
        // field is private to the model and writing is already disabled for
        // every version but this one — so it is an assertion, not a repair, and
        // it stays out of the release path where D11 forbids a panic.
        debug_assert_eq!(
            state.schema_version(),
            SCHEMA_VERSION,
            "only this build's schema version is ever written"
        );
        let dir = self.parent();
        self.prepare_directory(dir)?;

        let bytes =
            serde_json::to_vec_pretty(state).map_err(|source| PersistenceError::Serialize {
                path: self.path.clone(),
                source,
            })?;

        let seq = self.temp_seq.fetch_add(1, Ordering::Relaxed);
        let temp = dir.join(format!("state.json.tmp-{}-{seq}", std::process::id()));

        // The create-time mode already keeps the file private the instant it
        // exists; the explicit `set_permissions` right after is what makes the
        // 0600 an assertion the code makes rather than a side effect the
        // create call happened to have (D12).
        let write_temp = || -> Result<(), io::Error> {
            let mut file = private_file(&temp)?;
            set_private(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()
        };
        if let Err(source) = write_temp() {
            let _ = fs::remove_file(&temp);
            return Err(PersistenceError::Io {
                path: temp,
                op: "write",
                source,
            });
        }

        if let Err(source) = fs::rename(&temp, &self.path) {
            let _ = fs::remove_file(&temp);
            return Err(PersistenceError::Io {
                path: self.path.clone(),
                op: "replace",
                source,
            });
        }

        // Not fatal: the replacement already happened, and a parent that cannot
        // be fsynced is a durability gap, not a lost checkpoint.
        if let Ok(handle) = File::open(dir)
            && let Err(error) = handle.sync_all()
        {
            tracing::debug!(path = ?dir, %error, "cannot fsync the state directory");
        }
        Ok(())
    }

    fn parent(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }

    fn fresh(writable: bool, reason: LoadReason) -> LoadOutcome {
        LoadOutcome {
            state: PersistedState::default(),
            writable,
            reason,
        }
    }

    fn reject_malformed(&self) -> LoadOutcome {
        match self.quarantine() {
            Some(moved_to) => {
                tracing::warn!(path = ?self.path, ?moved_to, "state file quarantined");
                Self::fresh(true, LoadReason::Quarantined { moved_to })
            }
            None => {
                tracing::warn!(
                    path = ?self.path,
                    "cannot quarantine the state file; not writing this session"
                );
                Self::fresh(false, LoadReason::QuarantineFailed)
            }
        }
    }

    /// Move the file aside under a timestamped name, never over one that
    /// already exists. Single-user, single-process by design (§17), so the
    /// exists-then-rename window is not a hazard worth more machinery.
    fn quarantine(&self) -> Option<PathBuf> {
        let stamp = stamp(self.clock.sample().wall);
        let dir = self.parent();
        for suffix in 1..=MAX_QUARANTINE_CANDIDATES {
            let name = if suffix == 1 {
                format!("state.json.rejected-{stamp}")
            } else {
                format!("state.json.rejected-{stamp}-{suffix}")
            };
            let candidate = dir.join(name);
            if candidate.exists() {
                continue;
            }
            if fs::rename(&self.path, &candidate).is_ok() {
                return Some(candidate);
            }
            return None;
        }
        None
    }

    #[cfg(unix)]
    fn prepare_directory(&self, dir: &Path) -> Result<(), PersistenceError> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        if !dir.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|source| PersistenceError::Io {
                    path: dir.to_path_buf(),
                    op: "create directory for",
                    source,
                })?;
            // Deliberately falling through to the check below rather than
            // returning here: `mkdir`'s mode is masked by the process umask, so
            // a directory this build just created is the one case an early
            // return would leave unverified. D12's 0700 is asserted for every
            // path through this function.
        }

        // D12 says the mode is set explicitly, and a create-time mode reaches
        // exactly the case that never needs it. A directory left at 0755 by an
        // earlier build, a restore, or a hand-made `mkdir` is the one that does.
        let metadata = match fs::metadata(dir) {
            Ok(metadata) => metadata,
            Err(error) => {
                // Not fatal (D11): whatever is wrong with the directory will
                // surface again, more informatively, at the write that follows.
                tracing::warn!(
                    path = ?dir,
                    %error,
                    "cannot stat the state directory to check its permissions"
                );
                return Ok(());
            }
        };
        if metadata.permissions().mode() & 0o777 != 0o700
            && let Err(error) = fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        {
            // Not fatal, and deliberately not a refusal to write: the listening
            // history lives in the 0600 file, and a directory this process
            // cannot chmod leaks a filename at worst. Losing the checkpoint
            // over it would be the larger harm.
            tracing::warn!(path = ?dir, %error, "cannot tighten the state directory to 0700");
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn prepare_directory(&self, dir: &Path) -> Result<(), PersistenceError> {
        // Platform defaults: a documented gap (§17, D12).
        fs::create_dir_all(dir).map_err(|source| PersistenceError::Io {
            path: dir.to_path_buf(),
            op: "create directory for",
            source,
        })
    }
}

#[cfg(unix)]
fn private_file(path: &Path) -> Result<File, io::Error> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> Result<File, io::Error> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// D12 requires the destination's mode to be set explicitly rather than left
/// to whatever the create call happened to produce. On Unix, `set_permissions`
/// re-asserts the 0600 the create already applied, so there is no window
/// where the file is briefly more open than this.
#[cfg(unix)]
fn set_private(path: &Path) -> Result<(), io::Error> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<(), io::Error> {
    Ok(())
}

/// `20260908T143211Z` — filesystem-safe, no colons (§13).
fn stamp(at: OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
    )
}
