//! The one way a test launches `continuo` (M5 §12). Every child gets its own
//! state, data, cache and config directories, so no test can read, lock or
//! write the developer's playback profile. Keep the `Profile` alive until the
//! child has exited: dropping it deletes the directories under the child.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

#[allow(clippy::assertions_on_constants)]
pub fn binary() -> &'static str {
    assert!(
        cfg!(target_os = "linux"),
        "subprocess profile isolation is verified only on Linux"
    );
    env!("CARGO_BIN_EXE_continuo")
}

/// A command for `continuo` whose profile lives under `root`, laid out the
/// way the M4 CLI tests already seed it: `root/{state,data,cache,config}`.
pub fn command_in(root: &Path) -> Command {
    let mut command = Command::new(binary());
    command
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env_remove("CONTINUO_TEST_HOOK")
        .env_remove("CONTINUO_AUDIO_OUTPUT");
    command
}

pub struct Profile {
    root: tempfile::TempDir,
}

impl Profile {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            root: tempfile::tempdir()?,
        })
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// Where `StateStore::platform_path` resolves on Linux under this profile.
    pub fn state_dir(&self) -> PathBuf {
        self.root().join("state").join("continuo")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir().join("state.json")
    }

    pub fn command(&self) -> Command {
        command_in(self.root())
    }
}
