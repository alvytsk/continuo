//! Durable playback state: the model, the store that owns the file, and the
//! writer thread that owns the disk.

pub mod model;

use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("cannot {op} state file {path:?}")]
    Io {
        path: PathBuf,
        op: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("cannot serialize state for {path:?}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("cannot deserialize state from {path:?}")]
    Deserialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("state file {path:?} is schema version {found}, and this build supports {supported}")]
    UnsupportedVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    #[error("no platform state directory is available")]
    NoStateDirectory,
}
