//! The file on disk: read it, classify it, replace it atomically.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use time::OffsetDateTime;

use crate::clock::Clock;

use super::PersistenceError;
use super::atomic::replace_bytes;
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
}

impl StateStore {
    pub fn new(path: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self { path, clock }
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

        let bytes =
            serde_json::to_vec_pretty(state).map_err(|source| PersistenceError::Serialize {
                path: self.path.clone(),
                source,
            })?;

        replace_bytes(&self.path, &bytes)
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
