# Continuo M2 — Durable Playback State Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist playback position, completion and volume to an atomically written JSON file, so quitting and relaunching `continuo play <file>` resumes close to where the listener left off.

**Architecture:** The playback engine stays ignorant of persistence — it publishes `Progress` and `PlaybackEvent`, nothing more. A pure `Session` policy turns that stream into `PersistedState` snapshots; a dedicated writer thread owns a keep-latest slot and the disk. Four small engine changes make the facts the policy needs reachable: a position on `Loaded`, a final published position at shutdown, and a shutdown report that carries the events the app never drained.

**Tech Stack:** Rust 2024, `serde` + `serde_json`, `directories` (state path), `time` (RFC 3339 timestamps), `crossbeam-channel`, `tracing`. Dev: `tempfile`.

**Spec:** `docs/superpowers/specs/2026-09-08-continuo-durable-state-design.md` — read it alongside this plan. Every decision reference below (`D1`…`D20`, `§3`…`§18`) points into that document.

## Global Constraints

- Rust edition 2024, `rust-version = "1.98.1"`. Do not raise either.
- `[lints.rust] unsafe_code = "forbid"`. `[lints.clippy] unwrap_used = "deny"`, `expect_used = "deny"`. `clippy.toml`'s `allow-unwrap-in-tests` / `allow-expect-in-tests` exempt the **body of a `#[test]` function only**. A bare helper in the same file is not exempt, even in `tests/`. Verified: a helper calling `.unwrap()` fails `cargo clippy --all-targets -- -D warnings` while the identical call inside the `#[test]` beneath it passes.
  Every bare helper in this plan therefore handles its own error with an explicit `match` or `let … else` plus a `panic!` carrying a message — `panic` is not among the denied lints. `tests/support/mod.rs` instead annotates its helpers with `#[allow(clippy::unwrap_used)]`; either is acceptable, and new helpers should say *why* the failure is impossible.
- Every task ends green on all three: `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --locked`.
- Linux needs `libasound2-dev` installed for `cpal` to build.
- Baseline before this plan starts: 99 passed · 0 failed · 1 ignored (`device_smoke`, needs real hardware).
- **All 20 engine contract tests in `tests/engine_contract.rs` stay byte-identical** (§17). Additions to `tests/support/mod.rs` must be purely additive — no existing signature changes.
- `schema_version` is the field name (not `version`). `SCHEMA_VERSION = 1`.
- Entry cap is **512 counting the current entry**; `current_media`'s entry is never evictable (D2).
- Capture interval **5 s**; coalescing window is a **2 s maximum age**, never extended by replacement (D5). Worst-case loss 7 s.
- Writer shutdown ACK timeout **2 s**, then report unconfirmed and **detach** — never an unconditional join (D10).
- Warn once after **3 consecutive** write failures, via `tracing::warn!`. Persistence failure is never fatal and never self-disabling (D11). Only D3 disables writing.
- Unix permissions: directory `0700`, destination and temp files `0600`, set explicitly (D12).
- `PersistenceError` gets **no stringly-typed variants** (§15) — that is the trap `docs/m1-known-debt.md` records for `PlaybackError::Failed`.
- `SeekCompleted.actual` is **never** persisted (D6). Positions come from `Progress.position`, except the two named exceptions: `SeekTargetStored.target` (D7) and `EndOfTrack.position`.
- Nothing in `src/playback/` may learn that persistence exists.

---

## File Structure

**Created**

| File | Responsibility |
|---|---|
| `src/clock.rs` | `Clock` trait yielding `ClockSample { monotonic, wall }`; `SystemClock` and `FakeClock` (D16) |
| `src/persistence/mod.rs` | Module re-exports and `PersistenceError` |
| `src/persistence/model.rs` | `PersistedState`, `PersistedCheckpoint`, `SCHEMA_VERSION`, `record`, eviction, `checkpoint_for`, derived `next_seq` |
| `src/persistence/store.rs` | `StateStore` — `load() -> LoadOutcome`, `write()`; atomic replace, permissions, version envelope, quarantine |
| `src/persistence/writer.rs` | `Slot` (pure keep-latest policy), `StateSink`, `WriterHandle::{submit, shutdown}`, the coalescing thread, the ACK |
| `src/session.rs` | `Session::{observe, tick, shutdown_snapshot}` and `decide_resume` — pure policy, no I/O, no threads |
| `tests/persistence_model.rs` | Model unit coverage that needs no filesystem |
| `tests/persistence_store.rs` | Store coverage against `tempfile` directories |
| `tests/persistence_writer.rs` | Writer thread coverage against a scripted sink |
| `tests/session_policy.rs` | Checkpoint policy, driven synchronously with a `FakeClock` |
| `tests/engine_shutdown.rs` | The two new engine facts (D14, D19), additive to the contract suite |
| `tests/resume_contract.rs` | Session 1 → persist → session 2, staged with `TestEngine` and a `StateStore` |

**Modified**

| File | Change |
|---|---|
| `Cargo.toml` | `serde_json` moves to a runtime dependency; `directories` added; `tempfile` added as a dev-dependency |
| `src/lib.rs` | `pub mod clock; pub mod persistence; pub mod session;` |
| `src/media/id.rs` | Derive `PartialOrd, Ord` on the identity newtypes so `MediaId` can key a `BTreeMap` |
| `src/playback/event.rs` | `Loaded` gains `position: Duration` (D8); new `ShutdownReport` |
| `src/playback/engine.rs` | Four changes (D8, D14, D19) — see Task 5 |
| `src/app.rs` | Wiring, the D18 restructure, the resume sequence, the shutdown handoff |
| `tests/support/mod.rs` | Additive `TestEngine::start_at` and `TestEngine::shutdown_report` |
| `README.md`, `docs/architecture.md` | Document the state file and its location |

**Dependency order.** 1 → 2 → 3 → 4 are independent of the engine. 5 is independent of 1–4. 6 → 7 need 1, 2 and 4's `Urgency`. 8 needs 2. 9 needs everything.

---

## Task 1: Dependencies and the injected clock

**Files:**
- Modify: `Cargo.toml`
- Create: `src/clock.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `continuo::clock::{Clock, ClockSample, SystemClock, FakeClock}`. `Clock::sample(&self) -> ClockSample`; `ClockSample { monotonic: std::time::Instant, wall: time::OffsetDateTime }`. `FakeClock::{new, advance, advance_monotonic, set_wall}`.

**Why two hands (D16):** deadlines and the 5 s capture interval use `monotonic`; `updated_at` uses `wall`. A wall clock that steps backwards must not stall a deadline, and an `Instant` cannot be written into an RFC 3339 field.

`FakeClock` is public, not `#[cfg(test)]`: the policy and writer tests live in `tests/`, which cannot reach a `cfg(test)` item in the library.

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml`, move `serde_json` out of `[dev-dependencies]` into `[dependencies]` and add `directories`; add `tempfile` as a dev-dependency:

```toml
[dependencies]
# ... existing entries unchanged ...
serde_json = "1"
directories = "6"

[dev-dependencies]
tempfile = "3"
```

Run: `cargo build --locked 2>&1 | tail -20`
Expected: it fails, because `--locked` forbids updating `Cargo.lock`. Re-run without it once to let the lockfile take the new crates: `cargo build`. Then confirm `cargo build --locked` succeeds. Check the resolved `directories` major version with `cargo tree -p directories --depth 0` and pin the `Cargo.toml` requirement to whatever major it resolved to.

- [ ] **Step 2: Write the failing clock test**

Create `src/clock.rs` containing only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use time::OffsetDateTime;

    #[test]
    fn the_two_hands_move_independently() {
        let clock = FakeClock::new();
        let start = clock.sample();

        clock.advance_monotonic(Duration::from_secs(5));
        let stepped = clock.sample();
        assert_eq!(
            stepped.monotonic.duration_since(start.monotonic),
            Duration::from_secs(5)
        );
        assert_eq!(stepped.wall, start.wall, "monotonic advance must not move the wall clock");

        // A wall clock that jumps backwards must leave deadlines alone.
        clock.set_wall(start.wall - Duration::from_secs(3600));
        let jumped = clock.sample();
        assert!(jumped.wall < start.wall);
        assert_eq!(jumped.monotonic, stepped.monotonic);
    }

    #[test]
    fn advance_moves_both_hands_together() {
        let clock = FakeClock::new();
        let start = clock.sample();
        clock.advance(Duration::from_secs(2));
        let after = clock.sample();
        assert_eq!(after.monotonic.duration_since(start.monotonic), Duration::from_secs(2));
        assert_eq!(after.wall - start.wall, time::Duration::seconds(2));
    }

    #[test]
    fn the_system_clock_reports_a_plausible_wall_time() {
        let sample = SystemClock.sample();
        assert!(sample.wall.year() >= 2024);
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --lib clock::`
Expected: FAIL — `cannot find type FakeClock in this scope` (the module has no items yet). Add `pub mod clock;` to `src/lib.rs` first if the module is not compiled at all.

- [ ] **Step 4: Implement the clock**

Prepend to `src/clock.rs`:

```rust
//! The clock the session and the writer read time from.
//!
//! Two hands, deliberately (D16): `monotonic` orders deadlines and intervals,
//! `wall` stamps `updated_at`. A wall clock that steps backwards must not stall
//! the 5 s capture or the 2 s coalesce, and a monotonic instant cannot be
//! written into an RFC 3339 field.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use time::OffsetDateTime;

#[derive(Clone, Copy, Debug)]
pub struct ClockSample {
    pub monotonic: Instant,
    pub wall: OffsetDateTime,
}

pub trait Clock: Send + Sync {
    fn sample(&self) -> ClockSample;
}

/// The real clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn sample(&self) -> ClockSample {
        ClockSample {
            monotonic: Instant::now(),
            wall: OffsetDateTime::now_utc(),
        }
    }
}

/// A clock the tests drive. Both hands are settable on their own, which is what
/// makes "a wall-clock jump does not disturb the 5 s interval" a test rather
/// than a claim.
#[derive(Debug)]
pub struct FakeClock {
    inner: Mutex<ClockSample>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ClockSample {
                monotonic: Instant::now(),
                wall: OffsetDateTime::UNIX_EPOCH,
            }),
        }
    }

    pub fn advance(&self, span: Duration) {
        self.advance_monotonic(span);
        let mut inner = self.lock();
        inner.wall += span;
    }

    pub fn advance_monotonic(&self, span: Duration) {
        let mut inner = self.lock();
        inner.monotonic += span;
    }

    pub fn set_wall(&self, wall: OffsetDateTime) {
        self.lock().wall = wall;
    }

    /// A poisoned fake clock means a test thread already panicked; the sample
    /// inside is still the truth, so recover rather than compound the panic.
    fn lock(&self) -> std::sync::MutexGuard<'_, ClockSample> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn sample(&self) -> ClockSample {
        *self.lock()
    }
}
```

Note `inner.wall += span` works because `time::OffsetDateTime` implements `AddAssign<std::time::Duration>`. `inner.monotonic += span` likewise for `Instant`.

- [ ] **Step 5: Register the module**

In `src/lib.rs`, add `pub mod clock;` in alphabetical position (before `pub mod cli;`).

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib clock::`
Expected: PASS — 3 tests.

- [ ] **Step 7: Verify the gates**

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --locked`
Expected: all clean, 102 passed.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/clock.rs src/lib.rs
git commit -m "Add the injected two-handed clock

Deadlines and the capture interval read the monotonic hand, updated_at
reads the wall hand. Keeping them separate is what lets a wall-clock step
backwards leave a deadline alone, and it is why FakeClock can move one
without the other."
```

---

## Task 2: The persisted model

**Files:**
- Create: `src/persistence/mod.rs`
- Create: `src/persistence/model.rs`
- Modify: `src/media/id.rs`
- Modify: `src/lib.rs`
- Test: `tests/persistence_model.rs`

**Interfaces:**
- Consumes: `continuo::playback::checkpoint::PlaybackCheckpoint`, `continuo::playback::volume::Volume`, `continuo::media::id::MediaId`.
- Produces:
  - `continuo::persistence::model::{PersistedState, PersistedCheckpoint, SCHEMA_VERSION, MAX_ENTRIES}`
  - `PersistedState::{default, volume, set_volume, current_media (pub field), checkpoints (pub field), record, checkpoint_for, entry_for, completed_for}`
  - `PersistedState::record(&mut self, checkpoint: &PlaybackCheckpoint, completed: bool)`
  - `PersistedState::checkpoint_for(&self, media: &MediaId) -> Option<PlaybackCheckpoint>`
  - `PersistedState::entry_for(&self, media: &MediaId) -> Option<&PersistedCheckpoint>`
  - `PersistedState::completed_for(&self, media: &MediaId) -> bool`
  - `continuo::persistence::PersistenceError`

**Why `MediaId` needs `Ord`:** D2 keys the map by `MediaId` and §5 built its string serde precisely to serve as a JSON map key. A `BTreeMap` key needs `Ord`, which the M0 identity types do not derive. The derives are purely additive — every one of these newtypes wraps a `String` or another newtype that does.

**Why `next_seq` is derived through a shadow struct:** §10 requires `1 + max(touch_seq)` computed at load and never stored, so that a hand-edited or truncated file cannot produce a regressing sequence. A `#[serde(skip)]` field deserializes to `0` and would need a fixup call every caller could forget. Deserializing through `RawState` makes the derivation the only way in.

- [ ] **Step 1: Write the failing model tests**

Create `tests/persistence_model.rs`:

```rust
//! The persisted model: ordering, eviction and the round trip.

use std::time::Duration;

use continuo::media::id::{AbsolutePath, MediaId};
use continuo::persistence::model::{MAX_ENTRIES, PersistedState, SCHEMA_VERSION};
use continuo::playback::checkpoint::PlaybackCheckpoint;
use continuo::playback::volume::Volume;
use time::OffsetDateTime;

fn media(name: &str) -> MediaId {
    // A bare helper, so it handles its own error: the lint exemption stops at
    // the `#[test]` boundary.
    match AbsolutePath::new(format!("/music/{name}.flac").into()) {
        Ok(path) => MediaId::LocalFile(path),
        Err(error) => panic!("a literal absolute path must parse: {error}"),
    }
}

fn checkpoint(name: &str, secs: u64) -> PlaybackCheckpoint {
    PlaybackCheckpoint {
        media: media(name),
        position: Duration::from_secs(secs),
        updated_at: OffsetDateTime::UNIX_EPOCH,
    }
}

#[test]
fn a_recorded_state_round_trips_through_json() {
    let mut state = PersistedState::default();
    state.set_volume(Volume::new(0.5));
    state.current_media = Some(media("a"));
    state.record(&checkpoint("a", 93), false);

    let json = serde_json::to_string(&state).unwrap();
    assert!(json.contains("\"schema_version\":1"), "{json}");
    assert!(json.contains("local:/music/a.flac"), "{json}");
    assert!(!json.contains("next_seq"), "session counters stay out of the file: {json}");

    let back: PersistedState = serde_json::from_str(&json).unwrap();
    assert_eq!(back.schema_version, SCHEMA_VERSION);
    assert_eq!(back.volume(), Volume::new(0.5));
    assert_eq!(back.current_media, Some(media("a")));
    assert_eq!(
        back.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(93)
    );
}

#[test]
fn touch_seq_is_assigned_only_by_record_and_never_regresses() {
    let mut state = PersistedState::default();
    state.record(&checkpoint("a", 10), false);
    state.record(&checkpoint("b", 20), false);
    state.record(&checkpoint("a", 30), false);

    let a = state.entry_for(&media("a")).unwrap().touch_seq;
    let b = state.entry_for(&media("b")).unwrap().touch_seq;
    assert!(a > b, "the later update to a must outrank b: {a} vs {b}");

    // Reading does not touch anything.
    let before = state.entry_for(&media("b")).unwrap().touch_seq;
    let _ = state.checkpoint_for(&media("b"));
    let _ = state.completed_for(&media("b"));
    assert_eq!(state.entry_for(&media("b")).unwrap().touch_seq, before);
}

#[test]
fn next_seq_is_derived_at_load_and_beats_the_highest_stored() {
    let mut state = PersistedState::default();
    state.record(&checkpoint("a", 10), false);
    state.record(&checkpoint("b", 20), false);
    let highest = state.entry_for(&media("b")).unwrap().touch_seq;

    let json = serde_json::to_string(&state).unwrap();
    let mut reloaded: PersistedState = serde_json::from_str(&json).unwrap();
    reloaded.record(&checkpoint("c", 30), false);

    assert!(
        reloaded.entry_for(&media("c")).unwrap().touch_seq > highest,
        "a reloaded state must not reissue a sequence the file already holds"
    );
}

#[test]
fn a_truncated_file_cannot_produce_a_regressing_sequence() {
    // A hand-edited file whose entries carry large sequences: the next one
    // issued must still beat them.
    let json = r#"{
        "schema_version": 1,
        "current_media": null,
        "volume": 1.0,
        "checkpoints": {
            "local:/music/a.flac": {
                "position": { "secs": 5, "nanos": 0 },
                "completed": false,
                "touch_seq": 9000,
                "updated_at": "1970-01-01T00:00:00Z"
            }
        }
    }"#;
    let mut state: PersistedState = serde_json::from_str(json).unwrap();
    state.record(&checkpoint("b", 1), false);
    assert!(state.entry_for(&media("b")).unwrap().touch_seq > 9000);
}

