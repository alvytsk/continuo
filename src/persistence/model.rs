//! The state file's shape: one checkpoint per media identity (D2), plus the
//! session-wide facts the file carries.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::media::id::MediaId;
use crate::playback::checkpoint::PlaybackCheckpoint;
use crate::playback::volume::Volume;

pub const SCHEMA_VERSION: u32 = 1;

/// Counting the current entry, which is never evictable (D2).
pub const MAX_ENTRIES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PersistedCheckpoint {
    pub position: Duration,
    pub completed: bool,
    pub touch_seq: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// Every field is private, so the paths that can write a stored position are an
/// enumeration the compiler keeps rather than one a reader has to trust: a
/// position reaches the map through [`PersistedState::record`] and nowhere else.
/// The accessors below are the whole surface.
///
/// `next_seq` is derived, never stored (§10). Deserialization goes through
/// [`RawState`] so that deriving it is the only way to build one from a file:
/// a `#[serde(skip)]` field would arrive as `0` and hand every caller a
/// sequence that regresses.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(from = "RawState")]
pub struct PersistedState {
    schema_version: u32,
    current_media: Option<MediaId>,
    volume: f32,
    checkpoints: BTreeMap<MediaId, PersistedCheckpoint>,
    #[serde(skip)]
    next_seq: u64,
}

#[derive(Deserialize)]
struct RawState {
    schema_version: u32,
    #[serde(default)]
    current_media: Option<MediaId>,
    #[serde(default = "full_gain")]
    volume: f32,
    #[serde(default)]
    checkpoints: BTreeMap<MediaId, PersistedCheckpoint>,
}

fn full_gain() -> f32 {
    Volume::FULL.as_gain()
}

impl From<RawState> for PersistedState {
    fn from(raw: RawState) -> Self {
        let next_seq = raw
            .checkpoints
            .values()
            .map(|entry| entry.touch_seq)
            .max()
            .map_or(1, |highest| highest.saturating_add(1));
        Self {
            schema_version: raw.schema_version,
            current_media: raw.current_media,
            volume: raw.volume,
            checkpoints: raw.checkpoints,
            next_seq,
        }
    }
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            current_media: None,
            volume: Volume::FULL.as_gain(),
            checkpoints: BTreeMap::new(),
            next_seq: 1,
        }
    }
}

impl PersistedState {
    /// Read back through `Volume::new`, which already clamps to `[0, 1]` and
    /// maps non-finite input to `0.0` — so a hand-edited file needs no separate
    /// validation rule (D15).
    pub fn volume(&self) -> Volume {
        Volume::new(self.volume)
    }

    pub fn set_volume(&mut self, volume: Volume) {
        self.volume = volume.as_gain();
    }

    /// Read-only: no path outside this module can change the version a snapshot
    /// carries, and the store asserts [`SCHEMA_VERSION`] again before it writes.
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn current_media(&self) -> Option<&MediaId> {
        self.current_media.as_ref()
    }

    /// Named rather than ambient: the policy moves `current_media` in the same
    /// mutation that records the outgoing entry, and a plain field would let a
    /// later edit move it from anywhere.
    pub fn set_current_media(&mut self, media: MediaId) {
        self.current_media = Some(media);
    }

    /// How many media identities the file remembers, which is what the cap
    /// bounds (D2).
    pub fn len(&self) -> usize {
        self.checkpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.checkpoints.is_empty()
    }

    pub fn entry_for(&self, media: &MediaId) -> Option<&PersistedCheckpoint> {
        self.checkpoints.get(media)
    }

    pub fn completed_for(&self, media: &MediaId) -> bool {
        self.checkpoints
            .get(media)
            .is_some_and(|entry| entry.completed)
    }

    /// The only place a `touch_seq` is ever assigned (D4). Loading, reading and
    /// restoring never touch one.
    ///
    /// The cap is a guard, not a hope (D2). If a fresh entry arrives at
    /// `MAX_ENTRIES` and eviction cannot find a victim — unreachable while
    /// `MAX_ENTRIES > 1`, since the current entry is the only protected one and
    /// the incoming media is by definition not yet in the map — the incoming
    /// checkpoint is dropped rather than let the map grow past the cap, and the
    /// fact is logged instead of swallowed.
    pub fn record(&mut self, checkpoint: &PlaybackCheckpoint, completed: bool) {
        let fresh = !self.checkpoints.contains_key(&checkpoint.media);
        if fresh && self.checkpoints.len() >= MAX_ENTRIES {
            let evicted = self.evict_one(&checkpoint.media);
            if !evicted {
                debug_assert!(
                    false,
                    "MAX_ENTRIES ({MAX_ENTRIES}) left no evictable entry; the cap would be exceeded"
                );
                tracing::warn!(
                    media = %checkpoint.media,
                    "no entry could be evicted at the MAX_ENTRIES cap; dropping the incoming checkpoint rather than exceeding it"
                );
                return;
            }
        }
        let touch_seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.checkpoints.insert(
            checkpoint.media.clone(),
            PersistedCheckpoint {
                position: checkpoint.position,
                completed,
                touch_seq,
                updated_at: checkpoint.updated_at,
            },
        );
    }

    /// The lowest `touch_seq` among entries that are neither current nor the
    /// one arriving. Returns whether a victim was found and removed; `record`
    /// treats a `false` result as the cap guard firing.
    fn evict_one(&mut self, incoming: &MediaId) -> bool {
        let victim = self
            .checkpoints
            .iter()
            .filter(|(key, _)| Some(*key) != self.current_media.as_ref() && *key != incoming)
            .min_by_key(|(_, entry)| entry.touch_seq)
            .map(|(key, _)| key.clone());
        match victim {
            Some(victim) => {
                self.checkpoints.remove(&victim);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::id::AbsolutePath;

    fn id(name: &str) -> MediaId {
        match AbsolutePath::new(format!("/music/{name}.flac").into()) {
            Ok(path) => MediaId::LocalFile(path),
            Err(error) => panic!("a literal absolute path must parse: {error}"),
        }
    }

    /// `evict_one` must report "no victim" rather than evict the current entry,
    /// when the current entry is the only one in the map. This
    /// state is unreachable through `record` at `MAX_ENTRIES = 512` (the
    /// current entry is the only protected one, so 511 candidates always
    /// remain), so the guard is pinned directly against the private helper.
    #[test]
    fn evict_one_reports_no_victim_when_the_only_entry_is_current() {
        let mut state = PersistedState::default();
        let only = id("only");
        state.checkpoints.insert(
            only.clone(),
            PersistedCheckpoint {
                position: Duration::ZERO,
                completed: false,
                touch_seq: 1,
                updated_at: OffsetDateTime::UNIX_EPOCH,
            },
        );
        state.current_media = Some(only.clone());

        let evicted = state.evict_one(&id("incoming"));

        assert!(!evicted, "the current entry must never be evicted");
        assert_eq!(state.checkpoints.len(), 1, "nothing was removed");
        assert!(state.entry_for(&only).is_some());
    }
}