#[test]
fn eviction_takes_the_lowest_touch_seq_that_is_not_current() {
    let mut state = PersistedState::default();
    for index in 0..MAX_ENTRIES {
        state.record(&checkpoint(&format!("m{index}"), index as u64), false);
    }
    // m0 is the oldest, so make it current and watch m1 go instead.
    state.current_media = Some(media("m0"));
    assert_eq!(state.checkpoints.len(), MAX_ENTRIES);

    state.record(&checkpoint("incoming", 1), false);

    assert_eq!(state.checkpoints.len(), MAX_ENTRIES, "the cap counts the current entry");
    assert!(state.entry_for(&media("m0")).is_some(), "the current entry is never evictable");
    assert!(state.entry_for(&media("m1")).is_none(), "the lowest non-current entry goes");
    assert!(state.entry_for(&media("incoming")).is_some());
}

#[test]
fn updating_an_existing_entry_at_the_cap_evicts_nothing() {
    let mut state = PersistedState::default();
    for index in 0..MAX_ENTRIES {
        state.record(&checkpoint(&format!("m{index}"), index as u64), false);
    }
    state.record(&checkpoint("m5", 999), false);
    assert_eq!(state.checkpoints.len(), MAX_ENTRIES);
    assert_eq!(
        state.entry_for(&media("m5")).unwrap().position,
        Duration::from_secs(999)
    );
}

#[test]
fn completion_is_stored_alongside_the_position_it_retains() {
    let mut state = PersistedState::default();
    state.record(&checkpoint("a", 240), true);
    assert!(state.completed_for(&media("a")));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(240),
        "D1 retains the position a completed entry finished at"
    );
}

#[test]
fn a_hand_edited_volume_cannot_deafen_or_silence() {
    let json = r#"{"schema_version":1,"volume":2.0,"checkpoints":{}}"#;
    let state: PersistedState = serde_json::from_str(json).unwrap();
    assert_eq!(state.volume(), Volume::FULL);

    let json = r#"{"schema_version":1,"volume":-1.0,"checkpoints":{}}"#;
    let state: PersistedState = serde_json::from_str(json).unwrap();
    assert_eq!(state.volume().as_gain(), 0.0);

    // A missing field is the value M1 starts at.
    let json = r#"{"schema_version":1,"checkpoints":{}}"#;
    let state: PersistedState = serde_json::from_str(json).unwrap();
    assert_eq!(state.volume(), Volume::FULL);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test persistence_model 2>&1 | tail -20`
Expected: FAIL to compile — `unresolved import continuo::persistence`.

- [ ] **Step 3: Give the identity types a total order**

In `src/media/id.rs`, add `PartialOrd, Ord` to the derive list of `AbsolutePath`, `NormalizedUrl`, `FeedId`, `EpisodeKey`, `EpisodeIdentity` and `MediaId`. Each becomes:

```rust
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct AbsolutePath(String);
```

and for `MediaId`, which also carries serde attributes:

```rust
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum MediaId {
```

Nothing else about these types changes; the order exists only to key a map.

- [ ] **Step 4: Write the model**

Create `src/persistence/model.rs`:

```rust
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

/// `next_seq` is derived, never stored (§10). Deserialization goes through
/// [`RawState`] so that deriving it is the only way to build one from a file:
/// a `#[serde(skip)]` field would arrive as `0` and hand every caller a
/// sequence that regresses.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(from = "RawState")]
pub struct PersistedState {
    pub schema_version: u32,
    pub current_media: Option<MediaId>,
    volume: f32,
    pub checkpoints: BTreeMap<MediaId, PersistedCheckpoint>,
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

    pub fn checkpoint_for(&self, media: &MediaId) -> Option<PlaybackCheckpoint> {
        self.checkpoints.get(media).map(|entry| PlaybackCheckpoint {
            media: media.clone(),
            position: entry.position,
            updated_at: entry.updated_at,
        })
    }

    pub fn entry_for(&self, media: &MediaId) -> Option<&PersistedCheckpoint> {
        self.checkpoints.get(media)
    }

    pub fn completed_for(&self, media: &MediaId) -> bool {
        self.checkpoints.get(media).is_some_and(|entry| entry.completed)
    }

    /// The only place a `touch_seq` is ever assigned (D4). Loading, reading and
    /// restoring never touch one.
    pub fn record(&mut self, checkpoint: &PlaybackCheckpoint, completed: bool) {
        let fresh = !self.checkpoints.contains_key(&checkpoint.media);
        if fresh && self.checkpoints.len() >= MAX_ENTRIES {
            self.evict_one(&checkpoint.media);
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
    /// one arriving. With a cap above 1 there is always such an entry, so the
    /// `None` arm is unreachable rather than a silent policy.
    fn evict_one(&mut self, incoming: &MediaId) {
        let victim = self
            .checkpoints
            .iter()
            .filter(|(key, _)| Some(*key) != self.current_media.as_ref() && *key != incoming)
            .min_by_key(|(_, entry)| entry.touch_seq)
            .map(|(key, _)| key.clone());
        if let Some(victim) = victim {
            self.checkpoints.remove(&victim);
        }
    }
}
```

- [ ] **Step 5: Write the module root**

Create `src/persistence/mod.rs`:

```rust
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
```

In `src/lib.rs`, add `pub mod persistence;` in alphabetical position (after `pub mod media;`).

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --test persistence_model`
Expected: PASS — 8 tests.

- [ ] **Step 7: Verify the gates**

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --locked`
Expected: clean, 110 passed.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml src/lib.rs src/media/id.rs src/persistence/ tests/persistence_model.rs
git commit -m "Add the persisted state model

One checkpoint per media identity, keyed by MediaId, whose string serde
was built for exactly this. The identity newtypes gain a total order so
they can key a BTreeMap; nothing else about them changes.

touch_seq is assigned only by record, and next_seq is derived at load
rather than stored, so a hand-edited or truncated file cannot hand back a
sequence that regresses. Deserialization goes through a shadow struct to
make the derivation the only way in."
```

---

## Task 3: The store — atomic write, version envelope, quarantine

**Files:**
- Create: `src/persistence/store.rs`
- Modify: `src/persistence/mod.rs`
- Test: `tests/persistence_store.rs`

**Interfaces:**
- Consumes: `PersistedState`, `SCHEMA_VERSION`, `PersistenceError` (Task 2); `Clock` (Task 1).
- Produces:
  - `continuo::persistence::store::{StateStore, LoadOutcome, LoadReason}`
  - `StateStore::new(path: PathBuf, clock: Arc<dyn Clock>) -> Self`
  - `StateStore::platform_path() -> Result<PathBuf, PersistenceError>`
  - `StateStore::load(&self) -> LoadOutcome` — never fails; always yields a usable state
  - `StateStore::write(&self, state: &PersistedState) -> Result<(), PersistenceError>`
  - `LoadOutcome { state: PersistedState, writable: bool, reason: LoadReason }`

**Why the store takes a clock:** the quarantine file is named from a wall-clock stamp. Injecting the clock makes that name deterministic in tests, which is what lets the collision-suffixing and the unquarantinable case be tested at all — otherwise the test cannot know the name to collide with. It also keeps §13's "tests never touch `$HOME`" true, since only `app` calls `platform_path`.

**Why `load` never returns an error:** §13 requires that a malformed or unsupported file cost at most this session's *writing*, never the session. Every failure path yields a usable empty state plus a `writable` flag, and the reason is logged once.

- [ ] **Step 1: Write the failing store tests**

Create `tests/persistence_store.rs`:

```rust
//! The store: atomic replacement, permissions, and what happens to a file this
//! build cannot use.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use continuo::clock::FakeClock;
use continuo::media::id::{AbsolutePath, MediaId};
use continuo::persistence::model::PersistedState;
use continuo::persistence::store::{LoadReason, StateStore};
use continuo::playback::checkpoint::PlaybackCheckpoint;
use time::OffsetDateTime;

/// The stamp a `FakeClock` produces, which starts at the epoch.
const EPOCH_STAMP: &str = "19700101T000000Z";

fn store(dir: &Path) -> StateStore {
    StateStore::new(dir.join("state.json"), Arc::new(FakeClock::new()))
}

fn media(name: &str) -> MediaId {
    // A bare helper, so it handles its own error: the lint exemption stops at
    // the `#[test]` boundary.
    match AbsolutePath::new(format!("/music/{name}.flac").into()) {
        Ok(path) => MediaId::LocalFile(path),
        Err(error) => panic!("a literal absolute path must parse: {error}"),
    }
}

fn state_with(name: &str, secs: u64) -> PersistedState {
    let mut state = PersistedState::default();
    state.current_media = Some(media(name));
    state.record(
        &PlaybackCheckpoint {
            media: media(name),
            position: Duration::from_secs(secs),
            updated_at: OffsetDateTime::UNIX_EPOCH,
        },
        false,
    );
    state
}

fn temp_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        panic!("the tempdir must be readable");
    };
    entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("state.json.tmp-"))
        })
        .collect()
}

#[test]
fn a_missing_file_yields_empty_state_and_writing_stays_on() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = store(dir.path()).load();
    assert!(matches!(outcome.reason, LoadReason::Missing));
    assert!(outcome.writable);
    assert!(outcome.state.checkpoints.is_empty());
}

#[test]
fn a_write_is_readable_back_and_leaves_no_temp_behind() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    store.write(&state_with("a", 93)).unwrap();

    let outcome = store.load();
    assert!(matches!(outcome.reason, LoadReason::Loaded));
    assert!(outcome.writable);
    assert_eq!(
        outcome.state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(93)
    );
    assert!(temp_files(dir.path()).is_empty(), "the temp file must not survive the write");
}

#[test]
fn an_overwrite_replaces_rather_than_truncates() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());

    // A large state first, then a small one: an in-place write would leave the
    // tail of the large one behind and the result would not parse.
    let mut large = PersistedState::default();
    for index in 0..200 {
        large.record(
            &PlaybackCheckpoint {
                media: media(&format!("m{index}")),
                position: Duration::from_secs(index),
                updated_at: OffsetDateTime::UNIX_EPOCH,
            },
            false,
        );
    }
    store.write(&large).unwrap();
    store.write(&state_with("a", 5)).unwrap();

    let outcome = store.load();
    assert!(matches!(outcome.reason, LoadReason::Loaded));
    assert_eq!(outcome.state.checkpoints.len(), 1);
}

#[test]
fn a_malformed_file_is_quarantined_and_writing_continues() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("state.json"), b"{ this is not json").unwrap();

    let outcome = store(dir.path()).load();
    let LoadReason::Quarantined { moved_to } = &outcome.reason else {
        panic!("expected a quarantine, got {:?}", outcome.reason);
    };
    assert!(outcome.writable, "garbage must not cost a session of persistence");
    assert_eq!(
        fs::read(moved_to).unwrap(),
        b"{ this is not json",
        "the original bytes are preserved under the new name"
    );
    assert!(!dir.path().join("state.json").exists());
    assert_eq!(
        moved_to.file_name().unwrap().to_str().unwrap(),
        format!("state.json.rejected-{EPOCH_STAMP}")
    );
}

#[test]
fn a_quarantine_name_collision_is_suffixed_rather_than_clobbered() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("state.json"), b"garbage").unwrap();
    let taken = dir.path().join(format!("state.json.rejected-{EPOCH_STAMP}"));
    fs::write(&taken, b"an earlier rejection").unwrap();

    let outcome = store(dir.path()).load();
    let LoadReason::Quarantined { moved_to } = &outcome.reason else {
        panic!("expected a quarantine, got {:?}", outcome.reason);
    };
    assert_eq!(
        moved_to.file_name().unwrap().to_str().unwrap(),
        format!("state.json.rejected-{EPOCH_STAMP}-2")
    );
    assert_eq!(fs::read(&taken).unwrap(), b"an earlier rejection");
}

#[test]
fn a_quarantine_that_cannot_be_performed_disables_writing_and_keeps_the_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("state.json"), b"garbage").unwrap();
    // Every candidate name taken: the store must refuse to clobber any of them.
    for suffix in 1..=100 {
        let name = if suffix == 1 {
            format!("state.json.rejected-{EPOCH_STAMP}")
        } else {
            format!("state.json.rejected-{EPOCH_STAMP}-{suffix}")
        };
        fs::write(dir.path().join(name), b"taken").unwrap();
    }

    let outcome = store(dir.path()).load();
    assert!(matches!(outcome.reason, LoadReason::QuarantineFailed));
    assert!(
        !outcome.writable,
        "the only way to preserve a file whose quarantine failed is to stop writing"
    );
    assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), b"garbage");
}

#[test]
fn an_unsupported_version_is_preserved_in_place_and_disables_writing() {
    let dir = tempfile::tempdir().unwrap();
    let newer = br#"{"schema_version":2,"checkpoints":{}}"#;
    fs::write(dir.path().join("state.json"), newer).unwrap();

    let outcome = store(dir.path()).load();
    assert!(matches!(outcome.reason, LoadReason::UnsupportedVersion { found: 2 }));
    assert!(!outcome.writable);
    assert_eq!(
        fs::read(dir.path().join("state.json")).unwrap(),
        newer,
        "a newer build's state must survive a downgrade"
    );
    assert!(
        fs::read_dir(dir.path()).unwrap().count() == 1,
        "an unsupported file is preserved in place, not quarantined"
    );
}

#[test]
fn a_newer_file_is_classified_by_version_even_when_its_shape_is_alien() {
    // Deserializing the model first would call this garbage; the envelope is
    // the only thing every future version is obliged to keep.
    let dir = tempfile::tempdir().unwrap();
    let alien = br#"{"schema_version":2,"checkpoints":[1,2,3],"queues":{"a":true}}"#;
    fs::write(dir.path().join("state.json"), alien).unwrap();

    let outcome = store(dir.path()).load();
    assert!(matches!(outcome.reason, LoadReason::UnsupportedVersion { found: 2 }));
    assert_eq!(fs::read(dir.path().join("state.json")).unwrap(), alien);
}

#[test]
fn a_version_one_file_that_will_not_deserialize_is_malformed() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("state.json"),
        br#"{"schema_version":1,"checkpoints":{"not a media id":{}}}"#,
    )
    .unwrap();

    let outcome = store(dir.path()).load();
    assert!(matches!(outcome.reason, LoadReason::Quarantined { .. }));
    assert!(outcome.writable);
}

#[cfg(unix)]
#[test]
fn the_directory_and_the_file_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("continuo");
    let store = StateStore::new(nested.join("state.json"), Arc::new(FakeClock::new()));
    store.write(&state_with("a", 1)).unwrap();

    let dir_mode = fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
    let file_mode = fs::metadata(nested.join("state.json")).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "a permissive umask must not expose listening history");
    assert_eq!(file_mode, 0o600);
}

#[cfg(unix)]
#[test]
fn an_existing_permissive_directory_is_tightened() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("continuo");
    fs::create_dir_all(&nested).unwrap();
    fs::set_permissions(&nested, fs::Permissions::from_mode(0o755)).unwrap();

    let store = StateStore::new(nested.join("state.json"), Arc::new(FakeClock::new()));
    store.write(&state_with("a", 1)).unwrap();

    assert_eq!(
        fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
        0o700,
        "a directory that already existed is exactly the one a create-time mode never reaches"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test persistence_store 2>&1 | tail -20`
Expected: FAIL to compile — `unresolved import continuo::persistence::store`.

- [ ] **Step 3: Write the store**

Create `src/persistence/store.rs`:

```rust
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
const MAX_QUARANTINE_CANDIDATES: u32 = 100;

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
    Quarantined { moved_to: PathBuf },
    /// The file was garbage and could not be moved aside; writing is disabled
    /// so that the next checkpoint does not overwrite what §6 requires be kept.
    QuarantineFailed,
    /// A version this build does not support; preserved in place.
    UnsupportedVersion { found: u32 },
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
                return self.reject_malformed(&bytes);
            }
        };
        // Not `> SCHEMA_VERSION`: a file from a build that renumbered downward
        // is just as unreadable as one from a build ahead of this one.
        if envelope.schema_version != SCHEMA_VERSION {
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
            Ok(state) => LoadOutcome {
                state,
                writable: true,
                reason: LoadReason::Loaded,
            },
            Err(error) => {
                tracing::warn!(path = ?self.path, %error, "the state file is version 1 but unreadable");
                self.reject_malformed(&bytes)
            }
        }
    }

    pub fn write(&self, state: &PersistedState) -> Result<(), PersistenceError> {
        let dir = self.parent();
        self.prepare_directory(dir)?;

        let bytes = serde_json::to_vec_pretty(state).map_err(|source| {
            PersistenceError::Serialize {
                path: self.path.clone(),
                source,
            }
        })?;

        let seq = self.temp_seq.fetch_add(1, Ordering::Relaxed);
        let temp = dir.join(format!("state.json.tmp-{}-{seq}", std::process::id()));

        // The private mode is carried by the create, before any content is
        // written; a chmod afterwards leaves a window where it is not.
        let write_temp = || -> Result<(), io::Error> {
            let mut file = private_file(&temp)?;
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

    fn reject_malformed(&self, _bytes: &[u8]) -> LoadOutcome {
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
            return fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|source| PersistenceError::Io {
                    path: dir.to_path_buf(),
                    op: "create directory for",
                    source,
                });
        }

        // D12 says the mode is set explicitly, and a create-time mode reaches
        // exactly the case that never needs it. A directory left at 0755 by an
        // earlier build, a restore, or a hand-made `mkdir` is the one that does.
        let Ok(metadata) = fs::metadata(dir) else {
            return Ok(());
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
```

Add `pub mod store;` to `src/persistence/mod.rs`, after `pub mod model;`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test persistence_store`
Expected: PASS — 10 tests (12 on Unix).

If `a_version_one_file_that_will_not_deserialize_is_malformed` passes for the wrong reason, check that `"not a media id"` really fails `MediaId`'s `try_from` — it has no `:` separator, so it does.

- [ ] **Step 5: Verify the gates**

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --locked`
Expected: clean, 122 passed.

- [ ] **Step 6: Commit**

```bash
git add src/persistence/ tests/persistence_store.rs
git commit -m "Add the state store with an atomic replace and a version envelope

The version is read from a minimal envelope before the model, so a file
from a newer build is classified by its version rather than mistaken for
garbage and moved aside. Garbage is quarantined and writing continues; a
quarantine that cannot be performed disables writing instead, because the
only way to preserve a file you could not move is to stop overwriting it.

The store takes the injected clock because the quarantine name is a
wall-clock stamp. That is what makes the collision suffixing and the
exhausted-candidates path testable at all."
```

---

## Task 4: The writer thread

**Files:**
- Create: `src/persistence/writer.rs`
- Modify: `src/persistence/mod.rs`
- Test: `tests/persistence_writer.rs`

**Interfaces:**
- Consumes: `PersistedState`, `PersistenceError`, `StateStore` (Tasks 2–3); `Clock` (Task 1).
- Produces:
  - `continuo::persistence::writer::{Urgency, StateSink, WriterHandle, ShutdownOutcome, COALESCE_WINDOW}`
  - `Urgency::{Ordinary, Forced}` — derives `Clone, Copy, Debug, Eq, PartialEq`
  - `StateSink: Send { fn write(&self, state: &PersistedState) -> Result<(), PersistenceError>; }`, implemented for `StateStore`
  - `WriterHandle::spawn(sink: Box<dyn StateSink>, clock: Arc<dyn Clock>) -> Self`
  - `WriterHandle::submit(&self, state: PersistedState, urgency: Urgency)` — never blocks, never fails
  - `WriterHandle::shutdown(&mut self) -> ShutdownOutcome`
  - `ShutdownOutcome::{Written, Failed(PersistenceError), Unconfirmed}`

**Why a sink trait:** the retry and failure-counting paths need a store that fails on command. A trait keeps that out of `StateStore` and off the filesystem.

**Why the thread reads the injected clock:** the slot's deadlines are monotonic (D16). The thread still *waits* on real time — a `Condvar` has no other kind — but what is *due* is decided against the injected clock, so a test can advance two seconds instantly and the thread notices on its next idle pass.

**Why keep-latest cannot be violated by a retry (§9):** the writer holds the snapshot it is writing *outside* the slot, so the producer can fill the emptied slot while the I/O is in flight. A failed write reinserts only if the slot is still empty; if a newer snapshot arrived, that one supersedes it and the failed one is dropped — the retry still happens, with better data. The failure counter lives on the writer, not on the snapshot, so it survives the replacement.

- [ ] **Step 1: Write the failing writer tests**

Create `tests/persistence_writer.rs`:

```rust
//! The writer thread: coalescing, retry, and the bounded shutdown.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use continuo::clock::{Clock, FakeClock};
use continuo::persistence::PersistenceError;
use continuo::persistence::model::PersistedState;
use continuo::persistence::writer::{ShutdownOutcome, StateSink, Urgency, WriterHandle};
use continuo::playback::volume::Volume;

const PATIENCE: Duration = Duration::from_secs(5);

/// Records what it was asked to write, and fails the first `failures` attempts.
#[derive(Default)]
struct ScriptedSink {
    written: Mutex<Vec<f32>>,
    attempts: AtomicUsize,
    fail_first: usize,
}

impl ScriptedSink {
    fn new(fail_first: usize) -> Arc<Self> {
        Arc::new(Self {
            fail_first,
            ..Self::default()
        })
    }
    fn written(&self) -> Vec<f32> {
        self.written.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Relaxed)
    }
}

/// `StateSink` is implemented for the `Arc` so the test can keep a handle to
/// the same sink the writer owns.
impl StateSink for Arc<ScriptedSink> {
    fn write(&self, state: &PersistedState) -> Result<(), PersistenceError> {
        let attempt = self.attempts.fetch_add(1, Ordering::Relaxed);
        if attempt < self.fail_first {
            return Err(PersistenceError::NoStateDirectory);
        }
        self.written
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(state.volume().as_gain());
        Ok(())
    }
}

struct BlockingSink;

impl StateSink for BlockingSink {
    fn write(&self, _state: &PersistedState) -> Result<(), PersistenceError> {
        std::thread::sleep(Duration::from_secs(10));
        Ok(())
    }
}

/// Volume is the marker: it is one `f32` on the snapshot, so a test can name
/// each submission by a number and read back exactly which ones landed.
fn snapshot(marker: f32) -> PersistedState {
    let mut state = PersistedState::default();
    state.set_volume(Volume::new(marker));
    state
}

fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn a_forced_submit_is_written_without_waiting_for_the_window() {
    let sink = ScriptedSink::new(0);
    let clock = Arc::new(FakeClock::new());
    let writer = WriterHandle::spawn(Box::new(Arc::clone(&sink)), clock);

    writer.submit(snapshot(0.5), Urgency::Forced);

    assert!(wait_until(|| sink.written() == vec![0.5]), "forced bypasses the coalescing window");
}

#[test]
fn an_ordinary_submit_waits_out_the_window() {
    let sink = ScriptedSink::new(0);
    let clock = Arc::new(FakeClock::new());
    // `Arc::clone(&clock)` here would be E0308: the annotation makes the
    // argument position expect `&Arc<dyn Clock>`, and `&Arc<FakeClock>` does
    // not coerce through a reference. A method call resolves on the concrete
    // type first, and it is the *result* that unsizes.
    let injected: Arc<dyn Clock> = clock.clone();
    let writer = WriterHandle::spawn(Box::new(Arc::clone(&sink)), injected);

    writer.submit(snapshot(0.5), Urgency::Ordinary);
    clock.advance_monotonic(Duration::from_millis(1900));
    std::thread::sleep(Duration::from_millis(250));
    assert!(sink.written().is_empty(), "1.9 s is inside the 2 s window");

    clock.advance_monotonic(Duration::from_millis(200));
    assert!(wait_until(|| sink.written() == vec![0.5]));
}

#[test]
fn a_replacement_never_extends_the_deadline_and_the_newest_wins() {
    let sink = ScriptedSink::new(0);
    let clock = Arc::new(FakeClock::new());
    // `Arc::clone(&clock)` here would be E0308: the annotation makes the
    // argument position expect `&Arc<dyn Clock>`, and `&Arc<FakeClock>` does
    // not coerce through a reference. A method call resolves on the concrete
    // type first, and it is the *result* that unsizes.
    let injected: Arc<dyn Clock> = clock.clone();
    let writer = WriterHandle::spawn(Box::new(Arc::clone(&sink)), injected);

    writer.submit(snapshot(0.1), Urgency::Ordinary);
    clock.advance_monotonic(Duration::from_millis(1500));
    writer.submit(snapshot(0.2), Urgency::Ordinary);
    clock.advance_monotonic(Duration::from_millis(600));

    // 2.1 s after the first submission, so the deadline anchored on the first
    // has passed even though the second arrived 0.6 s ago.
    assert!(wait_until(|| sink.written() == vec![0.2]), "the newest snapshot wins, on the original deadline");
}

#[test]
fn a_failed_write_is_retried_and_the_retry_carries_whatever_is_newest() {
    let sink = ScriptedSink::new(1);
    let clock = Arc::new(FakeClock::new());
    // `Arc::clone(&clock)` here would be E0308: the annotation makes the
    // argument position expect `&Arc<dyn Clock>`, and `&Arc<FakeClock>` does
    // not coerce through a reference. A method call resolves on the concrete
    // type first, and it is the *result* that unsizes.
    let injected: Arc<dyn Clock> = clock.clone();
    let writer = WriterHandle::spawn(Box::new(Arc::clone(&sink)), injected);

    writer.submit(snapshot(0.1), Urgency::Forced);
    assert!(wait_until(|| sink.attempts() >= 1));
    assert!(sink.written().is_empty(), "the first attempt fails");

    // A newer snapshot lands while the failed one waits to be retried; it must
    // supersede rather than be replaced by the failure.
    writer.submit(snapshot(0.9), Urgency::Forced);
    assert!(
        wait_until(|| sink.written() == vec![0.9]),
        "keep-latest survives a retry: got {:?}",
        sink.written()
    );
}

#[test]
fn shutdown_flushes_what_is_pending_and_confirms_it() {
    let sink = ScriptedSink::new(0);
    let clock = Arc::new(FakeClock::new());
    let mut writer = WriterHandle::spawn(Box::new(Arc::clone(&sink)), clock);

    // Ordinary, so nothing is due: shutdown must still flush it.
    writer.submit(snapshot(0.7), Urgency::Ordinary);
    assert!(matches!(writer.shutdown(), ShutdownOutcome::Written));
    assert_eq!(sink.written(), vec![0.7]);
}

#[test]
fn shutdown_reports_a_final_write_that_failed() {
    let sink = ScriptedSink::new(usize::MAX);
    let clock = Arc::new(FakeClock::new());
    let mut writer = WriterHandle::spawn(Box::new(Arc::clone(&sink)), clock);

    writer.submit(snapshot(0.7), Urgency::Forced);
    assert!(matches!(writer.shutdown(), ShutdownOutcome::Failed(_)));
}

#[test]
fn a_shutdown_that_is_not_acknowledged_detaches_rather_than_hanging() {
    let clock = Arc::new(FakeClock::new());
    let mut writer = WriterHandle::spawn(Box::new(BlockingSink), clock);
    writer.submit(snapshot(0.7), Urgency::Forced);

    let started = Instant::now();
    assert!(matches!(writer.shutdown(), ShutdownOutcome::Unconfirmed));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(4),
        "the 2 s bound must not be defeated by an unconditional join: took {elapsed:?}"
    );
}

#[test]
fn submits_after_shutdown_are_rejected() {
    let sink = ScriptedSink::new(0);
    let clock = Arc::new(FakeClock::new());
    let mut writer = WriterHandle::spawn(Box::new(Arc::clone(&sink)), clock);

    assert!(matches!(writer.shutdown(), ShutdownOutcome::Written));
    writer.submit(snapshot(0.4), Urgency::Forced);
    std::thread::sleep(Duration::from_millis(100));
    assert!(sink.written().is_empty());
}
```

Note `shutdown_flushes_what_is_pending_and_confirms_it` asserts `Written` even though nothing was pending in the `Forced` sense — with an empty slot the thread acknowledges `Ok(())` immediately, so the same variant covers both.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test persistence_writer 2>&1 | tail -20`
Expected: FAIL to compile — `unresolved import continuo::persistence::writer`.

- [ ] **Step 3: Write the slot and its unit tests**

Create `src/persistence/writer.rs` starting with the pure policy:

```rust
//! The disk's owner: a keep-latest slot, a coalescing thread, and a bounded
//! shutdown.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};

use crate::clock::Clock;

use super::PersistenceError;
use super::model::PersistedState;
use super::store::StateStore;

/// A maximum age, not a minimum spacing (D5).
pub const COALESCE_WINDOW: Duration = Duration::from_secs(2);
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
const IDLE_WAIT: Duration = Duration::from_millis(100);
const FAILURE_WARNING_THRESHOLD: u32 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Urgency {
    Ordinary,
    Forced,
}

pub trait StateSink: Send {
    fn write(&self, state: &PersistedState) -> Result<(), PersistenceError>;
}

impl StateSink for StateStore {
    fn write(&self, state: &PersistedState) -> Result<(), PersistenceError> {
        StateStore::write(self, state)
    }
}

struct Pending {
    state: PersistedState,
    deadline: Instant,
    submit_seq: u64,
}

/// One producer, one consumer, replace-on-submit: the newest snapshot wins
/// structurally. `submit_seq` makes that checkable rather than assumed, and is
/// never persisted (§9).
#[derive(Default)]
struct Slot {
    pending: Option<Pending>,
    last_written: u64,
}

impl Slot {
    fn submit(&mut self, state: PersistedState, urgency: Urgency, now: Instant, submit_seq: u64) {
        let deadline = match (&self.pending, urgency) {
            (_, Urgency::Forced) => now,
            // Anchored when the first snapshot entered an empty slot;
            // replacement never extends it.
            (Some(pending), Urgency::Ordinary) => pending.deadline,
            (None, Urgency::Ordinary) => now + COALESCE_WINDOW,
        };
        self.pending = Some(Pending {
            state,
            deadline,
            submit_seq,
        });
    }

    fn take_due(&mut self, now: Instant) -> Option<Pending> {
        if self.pending.as_ref().is_some_and(|p| p.deadline <= now) {
            self.pending.take()
        } else {
            None
        }
    }

    fn take_pending(&mut self) -> Option<Pending> {
        self.pending.take()
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|pending| pending.deadline)
    }

    fn is_stale(&self, submit_seq: u64) -> bool {
        submit_seq <= self.last_written
    }

    fn mark_written(&mut self, submit_seq: u64) {
        self.last_written = self.last_written.max(submit_seq);
    }

    /// Only into an empty slot: a newer snapshot that arrived while the write
    /// was in flight supersedes the one that failed.
    fn reinsert_failed(&mut self, pending: Pending, now: Instant) {
        if self.pending.is_none() {
            self.pending = Some(Pending {
                deadline: now + COALESCE_WINDOW,
                ..pending
            });
        }
    }
}

fn should_warn(consecutive_failures: u32) -> bool {
    consecutive_failures == FAILURE_WARNING_THRESHOLD
}
```

and append the unit tests for it at the end of the same file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    #[test]
    fn an_ordinary_replacement_does_not_extend_the_deadline() {
        let base = Instant::now();
        let mut slot = Slot::default();
        slot.submit(PersistedState::default(), Urgency::Ordinary, base, 1);
        slot.submit(PersistedState::default(), Urgency::Ordinary, at(base, 1_500), 2);

        assert_eq!(slot.next_deadline(), Some(base + COALESCE_WINDOW));
        assert!(slot.take_due(at(base, 1_999)).is_none());
        assert!(slot.take_due(at(base, 2_000)).is_some());
    }

    #[test]
    fn a_forced_submit_is_due_immediately_even_over_a_waiting_one() {
        let base = Instant::now();
        let mut slot = Slot::default();
        slot.submit(PersistedState::default(), Urgency::Ordinary, base, 1);
        slot.submit(PersistedState::default(), Urgency::Forced, at(base, 10), 2);

        let due = slot.take_due(at(base, 10)).expect("forced is due at once");
        assert_eq!(due.submit_seq, 2);
    }

    #[test]
    fn an_older_submission_is_recognized_as_stale() {
        let mut slot = Slot::default();
        slot.mark_written(7);
        assert!(slot.is_stale(7));
        assert!(slot.is_stale(6));
        assert!(!slot.is_stale(8));
    }

    #[test]
    fn a_failed_snapshot_never_displaces_a_newer_one() {
        let base = Instant::now();
        let mut slot = Slot::default();
        slot.submit(PersistedState::default(), Urgency::Forced, base, 1);
        let failed = slot.take_pending().expect("in flight");

        // A newer snapshot arrives while the write is failing.
        slot.submit(PersistedState::default(), Urgency::Forced, at(base, 5), 2);
        slot.reinsert_failed(failed, at(base, 10));

        assert_eq!(
            slot.take_pending().map(|pending| pending.submit_seq),
            Some(2),
            "keep-latest is never violated by a retry"
        );
    }

    #[test]
    fn a_failed_snapshot_returns_to_an_empty_slot_with_a_fresh_deadline() {
        let base = Instant::now();
        let mut slot = Slot::default();
        slot.submit(PersistedState::default(), Urgency::Forced, base, 1);
        let failed = slot.take_pending().expect("in flight");
        slot.reinsert_failed(failed, at(base, 10));

        assert_eq!(slot.next_deadline(), Some(at(base, 10) + COALESCE_WINDOW));
    }

    #[test]
    fn the_warning_fires_once_at_three_consecutive_failures() {
        assert!(!should_warn(1));
        assert!(!should_warn(2));
        assert!(should_warn(3));
        assert!(!should_warn(4), "one warning, not one per failure");
    }
}
```

- [ ] **Step 4: Run the slot unit tests**

Run: `cargo test --lib persistence::writer`
Expected: PASS — 6 tests.

- [ ] **Step 5: Write the handle and the thread**

Insert between the slot and the test module in `src/persistence/writer.rs`:

```rust
#[derive(Debug)]
pub enum ShutdownOutcome {
    Written,
    Failed(PersistenceError),
    /// The writer did not answer inside the bound; it has been detached.
    Unconfirmed,
}

struct Shared {
    slot: Mutex<Slot>,
    wake: Condvar,
    closing: AtomicBool,
}

pub struct WriterHandle {
    shared: Arc<Shared>,
    ack: Receiver<Result<(), PersistenceError>>,
    thread: Option<JoinHandle<()>>,
    clock: Arc<dyn Clock>,
    next_seq: AtomicU64,
}

/// A poisoned slot means the writer thread panicked mid-write. The snapshot
/// inside is still a valid snapshot, so recover rather than take the app down
/// with it — D11 says persistence is never fatal.
fn lock(slot: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl WriterHandle {
    pub fn spawn(sink: Box<dyn StateSink>, clock: Arc<dyn Clock>) -> Self {
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::default()),
            wake: Condvar::new(),
            closing: AtomicBool::new(false),
        });
        let (ack_tx, ack_rx) = bounded(1);
        let thread = {
            let shared = Arc::clone(&shared);
            let clock = Arc::clone(&clock);
            std::thread::Builder::new()
                .name("continuo-state".into())
                .spawn(move || run(shared, sink, clock, ack_tx))
                .ok()
        };
        Self {
            shared,
            ack: ack_rx,
            thread,
            clock,
            next_seq: AtomicU64::new(1),
        }
    }

    /// Never blocks and never fails: the slot is keep-latest, so the worst a
    /// submission can do is replace one that had not been written yet.
    pub fn submit(&self, state: PersistedState, urgency: Urgency) {
        if self.shared.closing.load(Ordering::Acquire) {
            return;
        }
        let submit_seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let now = self.clock.sample().monotonic;
        lock(&self.shared.slot).submit(state, urgency, now, submit_seq);
        self.shared.wake.notify_all();
    }

    /// Bounded by design (D10): an unconditional join after the timeout would
    /// defeat the bound it exists to enforce.
    pub fn shutdown(&mut self) -> ShutdownOutcome {
        self.shared.closing.store(true, Ordering::Release);
        self.shared.wake.notify_all();
        match self.ack.recv_timeout(ACK_TIMEOUT) {
            Ok(Ok(())) => {
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
                ShutdownOutcome::Written
            }
            Ok(Err(error)) => {
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
                ShutdownOutcome::Failed(error)
            }
            // Detach: dropping the handle abandons the thread without waiting.
            Err(_) => {
                drop(self.thread.take());
                ShutdownOutcome::Unconfirmed
            }
        }
    }
}

impl Drop for WriterHandle {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        self.shared.wake.notify_all();
    }
}

fn run(
    shared: Arc<Shared>,
    sink: Box<dyn StateSink>,
    clock: Arc<dyn Clock>,
    ack: Sender<Result<(), PersistenceError>>,
) {
    let mut consecutive_failures: u32 = 0;
    loop {
        let closing = shared.closing.load(Ordering::Acquire);
        let now = clock.sample().monotonic;
        let taken = {
            let mut slot = lock(&shared.slot);
            // Closing takes whatever is there: the final write does not wait
            // out a coalescing window nobody is left to fill.
            if closing {
                slot.take_pending()
            } else {
                slot.take_due(now)
            }
        };

        let Some(pending) = taken else {
            if closing {
                let _ = ack.send(Ok(()));
                return;
            }
            let slot = lock(&shared.slot);
            let wait = slot
                .next_deadline()
                .map(|deadline| deadline.saturating_duration_since(clock.sample().monotonic))
                .unwrap_or(IDLE_WAIT)
                .clamp(Duration::from_millis(1), IDLE_WAIT);
            let _ = shared.wake.wait_timeout(slot, wait);
            continue;
        };

        if lock(&shared.slot).is_stale(pending.submit_seq) {
            continue;
        }

        // The snapshot is held outside the slot while the I/O is in flight, so
        // the producer can fill the emptied slot meanwhile (§9).
        match sink.write(&pending.state) {
            Ok(()) => {
                consecutive_failures = 0;
                lock(&shared.slot).mark_written(pending.submit_seq);
                if closing {
                    let _ = ack.send(Ok(()));
                    return;
                }
            }
            Err(error) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                if should_warn(consecutive_failures) {
                    tracing::warn!(
                        %error,
                        failures = consecutive_failures,
                        "the playback state file has failed to write three times running"
                    );
                }
                if closing {
                    let _ = ack.send(Err(error));
                    return;
                }
                tracing::debug!(%error, "state write failed; retrying at the coalescing cadence");
                lock(&shared.slot).reinsert_failed(pending, clock.sample().monotonic);
            }
        }
    }
}
```

The `wait` clamp to `IDLE_WAIT` is what lets a `FakeClock` test work: the thread re-checks what is due at least ten times a second regardless of how far away the injected deadline looks.

Add `pub mod writer;` to `src/persistence/mod.rs`, after `pub mod store;`.

- [ ] **Step 6: Run the writer tests to verify they pass**

Run: `cargo test --test persistence_writer`
Expected: PASS — 8 tests. `a_shutdown_that_is_not_acknowledged_detaches_rather_than_hanging` takes about 2 s; the rest are fast.

- [ ] **Step 7: Verify the gates**

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --locked`
Expected: clean, 136 passed.

- [ ] **Step 8: Commit**

```bash
git add src/persistence/writer.rs src/persistence/mod.rs tests/persistence_writer.rs
git commit -m "Add the state writer thread

A keep-latest slot whose deadline is a maximum age rather than a minimum
spacing: the anchor is set when the first snapshot enters an empty slot
and replacement never moves it, so coalescing cannot delay a checkpoint
indefinitely under a stream of updates.

The snapshot being written is held outside the slot, so a producer can
fill the emptied slot while the I/O is in flight, and a failed write
returns only to a slot that is still empty. The retry therefore happens
with better data rather than worse, and keep-latest is never violated by
it.

Shutdown is bounded by an acknowledgment channel and a two second
timeout, after which the thread is detached rather than joined, since an
unconditional join defeats the bound it enforces."
```

---

## Task 5: The four engine changes

**Files:**
- Modify: `src/playback/event.rs`
- Modify: `src/playback/engine.rs:1173-1178` (the `Loaded` emit), `src/playback/engine.rs:225-231` (`join`), `src/playback/engine.rs:347-424` (`run`), `src/playback/engine.rs:960-965` (`shutdown`)
- Modify: `src/app.rs:183-196` (`Mirror::apply` for `Loaded`)
- Modify: `tests/support/mod.rs` (additive only)
- Test: `tests/engine_shutdown.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `PlaybackEvent::Loaded { session_rev, media, metadata, capabilities, position: Duration }` (D8)
  - `continuo::playback::event::ShutdownReport { progress: Progress, events: Vec<PlaybackEvent> }` (D19)
  - `EngineHandle::join(self) -> ShutdownReport`
  - `TestEngine::start_at(name: &str, start_at: Duration) -> Self`
  - `TestEngine::shutdown_report(&mut self) -> Option<ShutdownReport>`
  - `TestEngine::await_commands_taken(&mut self)`

**None of this knows persistence exists.** The engine publishes truth; what the application stores is not its business.

**Do not touch** `tests/engine_contract.rs`. Adding a field to `Loaded` cannot break it: the only pattern match on `Loaded` outside the engine is `Mirror::apply`, and it already ends in `..`.

- [ ] **Step 1: Write the failing engine tests**

Create `tests/engine_shutdown.rs`:

```rust
//! What `join` owes the caller: the position the shutdown captured, and every
//! event the application never got to drain.

use std::time::Duration;

use continuo::playback::command::PlaybackCommand;
use continuo::playback::event::PlaybackEvent;
use continuo::playback::state::PlaybackState;
use continuo::playback::volume::Volume;

mod support;

use support::TestEngine;

const TRACK: &str = "sine-5s.flac";

#[test]
fn join_returns_the_position_the_shutdown_captured() {
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_secs(1));
    let published = engine.position();

    let report = engine.shutdown_report().expect("the engine was still running");

    assert!(
        report.progress.position >= published,
        "the captured position must be at least the last one published: {:?} < {:?}",
        report.progress.position,
        published
    );
    assert!(
        report.progress.position < Duration::from_secs(5),
        "and it must still be a position in this track: {:?}",
        report.progress.position
    );
}

#[test]
fn join_returns_the_events_the_application_never_drained() {
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_secs(1));

    // From here the application is exactly as blind as `app::run` is between
    // pressing `q` and breaking out of its loop.
    engine.stop_draining_events();
    engine.send(PlaybackCommand::SetVolume(Volume::new(0.5)));
    engine.send(PlaybackCommand::SetVolume(Volume::new(0.25)));
    engine.send(PlaybackCommand::Stop);

    // The interrupt is checked before any command is read, so without this the
    // test would be asking after events the worker never produced.
    engine.await_commands_taken();
    // And one flush pass, so all three are in the channel the application is
    // no longer draining. Which side of the handoff they sit on is the next
    // test's subject, not this one's.
    std::thread::sleep(Duration::from_millis(50));

    let report = engine.shutdown_report().expect("the engine was still running");

    let volumes: Vec<f32> = report
        .events
        .iter()
        .filter_map(|event| match event {
            PlaybackEvent::VolumeChanged { volume, .. } => Some(volume.as_gain()),
            _ => None,
        })
        .collect();
    assert_eq!(volumes, vec![0.5, 0.25], "in emission order: {:?}", report.events);

    assert!(
        report.events.iter().any(|event| matches!(
            event,
            PlaybackEvent::StateChanged { state: PlaybackState::Stopped, .. }
        )),
        "the stop must survive the handoff too: {:?}",
        report.events
    );
}

#[test]
fn an_event_racing_the_shutdown_interrupt_still_arrives() {
    let mut engine = TestEngine::start(TRACK);
    engine.stop_draining_events();
    engine.send(PlaybackCommand::SetVolume(Volume::new(0.5)));

    // Wait only for the command to be *taken*, never for its event to be
    // flushed. That the event exists is the premise; whether it is still in the
    // worker's backlog or already in the channel when the interrupt lands is
    // the race, and the report is required to be indifferent to which.
    engine.await_commands_taken();
    let report = engine.shutdown_report().expect("the engine was still running");

    assert!(
        report.events.iter().any(|event| matches!(
            event,
            PlaybackEvent::VolumeChanged { volume, .. } if volume.as_gain() == 0.5
        )),
        "an event emitted just before the interrupt is not the application's to lose: {:?}",
        report.events
    );
}

#[test]
fn a_load_reports_where_it_landed() {
    let mut engine = TestEngine::start_at(TRACK, Duration::from_secs(2));

    let loaded = engine.await_event(|event| matches!(event, PlaybackEvent::Loaded { .. }));
    let PlaybackEvent::Loaded { position, .. } = loaded else {
        panic!("await_event returned the wrong event");
    };
    assert!(
        position >= Duration::from_secs(2) && position < Duration::from_secs(3),
        "Loaded must carry the adopted landing, not zero: {position:?}"
    );
}
```

`a_load_reports_where_it_landed` needs the `Loaded` event to still be in the inbox, so `start_at` must not clear it the way `start` does — see Step 4.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test engine_shutdown 2>&1 | tail -20`
Expected: FAIL to compile — no `shutdown_report`, no `start_at`, and `Loaded` has no `position`.

- [ ] **Step 3: Add the position to `Loaded` and the report type**

In `src/playback/event.rs`, add the field:

```rust
    Loaded {
        session_rev: u64,
        media: MediaId,
        metadata: MediaMetadata,
        capabilities: MediaCapabilities,
        /// Where the load actually landed after its refined seek to `start_at`.
        /// Without it the application cannot report where a resume landed and
        /// would render 00:00:00 after one (D8).
        position: Duration,
    },
```

and append the report type at the end of the same file:

```rust
/// What a shutdown hands back: the position the worker captured on its way out,
/// and every event that never reached the application — whether it was still in
/// the worker's backlog or already in the channel when the interrupt landed
/// (D14, D19).
#[derive(Debug)]
pub struct ShutdownReport {
    pub progress: Progress,
    pub events: Vec<PlaybackEvent>,
}
```

In `src/playback/engine.rs`, populate it at the single emit site inside `fn load`:

```rust
        let session_rev = self.session_rev;
        let position = self.position;
        self.emit(PlaybackEvent::Loaded {
            session_rev,
            media,
            metadata: decoded.metadata().clone(),
            capabilities: decoded.capabilities(),
            position,
        });
```

`self.position` at that point is the adopted landing: `load` pins it to `start_at` before opening and replaces it with `adopt_preserved(start_at, outcome.actual)` if the refined seek ran.

In `src/app.rs`, `Mirror::apply` currently zeroes the position on `Loaded`. Take the reported one instead:

```rust
            PlaybackEvent::Loaded {
                session_rev,
                media,
                metadata,
                position,
                ..
            } => {
                self.session_rev = session_rev;
                self.name = Some(display_name(&media));
                self.duration = metadata.duration;
                self.position = position;
                self.quality = PositionQuality::Exact;
                self.state = PlaybackState::Loading;
            }
```

- [ ] **Step 4: Publish the captured position and return the backlog**

In `src/playback/engine.rs`, `fn shutdown` gains one line:

```rust
    fn shutdown(&mut self) {
        self.capture_position();
        self.teardown();
        self.source = None;
        self.state = PlaybackState::Idle;
        // With the transport gone the recompute branch is skipped, so this
        // publishes precisely the captured position (D14). Without it, the last
        // Progress a reader can see is the one from the previous pass and the
        // capture is unreachable.
        self.publish_progress();
    }
```

`fn run` changes its return type and each of its four exits. The signature becomes:

```rust
    fn run(mut self) -> Vec<PlaybackEvent> {
```

and every `return;` that follows a `self.shutdown()` becomes:

```rust
                self.shutdown();
                return Vec::from(self.pending_events);
```

There are four such sites: the `SHUTDOWN` interrupt in step 1, the `Some(Err(_))` command-disconnect arm, the `if self.shutting_down` check after dispatch, and the `if self.receivers_gone` check at the foot of the loop. `Worker` has no `Drop` impl, so moving `pending_events` out of `self` is allowed.

The spawn site changes only in that the closure now yields a value:

```rust
        let join = std::thread::Builder::new()
            .name("continuo-decode".into())
            .spawn(move || worker.run())
            .ok();
```

so the field's type becomes `worker: Option<JoinHandle<Vec<PlaybackEvent>>>`.

`fn join` becomes:

```rust
    /// Joins the worker, then drains what it left behind.
    ///
    /// The channel drain is safe only because the thread has already gone —
    /// nothing can send again — and that is the same happens-before the final
    /// `Progress` rests on. Channel first, backlog after: everything the worker
    /// flushed was emitted before anything it could not.
    pub fn join(mut self) -> ShutdownReport {
        let backlog = match self.worker.take() {
            Some(worker) => worker.join().unwrap_or_default(),
            None => Vec::new(),
        };
        let mut events = Vec::new();
        while let Ok(event) = self.stream.events.try_recv() {
            events.push(event);
        }
        events.extend(backlog);
        ShutdownReport {
            progress: self.progress(),
            events,
        }
    }
```

`worker.join()` returns `Result<Vec<PlaybackEvent>, Box<dyn Any>>`; `unwrap_or_default()` on a `Result` whose `Ok` is `Vec` yields the empty vec on a panicked worker without tripping the `unwrap_used` lint — it is `Result::unwrap_or_default`, not `unwrap`.

Import `ShutdownReport` in `engine.rs` alongside the existing `event` imports.

- [ ] **Step 5: Extend the harness, additively**

In `tests/support/mod.rs`, add `use continuo::playback::event::ShutdownReport;` to the imports, then refactor `start` to delegate and add the two new methods:

```rust
    pub fn start(name: &str) -> Self {
        let mut engine = Self::start_at(name, Duration::ZERO);
        // The events a start-up emits are not what any test is looking at.
        lock(&engine.inbox).clear();
        engine
    }

    /// A start that resumes at `start_at`. Deliberately does **not** clear the
    /// inbox: the `Loaded` it produces is the subject of the resume tests.
    pub fn start_at(name: &str, start_at: Duration) -> Self {
        // ... the entire existing body of `start`, with `start_at` in place of
        // `Duration::ZERO` in the `Load` command, and WITHOUT the final
        // `lock(&engine.inbox).clear();`
    }

    /// Interrupt and join, handing back what the engine captured on its way
    /// out. `Drop` then finds the handle already taken and skips its own join.
    pub fn shutdown_report(&mut self) -> Option<ShutdownReport> {
        let handle = lock(&self.handle).take()?;
        handle.interrupt_shutdown();
        Some(handle.join())
    }

    /// Block until the worker has taken every queued command.
    ///
    /// The shutdown interrupt is checked at the **top** of the worker's pass,
    /// before it reads any command, so a test that sends and interrupts in the
    /// same breath is asking about events that were never produced. Commands
    /// are dispatched in the same pass they are received, so an empty channel
    /// means the work is done — what is still open, deliberately, is whether
    /// the events it produced have been flushed yet.
    pub fn await_commands_taken(&mut self) {
        let deadline = Instant::now() + PATIENCE;
        while self.pending_commands() > 0 {
            if Instant::now() >= deadline {
                panic!("the worker never took the queued commands");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
```

Move the existing body of `start` into `start_at` verbatim except for those two edits. No existing signature changes and no existing test is touched.

- [ ] **Step 6: Run the new tests to verify they pass**

Run: `cargo test --test engine_shutdown`
Expected: PASS — 4 tests.

- [ ] **Step 7: Prove the contract suite is untouched**

Run: `git diff --stat tests/engine_contract.rs`
Expected: no output — the file is byte-identical (§17).

Run: `cargo test --test engine_contract`
Expected: PASS — the same 20 contract tests as the baseline.

- [ ] **Step 8: Verify the gates**

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --locked`
Expected: clean, 140 passed.

- [ ] **Step 9: Commit**

```bash
git add src/playback/event.rs src/playback/engine.rs src/app.rs tests/support/mod.rs tests/engine_shutdown.rs
git commit -m "Make the shutdown position and the undelivered events reachable

Three facts were unreachable from outside the engine. shutdown captured a
position and returned without publishing it, so the last Progress a reader
could see was the one from the pass before. Loaded carried no position, so
nothing downstream could say where a resume landed. And the worker
discarded its pending events at the shutdown interrupt while the caller
had already stopped draining the channel, so a seek target or an end of
track stored in the last moments of a run had nowhere to go.

join now returns both: the captured position, and every event that never
reached the application, channel first and worker backlog after, which is
the order they were emitted in. Draining the channel is safe there only
because the thread has already been joined.

None of this knows that persistence exists."
```

---

## Task 6: The session policy — ordinary capture, media switch, volume, completion

**Files:**
- Create: `src/session.rs`
- Modify: `src/lib.rs`
- Modify: `src/playback/event.rs` (a `session_rev()` accessor)
- Test: `tests/session_policy.rs`

**Interfaces:**
- Consumes: `ClockSample` (Task 1), `PersistedState` (Task 2), `Urgency` (Task 4), `PlaybackEvent`/`Progress` (Task 5).
- Produces:
  - `continuo::session::{Session, Action, CAPTURE_INTERVAL}`
  - `Session::new(state: PersistedState) -> Self`
  - `Session::observe(&mut self, event: &PlaybackEvent, now: ClockSample) -> Action`
  - `Session::tick(&mut self, progress: &Progress, now: ClockSample) -> Action`
  - `Session::snapshot(&self) -> &PersistedState`
  - `Action::{None, Submit { state: PersistedState, urgency: Urgency }}`
  - `PlaybackEvent::session_rev(&self) -> u64`

**Everything here is pure.** No threads, no tempdirs, no sleeps, no filesystem. The tests call `observe` and `tick` in the order `app::run` calls them and read the returned `Action`.

**The one ordering rule that matters:** the application drains events, then samples `Progress` **once**, and hands that one sample to `tick`. Intermediate positions are discarded by design.

- [ ] **Step 1: Write the failing policy tests**

Create `tests/session_policy.rs`:

```rust
//! The checkpoint policy, driven synchronously.

use std::time::Duration;

use continuo::clock::{Clock, FakeClock};
use continuo::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use continuo::media::id::{AbsolutePath, MediaId};
use continuo::media::metadata::MediaMetadata;
use continuo::persistence::model::PersistedState;
use continuo::persistence::writer::Urgency;
use continuo::playback::event::{PlaybackEvent, Progress};
use continuo::playback::state::PlaybackState;
use continuo::playback::timeline::PositionQuality;
use continuo::playback::volume::Volume;
use continuo::session::{Action, Session};

fn media(name: &str) -> MediaId {
    // A bare helper, so it handles its own error: the lint exemption stops at
    // the `#[test]` boundary.
    match AbsolutePath::new(format!("/music/{name}.flac").into()) {
        Ok(path) => MediaId::LocalFile(path),
        Err(error) => panic!("a literal absolute path must parse: {error}"),
    }
}

fn loaded(session_rev: u64, name: &str, position: Duration) -> PlaybackEvent {
    PlaybackEvent::Loaded {
        session_rev,
        media: media(name),
        metadata: MediaMetadata::default(),
        capabilities: MediaCapabilities {
            continuity: Continuity::Finite,
            seek: SeekSupport::Native,
        },
        position,
    }
}

fn state_changed(session_rev: u64, state: PlaybackState) -> PlaybackEvent {
    PlaybackEvent::StateChanged { session_rev, state }
}

fn progress(session_rev: u64, name: &str, secs: u64) -> Progress {
    Progress {
        session_rev,
        media: Some(media(name)),
        position: Duration::from_secs(secs),
        quality: PositionQuality::Exact,
    }
}

fn submitted(action: Action) -> (PersistedState, Urgency) {
    match action {
        Action::Submit { state, urgency } => (state, urgency),
        Action::None => panic!("expected a submission"),
    }
}

fn is_none(action: &Action) -> bool {
    matches!(action, Action::None)
}

/// A session already playing `a`, with the clock parked at the moment playback
/// started. Returns the session and the clock that drives it.
fn playing(name: &str) -> (Session, FakeClock) {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let _ = session.observe(&loaded(1, name, Duration::ZERO), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());
    (session, clock)
}

#[test]
fn five_seconds_of_playback_becomes_an_ordinary_submission() {
    let (mut session, clock) = playing("a");

    clock.advance(Duration::from_millis(4_900));
    assert!(is_none(&session.tick(&progress(1, "a", 4), clock.sample())), "4.9 s is not yet due");

    clock.advance(Duration::from_millis(100));
    let (state, urgency) = submitted(session.tick(&progress(1, "a", 5), clock.sample()));
    assert_eq!(urgency, Urgency::Ordinary);
    assert_eq!(
        state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(5),
        "the position is the tick's own sample"
    );
}

#[test]
fn the_interval_restarts_after_each_capture() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 5), clock.sample()));

    clock.advance(Duration::from_secs(4));
    assert!(is_none(&session.tick(&progress(1, "a", 9), clock.sample())));
    clock.advance(Duration::from_secs(1));
    let _ = submitted(session.tick(&progress(1, "a", 10), clock.sample()));
}

#[test]
fn a_wall_clock_that_jumps_backwards_does_not_disturb_the_interval() {
    let (mut session, clock) = playing("a");
    clock.advance_monotonic(Duration::from_secs(5));
    // An hour backwards on the wall, mid-interval.
    clock.set_wall(time::OffsetDateTime::UNIX_EPOCH - Duration::from_secs(3600));

    let (state, _) = submitted(session.tick(&progress(1, "a", 5), clock.sample()));
    assert_eq!(
        state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(5),
        "deadlines read the monotonic hand; only updated_at reads the wall"
    );
}

#[test]
fn no_ordinary_capture_happens_while_paused() {
    let (mut session, clock) = playing("a");
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    // Whatever the pause itself is worth, it is worth it once. Task 7 makes
    // this first tick resolve a forced checkpoint; the claim here is only that
    // nothing keeps firing behind it.
    let _ = session.tick(&progress(1, "a", 5), clock.sample());

    clock.advance(Duration::from_secs(30));
    assert!(is_none(&session.tick(&progress(1, "a", 5), clock.sample())));
}

#[test]
fn a_sample_from_a_session_the_policy_is_not_tracking_is_ignored() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(10));
    assert!(
        is_none(&session.tick(&progress(7, "a", 10), clock.sample())),
        "a stale revision must not move a checkpoint"
    );
}

#[test]
fn a_revision_is_adopted_from_an_event_the_policy_otherwise_ignores() {
    // §7: a DeviceRecovered can be dropped when the backlog is full, so the
    // revision must be adopted from every event, not only the acted-on ones.
    let (mut session, clock) = playing("a");
    let _ = session.observe(
        &PlaybackEvent::Warning {
            session_rev: 9,
            message: "a device warning".into(),
        },
        clock.sample(),
    );
    clock.advance(Duration::from_secs(5));
    let (state, _) = submitted(session.tick(&progress(9, "a", 5), clock.sample()));
    assert_eq!(
        state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(5)
    );
}

#[test]
fn a_volume_change_submits_at_ordinary_urgency_and_touches_no_checkpoint() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(2));
    let _ = session.tick(&progress(1, "a", 2), clock.sample());

    let (state, urgency) = submitted(session.observe(
        &PlaybackEvent::VolumeChanged {
            session_rev: 1,
            volume: Volume::new(0.25),
        },
        clock.sample(),
    ));
    assert_eq!(urgency, Urgency::Ordinary);
    assert_eq!(state.volume(), Volume::new(0.25));
    assert!(
        state.checkpoint_for(&media("a")).is_none(),
        "volume is not a position"
    );
}

#[test]
fn end_of_track_records_the_events_own_position_and_marks_completion() {
    let (mut session, clock) = playing("a");
    let (state, urgency) = submitted(session.observe(
        &PlaybackEvent::EndOfTrack {
            session_rev: 1,
            position: Duration::from_secs(240),
        },
        clock.sample(),
    ));
    assert_eq!(urgency, Urgency::Forced);
    assert!(state.completed_for(&media("a")));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(240),
        "D1 retains the position"
    );
}

#[test]
fn a_media_switch_produces_one_snapshot_carrying_both_halves() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));

    let (state, urgency) = submitted(session.observe(&loaded(2, "b", Duration::ZERO), clock.sample()));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(93),
        "the outgoing entry comes from last_sample; load() has already overwritten the engine's position"
    );
    assert_eq!(
        state.current_media,
        Some(media("b")),
        "and the move of current_media is the same mutation"
    );
}

#[test]
fn reloading_the_same_media_submits_nothing() {
    let (mut session, clock) = playing("a");
    assert!(is_none(&session.observe(&loaded(2, "a", Duration::from_secs(30)), clock.sample())));
}

#[test]
fn playing_clears_a_completed_flag_carried_in_from_the_file() {
    let mut opening = PersistedState::default();
    opening.record(
        &continuo::playback::checkpoint::PlaybackCheckpoint {
            media: media("a"),
            position: Duration::from_secs(240),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        true,
    );

    let clock = FakeClock::new();
    let mut session = Session::new(opening);
    let _ = session.observe(&loaded(1, "a", Duration::ZERO), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());

    clock.advance(Duration::from_secs(5));
    let (state, _) = submitted(session.tick(&progress(1, "a", 5), clock.sample()));
    assert!(
        !state.completed_for(&media("a")),
        "§12: a successful establishment after a completed state clears it"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test session_policy 2>&1 | tail -20`
Expected: FAIL to compile — `unresolved import continuo::session`.

- [ ] **Step 3: Add the revision accessor**

In `src/playback/event.rs`, inside `impl PlaybackEvent`:

```rust
    /// Every event carries the revision it was emitted under. A reader that
    /// adopts it from **every** event — not only the ones it acts on — bounds
    /// its exposure to a dropped `DeviceRecovered` to "until the next event of
    /// any kind" (§7).
    pub fn session_rev(&self) -> u64 {
        match self {
            Self::Loaded { session_rev, .. }
            | Self::StateChanged { session_rev, .. }
            | Self::SeekCompleted { session_rev, .. }
            | Self::SeekTargetStored { session_rev, .. }
            | Self::SeekRejected { session_rev, .. }
            | Self::VolumeChanged { session_rev, .. }
            | Self::EndOfTrack { session_rev, .. }
            | Self::DeviceRecovered { session_rev }
            | Self::Warning { session_rev, .. }
            | Self::Failed { session_rev, .. } => *session_rev,
        }
    }
```

- [ ] **Step 4: Write the session**

Create `src/session.rs`:

```rust
//! The checkpoint policy: what to persist, and when.
//!
//! Pure by construction — no I/O, no threads, no clock of its own. Every
//! decision is a function of the lossless event stream, the single `Progress`
//! sample the application takes per iteration, and an injected `ClockSample`.
//!
//! The application's order is load-bearing: drain events, then sample once.
//! §3 establishes that a command's event reaches the application on the pass
//! *after* the one that applied it, and that the pass publishes progress before
//! it flushes events — so the sample that follows a transition event is
//! strictly newer than the transition it reports.

use std::time::{Duration, Instant};

use crate::clock::ClockSample;
use crate::media::id::MediaId;
use crate::persistence::model::{PersistedCheckpoint, PersistedState};
use crate::persistence::writer::Urgency;
use crate::playback::checkpoint::PlaybackCheckpoint;
use crate::playback::event::{PlaybackEvent, Progress};
use crate::playback::state::PlaybackState;

/// The capture interval §6 requires while playing. With the writer's 2 s
/// coalescing window it bounds worst-case loss at 7 s.
pub const CAPTURE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum Action {
    None,
    Submit {
        state: PersistedState,
        urgency: Urgency,
    },
}

/// The position the policy would use for a checkpoint it can no longer sample.
struct Sample {
    session_rev: u64,
    media: MediaId,
    position: Duration,
}

pub struct Session {
    /// The authoritative state. `Action::Submit` carries a clone, which becomes
    /// the writer's property; the session never shares a reference into it.
    state: PersistedState,
    session_rev: u64,
    playback: PlaybackState,
    current_media: Option<MediaId>,
    /// Tracked for the current media, so a checkpoint written before any
    /// further event still carries the right completion.
    completed: bool,
    last_sample: Option<Sample>,
    /// Monotonic anchor for the 5 s rule; set when playback establishes.
    last_capture: Option<Instant>,
}

impl Session {
    pub fn new(state: PersistedState) -> Self {
        Self {
            state,
            session_rev: 0,
            playback: PlaybackState::Idle,
            // Learned from `Loaded`, never from the file: what was current last
            // run says nothing about what this run is playing.
            current_media: None,
            completed: false,
            last_sample: None,
            last_capture: None,
        }
    }

    pub fn snapshot(&self) -> &PersistedState {
        &self.state
    }

    pub fn observe(&mut self, event: &PlaybackEvent, now: ClockSample) -> Action {
        self.session_rev = event.session_rev();

        match event {
            PlaybackEvent::Loaded { media, position, .. } => self.on_loaded(media, *position, now),
            PlaybackEvent::StateChanged { state, .. } => self.on_state(*state, now),
            PlaybackEvent::VolumeChanged { volume, .. } => {
                self.state.set_volume(*volume);
                self.submit(Urgency::Ordinary)
            }
            PlaybackEvent::EndOfTrack { position, .. } => {
                self.completed = true;
                self.record_current(*position, now);
                self.submit(Urgency::Forced)
            }
            _ => Action::None,
        }
    }

    pub fn tick(&mut self, progress: &Progress, now: ClockSample) -> Action {
        // The same guard `Mirror` already applies: never persist a position
        // from a session the policy was not tracking.
        if progress.session_rev != self.session_rev {
            return Action::None;
        }
        let Some(media) = self.current_media.clone() else {
            return Action::None;
        };
        self.last_sample = Some(Sample {
            session_rev: progress.session_rev,
            media,
            position: progress.position,
        });

        if self.playback != PlaybackState::Playing {
            return Action::None;
        }
        let due = self
            .last_capture
            .is_none_or(|last| now.monotonic.duration_since(last) >= CAPTURE_INTERVAL);
        if !due {
            return Action::None;
        }
        self.last_capture = Some(now.monotonic);
        self.record_current(progress.position, now);
        self.submit(Urgency::Ordinary)
    }

    fn on_state(&mut self, state: PlaybackState, now: ClockSample) -> Action {
        self.playback = state;
        if state == PlaybackState::Playing {
            // §12: a successful establishment after a completed state clears
            // it. Persistence restoration alone does not.
            self.completed = false;
            self.last_capture = Some(now.monotonic);
        }
        Action::None
    }

    /// A `Loaded` for a different media is **one** snapshot: the outgoing entry
    /// is recorded from `last_sample` and `current_media` moves in a single
    /// mutation. A keep-latest slot cannot promise that an intermediate
    /// submission reaches disk, so "flush, then move" is unenforceable — and
    /// unnecessary, since the snapshot is the whole state.
    fn on_loaded(&mut self, media: &MediaId, position: Duration, now: ClockSample) -> Action {
        let switching = self.current_media.as_ref() != Some(media);
        if !switching {
            self.last_sample = Some(Sample {
                session_rev: self.session_rev,
                media: media.clone(),
                position,
            });
            return Action::None;
        }

        // §3: `load()` overwrites the engine's position with `start_at` before
        // anything publishes, so the outgoing media's final position is only
        // reachable from what the session retained.
        if let Some(previous) = self.last_sample.take() {
            let completed = self.completed;
            self.state.record(
                &PlaybackCheckpoint {
                    media: previous.media,
                    position: previous.position,
                    updated_at: now.wall,
                },
                completed,
            );
        }

        self.current_media = Some(media.clone());
        self.state.current_media = Some(media.clone());
        self.completed = self.state.completed_for(media);
        self.last_sample = Some(Sample {
            session_rev: self.session_rev,
            media: media.clone(),
            position,
        });
        self.last_capture = None;
        self.submit(Urgency::Forced)
    }

    fn record_current(&mut self, position: Duration, now: ClockSample) {
        let Some(media) = self.current_media.clone() else {
            return;
        };
        let completed = self.completed;
        self.state.record(
            &PlaybackCheckpoint {
                media,
                position,
                updated_at: now.wall,
            },
            completed,
        );
    }

    fn submit(&self, urgency: Urgency) -> Action {
        Action::Submit {
            state: self.state.clone(),
            urgency,
        }
    }
}
```

Add `pub mod session;` to `src/lib.rs`, after `pub mod playback;`.

The `Sample.session_rev` field is not read yet — Task 7 reads it in `shutdown_snapshot`. If `dead_code` trips clippy before then, keep the field and the warning will clear in Task 7; do not delete it.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test session_policy`
Expected: PASS — 11 tests.

- [ ] **Step 6: Verify the gates**

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --locked`
Expected: clean, 151 passed.

- [ ] **Step 7: Commit**

```bash
git add src/session.rs src/lib.rs src/playback/event.rs tests/session_policy.rs
git commit -m "Add the checkpoint policy: capture, media switch, volume, completion

Pure: no I/O, no threads, no clock of its own. The application drains the
event stream, samples progress once, and hands both to this. What the
policy persists is the engine's canonical position, never a decoder's
claim about where a seek landed.

The revision is adopted from every event rather than only the acted-on
ones. A DeviceRecovered can be dropped when the backlog is full, and a
policy whose revision has fallen behind rejects every sample that follows
-- which would suspend checkpointing silently for the rest of the run.

A media switch is one snapshot, not a flush and then a move. The outgoing
entry has to come from the position the session retained, because load
overwrites the engine's own before anything publishes, and a keep-latest
slot cannot promise that an intermediate submission reaches disk."
```

---

## Task 7: Pending forces, the outstanding target, and the establishment gate

**Files:**
- Modify: `src/session.rs`
- Test: `tests/session_policy.rs` (append)

**Interfaces:**
- Consumes: everything from Task 6.
- Produces:
  - `Session::shutdown_snapshot(&mut self, progress: &Progress, now: ClockSample) -> PersistedState`
  - `continuo::session::Trigger::{Paused, Stopped, SeekCompleted}`

**The three rules this task adds, and why each exists:**

- **D13, pending force.** `observe` runs *before* the iteration's single `Progress` sample, so it has no position to build a snapshot from. Pause, stop and `SeekCompleted` therefore raise a force keyed by the revision the event carries, which the same iteration's `tick` resolves. That is not a delay: §3's pass ordering guarantees the sample that follows is *newer* than the transition. A newer revision **re-keys** the force rather than dropping it — `rebuild` bumps the revision on device recovery with the position continuous across it, and dropping the force would lose a real pause for good, because no ordinary trigger fires while paused. Only `Loaded` retires a force, and its own handling has already recorded the outgoing entry.
- **D17, outstanding target.** A stopped seek stores a target and leaves the engine's position where it was, so the next position-derived checkpoint writes the pre-seek value back over it. The target therefore supersedes `Progress.position` until the engine **resolves** it — `SeekCompleted`, `StateChanged{Playing}`, `Loaded` or `EndOfTrack`. `Playing` is in that set because `restart()` *discards* the target and starts at zero without emitting a `SeekCompleted` at all.
- **D20, establishment gate.** §11 maps `completed`, `position == duration` and `position > duration` all to `start_at = 0` while retaining a position, and `load()` emits `Loaded` before opening the device. A device-open failure therefore ends at `Failed` reporting zero, with the session already knowing the media. Without the gate, the shutdown force writes that zero over the retained position.

- [ ] **Step 1: Write the failing tests**

Append to `tests/session_policy.rs`:

```rust
// --------------------------------------------------- pending forces (D13)

#[test]
fn a_pause_from_playing_is_resolved_by_the_same_iterations_tick() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(2));

    // The application drains events first...
    assert!(is_none(&session.observe(&state_changed(1, PlaybackState::Paused), clock.sample())));
    // ...then samples once, and that sample is newer than the transition.
    let (state, urgency) = submitted(session.tick(&progress(1, "a", 2), clock.sample()));

    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(2)
    );
}

#[test]
fn a_pause_that_interrupts_no_playback_raises_nothing() {
    // Every launch emits a StateChanged{Paused} nobody asked for, before the
    // queued Play is dispatched. Ungated, that would checkpoint the resume
    // landing on every launch — and zero it for a completed entry.
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let _ = session.observe(&loaded(1, "a", Duration::ZERO), clock.sample());

    assert!(is_none(&session.observe(&state_changed(1, PlaybackState::Paused), clock.sample())));
    assert!(is_none(&session.tick(&progress(1, "a", 0), clock.sample())));
}

#[test]
fn a_stop_raises_a_force_that_the_tick_resolves() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(2));
    assert!(is_none(&session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample())));

    let (state, urgency) = submitted(session.tick(&progress(2, "a", 93), clock.sample()));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(93)
    );
}

#[test]
fn a_seek_persists_the_canonical_position_never_the_events_actual() {
    let (mut session, clock) = playing("a");
    let seek = PlaybackEvent::SeekCompleted {
        session_rev: 1,
        requested: Duration::from_secs(60),
        // A landing the M1 debt entry says can disagree with the position.
        actual: Duration::from_secs(59),
        refinement_truncated: false,
    };
    assert!(is_none(&session.observe(&seek, clock.sample())));

    let (state, urgency) = submitted(session.tick(&progress(1, "a", 60), clock.sample()));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(60),
        "D6 sidesteps the debt by never persisting SeekCompleted.actual"
    );
}

#[test]
fn a_force_is_rekeyed_across_a_device_recovery_and_still_resolves() {
    let (mut session, clock) = playing("a");
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    // rebuild bumps the revision, with the position continuous across it.
    let _ = session.observe(&PlaybackEvent::DeviceRecovered { session_rev: 2 }, clock.sample());

    let (state, urgency) = submitted(session.tick(&progress(2, "a", 40), clock.sample()));
    assert_eq!(urgency, Urgency::Forced, "a real pause must not be lost to an unrelated fault");
    assert_eq!(
        state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(40)
    );
}

#[test]
fn a_load_retires_a_force_raised_against_the_previous_media() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());

    // The load replaces the media; its own handling already recorded `a`.
    let (state, _) = submitted(session.observe(&loaded(2, "b", Duration::ZERO), clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(93)
    );

    assert!(
        is_none(&session.tick(&progress(2, "b", 1), clock.sample())),
        "the retired force must not fire against the new media"
    );
}

// --------------------------------------------- the outstanding target (D17)

/// play → 93 s → stop → seek to 30 s → quit. The sequence D7 exists for.
#[test]
fn a_stopped_seek_target_survives_the_shutdown_force() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.tick(&progress(2, "a", 93), clock.sample());

    let (state, urgency) = submitted(session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    ));
    assert_eq!(urgency, Urgency::Forced);
    assert_eq!(
        state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(30)
    );

    // The engine's canonical position still reads the pre-seek value, because a
    // stopped seek deliberately does not move it.
    let final_state = session.shutdown_snapshot(&progress(2, "a", 93), clock.sample());
    assert_eq!(
        final_state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(30),
        "the target supersedes Progress.position until the engine resolves it"
    );
}

/// stop → seek to 30 s → Home → play → quit. `restart()` discards the target
/// and announces Playing with no SeekCompleted, so Playing has to clear it.
#[test]
fn a_restart_clears_the_target_it_discarded() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.tick(&progress(2, "a", 93), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    );

    // Home: restart() seeks to zero, clears its own target and announces
    // Playing. No SeekCompleted is emitted for it, ever.
    let _ = session.observe(&state_changed(2, PlaybackState::Playing), clock.sample());
    clock.advance(Duration::from_secs(5));
    let (state, _) = submitted(session.tick(&progress(2, "a", 5), clock.sample()));
    assert_eq!(
        state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(5),
        "an ordinary checkpoint after a restart uses the sample, not the discarded target"
    );

    let final_state = session.shutdown_snapshot(&progress(2, "a", 7), clock.sample());
    assert_eq!(
        final_state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(7)
    );
}

#[test]
fn a_resumed_stopped_seek_clears_the_target_through_its_seek_completed() {
    let (mut session, clock) = playing("a");
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    );
    // `restore()` validates the stored target and emits the SeekCompleted the
    // caller has been waiting for.
    let _ = session.observe(
        &PlaybackEvent::SeekCompleted {
            session_rev: 2,
            requested: Duration::from_secs(30),
            actual: Duration::from_secs(30),
            refinement_truncated: false,
        },
        clock.sample(),
    );
    let (state, _) = submitted(session.tick(&progress(2, "a", 31), clock.sample()));
    assert_eq!(
        state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(31)
    );
}

// ------------------------------------------- the establishment gate (D20)

#[test]
fn a_launch_that_never_establishes_writes_no_checkpoint() {
    // A completed entry: §11 resumes it at 0, load() emits Loaded before
    // opening the device, and the device refuses to open.
    let mut opening = PersistedState::default();
    opening.record(
        &continuo::playback::checkpoint::PlaybackCheckpoint {
            media: media("a"),
            position: Duration::from_secs(240),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        true,
    );

    let clock = FakeClock::new();
    let mut session = Session::new(opening);
    let _ = session.observe(&loaded(1, "a", Duration::ZERO), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::Failed {
            session_rev: 1,
            message: "cannot open the audio device".into(),
        },
        clock.sample(),
    );
    let _ = session.tick(&progress(1, "a", 0), clock.sample());

    let final_state = session.shutdown_snapshot(&progress(1, "a", 0), clock.sample());
    assert_eq!(
        final_state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(240),
        "a failed establishment must not overwrite the position D1 retains"
    );
    assert!(final_state.completed_for(&media("a")));
}

#[test]
fn a_launch_that_never_establishes_still_writes_volume_and_current_media() {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let _ = session.observe(&loaded(1, "a", Duration::ZERO), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::VolumeChanged {
            session_rev: 1,
            volume: Volume::new(0.25),
        },
        clock.sample(),
    );

    let final_state = session.shutdown_snapshot(&progress(1, "a", 0), clock.sample());
    assert_eq!(final_state.volume(), Volume::new(0.25));
    assert_eq!(final_state.current_media, Some(media("a")));
    assert!(
        final_state.entry_for(&media("a")).is_none(),
        "neither of those is a position claim"
    );
}

#[test]
fn a_second_load_cannot_inherit_the_first_ones_establishment() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));

    // A new media that fails to open after Loaded.
    let _ = session.observe(&loaded(2, "b", Duration::ZERO), clock.sample());
    let final_state = session.shutdown_snapshot(&progress(2, "b", 0), clock.sample());
    assert!(final_state.entry_for(&media("b")).is_none());
    assert_eq!(
        final_state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(93),
        "and the outgoing entry the load recorded stands"
    );
}

#[test]
fn a_media_switch_carries_the_outgoing_stopped_seek_target_out_with_it() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));
    let _ = session.observe(&state_changed(2, PlaybackState::Stopped), clock.sample());
    let _ = session.tick(&progress(2, "a", 93), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored {
            session_rev: 2,
            target: Duration::from_secs(30),
        },
        clock.sample(),
    );

    let (state, _) = submitted(session.observe(&loaded(3, "b", Duration::ZERO), clock.sample()));
    assert_eq!(
        state.entry_for(&media("a")).unwrap().position,
        Duration::from_secs(30),
        "the outgoing entry is recorded from the effective position, not the pre-seek sample"
    );
}

#[test]
fn a_media_switch_does_not_walk_a_completed_entry_backwards() {
    let (mut session, clock) = playing("a");
    let _ = submitted(session.observe(
        &PlaybackEvent::EndOfTrack {
            session_rev: 1,
            position: Duration::from_secs(240),
        },
        clock.sample(),
    ));
    // A tick after the end can only report a position at or behind the one
    // EndOfTrack already recorded.
    let _ = session.tick(&progress(1, "a", 239), clock.sample());

    let (state, _) = submitted(session.observe(&loaded(2, "b", Duration::ZERO), clock.sample()));
    let entry = state.entry_for(&media("a")).unwrap();
    assert_eq!(entry.position, Duration::from_secs(240), "D1 retains what it retained");
    assert!(entry.completed, "and the switch does not clear it either");
}

#[test]
fn the_shutdown_snapshot_refuses_a_position_from_a_session_it_was_not_tracking() {
    let (mut session, clock) = playing("a");
    clock.advance(Duration::from_secs(5));
    let _ = submitted(session.tick(&progress(1, "a", 93), clock.sample()));

    // A final Progress carrying a revision the policy never learned.
    let final_state = session.shutdown_snapshot(&progress(99, "a", 5), clock.sample());
    assert_eq!(
        final_state.checkpoint_for(&media("a")).unwrap().position,
        Duration::from_secs(93),
        "it falls back to last_sample rather than trusting the stranger"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test session_policy 2>&1 | tail -20`
Expected: FAIL to compile — no `shutdown_snapshot`; and the pending-force tests fail at runtime with "expected a submission".

- [ ] **Step 3: Add the three rules to the session**

In `src/session.rs`, add the trigger type near `Action`:

```rust
/// Which transition raised a pending force. Carried for diagnostics; the policy
/// treats all three identically.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Trigger {
    Paused,
    Stopped,
    SeekCompleted,
}
```

Add three fields to `Session` and initialize them in `new`:

```rust
    /// Raised by an event that forces a checkpoint but carries no position,
    /// keyed by the revision that event carried (D13).
    pending_force: Option<(u64, Trigger)>,
    /// A stopped seek's target, which supersedes `Progress.position` until the
    /// engine resolves it (D17).
    outstanding_target: Option<Duration>,
    /// Whether playback established, or the position changed explicitly, at the
    /// current revision. Gates the shutdown checkpoint (D20).
    established: bool,
```

```rust
            pending_force: None,
            outstanding_target: None,
            established: false,
```

Replace `observe` with:

```rust
    pub fn observe(&mut self, event: &PlaybackEvent, now: ClockSample) -> Action {
        let session_rev = event.session_rev();
        self.session_rev = session_rev;
        // A newer revision re-keys a pending force rather than dropping it:
        // `rebuild` bumps the revision on device recovery with the position
        // continuous across it, so the force is still answerable — and dropping
        // it would lose a real pause for good, since no ordinary trigger fires
        // while paused. `Loaded` is the one exception, retired in `on_loaded`.
        if let Some((_, trigger)) = self.pending_force.take() {
            self.pending_force = Some((session_rev, trigger));
        }

        match event {
            PlaybackEvent::Loaded { media, position, .. } => self.on_loaded(media, *position, now),
            PlaybackEvent::StateChanged { state, .. } => self.on_state(*state, now),
            PlaybackEvent::SeekCompleted { .. } => {
                self.resolve_target();
                self.established = true;
                self.completed = false;
                self.pending_force = Some((session_rev, Trigger::SeekCompleted));
                Action::None
            }
            PlaybackEvent::SeekTargetStored { target, .. } => {
                self.outstanding_target = Some(*target);
                self.established = true;
                self.record_current(*target, now);
                self.submit(Urgency::Forced)
            }
            PlaybackEvent::VolumeChanged { volume, .. } => {
                self.state.set_volume(*volume);
                self.submit(Urgency::Ordinary)
            }
            PlaybackEvent::EndOfTrack { position, .. } => {
                self.resolve_target();
                self.established = true;
                self.completed = true;
                self.record_current(*position, now);
                self.submit(Urgency::Forced)
            }
            _ => Action::None,
        }
    }
```

Replace `on_state` with:

```rust
    fn on_state(&mut self, state: PlaybackState, now: ClockSample) -> Action {
        let previous = self.playback;
        self.playback = state;
        match state {
            PlaybackState::Playing => {
                // The engine can resolve a stored target by *discarding* it:
                // `restart()` clears it, seeks to zero and lands here with no
                // SeekCompleted ever emitted (D17).
                self.resolve_target();
                self.established = true;
                // §12: a successful establishment after a completed state
                // clears it. Persistence restoration alone does not.
                self.completed = false;
                self.last_capture = Some(now.monotonic);
            }
            // A pause that interrupts no playback is not a checkpoint: every
            // launch emits one before the queued Play is dispatched.
            PlaybackState::Paused if previous == PlaybackState::Playing => {
                self.pending_force = Some((self.session_rev, Trigger::Paused));
            }
            // `do_stop` returns early from Idle, Stopped and Failed, so this
            // event only exists when something was actually running.
            PlaybackState::Stopped => {
                self.pending_force = Some((self.session_rev, Trigger::Stopped));
            }
            _ => {}
        }
        Action::None
    }
```

Replace `on_loaded` outright. The order is the whole point: the outgoing entry
is recorded **before** the per-media flags reset, and from the *effective*
position rather than the raw sample.

```rust
    /// A `Loaded` for a different media is **one** snapshot: the outgoing entry
    /// is recorded from `last_sample` and `current_media` moves in a single
    /// mutation. A keep-latest slot cannot promise that an intermediate
    /// submission reaches disk, so "flush, then move" is unenforceable — and
    /// unnecessary, since the snapshot is the whole state.
    fn on_loaded(&mut self, media: &MediaId, position: Duration, now: ClockSample) -> Action {
        let switching = self.current_media.as_ref() != Some(media);

        // Recorded before anything resets, and through `position_for`: a
        // stopped seek's target is the outgoing media's real position, and
        // clearing it first would write the pre-seek sample back over it. A
        // completed entry is left alone entirely — `EndOfTrack` already
        // recorded the position D1 retains, and `last_sample` can only be
        // behind it.
        let outgoing = if switching { self.last_sample.take() } else { None };
        if let Some(previous) = outgoing
            && !self.completed
        {
            // §3: `load()` overwrites the engine's own position with `start_at`
            // before anything publishes, so this is the only place the outgoing
            // media's final position still exists.
            let position = self.position_for(previous.position);
            self.state.record(
                &PlaybackCheckpoint {
                    media: previous.media,
                    position,
                    updated_at: now.wall,
                },
                false,
            );
        }

        // Only now: a force raised against the previous media cannot answer for
        // this one, and none of these carry across a load.
        self.pending_force = None;
        self.resolve_target();
        self.established = false;
        self.last_capture = None;

        if switching {
            self.current_media = Some(media.clone());
            self.state.current_media = Some(media.clone());
        }
        self.completed = self.state.completed_for(media);
        self.last_sample = Some(Sample {
            session_rev: self.session_rev,
            media: media.clone(),
            position,
        });

        if switching {
            self.submit(Urgency::Forced)
        } else {
            Action::None
        }
    }
```

In `tick`, resolve a pending force before the ordinary check. Insert immediately after `self.last_sample = Some(...)`:

```rust
        if let Some((rev, _trigger)) = self.pending_force
            && rev == progress.session_rev
        {
            self.pending_force = None;
            self.last_capture = Some(now.monotonic);
            let position = self.position_for(progress.position);
            self.record_current(position, now);
            return self.submit(Urgency::Forced);
        }
```

Add the three helpers:

```rust
    fn resolve_target(&mut self) {
        self.outstanding_target = None;
    }

    /// A stopped seek stores a target and leaves the engine's position where it
    /// was, so any position-derived checkpoint that follows would write the
    /// pre-seek value back over it (D17).
    fn position_for(&self, sampled: Duration) -> Duration {
        self.outstanding_target.unwrap_or(sampled)
    }

    /// The final snapshot. Records a checkpoint for the current media only once
    /// something established at this revision (D20), and never a position from
    /// a session the policy was not tracking.
    pub fn shutdown_snapshot(&mut self, progress: &Progress, now: ClockSample) -> PersistedState {
        let Some(media) = self.current_media.clone() else {
            return self.state.clone();
        };
        if !self.established {
            return self.state.clone();
        }
        let sampled = if progress.session_rev == self.session_rev {
            Some(progress.position)
        } else {
            self.last_sample
                .as_ref()
                .filter(|sample| sample.session_rev == self.session_rev && sample.media == media)
                .map(|sample| sample.position)
        };
        let Some(sampled) = sampled else {
            return self.state.clone();
        };
        let position = self.position_for(sampled);
        self.record_current(position, now);
        self.state.clone()
    }
```

`record_current` already reads `self.current_media` and `self.completed`; the only thing the shutdown path adds is `position_for`, which is where an outstanding target wins over the sampled position.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test session_policy`
Expected: PASS — 27 tests.

If `a_restart_clears_the_target_it_discarded` fails at the ordinary checkpoint, check that `on_state` calls `resolve_target()` **before** anything reads `position_for`.

- [ ] **Step 5: Verify the gates**

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --locked`
Expected: clean, 167 passed.

- [ ] **Step 6: Commit**

```bash
git add src/session.rs tests/session_policy.rs
git commit -m "Add pending forces, the outstanding seek target, and the shutdown gate

observe runs before the iteration's single Progress sample, so a pause, a
stop and a completed seek raise a force keyed by the revision the event
carried, which that iteration's tick resolves. That is not a delay: the
worker publishes progress before it flushes events, so the sample that
follows a transition is newer than the transition. A newer revision
re-keys the force instead of dropping it, because a device recovery bumps
the revision with the position continuous across it and a dropped pause
would never come back -- nothing ordinary fires while paused.

A seek taken while stopped stores a target the engine deliberately does
not move its position for, so the target supersedes the sampled position
until the engine resolves it. Resolving is not the same as adopting:
restart discards the target and reaches Playing without ever emitting a
SeekCompleted, which is why Playing is in the clearing set alongside it.

The final snapshot records a position only once something established at
the current revision. A completed entry resumes at zero, and load reports
Loaded before it opens the device -- so a device that refuses to open
leaves a session that knows the media, reports zero, and would otherwise
write that zero over the position completion deliberately retains."
```

---

## Task 8: Resume validation

**Files:**
- Modify: `src/session.rs`

**Interfaces:**
- Consumes: `PersistedCheckpoint` (Task 2).
- Produces:
  - `continuo::session::{decide_resume, ResumeDecision}`
  - `decide_resume(entry: Option<&PersistedCheckpoint>, duration: Option<Duration>) -> ResumeDecision`
  - `ResumeDecision::start_at(&self) -> Duration`

**Why it is a pure function:** §11 applies this against the duration from the probe `app::run` already performs, so it opens no file of its own. Keeping it separate from `app` is what makes the whole table testable without a terminal or a device.

**No near-end heuristic anywhere.** `position == duration` is preserved in *storage* as distinct from `completed`, but is not a usable *start*: seeking exactly to EOF is degenerate. Completion is never inferred from `position >= duration`.

- [ ] **Step 1: Write the failing tests**

Append to `src/session.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    /// Deliberately not called `entry`: the tests bind their subject to
    /// `entry`, and a helper of the same name would be shadowed out of reach
    /// the moment a test needed a second one.
    fn stored(secs: u64, completed: bool) -> PersistedCheckpoint {
        PersistedCheckpoint {
            position: Duration::from_secs(secs),
            completed,
            touch_seq: 1,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn secs(value: u64) -> Option<Duration> {
        Some(Duration::from_secs(value))
    }

    #[test]
    fn no_entry_starts_at_the_beginning() {
        assert_eq!(decide_resume(None, secs(300)), ResumeDecision::NoEntry);
        assert_eq!(decide_resume(None, secs(300)).start_at(), Duration::ZERO);
    }

    #[test]
    fn a_completed_entry_declines_the_resume_without_losing_its_position() {
        let entry = stored(300, true);
        assert_eq!(decide_resume(Some(&entry), secs(300)), ResumeDecision::Completed);
        assert_eq!(decide_resume(Some(&entry), secs(300)).start_at(), Duration::ZERO);
        // And a completed entry short of the end declines just the same:
        // completion is a fact the engine reported, never one inferred here.
        let short = stored(120, true);
        assert_eq!(decide_resume(Some(&short), secs(300)), ResumeDecision::Completed);
    }

    #[test]
    fn an_ordinary_position_inside_the_media_is_the_start() {
        let entry = stored(93, false);
        assert_eq!(
            decide_resume(Some(&entry), secs(300)),
            ResumeDecision::Resume(Duration::from_secs(93))
        );
    }

    #[test]
    fn a_position_of_zero_is_a_start_rather_than_a_resume() {
        let entry = stored(0, false);
        assert_eq!(decide_resume(Some(&entry), secs(300)), ResumeDecision::AtStart);
    }

    #[test]
    fn a_position_exactly_at_the_end_is_degenerate_not_a_start() {
        let entry = stored(300, false);
        assert_eq!(decide_resume(Some(&entry), secs(300)), ResumeDecision::DegenerateEnd);
        assert_eq!(decide_resume(Some(&entry), secs(300)).start_at(), Duration::ZERO);
    }

    #[test]
    fn a_position_past_the_end_is_stale_state() {
        let entry = stored(400, false);
        assert_eq!(decide_resume(Some(&entry), secs(300)), ResumeDecision::StalePastEnd);
        assert_eq!(decide_resume(Some(&entry), secs(300)).start_at(), Duration::ZERO);
    }

    #[test]
    fn an_unknown_duration_keeps_the_position_unvalidated() {
        let entry = stored(93, false);
        assert_eq!(
            decide_resume(Some(&entry), None),
            ResumeDecision::Unvalidated(Duration::from_secs(93))
        );
        assert_eq!(
            decide_resume(Some(&entry), None).start_at(),
            Duration::from_secs(93)
        );
    }

    #[test]
    fn completion_outranks_every_position_rule() {
        // A completed entry past the end is still declined as completed, not
        // reported as stale: the two say different things about the file.
        let entry = stored(400, true);
        assert_eq!(decide_resume(Some(&entry), secs(300)), ResumeDecision::Completed);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib session::`
Expected: FAIL — `cannot find function decide_resume in this scope`.

- [ ] **Step 3: Implement the table**

Add to `src/session.rs`, above the test module:

```rust
/// What §11's table says about one persisted entry, and why.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeDecision {
    /// No file, or no entry for this media.
    NoEntry,
    /// The entry is complete. D1 retains its position; the resume declines it.
    Completed,
    /// An entry that never got anywhere.
    AtStart,
    Resume(Duration),
    /// `position == duration`: preserved in storage, not usable as a start.
    DegenerateEnd,
    /// `position > duration`: the file no longer describes this media.
    StalePastEnd,
    /// The duration is unknown, so the position is retained unvalidated.
    Unvalidated(Duration),
}

impl ResumeDecision {
    pub fn start_at(&self) -> Duration {
        match self {
            Self::Resume(position) | Self::Unvalidated(position) => *position,
            Self::NoEntry
            | Self::Completed
            | Self::AtStart
            | Self::DegenerateEnd
            | Self::StalePastEnd => Duration::ZERO,
        }
    }
}

/// Applied against the duration from the probe `app::run` already performs, so
/// no extra file is opened (§11). Completion is never inferred from
/// `position >= duration`, and there is no near-end heuristic anywhere.
pub fn decide_resume(
    entry: Option<&PersistedCheckpoint>,
    duration: Option<Duration>,
) -> ResumeDecision {
    let Some(entry) = entry else {
        return ResumeDecision::NoEntry;
    };
    if entry.completed {
        return ResumeDecision::Completed;
    }
    if entry.position.is_zero() {
        return ResumeDecision::AtStart;
    }
    let Some(duration) = duration else {
        return ResumeDecision::Unvalidated(entry.position);
    };
    match entry.position.cmp(&duration) {
        std::cmp::Ordering::Less => ResumeDecision::Resume(entry.position),
        std::cmp::Ordering::Equal => ResumeDecision::DegenerateEnd,
        std::cmp::Ordering::Greater => ResumeDecision::StalePastEnd,
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib session::`
Expected: PASS — 8 tests.

- [ ] **Step 5: Verify the gates**

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --locked`
Expected: clean, 175 passed.

- [ ] **Step 6: Commit**

```bash
git add src/session.rs
git commit -m "Add resume validation as a pure decision

Seven rows, applied against the duration the probe already reports, so no
extra file is opened. Completion outranks every position rule and is never
inferred from position >= duration: a position exactly at the end is
preserved in storage but is not a usable start, and one past the end says
the file no longer describes this media. There is no near-end heuristic
here, deliberately."
```

---

## Task 9: Wire it into the application

**Files:**
- Modify: `src/app.rs`
- Modify: `tests/support/mod.rs` (one additive helper)
- Test: `tests/resume_contract.rs`
- Modify: `README.md`, `docs/architecture.md`

**Interfaces:**
- Consumes: everything from Tasks 1–8.
- Produces: no new public API beyond what `app::run` already exposes.

**The one restructure (D18):** M1's loop has three exits — a `break` with `Ok`, a `break` with `Err`, and `render(&mirror)?`, which returns from `app::run` outright. Only the first two reach the shutdown sequence. A terminal write failure is not a reason to skip the final checkpoint, so the render error becomes the loop's outcome and is broken on, like the `Failed` path already is.

**The one item verified by inspection, not by a test:** "a render failure still writes the final checkpoint". Reaching it needs a failing terminal writer *and* a real audio device inside `app::run`, and CI has neither. Step 8 is a structural check instead, and the plan says so rather than pretending otherwise.

- [ ] **Step 1: Add the failed-device harness helper**

D20's end-to-end shape needs a session that emits `Loaded` and *then* fails to
open a device. `TestEngine` cannot stage it — it always reaches `Playing`, which
establishes. Append to `tests/support/mod.rs`, alongside the existing
`load_failure_on_device`, which this is modelled on:

```rust
/// A session whose device refuses to open: `Loaded` goes out, then negotiation
/// rejects the channel count and the load fails. Hands back what `app::run`
/// would have — every event, in order, and the final `Progress`.
///
/// Deliberately not a `TestEngine`: the engine never reaches `Paused` here, so
/// there is no transport to drive and nothing for a driver thread to do.
pub fn failed_device_session(name: &str, channels: u16, start_at: Duration) -> ShutdownReport {
    let device = Arc::new(Mutex::new(Device {
        output: TestOutput::new(channels, RATE, BUFFER_FRAMES, LATENCY),
        link: None,
    }));
    // Held for the call's duration, so the worker's fault receiver stays live.
    let (_faults, fault_rx) = crossbeam_channel::bounded(16);
    let handle = EngineHandle::spawn_with(
        Box::new(HarnessOutput {
            device: Arc::clone(&device),
        }),
        fault_rx,
    );
    let path = fixture(name);
    let sent = handle.commands().send(PlaybackCommand::Load {
        media: MediaId::LocalFile(path.clone()),
        source: SourceLocation::LocalPath(path.as_path().to_path_buf()),
        start_at,
    });
    if sent.is_err() {
        panic!("the engine stopped accepting commands");
    }

    let mut events = Vec::new();
    let deadline = Instant::now() + PATIENCE;
    loop {
        while let Ok(event) = handle.events().try_recv() {
            events.push(event);
        }
        if events
            .iter()
            .any(|event| matches!(event, PlaybackEvent::Failed { .. }))
        {
            break;
        }
        if Instant::now() >= deadline {
            panic!("the load never failed; saw {events:?}");
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    handle.interrupt_shutdown();
    let mut report = handle.join();
    events.extend(std::mem::take(&mut report.events));
    report.events = events;
    report
}
```

Run: `cargo build --tests`
Expected: clean. Nothing calls it yet.

- [ ] **Step 2: Write the failing resume contract tests**

Create `tests/resume_contract.rs`:

```rust
//! Session 1 → persist → session 2. The rig runs `app::run`'s ordering —
//! drain events, then sample once — against the test engine and a real store
//! in a tempdir, writing synchronously so nothing here depends on a thread.

use std::sync::Arc;
use std::time::Duration;

use continuo::clock::{Clock, FakeClock};
use continuo::media::id::{AbsolutePath, MediaId};
use continuo::persistence::model::PersistedState;
use continuo::persistence::store::StateStore;
use continuo::playback::command::PlaybackCommand;
use continuo::playback::state::PlaybackState;
use continuo::playback::volume::Volume;
use continuo::session::{Action, Session, decide_resume};

mod support;

use support::TestEngine;

const TRACK: &str = "sine-5s.flac";
/// The fixture's duration, which the probe would supply in `app::run`.
const TRACK_DURATION: Duration = Duration::from_secs(5);

fn track_id() -> MediaId {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(TRACK);
    let Ok(canonical) = path.canonicalize() else {
        panic!("the fixture must exist: {path:?}");
    };
    match AbsolutePath::new(canonical) {
        Ok(path) => MediaId::LocalFile(path),
        Err(error) => panic!("the fixture path must be identifiable: {error}"),
    }
}

struct Rig {
    engine: TestEngine,
    session: Session,
    store: StateStore,
    clock: Arc<FakeClock>,
}

impl Rig {
    /// Session 1: a fresh file, playing from the top.
    fn open(dir: &std::path::Path) -> Self {
        Self::open_at(dir, PersistedState::default(), Duration::ZERO)
    }

    fn open_at(dir: &std::path::Path, state: PersistedState, start_at: Duration) -> Self {
        let clock = Arc::new(FakeClock::new());
        // `clock.clone()`, not `Arc::clone(&clock)`: the annotation constrains
        // the argument position, and `&Arc<FakeClock>` does not coerce to
        // `&Arc<dyn Clock>`.
        let injected: Arc<dyn Clock> = clock.clone();
        let store = StateStore::new(dir.join("state.json"), injected);
        let mut rig = Self {
            engine: TestEngine::start_at(TRACK, start_at),
            session: Session::new(state),
            store,
            clock,
        };
        rig.pump();
        rig
    }

    /// One iteration of `app::run`: drain the events, then sample once.
    fn pump(&mut self) {
        while let Some(event) = self.engine.try_event() {
            let action = self.session.observe(&event, self.clock.sample());
            Self::write(&self.store, action);
        }
        let progress = self.engine.progress();
        let action = self.session.tick(&progress, self.clock.sample());
        Self::write(&self.store, action);
    }

    /// An associated function, not a method: `pump` already holds a mutable
    /// borrow of `session` when it calls this.
    ///
    /// The writer thread's coalescing has its own tests; what matters here is
    /// which snapshot the policy produced, so it is written straight through.
    fn write(store: &StateStore, action: Action) {
        if let Action::Submit { state, .. } = action
            && let Err(error) = store.write(&state)
        {
            panic!("the tempdir must be writable: {error}");
        }
    }

    fn send(&mut self, command: PlaybackCommand) {
        self.engine.send(command);
        // The worker applies a command on one pass and flushes the event it
        // produced on the next (§3), so run the application loop for a while
        // rather than once. Deliberately not `TestEngine::position`, which
        // settles by sending a volume command of its own and consuming the
        // answer — it would eat the very `VolumeChanged` a test is watching
        // for. The harness clock is frozen, so no playback time passes here.
        for _ in 0..40 {
            self.pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// `q`: interrupt, join, replay what the loop never drained, then one
    /// forced snapshot.
    fn quit(mut self) {
        let Some(report) = self.engine.shutdown_report() else {
            panic!("the engine was already gone");
        };
        for event in &report.events {
            let _ = self.session.observe(event, self.clock.sample());
        }
        let final_state = self
            .session
            .shutdown_snapshot(&report.progress, self.clock.sample());
        if let Err(error) = self.store.write(&final_state) {
            panic!("the tempdir must be writable: {error}");
        }
    }
}

fn reload(dir: &std::path::Path) -> PersistedState {
    let injected: Arc<dyn Clock> = Arc::new(FakeClock::new());
    StateStore::new(dir.join("state.json"), injected).load().state
}

#[test]
fn a_stop_and_a_quit_resume_where_playback_reached() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(2));
    rig.pump();
    rig.send(PlaybackCommand::Stop);
    rig.quit();

    let state = reload(dir.path());
    let entry = state.entry_for(&track_id()).expect("an entry for the track");
    assert!(
        entry.position >= Duration::from_secs(2) && entry.position < TRACK_DURATION,
        "session 2 must resume near where session 1 stopped: {:?}",
        entry.position
    );
    assert!(!entry.completed);

    let decision = decide_resume(state.entry_for(&track_id()), Some(TRACK_DURATION));
    assert!(decision.start_at() >= Duration::from_secs(2));
}

#[test]
fn a_pause_and_a_quit_resume_where_playback_reached() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(2));
    rig.pump();
    rig.send(PlaybackCommand::Pause);
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(entry.position >= Duration::from_secs(2));
}

#[test]
fn a_seek_is_persisted_from_the_canonical_position() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(1));
    rig.pump();
    rig.send(PlaybackCommand::SeekTo(Duration::from_secs(3)));
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position >= Duration::from_secs(3),
        "the seek's landing, taken from Progress rather than from the event: {:?}",
        entry.position
    );
}

/// play → stop → seek while stopped → quit. The sequence D7 and D17 exist for.
#[test]
fn a_stopped_seek_target_outlives_the_quit() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(3));
    rig.pump();
    rig.send(PlaybackCommand::Stop);
    rig.send(PlaybackCommand::SeekTo(Duration::from_secs(1)));
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position < Duration::from_secs(2),
        "the stored target, not the pre-seek position the engine still reports: {:?}",
        entry.position
    );
}

/// The same sequence with a `q` that gives the event no time to be drained.
#[test]
fn a_stopped_seek_target_outlives_a_quit_that_races_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(3));
    rig.pump();
    rig.send(PlaybackCommand::Stop);

    // No pump between the seek and the quit: exactly what pressing `←` and then
    // `q` inside one poll window does. The wait is for the command to be taken,
    // not for its event — `q` does not wait either, but a command the worker
    // never read is not a lost event, it is a test asking the wrong question.
    rig.engine.send(PlaybackCommand::SeekTo(Duration::from_secs(1)));
    rig.engine.await_commands_taken();
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position < Duration::from_secs(2),
        "the SeekTargetStored is not the application's to lose: {:?}",
        entry.position
    );
}

/// stop → seek to 1 s → Home → play → quit. `restart()` discards the target.
#[test]
fn a_restart_after_a_stopped_seek_persists_where_it_restarted() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(3));
    rig.pump();
    rig.send(PlaybackCommand::Stop);
    rig.send(PlaybackCommand::SeekTo(Duration::from_secs(1)));
    rig.send(PlaybackCommand::Restart);
    rig.engine.play_for(Duration::from_secs(2));
    rig.pump();
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position >= Duration::from_secs(2),
        "the restarted playback's position, not the target the restart threw away: {:?}",
        entry.position
    );
}

#[test]
fn a_finished_track_is_completed_and_reopens_at_zero_with_its_position_kept() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_to_end();
    rig.pump();
    rig.quit();

    let state = reload(dir.path());
    let entry = state.entry_for(&track_id()).cloned().expect("an entry");
    assert!(entry.completed);
    assert!(entry.position > Duration::from_secs(4), "D1 retains it: {:?}", entry.position);

    let decision = decide_resume(state.entry_for(&track_id()), Some(TRACK_DURATION));
    assert_eq!(decision.start_at(), Duration::ZERO, "and reopening starts at zero");
}

#[test]
fn a_completed_entry_survives_a_launch_whose_device_refuses_to_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_to_end();
    rig.pump();
    rig.quit();
    let kept = reload(dir.path()).entry_for(&track_id()).cloned().expect("an entry");
    assert!(kept.completed);

    // Session 2. §11 opens a completed entry at zero, `load()` emits `Loaded`
    // before it opens the device, and this device offers six channels — which
    // negotiation refuses, after the `Loaded` has already gone out. The
    // application therefore knows the media and reports a position of zero,
    // and playback never happened.
    //
    // Deliberately not the rig: `TestEngine` always reaches `Playing`, which
    // establishes, so it cannot stage this at all.
    let clock = FakeClock::new();
    let injected: Arc<dyn Clock> = Arc::new(FakeClock::new());
    let store = StateStore::new(dir.path().join("state.json"), injected);
    let mut session = Session::new(reload(dir.path()));

    let report = support::failed_device_session(TRACK, 6, Duration::ZERO);
    for event in &report.events {
        let _ = session.observe(event, clock.sample());
    }
    let final_state = session.shutdown_snapshot(&report.progress, clock.sample());
    store.write(&final_state).expect("the tempdir is writable");

    let after = reload(dir.path()).entry_for(&track_id()).cloned().expect("an entry");
    assert_eq!(
        after.position, kept.position,
        "D20: nothing established, so nothing overwrites the position D1 retains"
    );
    assert!(after.completed);
}

#[test]
fn volume_survives_the_restart_and_is_restored_before_the_load() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.send(PlaybackCommand::SetVolume(Volume::new(0.25)));
    rig.quit();

    let state = reload(dir.path());
    assert_eq!(state.volume(), Volume::new(0.25));
    assert_eq!(state.current_media, Some(track_id()));
}

#[test]
fn a_position_past_the_end_is_refused_as_a_start() {
    // A stale file, as a hand-edited or renamed-media one would be.
    let dir = tempfile::tempdir().unwrap();
    let injected: Arc<dyn Clock> = Arc::new(FakeClock::new());
    let store = StateStore::new(dir.path().join("state.json"), injected);
    let mut state = PersistedState::default();
    state.record(
        &continuo::playback::checkpoint::PlaybackCheckpoint {
            media: track_id(),
            position: Duration::from_secs(600),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        false,
    );
    store.write(&state).unwrap();

    let reloaded = reload(dir.path());
    let decision = decide_resume(reloaded.entry_for(&track_id()), Some(TRACK_DURATION));
    assert_eq!(decision.start_at(), Duration::ZERO);

    // And the engine can be started from that decision without complaint.
    let rig = Rig::open_at(dir.path(), reloaded, decision.start_at());
    assert_eq!(rig.engine_state(), PlaybackState::Playing);
    rig.quit();
}
```

Add one accessor to `Rig` for that last assertion:

```rust
impl Rig {
    fn engine_state(&mut self) -> PlaybackState {
        self.engine.state()
    }
}
```

and make the binding in that test `let mut rig` accordingly.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --test resume_contract 2>&1 | tail -20`
Expected: FAIL — the rig compiles only once Tasks 1–8 are in; the failures here should be assertion failures about positions, not compile errors. If they are compile errors, an earlier task is incomplete.

- [ ] **Step 4: Wire persistence into `app::run`**

In `src/app.rs`, add the imports:

```rust
use std::sync::Arc;

use crate::clock::{Clock, SystemClock};
use crate::persistence::PersistenceError;
use crate::persistence::model::PersistedState;
use crate::persistence::store::{LoadReason, StateStore};
use crate::persistence::writer::{ShutdownOutcome, StateSink, Urgency, WriterHandle};
use crate::session::{Action, ResumeDecision, Session, decide_resume};
```

Add the disabled sink and the opening helper below `run`:

```rust
/// Writing is off for this session — an unsupported file, a quarantine that
/// could not be performed, or no state directory at all. The session runs
/// normally with in-memory state; only the disk write is suppressed, and the
/// reason has already been logged once (D3).
struct DisabledSink;

impl StateSink for DisabledSink {
    fn write(&self, _state: &PersistedState) -> Result<(), PersistenceError> {
        Ok(())
    }
}

struct Persistence {
    session: Session,
    writer: WriterHandle,
    start_at: Duration,
    volume: Volume,
}

fn open_persistence(
    media: &MediaId,
    duration: Option<Duration>,
    clock: &Arc<dyn Clock>,
) -> Persistence {
    let store = match StateStore::platform_path() {
        Ok(path) => Some(StateStore::new(path, Arc::clone(clock))),
        Err(error) => {
            tracing::warn!(%error, "no state directory; this session will not be persisted");
            None
        }
    };

    let (state, writable) = match &store {
        Some(store) => {
            let outcome = store.load();
            match &outcome.reason {
                LoadReason::Loaded => tracing::debug!(path = ?store.path(), "state restored"),
                LoadReason::Missing => tracing::debug!(path = ?store.path(), "no state yet"),
                LoadReason::Quarantined { moved_to } => {
                    tracing::warn!(?moved_to, "state file was unreadable and has been moved aside");
                }
                LoadReason::QuarantineFailed => {
                    tracing::warn!("state file is unreadable and could not be moved aside; not writing");
                }
                LoadReason::UnsupportedVersion { found } => {
                    tracing::warn!(found, "state file is from a newer build; preserving it and not writing");
                }
                LoadReason::Unreadable => {
                    tracing::warn!("state file could not be read; preserving it and not writing");
                }
            }
            (outcome.state, outcome.writable)
        }
        None => (PersistedState::default(), false),
    };

    let decision = decide_resume(state.entry_for(media), duration);
    match decision {
        ResumeDecision::NoEntry => tracing::debug!("no stored position for this media"),
        ResumeDecision::Completed => tracing::info!("resume declined: this media is completed"),
        ResumeDecision::AtStart => {}
        ResumeDecision::Resume(position) => tracing::info!(?position, "resume position selected"),
        ResumeDecision::DegenerateEnd => tracing::debug!("stored position is exactly the end; starting over"),
        ResumeDecision::StalePastEnd => tracing::warn!("stored position is past the end of this media"),
        ResumeDecision::Unvalidated(position) => {
            tracing::info!(?position, "duration unknown; stored position retained unvalidated");
        }
    }

    let start_at = decision.start_at();
    let volume = state.volume();
    let sink: Box<dyn StateSink> = match (store, writable) {
        (Some(store), true) => Box::new(store),
        _ => Box::new(DisabledSink),
    };

    Persistence {
        session: Session::new(state),
        writer: WriterHandle::spawn(sink, Arc::clone(clock)),
        start_at,
        volume,
    }
}

fn submit(writer: &WriterHandle, action: Action) {
    if let Action::Submit { state, urgency } = action {
        writer.submit(state, urgency);
    }
}
```

In `run`, keep the duration before the probe is dropped and open persistence before the engine:

```rust
    let duration = probed.metadata().duration;
    drop(probed);

    let media = MediaId::LocalFile(absolute.clone());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let Persistence {
        mut session,
        mut writer,
        start_at,
        volume,
    } = open_persistence(&media, duration, &clock);

    let engine = EngineHandle::spawn_cpal();
    let raw = RawModeGuard::enable()?;
    let mut mirror = Mirror::default();

    // Volume first: the engine accepts it with no transport, and a transport
    // created later adopts the stored gain — so the restored level is in force
    // from the first buffer rather than after it (§11).
    engine.commands().send(PlaybackCommand::SetVolume(volume)).ok();
    engine
        .commands()
        .send(PlaybackCommand::Load {
            media: media.clone(),
            source: SourceLocation::LocalPath(absolute.as_path().to_path_buf()),
            start_at,
        })
        .ok();
    engine.commands().send(PlaybackCommand::Play).ok();
```

Note the `RawModeGuard` binding changes from `_raw` to `raw`, because the shutdown sequence drops it explicitly.

In the loop, feed the session and fold the render error into the outcome:

```rust
        let mut failure = None;
        while let Ok(event) = engine.events().try_recv() {
            if let PlaybackEvent::Failed { message, .. } = &event {
                failure = Some(message.clone());
            }
            submit(&writer, session.observe(&event, clock.sample()));
            mirror.apply(event);
        }
        if let Some(message) = failure {
            break Err(PlaybackError::Failed(message));
        }

        let progress = engine.progress();
        submit(&writer, session.tick(&progress, clock.sample()));
        if progress.session_rev == mirror.session_rev {
            mirror.position = progress.position;
            mirror.quality = progress.quality;
        }
        // D18: a terminal write failure is not a reason to skip the final
        // checkpoint, so it becomes the loop's outcome instead of returning.
        if let Err(error) = render(&mirror) {
            break Err(error);
        }
    };
```

and replace the shutdown tail:

```rust
    engine.interrupt_shutdown();
    engine.commands().send(PlaybackCommand::Shutdown).ok();
    let report = engine.join();

    // Reconciliation, not submission (D19): the forced snapshot below
    // supersedes anything these would have submitted on their own.
    for event in &report.events {
        let _ = session.observe(event, clock.sample());
    }
    writer.submit(
        session.shutdown_snapshot(&report.progress, clock.sample()),
        Urgency::Forced,
    );

    // Restore the terminal before waiting on the disk, so the 2 s bound is
    // never spent with the terminal still raw.
    drop(raw);
    match writer.shutdown() {
        ShutdownOutcome::Written => tracing::debug!("final checkpoint written"),
        ShutdownOutcome::Failed(error) => tracing::warn!(%error, "final checkpoint failed"),
        ShutdownOutcome::Unconfirmed => tracing::warn!("final checkpoint UNCONFIRMED"),
    }
    outcome
}
```

- [ ] **Step 5: Run the contract tests to verify they pass**

Run: `cargo test --test resume_contract`
Expected: PASS — 10 tests.

If `a_stopped_seek_target_outlives_a_quit_that_races_it` is flaky, that is the test doing its job on a machine where the worker happened to flush first: the assertion must hold on *either* side of the handoff. A genuine failure means `join` is not returning the backlog.

- [ ] **Step 6: Run everything**

Run: `cargo test --locked`
Expected: PASS — 185 passed, 1 ignored.

- [ ] **Step 7: Verify the gates**

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings`
Expected: clean.

- [ ] **Step 8: Structural check for D18**

Run: `grep -n "render(&mirror)" src/app.rs`
Expected: exactly one hit, inside an `if let Err(error) = render(&mirror)` that `break`s. No `render(&mirror)?` anywhere.

Then read the loop once and confirm by inspection that **every** exit — the `Shutdown` break, the input-error break, the `Failed` break and the render break — falls through to `engine.interrupt_shutdown()`. This is the one §16 item CI cannot cover, because reaching it needs both a failing terminal writer and a real audio device inside `app::run`.

- [ ] **Step 9: Document the state file**

Append to `README.md`:

```markdown
## Playback state

Position, completion and volume are written to a small JSON file so that
quitting and relaunching the same file resumes where you left off.

- **Linux:** `$XDG_STATE_HOME/continuo/state.json`, falling back to
  `~/.local/state/continuo/state.json`
- **macOS and Windows:** the platform's local data directory

The file holds one checkpoint per media identity, capped at 512 entries, and is
replaced atomically — a crash mid-write cannot leave a truncated file. A file
this build cannot read is preserved rather than overwritten: garbage is moved
aside as `state.json.rejected-<timestamp>`, and a file from a newer build is
left exactly where it is with writing disabled for that session.

Reaching the end of a track marks it complete and keeps the position it ended
at; reopening a completed track starts from the beginning.
```

In `docs/architecture.md` §6, after the sentence deferring the persistence
policy to M2, add:

```markdown
M2 settles this: see `docs/superpowers/specs/2026-09-08-continuo-durable-state-design.md`
for the decisions — completion semantics, the per-identity map and its cap,
rejected-file handling, and the checkpoint triggers.
```

- [ ] **Step 10: Final verification**

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test --locked`
Expected: clean, 185 passed, 1 ignored.

Run: `git diff --stat origin/main -- tests/engine_contract.rs`
Expected: no output.

- [ ] **Step 11: Commit**

```bash
git add src/app.rs tests/resume_contract.rs README.md docs/architecture.md
git commit -m "Persist and restore playback position across runs

The application now opens the state file before the engine, decides a
start position from it against the duration the probe already reports,
and issues volume before the load so the restored level is in force from
the first buffer rather than after it.

Every exit from the loop reaches the flush path. A terminal write failure
used to return from run outright, skipping the shutdown sequence
entirely; it is now the loop's outcome and is broken on, like the failure
path already was.

Quitting joins the engine, replays the events the loop never drained back
through the policy, and submits one forced snapshot built from a session
that has seen everything the run produced. The terminal comes out of raw
mode before the writer's two second bound is spent, not after."
```

---

## Self-Review

Checked against the spec, section by section.

**Coverage.** §6's decisions D1–D20 all land somewhere: D1/D12/D15 in Task 2, D2/D4 in Task 2, D3 in Task 3, D5/D9/D10/D11 in Task 4, D6/D7/D13/D17/D20 in Task 7, D8/D14/D19 in Task 5, D16 in Task 1, D18 in Task 9. §10's shape is Task 2, §11 is Task 8, §12 is Tasks 6–7, §13 is Task 3, §15 is Task 2, §16's suites map one-to-one onto the six test files.

**Two additions this plan makes that §14 does not list**, both needed to compile what the spec describes:

1. `MediaId` and the identity newtypes gain `PartialOrd, Ord` (Task 2). D2 keys a map by `MediaId`, and a `BTreeMap` key needs a total order the M0 types do not derive. Purely additive.
2. `PlaybackEvent::session_rev()` (Task 6). §7 requires the revision be adopted from *every* event; without an accessor that is a ten-arm match in `session.rs`. It lives in `event.rs`, knows nothing about persistence, and `Mirror` could use it too.

The engine change count in §14 therefore reads "four changes" to `engine.rs` plus two additive items in `event.rs`. Update §14 when this plan is executed.

**One §16 item is not covered by a test:** "a render failure still writes the final checkpoint" (D18). Task 9 Step 8 replaces it with a structural check and says why. Everything else in §16 has a named test.

**Known gap carried forward.** `src/persistence/store.rs` checks `candidate.exists()` before renaming rather than claiming the name atomically. §17 already documents that Continuo is single-user and single-process with no cross-process locking, so the window is not reachable by anything this milestone supports.
