# Tenuto M5 Compact Ratatui Player Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `tenuto tui`: a compact, local-first terminal player with a durable playback queue, token-correlated loads, one shared shutdown/lock lifecycle for `tui` and legacy `play`, on-demand browsing, best-effort cover art and a real post-gain frequency spectrum, while every existing CLI command keeps working.

**Architecture:** `Session` stays the only owner of `PersistedState`, which becomes schema 3 with a queue; it also owns a transient table of pending load tokens and adopts a queue occurrence only on `Loaded` for its token. The engine gains caller-supplied `LoadRequestId`s and protected load outcomes. A terminal-free `application::PlayerRuntime` owns engine, session, writer and HTTP service for the TUI; legacy `play` shares the lifecycle module (profile lock, signals) and token correlation but keeps its two loops. The TUI renders immutable `PlayerView` snapshots; metadata, artwork and spectrum work runs on bounded workers that report back to the application thread.

**Tech Stack:** Rust edition 2024 (toolchain 1.98.1), existing symphonia/cpal/rtrb/crossbeam/crossterm 0.29/clap/serde/tokio/reqwest; new direct dependencies `ratatui` 0.30, `ratatui-image` 11, `image` 0.25 (JPEG/PNG only), `rustfft` 6, and on Unix `signal-hook` 0.4 and `gag` 1; dev-dependency `portable-pty` 0.9. Locking uses `std::fs::File::try_lock` (stable since 1.89).

**Spec:** [Tenuto M5: compact Ratatui player](../specs/2026-09-14-tenuto-ratatui-design.md), all thirteen sections, at commit `9e46ffe`, with the [compact reference](../specs/assets/tenuto-compact-reference.html) for palette and layout only. Read the whole spec before starting any task; every task's requirements include this plan's Global Constraints and Implementation Decisions.

## Global Constraints

- Keep `edition = "2024"`, `rust-version = "1.98.1"`, `[lints.rust] unsafe_code = "forbid"`, `clippy::unwrap_used` and `clippy::expect_used` denied outside `#[test]` functions (`clippy.toml` already allows them inside tests).
- Exact new dependency lines (no others; no blanket upgrades of `Cargo.lock`):
  - `ratatui = { version = "0.30.2", default-features = false, features = ["crossterm_0_29", "layout-cache"] }`
  - `ratatui-image = { version = "11.0.8", default-features = false, features = ["crossterm"] }`
  - `image = { version = "0.25.10", default-features = false, features = ["jpeg", "png"] }`
  - `rustfft = "6.4.1"`
  - `[target.'cfg(unix)'.dependencies]` `signal-hook = "0.4.4"` and `gag = "1.0.0"`
  - `[dev-dependencies]` `portable-pty = "0.9.0"`
  - Do not add `fs2`, `fd-lock`, `nix`, `libc`, `lofty`, `tokio` signal features, a mocking crate, or an async TUI runtime.
- "This build supports **256 queue occurrences**, including duplicates." Oversized enqueue batches are rejected whole. The cap is operational, "**not a schema-validity rule**". The checkpoint cap stays **512** with existing eviction; "queue membership does not pin a checkpoint".
- `PersistedState` schema becomes **3**; schemas 1 and 2 migrate to an empty queue with no active entry.
- "Allow at most **16 pending loads** in the application and report busy when full."
- Size tiers, evaluated in order: below **30** columns or **8** rows → resize message; below **50** or **18** → minimal; below **80** or **28** → compact; otherwise normal. "Thus 100×20 is compact."
- Artwork: embedded front cover, then sibling `cover.jpg`, `cover.png`, `folder.jpg`, `folder.png`; JPEG and PNG only; **10 MiB** encoded input, **16 million** decoded pixels; modes `auto`, `blocks`, `off`.
- Spectrum: "**2048-sample Hann window**", 24 nominal logarithmic intervals from **40 Hz** to the lesser of **16 kHz** or Nyquist, every band ≥ 2 FFT bin centers, at most **20 frames per second**, power averaged across **all output channels**.
- Unix signals: SIGINT, SIGHUP, SIGTERM through the `signal-hook` iterator API; OS-signal exit status is `128 + signal_number` of the first recorded signal; `q` and the Ctrl-C key exit 0.
- Profile lock file: sibling `state.lock` next to `state.json`, never unlinked. Contention message, verbatim: `Another Tenuto player is using this state profile`.
- TUI log: a new uniquely named append log under `<state dir>/logs/`, keeping the **five** most recent prior logs.
- Status strings, verbatim: `Play a track before seeking`, `Queue is empty`, `Track ended; press play to replay`, `Still loading`, `Using saved episode source`.
- Bare `tenuto` is a usage error with exit status **2** on stderr; `tenuto --help` exits **0**.
- Never: IPC, network protocol, server/daemon mode, Tauri code, named playlists, shuffle/repeat/wrap, implicit feed refresh, remote metadata or duration probes outside an explicit load, recursive library indexing, raw terminal control characters or URL credentials in display strings.
- Every test that launches the `tenuto` binary goes through `tests/support/process.rs` with an isolated temporary profile.
- Acceptance: `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked`.

---

## Execution context

The inspected branch is `feat/ratatui-player` at `9e46ffe`. The worktree had one unrelated untracked file (`docs/superpowers/plans/2026-09-11-tenuto-feeds-subscriptions.md`); leave it alone. This plan is a planning deliverable. At execution time re-check `git status`, preserve unrelated changes, and follow the chosen execution skill's isolation rules.

The spec is one milestone with ordered increments (§12). Tasks below follow that order; each leaves `play`, `play --probe-only`, and every feed command working. Run the focused test first and watch it fail for the intended reason, then implement, then rerun. A task's code listings are binding for public names and behavior; private helper names may vary where no later task consumes them.

All new tests return `Result<(), Box<dyn std::error::Error>>` or use `expect`/`unwrap` inside `#[test]` functions only. Helper functions outside `#[test]` must not unwrap (use `panic!` with context, as `tests/support/mod.rs` already does).

## Implementation decisions

These resolve gaps found while reading the code against the spec. They are binding.

1. **Usage exit status is currently wrong.** `src/main.rs` prints every clap error with `eprint!` and returns `ExitCode::FAILURE`; measured on this commit, bare `tenuto` and `tenuto --help` both exit 1. §4 says bare exits 2 and `--help` succeeds, so Task 2 makes that true with clap's own `Error::print` and `Error::exit_code`.
2. **`Progress` carries the adopted load token.** The worker bumps `session_rev` on load, on stop (`do_stop`) and on device recovery, and `Progress.media` cannot tell duplicate occurrences apart. Neither can therefore implement "progress for an unadopted load cannot update that previous checkpoint". Add `Progress::load: Option<LoadRequestId>`: the worker sets it when it emits `Loaded`, keeps it across stop, pause and recovery, and clears it when the next `load` starts. `Session::tick` ignores progress whose `load` is not the adopted token.
3. **Completion advancement** is keyed by `(adopted token, session_rev)` and additionally requires that the most recent `Loaded` the session observed carried the adopted token. A `Loaded` for an invalidated target therefore suppresses advancement for any `EndOfTrack` its playback might produce before the stop lands.
4. **Protected events are never dropped or displaced.** `Loaded`, `LoadCancelled` and `Failed { request: Some(_) }` bypass `PENDING_CAP` and may occupy the reserved tail. The backlog stays bounded while running because admission already closes whenever `pending_events` is nonempty, so one pass dispatches at most one load and emits at most one load outcome. The shutdown drain of undelivered `Load` commands may exceed `PENDING_CAP`; it only extends the returned `Vec`.
5. **Reserve arithmetic, rechecked.** The load row's terminal-or-protected share becomes 3 (`Loaded`, then `Failed` + `StateChanged{Failed}` when the device does not open). Worst pass: stop 1 + fatal fault 2 + load 3 + end of track 2 = 8 ≤ `RESERVED_EVENT_SLOTS` (9). A cancelled load emits only `LoadCancelled` (1). Update the comment block in `engine.rs` with this table.
6. **Existing session tests register their loads.** `Loaded` now requires a registered token. Test helpers change shape (Task 8 gives the exact helpers); the policy they assert does not change.
7. **Virtual real-time audio output for subprocess tests.** CI has no audio device, but §12 requires signal tests "during playback". Add `playback::output::null_output::NullOutput`, a paced thread that runs `CallbackCore::fill` every buffer period and discards samples. `EngineHandle::spawn_for_environment()` selects it when `TENUTO_AUDIO_OUTPUT=null`, otherwise `spawn_cpal()`. Document it in `docs/architecture.md` as a diagnostic switch.
8. **Process test hooks.** Panic-at-stage and fd-2 probe tests need a deterministic trigger inside the child. `lifecycle::hooks::TestHook::from_env()` reads `TENUTO_TEST_HOOK` once at startup; unknown or absent values mean no hook. Values: `panic-before-redirect`, `panic-after-redirect`, `panic-after-terminal`, `stderr-probe`, `artwork-job-panic`, `artwork-encoding-panic`, `metadata-job-panic`, `worker-panic`.
9. **No production profile override.** Subprocess suites run on Linux only, with `XDG_STATE_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`, and `XDG_CONFIG_HOME` set per child. The shared launcher refuses unsupported platforms before spawning. `HOME` does not redirect Windows Known Folders; do not claim cross-platform isolation. Enable subprocess suites on another platform only after verifying its actual profile paths. Production platform support is unchanged.
10. **Queue decoding.** `PersistedState` loses its derived serde impls. A private `RawState` decodes the envelope and listening history with `queue` and `active_entry` as `Option<serde_json::Value>`; a pure `recover_queue` turns those values into a `Queue` plus an optional `QueueReset`. A wrong JSON type in either field can no longer fail the whole file.
11. **Local and remote queue sources duplicate their identity on disk** (`path`, `url`) so a source/identity mismatch is detectable, as §6 requires. A podcast source stores only `fallback_url`; its identity is the entry's `MediaId`.
12. **Saved-history label** for a checkpoint: completed → `played`; `estimated` present → `~mm:ss saved`; `position` present → `mm:ss saved`; an entry with neither → `position unknown`; no entry → no label.
13. **Embedded artwork** accepts only a visual whose `usage` is `StandardVisualKey::FrontCover`, as the spec says "front-cover artwork".
14. **Browser episodes need the enclosure URL**, which `EpisodeRow` does not carry. Add `library::episode_candidates`.
15. **`resume_intent_for` moves** from `app.rs` to `session.rs` (it reads `PersistedCheckpoint`, so it cannot live in the persistence-free `resume.rs`). `app.rs` imports it, so its tests keep compiling.
16. **`KeyRouter` and `SeekBurst` move** from `app.rs` to `application::seek`; `app.rs` keeps `pub use crate::application::seek::KeyRouter;` because `tests/app_cli.rs` imports `tenuto::app::KeyRouter`.
17. **The lock-holding process test uses volume** as the newer durable fact: `tui` accepts `+` without an engine and persists volume through `Session`, which is observable without an audio device.
18. **Spectrum mapping publication.** The callback labels tap blocks with `(transport instance, generation, epoch)`. The worker learns the epoch a `Run` publication will use through a new `Handshake::upcoming_epoch()` and publishes the mapping before calling `start_running` or `release`. The wait hook's thaw path publishes through `TransportCore`, which stores its own `session_rev` and tap instance.
19. **Adoption snapshots are built last.** Capture outgoing history and change `current_media`, active occurrence and metadata before cloning the single submitted snapshot. The old `on_loaded` submission cannot be reused.
20. **Lifecycle ownership gates precede history writes.** Track the engine's latest recognized revision separately from the adopted checkpoint revision. Reject stale/unowned media events before the existing checkpoint handlers, including in shutdown reconciliation; completion deduplication also precedes writes.
21. **Automatic start is token-scoped.** Add `PlaybackCommand::PlayLoaded { request }`: it calls `play()` only when the worker still owns that token and is `Paused` after a successful load/device open. Otherwise it does nothing. Initial `play` and TUI loads queue this command instead of unrestricted `Play`; failure therefore cannot trigger an implicit reopen, and an older start cannot affect a newer load.
22. **Seek cancellation and load failure are explicit runtime state.** Superseding loads, stop/removal/clear, restart, absolute seeks and shutdown cancel the router's pending burst. A failed request remains retryable even while an older occurrence stays adopted.
23. **Queue ID exhaustion is an admission error.** Keep `next_id: Option<u64>`; `None` means exhausted. A valid restored `u64::MAX` ID survives recovery, but further nonempty enqueue operations fail atomically with `QueueError::IdExhausted`.
24. **Artwork preparation is contained too.** Task 23 adds the `image` dependency before using it. Task 25 wraps resize/protocol encoding in a disposable contained job and replaces the cache only after success.
25. **Spectrum frames expire without PCM.** Use an injectable monotonic freshness check with a 150 ms limit. Pause, disabled analysis, starvation and unchanged stale frames must decay; revision/token equality alone does not make a frame fresh.

## File and responsibility map

| Files | Responsibility |
|---|---|
| `tests/support/process.rs`, `tests/launch_audit.rs` | The only binary launcher; audit that nothing else names the binary |
| `src/main.rs`, `src/cli.rs` | Clap exit status, `tui` subcommand and flags, signal exit status |
| `src/queue.rs` | Queue values, occurrence IDs, pure mutations, successor/neighbor decisions |
| `src/persistence/model.rs`, `src/persistence/queue_codec.rs`, `src/persistence/store.rs`, `src/persistence/atomic.rs` | Schema 3, independent queue decoding and recovery, recovery backup, create-new private files |
| `src/playback/command.rs`, `event.rs`, `engine.rs`, `wait.rs` | `LoadRequestId`, request echo, `LoadCancelled`, protected outcomes, `Progress::load` |
| `src/playback/output/null_output.rs` | Paced virtual output for headless runs |
| `src/session.rs` | Pending-load table, token adoption, queue mutations through state, advancement, resume intent |
| `src/lifecycle/mod.rs`, `lock.rs`, `signals.rs`, `hooks.rs`, `panic.rs`, `stderr.rs`, `terminal.rs` | Run outcome, profile lock, signal listener, test hooks, panic containment, fd-2 redirect and logs, idempotent terminal cleanup |
| `src/application/mod.rs`, `source.rs`, `podcast.rs`, `seek.rs`, `transport.rs`, `runtime.rs`, `view.rs`, `browse.rs`, `enrich.rs` | Terminal-free player coordination shared by TUI (and later Tauri) |
| `src/media/metadata.rs`, `src/media/display.rs`, `src/media/tags.rs` | Artist/album fields, display-name helpers moved from `app.rs`, local tag and cover probe |
| `src/artwork/mod.rs`, `resolve.rs`, `decode.rs`, `worker.rs` | Artwork lookup order, bounded decode, contained worker |
| `src/playback/spectrum/mod.rs`, `bands.rs`, `analyzer.rs`, `tap.rs`, `registry.rs`, `worker.rs` | Band geometry, FFT analysis, callback tap ring, mapping table, analysis thread |
| `src/tui/mod.rs`, `theme.rs`, `layout.rs`, `state.rs`, `input.rs`, `render.rs`, `images.rs`, `browser.rs` | Startup order, palette, tiers, UI state, key/mouse mapping, drawing, image protocol, browser overlay |
| `src/app.rs` | Legacy `play`: lock, signals, token correlation, re-exports |
| `src/library.rs` | `episode_candidates` for enqueueing episodes |
| `tests/m5_*.rs` | New suites; existing suites change only where a shape changed |
| `README.md`, `docs/architecture.md` | `tui` usage, queue, lock, logs, hooks, execution contexts |

`src/lib.rs` gains `pub mod application; pub mod artwork; pub mod lifecycle; pub mod queue; pub mod tui;` in the task that introduces each module. Do not create empty modules ahead of their task.

## Interface registry

Later tasks rely on these exact names. A task's **Interfaces** block repeats the part it consumes or produces.

```rust
// playback::command
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct LoadRequestId(u64);
impl LoadRequestId { pub const fn from_raw(raw: u64) -> Self; pub fn get(self) -> u64; }
PlaybackCommand::Load { request: LoadRequestId, media: MediaId, source: SourceLocation, resume: ResumeIntent }
PlaybackCommand::PlayLoaded { request: LoadRequestId }

// playback::event
PlaybackEvent::Loaded { session_rev, request: LoadRequestId, media, metadata, capabilities, position, disposition }
PlaybackEvent::StateChanged { session_rev, state, request: Option<LoadRequestId> }
PlaybackEvent::Failed { session_rev, message, cause, request: Option<LoadRequestId> }
PlaybackEvent::LoadCancelled { session_rev, request: LoadRequestId }
impl PlaybackEvent { pub fn load_outcome(&self) -> Option<LoadRequestId>; pub fn is_protected(&self) -> bool; }
Progress { session_rev, media, position, quality, provenance, buffering, load: Option<LoadRequestId> }

// queue
pub const MAX_QUEUE_ENTRIES: usize = 256;
pub struct QueueEntryId(u64);            // Copy, Ord, Hash; get(), pub(crate) from_raw()
pub enum QueueSource { LocalFile(AbsolutePath), RemoteUrl(NormalizedUrl), Podcast { fallback: Url } }
pub struct DisplayMetadata { pub title, pub artist, pub album: Option<String>, pub duration: Option<DisplayDuration> }
pub struct DisplayDuration { pub value: Duration, pub source: DurationSource }
pub enum DurationSource { Decoded(PositionProvenance), Declared }
pub struct NewQueueEntry;                // NewQueueEntry::new(media, source, display) -> Result<Self, QueueError>
pub struct QueueEntry;                   // id(), media(), source(), display()
pub enum Direction { Up, Down }
pub enum QueueError { Capacity { requested: usize, available: usize }, IdExhausted, SourceMismatch, UnknownEntry(QueueEntryId) }
pub struct Removed { pub entry: QueueEntry, pub was_active: bool, pub selection: Option<QueueEntryId> }
impl Queue { entries, len, is_empty, get, index_of, active, enqueue, move_entry, remove, clear, neighbor, first }

// persistence
pub const SCHEMA_VERSION: u32 = 3;
impl PersistedState { pub fn queue(&self) -> &Queue; pub(crate) fn queue_mut(&mut self) -> &mut Queue; }
pub mod queue_codec { pub enum QueueReset { ActiveReference(ActiveProblem), WholeQueue(QueueProblem) }
  pub enum ActiveProblem { Malformed, Dangling, MediaMismatch }
  pub enum QueueProblem { Malformed, DuplicateIds, SourceMismatch, OverCapacity { found: usize } }
  pub fn recover_queue(queue: Option<&Value>, active: Option<&Value>, current_media: Option<&MediaId>) -> (Queue, Option<QueueReset>) }
pub struct LoadOutcome { pub state, pub writable, pub reason, pub queue_repair: Option<QueueRepair> }
pub struct QueueRepair { pub reset: QueueReset, pub backup: QueueBackup }
pub enum QueueBackup { Saved(PathBuf), Failed }

// session
pub const MAX_PENDING_LOADS: usize = 16;
pub enum LoadTarget { Queue(QueueEntryId), Legacy }
pub enum RegisterLoadError { Busy, UnknownEntry, MediaMismatch }
pub struct AdoptedLoad { pub request: LoadRequestId, pub target: LoadTarget }
pub enum Advance { Next(QueueEntryId), EndOfQueue }
pub struct Removal { pub action: Action, pub stop_playback: bool, pub selection: Option<QueueEntryId> }
impl Session {
  register_load, retract_load, pending_load_count, adopted, accepts_media_event, take_stop_request, take_advance,
  enqueue, move_entry, remove_entry, clear_queue, set_volume, update_display, update_podcast_fallback, resume_intent
}
pub fn resume_intent_for(entry: Option<&PersistedCheckpoint>) -> Option<ResumeIntent>;

// lifecycle
pub enum RunOutcome { Completed, Signalled(i32) }   // exit_status(self) -> u8
pub struct ProfileLock;                             // acquire(state_file: &Path) -> Result<Self, LockError>
pub enum LockError { Contended, NoStateDirectory, Directory(PersistenceError), Io { path: PathBuf, op: &'static str, source: io::Error } }
pub struct ShutdownSignals;                         // install() -> Result<Self, io::Error>; request(); requested(); first_signal(); wake(); close()
pub enum TestHook { None, PanicBeforeRedirect, PanicAfterRedirect, PanicAfterTerminal, StderrProbe, ArtworkJobPanic, ArtworkEncodingPanic, MetadataJobPanic, WorkerPanic }
pub fn run_contained<R>(label: &'static str, job: impl FnOnce() -> R) -> Result<R, ContainedPanic>;
pub struct TakeOnceSlot<T>;                          // new, publish, try_take
pub struct FatalCleanup;                            // install_panic_hook(Arc<FatalCleanup>)
pub struct TerminalCleanup;                         // restore() idempotent

// application
pub fn resolve_source(input: &str) -> Result<(MediaId, SourceLocation), PlaybackError>;   // application::source
pub fn resolve_podcast(...) -> Result<PodcastResolution, PodcastResolveError>;              // application::podcast
pub fn decide(input: TransportInput, situation: &TransportSituation) -> TransportDecision;  // application::transport
pub struct PlayerRuntime; pub enum AppCommand; pub enum EnqueueItem; pub struct PlayerView;
```

## Task sequence and gates

| Phase | Tasks | Gate at the end of the phase |
|---|---|---|
| A. Test isolation | 1–2 | No test can touch the developer's profile; usage exit status matches §4 |
| B. Queue and durable state | 3–5 | Schema 3 round-trips; every recovery row holds; legacy CLI unchanged |
| C. Engine token contract | 6–7 | Exactly one protected outcome per accepted load, under saturation and shutdown |
| D. Session semantics | 8–10 | Token adoption, queue mutations, advancement and podcast resolution are pure and tested |
| E. Shared lifecycle | 11–12 | `play` locks, flushes on signals and exits `128 + n` |
| F. Runtime | 13–15 | Terminal-free runtime implements §4's transport table against a virtual device |
| G. TUI | 16–23 | `tenuto tui` starts in the §11 order, renders every tier, handles keys, mouse, browser and metadata |
| H. Artwork | 24–25 | Bounded decode, contained panics, protocol detection and cleanup |
| I. Spectrum | 26–28 | Band geometry table, tap ring, mapping, analysis on screen |
| J. Acceptance | 29 | Process/PTY suite, docs, manual terminal checklist, final gates |

---
## Phase A — Test isolation

### Task 1: Route every binary launch through an isolated profile

**Files:**
- Create: `tests/support/process.rs`, `tests/launch_audit.rs`
- Modify: `tests/cli.rs`, `tests/cli_playback.rs`, `tests/http_cli.rs`, `tests/m4_cli.rs:250-275`, `tests/m4_diagnostics.rs:769-778`

**Interfaces:**
- Produces: `process::Profile::new() -> io::Result<Profile>`, `Profile::root(&self) -> &Path`, `Profile::state_dir(&self) -> PathBuf` (`<root>/state/tenuto`), `Profile::state_file(&self) -> PathBuf`, `Profile::command(&self) -> Command`, `process::command_in(root: &Path) -> Command`, `process::binary() -> &'static str`. Include with `#[path = "support/process.rs"] mod process;`.

The audit found seven launch sites in five files. `a_stalled_remote_open_is_quittable_before_anything_loads` currently uses the developer's real `XDG_STATE_HOME`; it is the case §12 names explicitly. Add `#![cfg(target_os = "linux")]` to these five subprocess suites and every later suite using `process` or `pty`. Pure library tests remain portable. The launcher also fails closed on unsupported platforms so a missing suite gate cannot launch against a real profile.

- [ ] **Step 1: Write the failing audit.**

```rust
// tests/launch_audit.rs
//! §12: no test may launch the binary against the developer's real profile.
//! The only file allowed to name the binary is the process helper.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            rust_files(&path, found)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    Ok(())
}

#[test]
fn only_the_process_helper_names_the_binary() -> Result<(), Box<dyn std::error::Error>> {
    // Built with concat! so this file does not match its own search.
    let needle = concat!("CARGO_BIN_EXE_", "tenuto");
    let mut files = Vec::new();
    rust_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests"), &mut files)?;
    let offenders: Vec<_> = files
        .into_iter()
        .filter(|path| !path.ends_with("support/process.rs"))
        .filter(|path| std::fs::read_to_string(path).is_ok_and(|text| text.contains(needle)))
        .collect();
    assert!(offenders.is_empty(), "launch the binary through tests/support/process.rs: {offenders:?}");
    Ok(())
}
```

- [ ] **Step 2: Run `cargo test --locked --test launch_audit`.** Expected: FAIL listing `tests/cli.rs`, `tests/cli_playback.rs`, `tests/http_cli.rs`, `tests/m4_cli.rs`, `tests/m4_diagnostics.rs`.

- [ ] **Step 3: Create the helper.**

```rust
// tests/support/process.rs
//! The one way a test launches `tenuto` (M5 §12). Every child gets its own
//! state, data, cache and config directories, so no test can read, lock or
//! write the developer's playback profile. Keep the `Profile` alive until the
//! child has exited: dropping it deletes the directories under the child.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn binary() -> &'static str {
    assert!(cfg!(target_os = "linux"), "subprocess profile isolation is verified only on Linux");
    env!("CARGO_BIN_EXE_tenuto")
}

/// A command for `tenuto` whose profile lives under `root`, laid out the
/// way the M4 CLI tests already seed it: `root/{state,data,cache,config}`.
pub fn command_in(root: &Path) -> Command {
    let mut command = Command::new(binary());
    command
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env_remove("TENUTO_TEST_HOOK")
        .env_remove("TENUTO_AUDIO_OUTPUT");
    command
}

pub struct Profile {
    root: tempfile::TempDir,
}

impl Profile {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self { root: tempfile::tempdir()? })
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    /// Where `StateStore::platform_path` resolves on Linux under this profile.
    pub fn state_dir(&self) -> PathBuf {
        self.root().join("state").join("tenuto")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir().join("state.json")
    }

    pub fn command(&self) -> Command {
        command_in(self.root())
    }
}
```

- [ ] **Step 4: Migrate each launch site.** Replace `Command::new(env!("CARGO_BIN_EXE_tenuto"))` as follows, keeping every assertion unchanged:
  - `tests/cli.rs`, `tests/cli_playback.rs`, `tests/http_cli.rs`: add `#[path = "support/process.rs"] mod process;`. Their `run(args)` helpers become:

```rust
#[allow(clippy::unwrap_used)] // Fallible spawn of a fixed test binary.
fn run(args: &[&str]) -> std::process::Output {
    let profile = process::Profile::new().unwrap();
    // `output()` waits for the child, so `profile` outlives it.
    profile.command().args(args).output().unwrap()
}
```

  - Inline launches in `tests/cli.rs` (`bare`, `failure`) and `a_relative_path_is_accepted_and_canonicalized` use a local `let profile = process::Profile::new().unwrap();` and `profile.command()`, then keep their `.env`/`.current_dir` calls.
  - `tests/m4_cli.rs:252`: `fn command(root: &Path, args: &[&str]) -> Command { let mut command = process::command_in(root); command.args(args).env("RUST_LOG", "tenuto=warn"); command }`.
  - `tests/m4_diagnostics.rs:769`: `process::command_in(root.path()).args(args).env("RUST_LOG", "tenuto=debug").output()`.

- [ ] **Step 5: Run the audit and the migrated suites.**
Extend `launch_audit` with `subprocess_suites_are_gated_to_verified_platforms`: every top-level test file importing `mod process;` or `mod pty;` must contain `#![cfg(target_os = "linux")]` (build those search strings with `concat!` to avoid self-matches). In the ungated `launch_audit.rs`, import the helper as `unsupported_process` only under `cfg(not(target_os = "linux"))`; a non-Linux test catches the panic from `unsupported_process::binary()` and verifies refusal before constructing any child. This test must not live inside a Linux-gated suite. On Linux, launch a child with inherited `HOME` and a temporary XDG profile; verify the actual `state.json` is under `Profile::state_dir()` and not the inherited home. Later PTY launches must use the same guarded `binary()` and environment construction.
Run: `cargo test --locked --test launch_audit --test cli --test cli_playback --test http_cli --test m4_cli --test m4_diagnostics`
Expected: PASS.

- [ ] **Step 6: Commit.**

```bash
git add tests/support/process.rs tests/launch_audit.rs tests/cli.rs tests/cli_playback.rs tests/http_cli.rs tests/m4_cli.rs tests/m4_diagnostics.rs
git commit -m "test: launch the binary only through an isolated profile"
```

### Task 2: Honor clap's usage and help exit status

**Files:**
- Modify: `src/main.rs:22-28`
- Test: `tests/cli.rs`

**Interfaces:**
- Consumes: `process::Profile` (Task 1).
- Produces: bare `tenuto` exits 2 with usage on stderr; `--help` exits 0 with help on stdout. No library API change.

- [ ] **Step 1: Write failing tests** in `tests/cli.rs`.

```rust
#[test]
fn a_bare_invocation_is_a_usage_error_with_status_two() -> Result<(), Box<dyn std::error::Error>> {
    let profile = process::Profile::new()?;
    let output = profile.command().output()?;
    assert_eq!(output.status.code(), Some(2), "§4: usage errors exit 2");
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("play"));
    Ok(())
}

#[test]
fn help_is_a_successful_entry_point_on_stdout() -> Result<(), Box<dyn std::error::Error>> {
    let profile = process::Profile::new()?;
    let output = profile.command().arg("--help").output()?;
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("play"));
    Ok(())
}
```

- [ ] **Step 2: Run `cargo test --locked --test cli`.** Expected: both FAIL with status `Some(1)`.

- [ ] **Step 3: Implement.** In `src/main.rs` replace the `Err(error)` arm of `try_parse`:

```rust
Err(error) => {
    // clap sends help and version to stdout with status 0 and usage errors
    // to stderr with status 2 (§4); printing it ourselves lost both.
    let _ = error.print();
    return ExitCode::from(u8::try_from(error.exit_code()).unwrap_or(2));
}
```

- [ ] **Step 4: Run `cargo test --locked --test cli --test cli_playback`.** Expected: PASS (existing "exits nonzero" assertions still hold).

- [ ] **Step 5: Commit.**

```bash
git add src/main.rs tests/cli.rs
git commit -m "fix: exit 2 on usage errors and 0 on help"
```

---

## Phase B — Queue and durable state

### Task 3: Pure queue model with occurrence IDs

**Files:**
- Create: `src/queue.rs`, `tests/m5_queue.rs`
- Modify: `src/lib.rs` (add `pub mod queue;`)

**Interfaces:**
- Consumes: `media::id::{AbsolutePath, MediaId, NormalizedUrl}`, `playback::provenance::PositionProvenance`, `url::Url`.
- Produces: everything under `// queue` in the Interface registry. `QueueEntryId::from_raw` and `Queue::set_active`, `Queue::from_parts` are `pub(crate)` so only persistence and `Session` can create IDs from numbers or change the active entry.

Semantics that later tasks rely on:
- `enqueue` appends in batch order and returns the new IDs; it rejects the whole batch with `Capacity { requested, available }` when `len + batch.len() > MAX_QUEUE_ENTRIES`.
- IDs are never reused within a `Queue`'s lifetime: `next_id` starts at `Some(1)`; `from_parts` uses checked `max(id) + 1`, with `None` representing exhaustion. Preflight the entire batch's ID range before mutating entries or the allocator. Empty batches succeed even when exhausted.
- `move_entry(id, Up)` swaps with the previous entry (toward index 0); at an edge it returns `Ok(false)` and changes nothing.
- `remove(id)` returns `Removed { entry, was_active, selection }`; `selection` is the entry now at the removed index, else the new last entry, else `None`. Removing the active entry clears `active`.
- `neighbor(anchor, Up|Down)` returns the adjacent ID without wrapping.
- `clear()` removes all entries, clears `active`, keeps `next_id`.
- `source_matches(media, source)`: `LocalFile(p)` ↔ `MediaId::LocalFile(q)` with `p == q`; `RemoteUrl(u)` ↔ `MediaId::RemoteUrl(v)` with `u == v`; `Podcast { fallback }` ↔ `MediaId::PodcastEpisode { .. }` with an `http`/`https` fallback that has a host.

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_queue.rs
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::queue::{
    Direction, DisplayMetadata, MAX_QUEUE_ENTRIES, NewQueueEntry, Queue, QueueError, QueueSource,
};

fn local(name: &str) -> NewQueueEntry {
    let path = AbsolutePath::new(format!("/music/{name}.flac").into()).expect("absolute");
    NewQueueEntry::new(
        MediaId::LocalFile(path.clone()),
        QueueSource::LocalFile(path),
        DisplayMetadata::default(),
    )
    .expect("matching source")
}

fn many(prefix: &str, count: usize) -> Vec<NewQueueEntry> {
    (0..count).map(|i| local(&format!("{prefix}{i}"))).collect()
}

#[test]
fn duplicate_media_gets_distinct_occurrence_ids() {
    let mut queue = Queue::default();
    let ids = queue.enqueue(vec![local("a"), local("a")]).expect("fits");
    assert_ne!(ids[0], ids[1]);
    assert_eq!(queue.get(ids[0]).map(|e| e.media()), queue.get(ids[1]).map(|e| e.media()));
}

#[test]
fn an_oversized_batch_is_rejected_whole_and_changes_nothing() {
    let mut queue = Queue::default();
    queue.enqueue(many("t", 250)).expect("fits");
    let error = queue.enqueue(many("u", 7)).expect_err("257 exceeds the cap");
    assert!(matches!(error, QueueError::Capacity { requested: 7, available: 6 }));
    assert_eq!(queue.len(), 250);
    queue.enqueue(many("v", 6)).expect("exactly the cap fits");
    assert_eq!(queue.len(), MAX_QUEUE_ENTRIES);
}

#[test]
fn a_mismatched_source_is_refused_at_construction() {
    let a = AbsolutePath::new("/music/a.flac".into()).expect("absolute");
    let b = AbsolutePath::new("/music/b.flac".into()).expect("absolute");
    let result = NewQueueEntry::new(
        MediaId::LocalFile(a),
        QueueSource::LocalFile(b),
        DisplayMetadata::default(),
    );
    assert!(matches!(result, Err(QueueError::SourceMismatch)));
}

#[test]
fn reordering_keeps_ids_and_stops_at_the_edges() {
    let mut queue = Queue::default();
    let ids = queue.enqueue(many("r", 3)).expect("fits");
    assert!(queue.move_entry(ids[2], Direction::Up).expect("known"));
    assert_eq!(queue.entries().iter().map(|e| e.id()).collect::<Vec<_>>(), [ids[0], ids[2], ids[1]]);
    assert!(!queue.move_entry(ids[0], Direction::Up).expect("known"));
    assert!(matches!(
        queue.move_entry(tenuto_unknown_id(&mut queue), Direction::Down),
        Err(QueueError::UnknownEntry(_))
    ));
}

/// An ID that no longer exists: enqueue then remove it.
fn tenuto_unknown_id(queue: &mut Queue) -> tenuto::queue::QueueEntryId {
    let id = queue.enqueue(vec![local("gone")]).expect("fits")[0];
    queue.remove(id).expect("known");
    id
}

#[test]
fn removal_selects_the_successor_or_the_predecessor_at_the_end() {
    let mut queue = Queue::default();
    let ids = queue.enqueue(many("s", 3)).expect("fits");
    assert_eq!(queue.remove(ids[1]).expect("known").selection, Some(ids[2]));
    assert_eq!(queue.remove(ids[2]).expect("known").selection, Some(ids[0]));
    assert_eq!(queue.remove(ids[0]).expect("known").selection, None);
}

#[test]
fn ids_are_never_reused_after_removal_or_clear() {
    let mut queue = Queue::default();
    let first = queue.enqueue(vec![local("a")]).expect("fits")[0];
    queue.clear();
    let second = queue.enqueue(vec![local("a")]).expect("fits")[0];
    assert!(second > first);
}

#[test]
fn neighbors_do_not_wrap() {
    let mut queue = Queue::default();
    let ids = queue.enqueue(many("n", 2)).expect("fits");
    assert_eq!(queue.neighbor(ids[0], Direction::Down), Some(ids[1]));
    assert_eq!(queue.neighbor(ids[1], Direction::Down), None);
    assert_eq!(queue.neighbor(ids[0], Direction::Up), None);
}
```

- [ ] **Step 2: Run `cargo test --locked --test m5_queue`.** Expected: FAIL, unresolved module `tenuto::queue`.

- [ ] **Step 3: Implement `src/queue.rs`.** Core shape (write doc comments in the repository's explanatory style):

```rust
//! The playback queue (M5 §5): ordered occurrences with stable IDs. Pure
//! data and policy. It lives inside `PersistedState` and changes only
//! through `Session`; it never renders, decodes or touches the filesystem.

use std::time::Duration;
use url::Url;

use crate::media::id::{AbsolutePath, MediaId, NormalizedUrl};
use crate::playback::provenance::PositionProvenance;

pub const MAX_QUEUE_ENTRIES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct QueueEntryId(u64);

impl QueueEntryId {
    pub fn get(self) -> u64 { self.0 }
    pub(crate) fn from_raw(raw: u64) -> Self { Self(raw) }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueueSource {
    LocalFile(AbsolutePath),
    RemoteUrl(NormalizedUrl),
    /// The episode identity lives in the entry's `MediaId`; this is only the
    /// last resolved enclosure, used when the cache cannot answer (§5).
    Podcast { fallback: Url },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurationSource { Decoded(PositionProvenance), Declared }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayDuration { pub value: Duration, pub source: DurationSource }

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DisplayMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<DisplayDuration>,
}

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum QueueError {
    #[error("the queue holds at most {MAX_QUEUE_ENTRIES} entries; {requested} requested, {available} free")]
    Capacity { requested: usize, available: usize },
    #[error("queue entry IDs are exhausted; cannot enqueue more entries in this session")]
    IdExhausted,
    #[error("queue source does not match its media identity")]
    SourceMismatch,
    #[error("queue entry {} is no longer queued", .0.get())]
    UnknownEntry(QueueEntryId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction { Up, Down }

pub(crate) fn source_matches(media: &MediaId, source: &QueueSource) -> bool {
    match (media, source) {
        (MediaId::LocalFile(path), QueueSource::LocalFile(source)) => path == source,
        (MediaId::RemoteUrl(url), QueueSource::RemoteUrl(source)) => url == source,
        (MediaId::PodcastEpisode { .. }, QueueSource::Podcast { fallback }) => {
            matches!(fallback.scheme(), "http" | "https") && fallback.host_str().is_some()
        }
        _ => false,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewQueueEntry { media: MediaId, source: QueueSource, display: DisplayMetadata }

impl NewQueueEntry {
    pub fn new(media: MediaId, source: QueueSource, display: DisplayMetadata) -> Result<Self, QueueError> {
        if !source_matches(&media, &source) {
            return Err(QueueError::SourceMismatch);
        }
        Ok(Self { media, source, display })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueEntry { id: QueueEntryId, media: MediaId, source: QueueSource, display: DisplayMetadata }

impl QueueEntry {
    pub fn id(&self) -> QueueEntryId { self.id }
    pub fn media(&self) -> &MediaId { &self.media }
    pub fn source(&self) -> &QueueSource { &self.source }
    pub fn display(&self) -> &DisplayMetadata { &self.display }
    pub(crate) fn display_mut(&mut self) -> &mut DisplayMetadata { &mut self.display }
    pub(crate) fn set_source(&mut self, source: QueueSource) -> Result<(), QueueError> {
        if !source_matches(&self.media, &source) { return Err(QueueError::SourceMismatch); }
        self.source = source;
        Ok(())
    }
}

#[derive(Debug)]
pub struct Removed { pub entry: QueueEntry, pub was_active: bool, pub selection: Option<QueueEntryId> }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Queue { entries: Vec<QueueEntry>, active: Option<QueueEntryId>, next_id: Option<u64> }

impl Default for Queue {
    fn default() -> Self { Self { entries: Vec::new(), active: None, next_id: Some(1) } }
}

impl Queue {
    pub fn entries(&self) -> &[QueueEntry] { &self.entries }
    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
    pub fn active(&self) -> Option<QueueEntryId> { self.active }
    pub fn first(&self) -> Option<QueueEntryId> { self.entries.first().map(QueueEntry::id) }
    pub fn index_of(&self, id: QueueEntryId) -> Option<usize> { self.entries.iter().position(|e| e.id == id) }
    pub fn get(&self, id: QueueEntryId) -> Option<&QueueEntry> { self.entries.iter().find(|e| e.id == id) }
    pub(crate) fn get_mut(&mut self, id: QueueEntryId) -> Option<&mut QueueEntry> { self.entries.iter_mut().find(|e| e.id == id) }

    pub fn enqueue(&mut self, batch: Vec<NewQueueEntry>) -> Result<Vec<QueueEntryId>, QueueError> {
        let available = MAX_QUEUE_ENTRIES.saturating_sub(self.entries.len());
        if batch.len() > available {
            return Err(QueueError::Capacity { requested: batch.len(), available });
        }
        if batch.is_empty() { return Ok(Vec::new()); }
        let count = u64::try_from(batch.len()).map_err(|_| QueueError::IdExhausted)?;
        let first = self.next_id.ok_or(QueueError::IdExhausted)?;
        let last = first.checked_add(count - 1).ok_or(QueueError::IdExhausted)?;
        let ids = (first..=last).zip(batch).map(|(raw, new)| {
            let id = QueueEntryId(raw);
            self.entries.push(QueueEntry { id, media: new.media, source: new.source, display: new.display });
            id
        }).collect();
        self.next_id = last.checked_add(1);
        Ok(ids)
    }

    pub fn move_entry(&mut self, id: QueueEntryId, direction: Direction) -> Result<bool, QueueError> {
        let index = self.index_of(id).ok_or(QueueError::UnknownEntry(id))?;
        let other = match direction {
            Direction::Up => index.checked_sub(1),
            Direction::Down => Some(index + 1).filter(|next| *next < self.entries.len()),
        };
        match other {
            Some(other) => { self.entries.swap(index, other); Ok(true) }
            None => Ok(false),
        }
    }

    pub fn remove(&mut self, id: QueueEntryId) -> Result<Removed, QueueError> {
        let index = self.index_of(id).ok_or(QueueError::UnknownEntry(id))?;
        let entry = self.entries.remove(index);
        let was_active = self.active == Some(id);
        if was_active { self.active = None; }
        let selection = self.entries.get(index).or_else(|| self.entries.last()).map(QueueEntry::id);
        Ok(Removed { entry, was_active, selection })
    }

    pub fn clear(&mut self) -> Vec<QueueEntry> {
        self.active = None;
        std::mem::take(&mut self.entries)
    }

    pub fn neighbor(&self, anchor: QueueEntryId, direction: Direction) -> Option<QueueEntryId> {
        let index = self.index_of(anchor)?;
        let target = match direction { Direction::Up => index.checked_sub(1)?, Direction::Down => index + 1 };
        self.entries.get(target).map(QueueEntry::id)
    }

    /// Only `Session` adopts or clears an occurrence (§3).
    pub(crate) fn set_active(&mut self, id: Option<QueueEntryId>) -> Result<(), QueueError> {
        if let Some(id) = id && self.get(id).is_none() { return Err(QueueError::UnknownEntry(id)); }
        self.active = id;
        Ok(())
    }

    /// Persistence's constructor, used only after `queue_codec` validated
    /// uniqueness, identity and capacity.
    pub(crate) fn from_parts(entries: Vec<QueueEntry>, active: Option<QueueEntryId>) -> Self {
        let next_id = entries.iter().map(|e| e.id.0).max().map_or(Some(1), |max| max.checked_add(1));
        Self { entries, active, next_id }
    }

    pub(crate) fn entry_from_parts(id: QueueEntryId, media: MediaId, source: QueueSource, display: DisplayMetadata) -> QueueEntry {
        QueueEntry { id, media, source, display }
    }
}
```

Add this unit test inside `queue.rs`, where the allocator field is accessible:

```rust
#[cfg(test)]
mod exhaustion_tests {
    use super::*;
    #[test]
    fn exhaustion_rejects_the_whole_batch_without_reusing_an_id() {
        let path = AbsolutePath::new("/music/a.flac".into()).expect("absolute");
        let item = NewQueueEntry::new(MediaId::LocalFile(path.clone()),
            QueueSource::LocalFile(path), DisplayMetadata::default()).expect("entry");
        let mut queue = Queue { next_id: Some(u64::MAX), ..Queue::default() };
        let before = queue.clone();
        assert_eq!(queue.enqueue(vec![item.clone(), item.clone()]), Err(QueueError::IdExhausted));
        assert_eq!(queue, before, "no partial append or allocator change");
        assert_eq!(queue.enqueue(vec![item.clone()]).expect("last ID")[0].get(), u64::MAX);
        let mut restored = Queue::from_parts(queue.entries().to_vec(), None);
        assert_eq!(restored.enqueue(vec![item.clone()]), Err(QueueError::IdExhausted));
        queue.clear();
        assert_eq!(queue.enqueue(vec![item]), Err(QueueError::IdExhausted));
        assert!(queue.enqueue(Vec::new()).expect("empty batch").is_empty());
    }
}
```

- [ ] **Step 4: Run `cargo test --locked --test m5_queue --lib`.** Expected: PASS.
- [ ] **Step 5: Run `cargo clippy --locked --all-targets -- -D warnings`.** Expected: clean.
- [ ] **Step 6: Commit.**

```bash
git add src/queue.rs src/lib.rs tests/m5_queue.rs
git commit -m "feat: add the pure playback queue model"
```

### Task 4: Schema 3 with independently decoded queue fields

**Files:**
- Create: `src/persistence/queue_codec.rs`, `tests/m5_state_schema.rs`
- Modify: `src/persistence/model.rs`, `src/persistence/mod.rs`, `src/persistence/store.rs:380-419` (decode path only), `tests/persistence_model.rs` (schema number assertions only)

**Interfaces:**
- Consumes: `queue::*` (Task 3).
- Produces: `SCHEMA_VERSION = 3`; `PersistedState::queue()`, `pub(crate) queue_mut()`; `queue_codec::{recover_queue, QueueReset, ActiveProblem, QueueProblem}`; `pub(super) struct RawState` with `pub(super) fn into_state(self, accept_queue: bool) -> (PersistedState, Option<QueueReset>)`. `PersistedState` keeps implementing `Serialize` and `Deserialize` (hand-written) so existing `serde_json::from_*::<PersistedState>` test call sites still compile.

On-disk shape added by schema 3:

```json
{
  "schema_version": 3,
  "current_media": "local:/music/a.flac",
  "volume": 0.8,
  "checkpoints": {},
  "queue": [
    { "id": 1, "media": "local:/music/a.flac", "source": { "kind": "local", "path": "/music/a.flac" },
      "display": { "title": "A", "artist": null, "album": null, "duration_ms": 500, "duration_source": "decoded" } },
    { "id": 2, "media": "podcast:<feed>/guid:x", "source": { "kind": "podcast", "fallback_url": "https://cdn.example/x.mp3" },
      "display": { "title": "X", "duration_ms": 3600000, "duration_source": "declared" } }
  ],
  "active_entry": 1
}
```

`duration_source` is one of `decoded`, `decoded_estimated`, `declared`. `display` and each display field default when absent.

Recovery rules, in this order (first match reports its cause):

1. Base fields (envelope, `current_media`, `volume`, `checkpoints`) decode exactly as today; a failure there is still a whole-file malformed state.
2. Files at schema 1 or 2 ignore `queue`/`active_entry` entirely: empty queue, no reset.
3. `queue` absent or `null` → empty queue.
4. `queue` not an array, or any element fails `QueueEntryDto` decoding (including an unparseable `media` string) → `WholeQueue(Malformed)`.
5. A repeated `id` → `WholeQueue(DuplicateIds)`.
6. Any entry where the source does not match the media (Task 3 `source_matches`, and for local/remote the stored `path`/`url` equals the identity) → `WholeQueue(SourceMismatch)`.
7. `len > MAX_QUEUE_ENTRIES` → `WholeQueue(OverCapacity { found })`.
8. `active_entry` absent or `null` → no active entry, no reset.
9. `active_entry` not a non-negative integer → `ActiveReference(Malformed)`; not among the IDs → `ActiveReference(Dangling)`; that entry's media ≠ `current_media` (including `current_media` absent) → `ActiveReference(MediaMismatch)`. Entries are kept.
10. A whole-queue reset also clears the active reference.

Never infer an active entry from a `MediaId`.

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_state_schema.rs
use tenuto::persistence::model::{PersistedState, SCHEMA_VERSION};
use tenuto::persistence::queue_codec::{ActiveProblem, QueueProblem, QueueReset, recover_queue};
use serde_json::json;

mod support;
use support::media;

fn entry(id: u64, name: &str) -> serde_json::Value {
    json!({ "id": id, "media": format!("local:/music/{name}.flac"),
            "source": { "kind": "local", "path": format!("/music/{name}.flac") } })
}

fn history() -> serde_json::Value {
    json!({ "local:/music/a.flac": { "position": { "secs": 42, "nanos": 0 }, "completed": false,
             "touch_seq": 7, "updated_at": "2026-09-14T10:00:00Z" } })
}

#[test]
fn schema_three_round_trips_the_queue_and_active_entry() {
    assert_eq!(SCHEMA_VERSION, 3);
    let file = json!({ "schema_version": 3, "current_media": "local:/music/a.flac", "volume": 0.5,
        "checkpoints": history(), "queue": [entry(4, "a"), entry(9, "a")], "active_entry": 9 });
    let state: PersistedState = serde_json::from_value(file).expect("valid");
    assert_eq!(state.queue().len(), 2);
    assert_eq!(state.queue().active().map(|id| id.get()), Some(9));
    let back = serde_json::to_value(&state).expect("serializes");
    assert_eq!(back["schema_version"], 3);
    assert_eq!(back["active_entry"], 9);
    assert_eq!(back["queue"][1]["id"], 9);
}

#[test]
fn a_version_two_file_migrates_to_an_empty_queue_keeping_current_media() {
    let file = json!({ "schema_version": 2, "current_media": "local:/music/a.flac", "volume": 0.5,
        "checkpoints": history(), "queue": "ignored before schema 3" });
    let state: PersistedState = serde_json::from_value(file).expect("valid");
    assert!(state.queue().is_empty());
    assert_eq!(state.current_media(), Some(&media("a")));
    assert!(state.entry_for(&media("a")).is_some());
}

#[test]
fn a_wrong_queue_type_resets_only_the_queue() {
    let file = json!({ "schema_version": 3, "current_media": "local:/music/a.flac", "volume": 0.25,
        "checkpoints": history(), "queue": 7, "active_entry": "x" });
    let state: PersistedState = serde_json::from_value(file).expect("base survives");
    assert!(state.queue().is_empty());
    assert_eq!(state.volume().percent(), 25);
    assert_eq!(state.entry_for(&media("a")).and_then(|e| e.position).map(|p| p.as_secs()), Some(42));
    assert_eq!(state.current_media(), Some(&media("a")));
}

#[test]
fn every_whole_queue_problem_is_classified() {
    let cur = media("a");
    let cases = [
        (json!([entry(1, "a"), { "id": 2 }]), QueueProblem::Malformed),
        (json!([entry(1, "a"), entry(1, "b")]), QueueProblem::DuplicateIds),
        (json!([{ "id": 1, "media": "local:/music/a.flac", "source": { "kind": "local", "path": "/music/b.flac" } }]), QueueProblem::SourceMismatch),
        (serde_json::Value::Array((1..=257).map(|i| entry(i, &format!("t{i}"))).collect()), QueueProblem::OverCapacity { found: 257 }),
    ];
    for (queue, problem) in cases {
        let (recovered, reset) = recover_queue(Some(&queue), Some(&json!(1)), Some(&cur));
        assert!(recovered.is_empty() && recovered.active().is_none());
        assert_eq!(reset, Some(QueueReset::WholeQueue(problem)));
    }
}

#[test]
fn active_reference_problems_keep_the_entries() {
    let queue = json!([entry(1, "a"), entry(2, "b")]);
    let cur = media("a");
    for (active, current, problem) in [
        (json!("one"), Some(&cur), ActiveProblem::Malformed),
        (json!(99), Some(&cur), ActiveProblem::Dangling),
        (json!(2), Some(&cur), ActiveProblem::MediaMismatch),
        (json!(1), None, ActiveProblem::MediaMismatch),
    ] {
        let (recovered, reset) = recover_queue(Some(&queue), Some(&active), current);
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered.active(), None);
        assert_eq!(reset, Some(QueueReset::ActiveReference(problem)));
    }
}

#[test]
fn a_missing_or_null_active_reference_needs_no_recovery() {
    let queue = json!([entry(1, "a")]);
    assert_eq!(recover_queue(Some(&queue), None, None).1, None);
    assert_eq!(recover_queue(Some(&queue), Some(&serde_json::Value::Null), None).1, None);
    assert_eq!(recover_queue(None, None, None).1, None);
}

#[test]
fn a_duplicate_occurrence_is_never_inferred_active_from_media() {
    let file = json!({ "schema_version": 3, "current_media": "local:/music/a.flac", "volume": 1.0,
        "checkpoints": {}, "queue": [entry(1, "a"), entry(2, "a")] });
    let state: PersistedState = serde_json::from_value(file).expect("valid");
    assert_eq!(state.queue().active(), None);
}
```

Before running, confirm the checkpoint JSON for `position` against a file written by `StateStore::write` (serde's default `Duration` shape is `{"secs":..,"nanos":..}`); adjust `history()` if the model uses a different shape.

Add a schema-boundary regression using the existing `entry` helper:

```rust
#[test]
fn a_valid_maximum_id_is_preserved_but_cannot_be_reallocated() {
    use tenuto::queue::{DisplayMetadata, NewQueueEntry, QueueError, QueueSource};
    use tenuto::media::id::MediaId;
    let file = json!({ "schema_version": 3, "queue": [entry(u64::MAX, "a")] });
    let state: PersistedState = serde_json::from_value(file).expect("valid maximum ID");
    let mut queue = state.queue().clone();
    let before = queue.clone();
    let MediaId::LocalFile(path) = media("b") else { unreachable!() };
    let item = NewQueueEntry::new(media("b"), QueueSource::LocalFile(path), DisplayMetadata::default()).expect("entry");
    assert_eq!(queue.enqueue(vec![item]), Err(QueueError::IdExhausted));
    assert_eq!(queue, before);
    assert_eq!(queue.entries()[0].id().get(), u64::MAX);
}
```

- [ ] **Step 2: Run `cargo test --locked --test m5_state_schema`.** Expected: FAIL, unresolved `queue_codec` and `queue()`.

- [ ] **Step 3: Implement `queue_codec.rs`.**

```rust
//! Queue fields of the state file, decoded apart from listening history so a
//! damaged queue can never cost a checkpoint (M5 §6).

use std::collections::BTreeSet;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::media::id::{AbsolutePath, MediaId, NormalizedUrl};
use crate::playback::provenance::PositionProvenance;
use crate::queue::{DisplayDuration, DisplayMetadata, DurationSource, MAX_QUEUE_ENTRIES, Queue, QueueEntry, QueueEntryId, QueueSource};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveProblem { Malformed, Dangling, MediaMismatch }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueProblem { Malformed, DuplicateIds, SourceMismatch, OverCapacity { found: usize } }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueReset { ActiveReference(ActiveProblem), WholeQueue(QueueProblem) }

impl QueueReset {
    /// For the sanitized startup warning (§6): which fields were reset.
    pub fn fields_reset(self) -> &'static str {
        match self {
            Self::ActiveReference(_) => "the active queue entry",
            Self::WholeQueue(_) => "the queue and its active entry",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SourceDto { Local { path: String }, Remote { url: String }, Podcast { fallback_url: String } }

#[derive(Default, Serialize, Deserialize)]
struct DisplayDto {
    #[serde(default)] title: Option<String>,
    #[serde(default)] artist: Option<String>,
    #[serde(default)] album: Option<String>,
    #[serde(default)] duration_ms: Option<u64>,
    #[serde(default)] duration_source: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct QueueEntryDto { id: u64, media: MediaId, source: SourceDto, #[serde(default)] display: DisplayDto }

pub(crate) fn encode(queue: &Queue) -> Vec<QueueEntryDto> { /* map each entry; duration_source: "decoded" | "decoded_estimated" | "declared" */ }

pub fn recover_queue(queue: Option<&Value>, active: Option<&Value>, current_media: Option<&MediaId>) -> (Queue, Option<QueueReset>) {
    let entries = match queue {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => match decode_entries(items) {
            Ok(entries) => entries,
            Err(problem) => return (Queue::default(), Some(QueueReset::WholeQueue(problem))),
        },
        Some(_) => return (Queue::default(), Some(QueueReset::WholeQueue(QueueProblem::Malformed))),
    };
    let active_id = match active {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            None => return (Queue::from_parts(entries, None), Some(QueueReset::ActiveReference(ActiveProblem::Malformed))),
            Some(raw) => {
                let id = QueueEntryId::from_raw(raw);
                match entries.iter().find(|e| e.id() == id) {
                    None => return (Queue::from_parts(entries, None), Some(QueueReset::ActiveReference(ActiveProblem::Dangling))),
                    Some(entry) if Some(entry.media()) != current_media => {
                        return (Queue::from_parts(entries, None), Some(QueueReset::ActiveReference(ActiveProblem::MediaMismatch)));
                    }
                    Some(_) => Some(id),
                }
            }
        },
    };
    (Queue::from_parts(entries, active_id), None)
}

fn decode_entries(items: &[Value]) -> Result<Vec<QueueEntry>, QueueProblem> {
    let dtos = items.iter()
        .map(|item| serde_json::from_value::<QueueEntryDto>(item.clone()).map_err(|_| QueueProblem::Malformed))
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen = BTreeSet::new();
    if !dtos.iter().all(|dto| seen.insert(dto.id)) { return Err(QueueProblem::DuplicateIds); }
    let entries = dtos.into_iter().map(entry_from_dto).collect::<Result<Vec<_>, _>>()?;
    if entries.len() > MAX_QUEUE_ENTRIES { return Err(QueueProblem::OverCapacity { found: entries.len() }); }
    Ok(entries)
}
```

`entry_from_dto` builds the `QueueSource` (`Local` → `AbsolutePath::new(path)`; `Remote` → `NormalizedUrl::parse(url)`; `Podcast` → `Url::parse(fallback_url)`), returning `Malformed` when any of those constructors fail, then `SourceMismatch` unless `crate::queue::source_matches` holds. It maps `duration_ms` + `duration_source` to `DisplayDuration`, dropping the duration (not the entry) if the source label is unknown.

- [ ] **Step 4: Rework `PersistedState`.** Remove `#[derive(Serialize, Deserialize)]` and `#[serde(from = "RawState")]` from the struct; add `queue: Queue`; `Default` uses `Queue::default()`.

```rust
#[derive(Deserialize)]
pub(super) struct RawState {
    schema_version: u32,
    #[serde(default)] current_media: Option<MediaId>,
    #[serde(default = "full_gain")] volume: f32,
    #[serde(default)] checkpoints: BTreeMap<MediaId, PersistedCheckpoint>,
    #[serde(default)] queue: Option<serde_json::Value>,
    #[serde(default)] active_entry: Option<serde_json::Value>,
}

impl RawState {
    pub(super) fn schema_version(&self) -> u32 { self.schema_version }

    /// `accept_queue` is false for schema 1/2 files and for read-only
    /// snapshots, which never examine queue data.
    pub(super) fn into_state(self, accept_queue: bool) -> (PersistedState, Option<QueueReset>) {
        let next_seq = self.checkpoints.values().map(|e| e.touch_seq).max().map_or(1, |h| h.saturating_add(1));
        let (queue, reset) = if accept_queue && self.schema_version >= 3 {
            recover_queue(self.queue.as_ref(), self.active_entry.as_ref(), self.current_media.as_ref())
        } else {
            (Queue::default(), None)
        };
        (PersistedState { schema_version: self.schema_version, current_media: self.current_media,
            volume: self.volume, checkpoints: self.checkpoints, queue, next_seq }, reset)
    }
}

impl<'de> Deserialize<'de> for PersistedState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(RawState::deserialize(deserializer)?.into_state(true).0)
    }
}

impl Serialize for PersistedState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Out<'a> {
            schema_version: u32,
            current_media: &'a Option<MediaId>,
            volume: f32,
            checkpoints: &'a BTreeMap<MediaId, PersistedCheckpoint>,
            queue: Vec<crate::persistence::queue_codec::QueueEntryDto>,
            active_entry: Option<u64>,
        }
        Out { schema_version: self.schema_version, current_media: &self.current_media, volume: self.volume,
            checkpoints: &self.checkpoints, queue: queue_codec::encode(&self.queue),
            active_entry: self.queue.active().map(QueueEntryId::get) }.serialize(serializer)
    }
}
```

Add `pub fn queue(&self) -> &Queue` and `pub(crate) fn queue_mut(&mut self) -> &mut Queue`. `migrate_to_current_schema` is unchanged. `evict_one` still protects only `current_media` (queue membership does not pin).

- [ ] **Step 5: Point `decode_state` at `RawState`.** In `store.rs` `decode_state` deserializes `RawState`, calls `into_state(true)` and, for now, discards the reset (Task 5 consumes it). `read_snapshot` calls `into_state(false)`. Keep the version-first envelope check and `malformed` sanitization exactly as they are. Update `tests/persistence_model.rs` only where it asserts the literal schema number 2 for newly written state.

- [ ] **Step 6: Run the persistence suites.**
Run: `cargo test --locked --test m5_state_schema --test persistence_model --test persistence_store --test persistence_writer --test m4_state_snapshot --test m4_library_reads --test m4_library_mutations --test m4_library_refresh`
Expected: PASS.

- [ ] **Step 7: Commit.**

```bash
git add src/persistence tests/m5_state_schema.rs tests/persistence_model.rs
git commit -m "feat: add schema 3 queue fields with independent recovery"
```

### Task 5: Back up a repaired state file before any writer runs

**Files:**
- Modify: `src/persistence/store.rs`, `src/persistence/atomic.rs`, `src/app.rs:920-983` (log the repair), `tests/persistence_store.rs` only if it constructs `LoadOutcome` literals
- Create: `tests/m5_state_recovery.rs`

**Interfaces:**
- Consumes: `RawState::into_state`, `QueueReset` (Task 4).
- Produces: `LoadOutcome::queue_repair: Option<QueueRepair>`, `QueueRepair { reset, backup }`, `QueueBackup::{Saved(PathBuf), Failed}`; `atomic::create_private_new(path: &Path) -> io::Result<File>` (the existing `private_file`, renamed and made `pub(crate)`), `atomic::sync_parent_best_effort(dir: &Path)`.

Behavior: when `decode_state` yields a reset, `load` copies the **exact original bytes** to `state.json.queue-recovery-<stamp>` (then `-2` … `-100`), opened with create-new private semantics, `write_all` + `sync_all` + best-effort parent sync. Success → `writable: true`, `reason: Loaded`, `queue_repair: Some(QueueRepair { reset, backup: Saved(path) })`. Failure → `writable: false` and `backup: Failed`; the original file is left untouched and the recovered state is still returned. The original is never renamed. `read_snapshot` never copies or writes.

`open_persistence` in `app.rs` logs a sanitized warning: `tracing::warn!(fields = reset.fields_reset(), backup = ?path, "queue data in the state file was reset")` or the `Failed` variant's "persistence is disabled for this session" message. The TUI surfaces the same text as a status message in Task 17.

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_state_recovery.rs
use std::sync::Arc;
use tenuto::clock::FakeClock;
use tenuto::persistence::queue_codec::{QueueProblem, QueueReset};
use tenuto::persistence::store::{LoadReason, QueueBackup, StateStore};
use serde_json::json;

mod support;
use support::media;

fn write_state(dir: &std::path::Path, value: serde_json::Value) -> std::path::PathBuf {
    let path = dir.join("state.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&value).expect("json")).expect("write");
    path
}

fn over_capacity() -> serde_json::Value {
    json!({ "schema_version": 3, "current_media": "local:/music/a.flac", "volume": 0.3,
        "checkpoints": { "local:/music/a.flac": { "position": { "secs": 5, "nanos": 0 }, "completed": true,
            "touch_seq": 3, "updated_at": "2026-09-14T10:00:00Z" } },
        "queue": (1..=300).map(|i| json!({ "id": i, "media": format!("local:/music/t{i}.flac"),
            "source": { "kind": "local", "path": format!("/music/t{i}.flac") } })).collect::<Vec<_>>(),
        "active_entry": 1 })
}

#[test]
fn a_repaired_queue_is_backed_up_byte_for_byte_and_writing_stays_enabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_state(dir.path(), over_capacity());
    let original = std::fs::read(&path).expect("read");
    let outcome = StateStore::new(path.clone(), Arc::new(FakeClock::new())).load();

    assert!(outcome.writable);
    assert!(matches!(outcome.reason, LoadReason::Loaded));
    let repair = outcome.queue_repair.expect("repair reported");
    assert_eq!(repair.reset, QueueReset::WholeQueue(QueueProblem::OverCapacity { found: 300 }));
    let QueueBackup::Saved(backup) = repair.backup else { panic!("backup must succeed") };
    assert_eq!(std::fs::read(&backup).expect("backup"), original);
    assert_eq!(std::fs::read(&path).expect("original untouched"), original);
    assert!(outcome.state.queue().is_empty());
    assert_eq!(outcome.state.volume().percent(), 30);
    assert!(outcome.state.completed_for(&media("a")));
    assert_eq!(outcome.state.current_media(), Some(&media("a")));
}

#[test]
fn a_second_repair_never_overwrites_an_earlier_backup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_state(dir.path(), over_capacity());
    let store = StateStore::new(path.clone(), Arc::new(FakeClock::new()));
    let first = store.load().queue_repair.expect("repair");
    let second = store.load().queue_repair.expect("repair");
    let (QueueBackup::Saved(a), QueueBackup::Saved(b)) = (first.backup, second.backup) else { panic!("both saved") };
    assert_ne!(a, b);
}

#[test]
fn a_failed_backup_disables_writing_and_leaves_the_original_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_state(dir.path(), over_capacity());
    let original = std::fs::read(&path).expect("read");
    // FakeClock's wall clock is the Unix epoch; occupy every candidate name.
    for suffix in 1..=100 {
        let name = if suffix == 1 { "state.json.queue-recovery-19700101T000000Z".to_string() }
                   else { format!("state.json.queue-recovery-19700101T000000Z-{suffix}") };
        std::fs::write(dir.path().join(name), b"occupied").expect("occupy");
    }
    let outcome = StateStore::new(path.clone(), Arc::new(FakeClock::new())).load();
    assert!(!outcome.writable);
    assert!(matches!(outcome.queue_repair.expect("repair").backup, QueueBackup::Failed));
    assert_eq!(std::fs::read(&path).expect("original"), original);
    assert_eq!(outcome.state.volume().percent(), 30);
}

#[test]
fn read_only_snapshots_ignore_invalid_queue_data_without_side_effects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_state(dir.path(), over_capacity());
    let snapshot = StateStore::new(path, Arc::new(FakeClock::new())).read_snapshot().expect("history readable");
    assert!(snapshot.completed_for(&media("a")));
    assert_eq!(std::fs::read_dir(dir.path()).expect("dir").count(), 1, "no backup, no quarantine");
}

#[test]
fn invalid_base_state_and_newer_versions_keep_their_existing_handling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_state(dir.path(), json!({ "schema_version": 3, "checkpoints": 5, "queue": [] }));
    let outcome = StateStore::new(path.clone(), Arc::new(FakeClock::new())).load();
    assert!(matches!(outcome.reason, LoadReason::Quarantined { .. }));
    assert!(outcome.queue_repair.is_none());

    let path = write_state(dir.path(), json!({ "schema_version": 4, "queue": 7 }));
    let outcome = StateStore::new(path, Arc::new(FakeClock::new())).load();
    assert!(matches!(outcome.reason, LoadReason::UnsupportedVersion { found: 4 }));
    assert!(!outcome.writable);
}
```

- [ ] **Step 2: Run `cargo test --locked --test m5_state_recovery`.** Expected: FAIL, no field `queue_repair`.

- [ ] **Step 3: Implement the backup in `store.rs`.**

```rust
#[derive(Debug)]
pub enum QueueBackup { Saved(PathBuf), Failed }

#[derive(Debug)]
pub struct QueueRepair { pub reset: QueueReset, pub backup: QueueBackup }

// In `load`, replacing the `Ok(state)` arm:
Ok((state, None)) => LoadOutcome { state, writable: true, reason: LoadReason::Loaded, queue_repair: None },
Ok((state, Some(reset))) => {
    let backup = self.back_up_original(&bytes);
    let writable = matches!(backup, QueueBackup::Saved(_));
    tracing::warn!(fields = reset.fields_reset(), writable, "queue data in the state file was reset");
    LoadOutcome { state, writable, reason: LoadReason::Loaded, queue_repair: Some(QueueRepair { reset, backup }) }
}

/// §6: copy the exact bytes aside with create-new semantics before any
/// writer can replace them. Never renames or rewrites the original.
fn back_up_original(&self, bytes: &[u8]) -> QueueBackup {
    use std::io::Write;
    let stamp = stamp(self.clock.sample().wall);
    let dir = self.parent();
    for suffix in 1..=MAX_QUARANTINE_CANDIDATES {
        let name = if suffix == 1 { format!("state.json.queue-recovery-{stamp}") }
                   else { format!("state.json.queue-recovery-{stamp}-{suffix}") };
        let candidate = dir.join(name);
        let mut file = match crate::persistence::atomic::create_private_new(&candidate) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return QueueBackup::Failed,
        };
        if file.write_all(bytes).and_then(|()| file.sync_all()).is_err() {
            return QueueBackup::Failed;
        }
        crate::persistence::atomic::sync_parent_best_effort(dir);
        return QueueBackup::Saved(candidate);
    }
    QueueBackup::Failed
}
```

`decode_state` now returns `Result<(PersistedState, Option<QueueReset>), PersistenceError>`, calling `into_state(true)` and `migrate_to_current_schema` as before. Every other `LoadOutcome` constructor sets `queue_repair: None`. In `atomic.rs`, rename `private_file` to `pub(crate) fn create_private_new` and extract the parent `fsync` block of `replace_bytes` into `pub(crate) fn sync_parent_best_effort(dir: &Path)`.

- [ ] **Step 4: Run.**
Run: `cargo test --locked --test m5_state_recovery --test persistence_store --test m4_atomic --lib`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add src/persistence src/app.rs tests/m5_state_recovery.rs tests/persistence_store.rs
git commit -m "feat: back up repaired queue state before writing"
```

---
## Phase C — Engine token contract

### Task 6: Echo caller-supplied load tokens through the engine

**Files:**
- Modify: `src/playback/command.rs`, `src/playback/event.rs`, `src/playback/engine.rs` (`dispatch`, `load`, `set_state`, `fail_with`, `publish_progress`, new fields), `src/playback/wait.rs` (`SessionFacts`, `Progress` construction, two `StateChanged` announcements), `src/app.rs` (`resume_commands`, `Mirror::apply`, test literals), `src/session.rs` (patterns only), `tests/support/mod.rs`, and every test file that constructs `PlaybackCommand::Load`, `PlaybackEvent::{Loaded, StateChanged, Failed}`, `Progress` or `SessionFacts` literals (compiler-guided: `engine_shutdown`, `http_resume`, `resume_contract`, `wait_service`, `http_playback`, `engine_remote`, `estimated_seek`, `provenance_policy`, `estimated_resume`, `engine_contract`, `session_policy`, `http_protocol`, `m4_playback_identity`)
- Test: `tests/engine_contract.rs`

**Interfaces:**
- Produces: `LoadRequestId` (`from_raw`, `get`); the new `Load`, `PlayLoaded`, `Loaded`, `StateChanged`, `Failed` shapes; `Progress::load`; `SessionFacts::load`; `TestEngine::next_request(&self) -> LoadRequestId` (sequential from 1) and `TestEngine::load_with_resume_as(&mut self, request, path, resume)`, `TestEngine::load_remote_as(&mut self, request, url, resume)`.
- Does not yet add `LoadCancelled` or protection (Task 7) or session gating (Task 8).

Worker rules:
- New fields `loading: Option<LoadRequestId>` (the load in flight) and `adopted_load: Option<LoadRequestId>` (the load whose media the worker holds).
- `load(request, ..)` sets `adopted_load = None` and `loading = Some(request)` before `set_state(Loading)`.
- `set_state` attaches `request: self.loading` only when `state == Loading`; every other state carries `None`. `play()`'s remote-reopen `Loading` therefore carries `None`.
- `fail_with` attaches `request: self.loading.take()`, so only a failure before `Loaded` is a load failure.
- Emitting `Loaded` takes `loading` and sets `adopted_load = Some(request)`.
- `PlayLoaded { request }` is the automatic-start command queued after its `Load`. Dispatch it only when `adopted_load == Some(request)` and `state == Paused`; call `play()` in that case and do nothing otherwise. This worker-side guard prevents an intervening load, stop or device-open failure from turning an automatic start into a retry. It has no load outcome and adds no larger event-budget row than `Play`.
- Change legacy `resume_commands`' third command from `Play` to `PlayLoaded { request }`, and update its sequence assertions. Explicit user `Play` retains its existing behavior; TUI retries requiring adoption always issue a fresh `Load`.
- Both cancellation returns in `load` set `loading = None` (Task 7 turns this into `LoadCancelled`).
- `publish_progress` mirrors `adopted_load` into `facts.load`; `WaitService` copies `facts.load` into `Progress::load`. Stop, pause, seek and device recovery never change `adopted_load`.
- `app.rs` sends `LoadRequestId::from_raw(1)` for its single load in this task only; Task 8 replaces it with a session-registered token.

- [ ] **Step 1: Write failing tests** in `tests/engine_contract.rs`.

```rust
use tenuto::playback::command::LoadRequestId;

fn local_load(request: u64, name: &str) -> PlaybackCommand {
    let path = support::fixture(name);
    PlaybackCommand::Load {
        request: LoadRequestId::from_raw(request),
        media: MediaId::LocalFile(path.clone()),
        source: SourceLocation::LocalPath(path.as_path().to_path_buf()),
        resume: ResumeIntent::StartAt(Duration::ZERO),
    }
}

#[test]
fn a_load_echoes_its_request_on_loading_loaded_and_progress() {
    let mut engine = TestEngine::start_idle();
    engine.send(local_load(41, "sine.flac"));
    let loading = engine.await_event(|e| matches!(e, PlaybackEvent::StateChanged { state: PlaybackState::Loading, .. }));
    assert!(matches!(loading, PlaybackEvent::StateChanged { request: Some(r), .. } if r.get() == 41));
    let loaded = engine.await_event(|e| matches!(e, PlaybackEvent::Loaded { .. }));
    assert!(matches!(loaded, PlaybackEvent::Loaded { request, .. } if request.get() == 41));
    engine.await_state(PlaybackState::Paused);
    assert_eq!(engine.progress().load.map(LoadRequestId::get), Some(41));
    engine.interrupt_stop();
    engine.await_state(PlaybackState::Stopped);
    assert_eq!(engine.progress().load.map(LoadRequestId::get), Some(41), "stop keeps the adopted load");
    engine.finish();
}

#[test]
fn an_open_failure_is_the_load_outcome_for_its_request() {
    let mut engine = TestEngine::start_idle();
    let missing = std::env::temp_dir().join("tenuto-m5-definitely-missing.flac");
    engine.send(PlaybackCommand::Load {
        request: LoadRequestId::from_raw(42),
        media: MediaId::LocalFile(tenuto::media::id::AbsolutePath::new(missing.clone()).expect("absolute")),
        source: SourceLocation::LocalPath(missing),
        resume: ResumeIntent::StartAt(Duration::ZERO),
    });
    let failed = engine.await_event(|e| matches!(e, PlaybackEvent::Failed { .. }));
    assert!(matches!(failed, PlaybackEvent::Failed { request: Some(r), .. } if r.get() == 42));
    engine.finish();
}

#[test]
fn a_device_failure_after_loaded_is_not_a_load_outcome() {
    let report = support::failed_device_session("sine.flac", 6, Duration::ZERO);
    let loaded = report.events.iter().position(|e| matches!(e, PlaybackEvent::Loaded { .. })).expect("loaded");
    let failed = report.events.iter().position(|e| matches!(e, PlaybackEvent::Failed { .. })).expect("failed");
    assert!(loaded < failed);
    assert!(matches!(report.events[failed], PlaybackEvent::Failed { request: None, .. }));
}
```

- [ ] **Step 2: Run `cargo test --locked --test engine_contract a_load_echoes`.** Expected: compile FAIL, `LoadRequestId` unresolved.

- [ ] **Step 3: Implement the types.**

```rust
// src/playback/command.rs
/// A caller-chosen identity for one `Load` (M5 §6). The engine echoes it on
/// that load's outcome and never interprets it. Allocation belongs to
/// `Session`; `from_raw` exists for that allocator and for tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct LoadRequestId(u64);

impl LoadRequestId {
    pub const fn from_raw(raw: u64) -> Self { Self(raw) }
    pub fn get(self) -> u64 { self.0 }
}
```

Add `request` fields as listed in the Interface registry, `load: Option<LoadRequestId>` to `Progress` and `SessionFacts`. Update `PlaybackEvent::session_rev` patterns. In `dispatch`: `PlaybackCommand::Load { request, media, source, resume } => self.load(request, media, source, resume)`. `set_state`:

```rust
fn set_state(&mut self, state: PlaybackState) {
    if self.state == state { return; }
    self.state = state;
    let session_rev = self.session_rev;
    let request = if state == PlaybackState::Loading { self.loading } else { None };
    self.emit(PlaybackEvent::StateChanged { session_rev, state, request });
}
```

Add the guarded dispatch arm (unrestricted `Play` remains the explicit-resume command):

```rust
PlaybackCommand::PlayLoaded { request } => {
    if self.adopted_load == Some(request) && self.state == PlaybackState::Paused {
        self.play();
    }
}
```

- [ ] **Step 4: Migrate call sites mechanically.** Construction sites add `request: LoadRequestId::from_raw(n)`, `request: None` or `load: None`; destructuring patterns add `..` where they were exhaustive. In `tests/support/mod.rs`, add a `next_request: AtomicU64` to `TestEngine`, use it in every helper that sends `Load`, and add the two `*_as` helpers that take an explicit token. `failed_load_position`, `failed_device_session` and `load_failure_on_device` use `LoadRequestId::from_raw(1)`.

- [ ] **Step 5: Run the engine and policy suites.**
Run: `cargo test --locked --test engine_contract --test engine_shutdown --test engine_remote --test http_playback --test http_resume --test wait_service --test estimated_seek --test estimated_resume --test session_policy --test resume_contract --test provenance_policy --test m4_playback_identity --test app_cli --lib`
Expected: PASS.

- [ ] **Step 6: Commit.**

```bash
git add src tests
git commit -m "feat: correlate engine loads with caller-supplied tokens"
```

### Task 7: Protected load outcomes and `LoadCancelled`

**Files:**
- Modify: `src/playback/event.rs`, `src/playback/engine.rs` (`emit`, `flush_events`, `load` cancellation returns, `shutdown`, reserve comment), `src/app.rs` (`Mirror::apply` arm), `src/session.rs` (match arm)
- Create: `tests/m5_engine_load_outcomes.rs`

**Interfaces:**
- Consumes: Task 6 shapes.
- Produces: `PlaybackEvent::LoadCancelled { session_rev, request }`, `PlaybackEvent::load_outcome()`, `PlaybackEvent::is_protected()`; the guarantee "each accepted load has exactly one of `Loaded`, `Failed { request: Some }`, `LoadCancelled`, delivered in order through the channel or the shutdown report".

Worker rules:
- `fn cancel_load(&mut self)` emits `LoadCancelled { session_rev, request }` for `self.loading.take()`; call it from both `Err(error) if is_cancelled(&error) => return` arms in `load`.
- `emit`: a protected event is always pushed. At `PENDING_CAP`, a terminal non-protected event displaces the oldest event that is neither terminal nor protected; if none exists it is dropped and counted, exactly as before.
- `flush_events`: a front event may use the reserved tail when `is_terminal() || is_protected()`.
- `shutdown` (after `drain_outbox`, before capture): `while let Ok(command) = self.commands.try_recv()`, emit `LoadCancelled` for every `Load` and discard every other command. The returned backlog carries them after anything already queued.
- Extract the displacement choice into `fn displacement_victim(pending: &VecDeque<PlaybackEvent>) -> Option<usize>` so it can be unit-tested.
- Replace the reserve-budget comment with the table from Implementation decision 5.

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_engine_load_outcomes.rs
mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use tenuto::http::limits::Limits;
use tenuto::http::service::HttpService;
use tenuto::media::id::{MediaId, NormalizedUrl};
use tenuto::media::source::SourceLocation;
use tenuto::playback::command::{LoadRequestId, PlaybackCommand, ResumeIntent};
use tenuto::playback::event::PlaybackEvent;
use support::TestEngine;
use support::server::{Script, TestServer};

fn local_load(request: u64) -> PlaybackCommand {
    let path = support::fixture("sine.flac");
    PlaybackCommand::Load {
        request: LoadRequestId::from_raw(request),
        media: MediaId::LocalFile(path.clone()),
        source: SourceLocation::LocalPath(path.as_path().to_path_buf()),
        resume: ResumeIntent::StartAt(Duration::ZERO),
    }
}

fn remote_load(request: u64, url: &str) -> PlaybackCommand {
    PlaybackCommand::Load {
        request: LoadRequestId::from_raw(request),
        media: MediaId::RemoteUrl(NormalizedUrl::parse(url).expect("url")),
        source: SourceLocation::Http(url.parse().expect("url")),
        resume: ResumeIntent::StartAt(Duration::ZERO),
    }
}

#[test]
fn every_accepted_load_has_exactly_one_ordered_outcome_under_saturation() {
    let mut engine = TestEngine::start_idle();
    engine.stop_draining_events();
    for request in 1..=40 {
        engine.send(local_load(request));
    }
    std::thread::sleep(Duration::from_millis(500));
    let report = engine.shutdown_report().expect("report");

    let mut outcomes: BTreeMap<u64, usize> = BTreeMap::new();
    let mut order = Vec::new();
    for event in &report.events {
        if let Some(request) = event.load_outcome() {
            *outcomes.entry(request.get()).or_default() += 1;
            order.push(request.get());
        }
    }
    assert_eq!(outcomes.len(), 40, "every load has an outcome: {outcomes:?}");
    assert!(outcomes.values().all(|count| *count == 1), "never two: {outcomes:?}");
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "outcomes keep command order");
    assert!(report.events.iter().any(|e| matches!(e, PlaybackEvent::Loaded { .. })));
    assert!(report.events.iter().any(|e| matches!(e, PlaybackEvent::LoadCancelled { .. })));

    // A load's own outcome precedes the later events of its revision.
    for (index, event) in report.events.iter().enumerate() {
        if let PlaybackEvent::Loaded { session_rev, .. } = event {
            assert!(!report.events[..index].iter().any(|earlier| matches!(earlier,
                PlaybackEvent::StateChanged { session_rev: rev, state: tenuto::playback::state::PlaybackState::Paused, .. } if rev == session_rev)));
        }
    }
}

#[test]
fn a_stop_during_a_stalled_open_cancels_that_load() {
    let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
    let mut engine = TestEngine::start_idle();
    engine.handle().set_http(Some(HttpService::spawn(Limits::brisk()).expect("http")));
    engine.send(remote_load(7, &server.url("/audio.mp3")));
    assert!(server.wait_until_stalled(Duration::from_secs(5)));
    engine.interrupt_stop();
    let event = engine.await_event(|e| e.load_outcome().is_some());
    assert!(matches!(event, PlaybackEvent::LoadCancelled { request, .. } if request.get() == 7));
    engine.finish();
    server.shutdown();
}

#[test]
fn shutdown_during_a_stalled_open_reports_the_cancellation() {
    let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
    let mut engine = TestEngine::start_idle();
    engine.handle().set_http(Some(HttpService::spawn(Limits::brisk()).expect("http")));
    engine.send(remote_load(8, &server.url("/audio.mp3")));
    assert!(server.wait_until_stalled(Duration::from_secs(5)));
    let report = engine.shutdown_report().expect("report");
    let outcomes: Vec<_> = report.events.iter().filter_map(PlaybackEvent::load_outcome).collect();
    assert_eq!(outcomes, [LoadRequestId::from_raw(8)]);
    assert!(report.events.iter().any(|e| matches!(e, PlaybackEvent::LoadCancelled { .. })));
    server.shutdown();
}
```

Add a unit test beside the existing ones in `engine.rs`:

```rust
#[test]
fn a_terminal_event_never_displaces_a_protected_one() {
    let mut pending = VecDeque::new();
    pending.push_back(PlaybackEvent::LoadCancelled { session_rev: 1, request: LoadRequestId::from_raw(1) });
    pending.push_back(PlaybackEvent::EndOfTrack { session_rev: 1, position: Duration::ZERO, provenance: PositionProvenance::Established });
    pending.push_back(PlaybackEvent::Warning { session_rev: 1, message: String::new() });
    assert_eq!(displacement_victim(&pending), Some(2));
    pending.pop_back();
    assert_eq!(displacement_victim(&pending), None);
}
```

Add the guarded-start regression using this file's `remote_load` helper. A queued volume command is the deterministic barrier proving `PlayLoaded` was dispatched; no sleep is needed.

```rust
#[test]
fn automatic_start_does_not_reopen_a_failed_remote_load() {
    use tenuto::playback::volume::Volume;
    let server = TestServer::start(Script::serving(Vec::new()).status(404));
    let mut engine = TestEngine::start_idle();
    engine.handle().set_http(Some(HttpService::spawn(Limits::brisk()).expect("http")));
    engine.send(remote_load(71, &server.url("/missing.mp3")));
    engine.send(PlaybackCommand::PlayLoaded { request: LoadRequestId::from_raw(71) });
    engine.send(PlaybackCommand::SetVolume(Volume::new(0.25)));
    engine.await_event(|event| matches!(event, PlaybackEvent::VolumeChanged { volume, .. } if volume.percent() == 25));
    assert_eq!(server.requests().len(), 1, "failure waits for explicit retry");
    engine.finish();
    server.shutdown();
}

#[test]
fn an_older_automatic_start_cannot_play_a_newer_load() {
    use tenuto::playback::state::PlaybackState;
    use tenuto::playback::volume::Volume;
    let mut engine = TestEngine::start_idle();
    engine.send(local_load(81));
    engine.send(local_load(82));
    engine.send(PlaybackCommand::PlayLoaded { request: LoadRequestId::from_raw(81) });
    engine.send(PlaybackCommand::SetVolume(Volume::new(0.25)));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(event) = engine.try_event() {
            assert!(!matches!(event, PlaybackEvent::StateChanged { state: PlaybackState::Playing, .. }));
            if matches!(event, PlaybackEvent::VolumeChanged { volume, .. } if volume.percent() == 25) { break; }
        }
        assert!(std::time::Instant::now() < deadline, "volume barrier never arrived");
        std::thread::yield_now();
    }
    assert_eq!(engine.progress().load, Some(LoadRequestId::from_raw(82)));
    engine.finish();
}
```

Also exercise the same-token device-open failure followed by `PlayLoaded`: the post-load failure must remain `Failed`, with no second open attempt.

- [ ] **Step 2: Run `cargo test --locked --test m5_engine_load_outcomes`.** Expected: compile FAIL, no variant `LoadCancelled`.

- [ ] **Step 3: Implement.**

```rust
// src/playback/event.rs
LoadCancelled { session_rev: u64, request: LoadRequestId },

/// The load this event concludes, if it is a load outcome (M5 §6). Each
/// accepted `Load` produces exactly one.
pub fn load_outcome(&self) -> Option<LoadRequestId> {
    match self {
        Self::Loaded { request, .. } | Self::LoadCancelled { request, .. } => Some(*request),
        Self::Failed { request, .. } => *request,
        _ => None,
    }
}

/// Protected outcomes are never dropped or displaced, and may use the
/// reserved tail: correlation cannot be inferred from anything later.
pub fn is_protected(&self) -> bool {
    self.load_outcome().is_some()
}
```

```rust
// src/playback/engine.rs
fn displacement_victim(pending: &VecDeque<PlaybackEvent>) -> Option<usize> {
    pending.iter().position(|event| !event.is_terminal() && !event.is_protected())
}

fn emit(&mut self, event: PlaybackEvent) {
    if self.pending_events.len() >= PENDING_CAP && !event.is_protected() {
        if !event.is_terminal() {
            self.dropped_events += 1;
            return;
        }
        match displacement_victim(&self.pending_events) {
            Some(index) => { self.pending_events.remove(index); self.dropped_events += 1; }
            None => { self.dropped_events += 1; return; }
        }
    }
    self.pending_events.push_back(event);
    self.note_backlog();
}

fn cancel_load(&mut self) {
    if let Some(request) = self.loading.take() {
        let session_rev = self.session_rev;
        self.emit(PlaybackEvent::LoadCancelled { session_rev, request });
    }
}
```

In `shutdown`, right after `self.drain_outbox();`:

```rust
// Accepted but never dispatched: each still owes its caller an outcome.
while let Ok(command) = self.commands.try_recv() {
    if let PlaybackCommand::Load { request, .. } = command {
        let session_rev = self.session_rev;
        self.emit(PlaybackEvent::LoadCancelled { session_rev, request });
    }
}
```

`Mirror::apply` in `app.rs` handles `LoadCancelled { session_rev, .. }` like `Warning` (adopt the revision only).

- [ ] **Step 4: Run.**
Run: `cargo test --locked --test m5_engine_load_outcomes --test engine_contract --test engine_shutdown --test engine_remote --test http_cancellation --lib`
Expected: PASS.

- [ ] **Step 5: Commit.**

```bash
git add src tests/m5_engine_load_outcomes.rs
git commit -m "feat: protect load outcomes and cancel undelivered loads"
```

---

## Phase D — Session semantics

### Task 8: Pending-load table and token adoption in `Session`

**Files:**
- Modify: `src/session.rs`, `src/app.rs` (register the legacy load; move `resume_intent_for` into `session.rs`), `tests/session_policy.rs`, `tests/provenance_policy.rs`, `tests/resume_contract.rs`, `tests/estimated_resume.rs`, `tests/http_resume.rs`, `tests/http_playback.rs`, `tests/http_protocol.rs`, `tests/m4_playback_identity.rs`
- Create: `tests/m5_session_adoption.rs`

**Interfaces:**
- Consumes: `LoadRequestId`, `Progress::load`, `PlaybackEvent::load_outcome` (Tasks 6–7); `Queue` (Task 3); `PersistedState::queue_mut` (Task 4).
- Produces: `MAX_PENDING_LOADS`, `LoadTarget`, `RegisterLoadError`, `AdoptedLoad`, `Advance`, `Session::{register_load, retract_load, pending_load_count, adopted, accepts_media_event, take_stop_request, take_advance, resume_intent}`, `session::resume_intent_for`.

New `Session` fields: `next_request: u64` (0; first token is 1), `pending: BTreeMap<LoadRequestId, PendingLoad { target, media, invalidated }>`, `adopted: Option<AdoptedLoad>`, `adopted_rev_floor: u64`, `latest_engine_rev: u64`, `last_loaded: Option<LoadRequestId>`, `stop_requested: bool`, `advance: Option<Advance>`, `completion_seen: Option<(LoadRequestId, u64)>`. The existing `session_rev` belongs to adopted playback; `latest_engine_rev` tracks recognized engine transitions, including loads that cannot be adopted.

Rules:
- `register_load` → `Busy` at 16 pending; for `Queue(id)`: `UnknownEntry` if absent, `MediaMismatch` if the entry's media differs. Tokens are strictly increasing and never reused.
- `retract_load(request)` removes a registration (admission `Busy` or `Gone`).
- `observe` computes `accepts_media_event` before removing any registration, then processes load outcomes before ordinary event handlers: remove the pending registration first, even for a stale outcome. An unknown/duplicate outcome changes nothing, including revision tracking, `last_loaded`, checkpoint fields and advancement. A known outcome older than `latest_engine_rev` only retires its registration; it never stops newer playback.
  - A current `Loaded` sets `latest_engine_rev` and `last_loaded = Some(request)`. Adopt only if its registration is not invalidated, its media matches, and its queue occurrence still exists with that media. Otherwise request a stop and return `Action::None`, retaining the old adopted checkpoint state. On adoption set `session_rev` to the event revision, capture outgoing history, mutate incoming identity/active entry/metadata, and only then construct the submission. Never return a snapshot cloned before those mutations.
  - A current known `Failed { request: Some }` or `LoadCancelled` advances `latest_engine_rev` and clears `last_loaded`; it does not change the previous adopted checkpoint state. A `Loading` announcement for a registered request does the same before its outcome; losing that ordinary announcement is safe because the protected outcome precedes later media events.
- Expose `Session::accepts_media_event(&self, event: &PlaybackEvent) -> bool` as the shared pre-observation ownership check. For `Loaded`, require a current, valid registered target/media; for other media events use the gate below. The runtime records this boolean before `observe` and uses it for mirror updates, so a valid `Action::None` is never mistaken for rejection. Tokenized failure/cancellation retirement and profile-wide volume handling remain separate.
- **Before any other media-specific handler** (`StateChanged`, seek/target/restart events, `EndOfTrack`, capabilities, recovery and playback failure), require an adopted token, `last_loaded == Some(adopted.request)` and `event.session_rev() >= latest_engine_rev`. Otherwise return `Action::None` before changing revision, completion, provenance, protection, pending force or history. Accepted events update `latest_engine_rev` and `session_rev` monotonically; this permits stop and device recovery of the adopted token to advance its revision while unrelated loads remain pending. `VolumeChanged` is profile-wide and bypasses the media-ownership gate. Remove `observe`'s current unconditional revision assignment.
- `tick` accepts a live sample only when its revision, media and token match adopted playback and `last_loaded` still names that token. `shutdown_snapshot` uses the same live gate; otherwise it may capture the last accepted sample belonging to the retained adopted playback, using that sample's provenance (`Sample` gains `provenance: PositionProvenance`, populated with each accepted sample). Rejected load events must not re-key or clear that fallback. `reconcile_shutdown` routes every event through these same gates.
- On an eligible `EndOfTrack`, deduplicate `(adopted.request, session_rev)` **before** any checkpoint handling. Then record completion with its reported provenance and set `advance` to `Next(neighbor Down)` or `EndOfQueue` only for a queue target. Both provenances advance. Invalidated, stale and duplicate completions change neither history nor advancement.
- `take_stop_request` and `take_advance` return and clear.
- `resume_intent(&media)` = `resume_intent_for(self.state.entry_for(media)).unwrap_or(ResumeIntent::StartAt(Duration::ZERO))`.

Test-helper migration (apply the same pattern in every listed file):

```rust
fn loaded(session: &mut Session, session_rev: u64, name: &str, position: Duration) -> PlaybackEvent {
    let request = session
        .register_load(LoadTarget::Legacy, &media(name))
        .unwrap_or_else(|error| panic!("room for a load: {error:?}"));
    PlaybackEvent::Loaded { session_rev, request, media: media(name), metadata: MediaMetadata::default(),
        capabilities: MediaCapabilities { continuity: Continuity::Finite, seek: SeekSupport::Native },
        position, disposition: StartDisposition::Fresh }
}

/// Progress for whatever the session currently has adopted.
fn progress(session: &Session, session_rev: u64, name: &str, secs: u64) -> Progress {
    Progress { session_rev, media: Some(media(name)), position: Duration::from_secs(secs),
        quality: PositionQuality::Exact, provenance: PositionProvenance::Established,
        buffering: false, load: session.adopted().map(|adopted| adopted.request) }
}
```

Call sites become two statements (`let event = loaded(&mut session, ..); session.observe(&event, now);`). Engine-driven tests that also feed a `Session` register first and send the load with the Task 6 `*_as` helpers.

In `app.rs::run_resolved`: after `open_persistence`, `let request = session.register_load(LoadTarget::Legacy, &media)` (map `Busy` to an internal error; it cannot happen with one load), and pass `request` into `resume_commands`. If the command send fails, call `session.retract_load(request)`.

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_session_adoption.rs
mod support;

use std::time::Duration;

use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::MediaId;
use tenuto::media::metadata::MediaMetadata;
use tenuto::persistence::model::PersistedState;
use tenuto::playback::command::LoadRequestId;
use tenuto::playback::event::{PlaybackEvent, Progress, StartDisposition};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::state::PlaybackState;
use tenuto::playback::timeline::PositionQuality;
use tenuto::queue::{Direction, DisplayMetadata, NewQueueEntry, QueueEntryId, QueueSource};
use tenuto::session::{Advance, LoadTarget, MAX_PENDING_LOADS, RegisterLoadError, Session};
use support::media;

fn entry(name: &str) -> NewQueueEntry {
    let MediaId::LocalFile(path) = media(name) else { unreachable!() };
    NewQueueEntry::new(media(name), QueueSource::LocalFile(path), DisplayMetadata::default()).expect("entry")
}

fn queued(names: &[&str]) -> (Session, Vec<QueueEntryId>) {
    let mut session = Session::new(PersistedState::default());
    let (ids, _) = session.enqueue(names.iter().map(|n| entry(n)).collect()).expect("fits");
    (session, ids)
}

fn loaded(request: LoadRequestId, rev: u64, name: &str) -> PlaybackEvent {
    PlaybackEvent::Loaded { session_rev: rev, request, media: media(name), metadata: MediaMetadata::default(),
        capabilities: MediaCapabilities { continuity: Continuity::Finite, seek: SeekSupport::Native },
        position: Duration::ZERO, disposition: StartDisposition::Fresh }
}

fn progress(rev: u64, name: &str, secs: u64, load: Option<LoadRequestId>) -> Progress {
    Progress { session_rev: rev, media: Some(media(name)), position: Duration::from_secs(secs),
        quality: PositionQuality::Exact, provenance: PositionProvenance::Established, buffering: false, load }
}

fn playing(rev: u64) -> PlaybackEvent {
    PlaybackEvent::StateChanged { session_rev: rev, state: PlaybackState::Playing, request: None }
}

fn end(rev: u64, provenance: PositionProvenance) -> PlaybackEvent {
    PlaybackEvent::EndOfTrack { session_rev: rev, position: Duration::from_millis(500), provenance }
}

#[test]
fn two_pending_loads_of_one_media_adopt_their_own_rows_in_event_order() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b", "a"]);
    let first = session.register_load(LoadTarget::Queue(ids[0]), &media("a")).expect("registered");
    let second = session.register_load(LoadTarget::Queue(ids[2]), &media("a")).expect("registered");
    assert_ne!(first, second);
    assert_eq!(session.state().queue().active(), None, "submitting adopts nothing");

    session.observe(&loaded(first, 1, "a"), clock.sample());
    assert_eq!(session.state().queue().active(), Some(ids[0]));
    session.observe(&loaded(second, 2, "a"), clock.sample());
    assert_eq!(session.state().queue().active(), Some(ids[2]));
    assert_eq!(session.pending_load_count(), 0);
}

#[test]
fn reordering_a_pending_target_does_not_change_what_is_adopted() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b", "c"]);
    let request = session.register_load(LoadTarget::Queue(ids[2]), &media("c")).expect("registered");
    session.move_entry(ids[2], Direction::Up).expect("known");
    session.move_entry(ids[2], Direction::Up).expect("known");
    session.observe(&loaded(request, 1, "c"), clock.sample());
    assert_eq!(session.state().queue().active(), Some(ids[2]));
}

#[test]
fn a_removed_pending_target_is_never_resurrected_and_its_playback_is_stopped() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let request = session.register_load(LoadTarget::Queue(ids[1]), &media("b")).expect("registered");
    session.remove_entry(ids[1], &progress(0, "b", 0, None), clock.sample()).expect("known");
    session.observe(&loaded(request, 1, "b"), clock.sample());
    assert_eq!(session.state().queue().active(), None);
    assert!(session.state().queue().get(ids[1]).is_none());
    assert!(session.take_stop_request());
    assert!(!session.take_stop_request(), "taken once");
}

#[test]
fn failure_before_adoption_keeps_the_previous_entry_and_its_checkpoint() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let first = session.register_load(LoadTarget::Queue(ids[0]), &media("a")).expect("registered");
    session.observe(&loaded(first, 1, "a"), clock.sample());
    session.observe(&playing(1), clock.sample());
    clock.advance(Duration::from_secs(6));
    let _ = session.tick(&progress(1, "a", 30, Some(first)), clock.sample());
    let saved = session.state().entry_for(&media("a")).and_then(|e| e.position);

    let second = session.register_load(LoadTarget::Queue(ids[1]), &media("b")).expect("registered");
    // Progress for the unadopted load arrives before its outcome.
    let _ = session.tick(&progress(2, "b", 0, Some(second)), clock.sample());
    session.observe(&PlaybackEvent::Failed { session_rev: 2, message: "gone".into(), cause: None, request: Some(second) }, clock.sample());

    assert_eq!(session.state().queue().active(), Some(ids[0]));
    assert_eq!(session.state().entry_for(&media("a")).and_then(|e| e.position), saved);
    assert!(session.state().entry_for(&media("b")).is_none());
    assert_eq!(session.pending_load_count(), 0);
}

#[test]
fn pending_registrations_are_bounded_and_retractable() {
    let (mut session, ids) = queued(&["a"]);
    let tokens: Vec<_> = (0..MAX_PENDING_LOADS)
        .map(|_| session.register_load(LoadTarget::Queue(ids[0]), &media("a")).expect("room"))
        .collect();
    assert_eq!(session.register_load(LoadTarget::Queue(ids[0]), &media("a")), Err(RegisterLoadError::Busy));
    session.retract_load(tokens[0]);
    session.retract_load(tokens[1]);
    assert!(session.register_load(LoadTarget::Queue(ids[0]), &media("a")).is_ok());
    assert_eq!(session.pending_load_count(), MAX_PENDING_LOADS - 1);
    assert_eq!(session.register_load(LoadTarget::Queue(ids[0]), &media("zzz")), Err(RegisterLoadError::MediaMismatch));
}

#[test]
fn device_recovery_keeps_the_adopted_load_while_another_is_pending() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let first = session.register_load(LoadTarget::Queue(ids[0]), &media("a")).expect("registered");
    session.observe(&loaded(first, 1, "a"), clock.sample());
    session.observe(&playing(1), clock.sample());
    let _pending = session.register_load(LoadTarget::Queue(ids[1]), &media("b")).expect("registered");
    session.observe(&PlaybackEvent::DeviceRecovered { session_rev: 2 }, clock.sample());
    clock.advance(Duration::from_secs(6));
    let action = session.tick(&progress(2, "a", 12, Some(first)), clock.sample());
    assert!(matches!(action, tenuto::session::Action::Submit { .. }));
    assert_eq!(session.adopted().map(|a| a.request), Some(first));
}

#[test]
fn completion_advances_once_for_either_provenance_and_never_from_a_stale_revision() {
    for provenance in [PositionProvenance::Established, PositionProvenance::Estimated] {
        let clock = FakeClock::new();
        let (mut session, ids) = queued(&["a", "b"]);
        let first = session.register_load(LoadTarget::Queue(ids[0]), &media("a")).expect("registered");
        session.observe(&loaded(first, 3, "a"), clock.sample());
        session.observe(&end(2, provenance), clock.sample());
        assert_eq!(session.take_advance(), None, "stale revision");
        session.observe(&end(3, provenance), clock.sample());
        assert_eq!(session.take_advance(), Some(Advance::Next(ids[1])));
        session.observe(&end(3, provenance), clock.sample());
        assert_eq!(session.take_advance(), None, "deduplicated");
    }
}

#[test]
fn the_last_entry_ends_the_queue_without_wrapping() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a"]);
    let request = session.register_load(LoadTarget::Queue(ids[0]), &media("a")).expect("registered");
    session.observe(&loaded(request, 1, "a"), clock.sample());
    session.observe(&end(1, PositionProvenance::Established), clock.sample());
    assert_eq!(session.take_advance(), Some(Advance::EndOfQueue));
}

#[test]
fn a_legacy_adoption_clears_the_active_entry_and_keeps_the_queue() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let request = session.register_load(LoadTarget::Queue(ids[0]), &media("a")).expect("registered");
    session.observe(&loaded(request, 1, "a"), clock.sample());
    let legacy = session.register_load(LoadTarget::Legacy, &media("x")).expect("registered");
    session.observe(&loaded(legacy, 2, "x"), clock.sample());
    assert_eq!(session.state().queue().active(), None);
    assert_eq!(session.state().queue().len(), 2);
    assert_eq!(session.state().current_media(), Some(&media("x")));
}

#[test]
fn unknown_and_duplicate_outcomes_select_nothing() {
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a"]);
    session.observe(&loaded(LoadRequestId::from_raw(999), 1, "a"), clock.sample());
    assert_eq!(session.state().queue().active(), None);
    assert!(!session.take_stop_request());
    let request = session.register_load(LoadTarget::Queue(ids[0]), &media("a")).expect("registered");
    session.observe(&loaded(request, 2, "a"), clock.sample());
    session.observe(&loaded(request, 3, "a"), clock.sample());
    assert_eq!(session.adopted().map(|a| a.request), Some(request));
}
```

`RegisterLoadError` derives `Debug, Eq, PartialEq`. Registration checks capacity before the queue lookup, which is why the mismatch assertion runs only after two retractions leave room.

Add these regressions using this file's helpers:

```rust
#[test]
fn an_adoption_snapshot_contains_the_new_media_active_entry_and_metadata() {
    use tenuto::session::Action;
    let clock = FakeClock::new();
    let (mut session, ids) = queued(&["a", "b"]);
    let a = session.register_load(LoadTarget::Queue(ids[0]), &media("a")).expect("register");
    session.observe(&loaded(a, 1, "a"), clock.sample());
    let b = session.register_load(LoadTarget::Queue(ids[1]), &media("b")).expect("register");
    let mut event = loaded(b, 2, "b");
    if let PlaybackEvent::Loaded { metadata, .. } = &mut event { metadata.title = Some("B title".into()); }
    let Action::Submit { state, .. } = session.observe(&event, clock.sample()) else { panic!("snapshot") };
    assert_eq!(state.current_media(), Some(&media("b")));
    assert_eq!(state.queue().active(), Some(ids[1]));
    assert_eq!(state.queue().get(ids[1]).and_then(|e| e.display().title.as_deref()), Some("B title"));
    let legacy = session.register_load(LoadTarget::Legacy, &media("x")).expect("register");
    let Action::Submit { state, .. } = session.observe(&loaded(legacy, 3, "x"), clock.sample()) else { panic!("snapshot") };
    assert_eq!(state.current_media(), Some(&media("x")));
    assert_eq!(state.queue().active(), None);
}

#[test]
fn completion_for_a_removed_pending_load_cannot_write_the_previous_history() {
    use tenuto::session::Action;
    for provenance in [PositionProvenance::Established, PositionProvenance::Estimated] {
        let clock = FakeClock::new();
        let (mut session, ids) = queued(&["a", "b"]);
        let a = session.register_load(LoadTarget::Queue(ids[0]), &media("a")).expect("register");
        session.observe(&loaded(a, 1, "a"), clock.sample());
        session.observe(&playing(1), clock.sample());
        clock.advance(Duration::from_secs(6));
        session.tick(&progress(1, "a", 30, Some(a)), clock.sample());
        let b = session.register_load(LoadTarget::Queue(ids[1]), &media("b")).expect("register");
        session.remove_entry(ids[1], &progress(1, "a", 30, Some(a)), clock.sample()).expect("remove");
        let before = serde_json::to_value(session.state()).expect("snapshot");
        session.observe(&loaded(b, 2, "b"), clock.sample());
        session.observe(&playing(2), clock.sample());
        assert!(matches!(session.observe(&end(2, provenance), clock.sample()), Action::None));
        assert_eq!(serde_json::to_value(session.state()).expect("snapshot"), before);
        assert_eq!(session.take_advance(), None);
        assert!(session.take_stop_request());
    }
}
```

Extend `completion_advances_once_for_either_provenance_and_never_from_a_stale_revision` to compare the serialized state before/after the stale and duplicate events, including `touch_seq` and `updated_at`, not only `take_advance()`. Replay the invalidated-load sequence through `reconcile_shutdown` too and assert A remains incomplete with its retained 30-second sample and provenance; no B checkpoint appears. Repeat with `SeekTargetStored` from the rejected revision to cover the other direct checkpoint-writing handler.

- [ ] **Step 2: Run `cargo test --locked --test m5_session_adoption`.** Expected: compile FAIL, `LoadTarget` unresolved.

- [ ] **Step 3: Implement the fields and rules above in `session.rs`.** `enqueue`, `move_entry` and `remove_entry` are needed by these tests; implement them here exactly as specified in Task 9's rules (Task 9 adds the rest of the queue surface and its own tests). Change `on_loaded` to take the adoption target:

```rust
fn adopt_loaded(&mut self, request: LoadRequestId, target: LoadTarget, media: &MediaId, position: Duration,
                disposition: &StartDisposition, metadata: &MediaMetadata, now: ClockSample) -> Action {
    let previous_active = self.state.queue().active();
    let switched_media = self.on_loaded(media, position, disposition, now);
    let active = match target { LoadTarget::Queue(id) => Some(id), LoadTarget::Legacy => None };
    // The registration was validated against the queue a moment ago in `observe`.
    let _ = self.state.queue_mut().set_active(active);
    if let LoadTarget::Queue(id) = target {
        self.absorb_load_metadata(id, metadata);
    }
    self.adopted = Some(AdoptedLoad { request, target });
    self.adopted_rev_floor = self.session_rev;
    if switched_media || previous_active != active {
        self.submit(Urgency::Forced)
    } else {
        Action::None
    }
}
```

Change private `on_loaded` to return its existing `switching: bool` instead of `Action`. Its checkpoint/identity mutations remain, but its final `submit` moves exclusively to `adopt_loaded`, after the queue and metadata updates. Test the returned snapshot as well as the live state.

`absorb_load_metadata` copies a present decoder title/artist/album into the entry's display and replaces its duration with `DisplayDuration { value, source: DurationSource::Decoded(metadata.duration_provenance) }` when the decoder reported one. (Artist/album fields arrive in Task 23; until then only title and duration are copied.)

- [ ] **Step 4: Migrate existing tests** using the helper pattern above; run `cargo test --locked --test session_policy` first, since it has the most call sites.

- [ ] **Step 5: Run.**
Run: `cargo test --locked --test m5_session_adoption --test session_policy --test provenance_policy --test resume_contract --test estimated_resume --test http_resume --test http_playback --test http_protocol --test m4_playback_identity --lib`
Expected: PASS.

- [ ] **Step 6: Commit.**

```bash
git add src/session.rs src/app.rs tests
git commit -m "feat: adopt queue occurrences only on their own load token"
```

### Task 9: Queue mutations, volume and display updates through `Session`

**Files:**
- Modify: `src/session.rs`
- Create: `tests/m5_session_queue.rs`

**Interfaces:**
- Consumes: Task 8 fields; `Queue` (Task 3).
- Produces: `Session::{enqueue, move_entry, remove_entry, clear_queue, set_volume, update_display, update_podcast_fallback}`, `Removal`, `DisplayUpdate { pub title, pub artist, pub album: Option<String>, pub duration: Option<DisplayDuration> }`.

Rules:
- `enqueue(batch) -> Result<(Vec<QueueEntryId>, Action), QueueError>`: all-or-nothing; success submits `Ordinary`; failure returns the error with no submit.
- `move_entry(id, direction) -> Result<Action, QueueError>`: `Ordinary` submit only when something moved. Never touches adoption or pending targets.
- `remove_entry(id, progress, now) -> Result<Removal, QueueError>`: unknown → error before any change. Invalidate pending loads targeting `id`. If `id` is active: capture the current checkpoint through the same gated path as `shutdown_snapshot` (live sample only when revision and token match; otherwise `last_sample`), then clear `adopted` and `last_sample`. Remove from the queue; `Removal { action: Forced submit, stop_playback: was_active, selection }`. `current_media` is kept.
- `clear_queue(progress, now) -> Removal`: same capture when an entry is active; invalidate every pending queue target; remove all entries; `selection: None`; checkpoints untouched.
- `set_volume(volume) -> Action`: set volume, `Ordinary` submit (used before an engine exists).
- `update_display(&media, update) -> Action`: for every entry with that media, replace fields whose update value is `Some`; `Ordinary` submit when anything changed.
- `update_podcast_fallback(id, url) -> Result<Action, QueueError>`: replace `QueueSource::Podcast { fallback }` when the URL differs; `Ordinary` submit; `MediaId` and checkpoints unchanged.

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_session_queue.rs
mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tenuto::clock::{Clock, FakeClock};
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::persistence::writer::{StateSink, Urgency, WriterHandle};
use tenuto::persistence::PersistenceError;
use tenuto::playback::command::ResumeIntent;
use tenuto::queue::{DisplayMetadata, MAX_QUEUE_ENTRIES, NewQueueEntry, QueueError, QueueSource};
use tenuto::resume::ResumeCandidate;
use tenuto::session::{Action, DisplayUpdate, LoadTarget, Session};
use support::media;

// entry(), loaded(), progress(), playing() helpers: copy from tests/m5_session_adoption.rs.

#[test]
fn an_accepted_enqueue_submits_the_queue_and_a_rejected_one_submits_nothing() {
    let mut session = Session::new(PersistedState::default());
    let (_, action) = session.enqueue(vec![entry("a")]).expect("fits");
    let Action::Submit { state, .. } = action else { panic!("must submit") };
    assert_eq!(state.queue().len(), 1);
    let too_many = (0..MAX_QUEUE_ENTRIES).map(|i| entry(&format!("t{i}"))).collect();
    assert!(matches!(session.enqueue(too_many), Err(QueueError::Capacity { .. })));
    assert_eq!(session.state().queue().len(), 1);
}

#[test]
fn removing_the_active_entry_captures_it_stops_and_selects_the_successor() {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let (ids, _) = session.enqueue(vec![entry("a"), entry("b")]).expect("fits");
    let request = session.register_load(LoadTarget::Queue(ids[0]), &media("a")).expect("registered");
    session.observe(&loaded(request, 1, "a"), clock.sample());
    session.observe(&playing(1), clock.sample());

    let removal = session.remove_entry(ids[0], &progress(1, "a", 42, Some(request)), clock.sample()).expect("known");
    assert!(removal.stop_playback);
    assert_eq!(removal.selection, Some(ids[1]));
    assert_eq!(session.state().queue().active(), None);
    assert_eq!(session.adopted(), None);
    assert_eq!(session.state().entry_for(&media("a")).and_then(|e| e.position), Some(Duration::from_secs(42)));
    assert_eq!(session.state().current_media(), Some(&media("a")));
    assert_eq!(session.state().queue().active(), None, "selection never activates");
}

#[test]
fn removing_a_nonplaying_entry_leaves_playback_alone() {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let (ids, _) = session.enqueue(vec![entry("a"), entry("b")]).expect("fits");
    let request = session.register_load(LoadTarget::Queue(ids[0]), &media("a")).expect("registered");
    session.observe(&loaded(request, 1, "a"), clock.sample());
    let removal = session.remove_entry(ids[1], &progress(1, "a", 3, Some(request)), clock.sample()).expect("known");
    assert!(!removal.stop_playback);
    assert_eq!(session.state().queue().active(), Some(ids[0]));
}

#[test]
fn clearing_stops_and_keeps_listening_history() {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let (ids, _) = session.enqueue(vec![entry("a"), entry("b")]).expect("fits");
    let request = session.register_load(LoadTarget::Queue(ids[1]), &media("b")).expect("registered");
    session.observe(&loaded(request, 1, "b"), clock.sample());
    session.observe(&playing(1), clock.sample());
    let removal = session.clear_queue(&progress(1, "b", 9, Some(request)), clock.sample());
    assert!(removal.stop_playback);
    assert!(session.state().queue().is_empty());
    assert!(session.state().entry_for(&media("b")).is_some());
}

#[test]
fn advancing_into_partial_and_completed_entries_uses_the_resume_policy() {
    let file = serde_json::json!({ "schema_version": 3, "volume": 1.0, "checkpoints": {
        "local:/music/half.flac": { "position": { "secs": 40, "nanos": 0 }, "completed": false, "touch_seq": 1, "updated_at": "2026-09-14T10:00:00Z" },
        "local:/music/done.flac": { "position": { "secs": 90, "nanos": 0 }, "completed": true, "touch_seq": 2, "updated_at": "2026-09-14T10:00:00Z" } } });
    let session = Session::new(serde_json::from_value(file).expect("valid"));
    assert_eq!(session.resume_intent(&media("half")),
        ResumeIntent::Candidate(ResumeCandidate { position: Duration::from_secs(40), completed: false }));
    assert!(matches!(session.resume_intent(&media("done")), ResumeIntent::Candidate(ResumeCandidate { completed: true, .. })));
    assert_eq!(session.resume_intent(&media("new")), ResumeIntent::StartAt(Duration::ZERO));
}

#[test]
fn a_queued_track_can_lose_its_history_to_eviction_and_stays_queued() {
    let mut checkpoints = serde_json::Map::new();
    for i in 0..512 {
        checkpoints.insert(format!("local:/music/old{i}.flac"), serde_json::json!({
            "position": { "secs": 5, "nanos": 0 }, "completed": false, "touch_seq": i + 1, "updated_at": "2026-09-14T10:00:00Z" }));
    }
    let file = serde_json::json!({ "schema_version": 3, "volume": 1.0, "checkpoints": checkpoints,
        "queue": [{ "id": 1, "media": "local:/music/old0.flac", "source": { "kind": "local", "path": "/music/old0.flac" } },
                  { "id": 2, "media": "local:/music/fresh.flac", "source": { "kind": "local", "path": "/music/fresh.flac" } }] });
    let clock = FakeClock::new();
    let mut session = Session::new(serde_json::from_value(file).expect("valid"));
    let ids: Vec<_> = session.state().queue().entries().iter().map(|e| e.id()).collect();
    let request = session.register_load(LoadTarget::Queue(ids[1]), &media("fresh")).expect("registered");
    session.observe(&loaded(request, 1, "fresh"), clock.sample());
    session.observe(&playing(1), clock.sample());
    clock.advance(Duration::from_secs(6));
    let _ = session.tick(&progress(1, "fresh", 3, Some(request)), clock.sample());
    assert!(session.state().entry_for(&media("old0")).is_none(), "oldest history evicted");
    assert_eq!(session.state().queue().len(), 2, "queue membership does not pin history");
    assert_eq!(session.resume_intent(&media("old0")), ResumeIntent::StartAt(Duration::ZERO));
}

#[test]
fn volume_and_display_updates_submit_through_the_session() {
    let mut session = Session::new(PersistedState::default());
    let (ids, _) = session.enqueue(vec![entry("a"), entry("a")]).expect("fits");
    assert!(matches!(session.set_volume(tenuto::playback::volume::Volume::new(0.4)), Action::Submit { .. }));
    let update = DisplayUpdate { title: Some("Title".into()), artist: Some("Artist".into()), album: None, duration: None };
    assert!(matches!(session.update_display(&media("a"), update), Action::Submit { .. }));
    for id in ids {
        assert_eq!(session.state().queue().get(id).and_then(|e| e.display().title.as_deref()), Some("Title"));
    }
}

/// A sink slow enough that snapshots queue up behind it.
struct SlowSink { inner: StateStore, seen: Arc<Mutex<usize>> }

impl StateSink for SlowSink {
    fn write(&self, state: &PersistedState) -> Result<(), PersistenceError> {
        std::thread::sleep(Duration::from_millis(150));
        *self.seen.lock().unwrap_or_else(|p| p.into_inner()) += 1;
        self.inner.write(state)
    }
}

#[test]
fn queue_and_checkpoint_writes_interleave_into_one_latest_snapshot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = Arc::new(FakeClock::new());
    let store = StateStore::new(dir.path().join("state.json"), clock.clone());
    let seen = Arc::new(Mutex::new(0));
    let mut writer = WriterHandle::spawn(Box::new(SlowSink { inner: store, seen: seen.clone() }), clock.clone());
    let mut session = Session::new(PersistedState::default());

    let (ids, action) = session.enqueue(vec![entry("a"), entry("b")]).expect("fits");
    if let Action::Submit { state, .. } = action { writer.submit(state, Urgency::Forced); }
    let request = session.register_load(LoadTarget::Queue(ids[0]), &media("a")).expect("registered");
    if let Action::Submit { state, urgency } = session.observe(&loaded(request, 1, "a"), clock.sample()) { writer.submit(state, urgency); }
    session.observe(&playing(1), clock.sample());
    clock.advance(Duration::from_secs(6));
    if let Action::Submit { state, urgency } = session.tick(&progress(1, "a", 17, Some(request)), clock.sample()) { writer.submit(state, urgency); }
    let (_, action) = session.enqueue(vec![entry("c")]).expect("fits");
    if let Action::Submit { state, .. } = action { writer.submit(state, Urgency::Forced); }
    assert!(matches!(writer.shutdown(), tenuto::persistence::writer::ShutdownOutcome::Written));

    let written: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join("state.json")).expect("read")).expect("json");
    assert_eq!(written["queue"].as_array().map(Vec::len), Some(3), "latest queue");
    assert_eq!(written["active_entry"], ids[0].get());
    assert_eq!(written["checkpoints"]["local:/music/a.flac"]["position"]["secs"], 17, "latest checkpoint");
}
```

- [ ] **Step 2: Run `cargo test --locked --test m5_session_queue`.** Expected: FAIL on the missing methods.
- [ ] **Step 3: Implement.** Factor the capture in `shutdown_snapshot` into `fn capture_current(&mut self, progress: &Progress, now: ClockSample)` and call it from `shutdown_snapshot`, `remove_entry` and `clear_queue`.
- [ ] **Step 4: Run.** `cargo test --locked --test m5_session_queue --test m5_session_adoption --test session_policy` → PASS.
- [ ] **Step 5: Commit.**

```bash
git add src/session.rs tests/m5_session_queue.rs
git commit -m "feat: route queue, volume and display changes through Session"
```

### Task 10: Resolve podcast entries from the local cache only

**Files:**
- Create: `src/application/mod.rs`, `src/application/podcast.rs`, `tests/m5_podcast_resolve.rs`
- Modify: `src/lib.rs` (`pub mod application;`), `src/library.rs` (add `EpisodeCandidate`, `episode_candidates`)

**Interfaces:**
- Consumes: `SubscriptionStore::read_snapshot`, `CacheStore::read`, `FeedError` (existing).
- Produces:

```rust
pub const SAVED_SOURCE_NOTICE: &str = "Using saved episode source";
pub enum PodcastResolution {
    /// The cache still lists this exact episode with an enclosure.
    Current { location: SourceLocation, refreshed_fallback: Option<Url> },
    /// Subscription, cache file or episode absent: play the saved URL.
    Saved { location: SourceLocation },
}
#[derive(Debug, thiserror::Error)]
pub enum PodcastResolveError {
    #[error("not a podcast episode")] NotPodcast,
    #[error("this episode no longer has an audio enclosure")] NotPlayable,
    #[error(transparent)] Library(#[from] FeedError),
}
pub fn resolve_podcast(subs: &SubscriptionStore, cache: &CacheStore, media: &MediaId, fallback: &Url)
    -> Result<PodcastResolution, PodcastResolveError>;

// library.rs
pub struct EpisodeCandidate { pub media: MediaId, pub enclosure: Option<Url>, pub title: Option<String>, pub declared_duration: Option<Duration> }
pub fn episode_candidates(subs: &SubscriptionStore, cache: &CacheStore, slug: &str) -> Result<Vec<EpisodeCandidate>, FeedError>;
```

Rules (§5): match only `CachedEpisode::media_id == media`; never title, index or date. `CacheMissing` → `Saved`. Every other `FeedError` from subscriptions or cache (corrupt, parser mismatch, unreadable) → `Library(error)`. Present episode with `enclosure_url: None` → `NotPlayable`. `refreshed_fallback` is `Some(url)` only when it differs from `fallback`. No HTTP types appear in this module.

- [ ] **Step 1: Write failing tests** using `tests/support/feeds.rs`'s `Rig` (`#[path = "support/feeds.rs"] mod feeds;`). Seed with inline RSS:

```rust
fn rss(items: &str) -> Vec<u8> {
    format!(r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Radio-T</title>{items}</channel></rss>"#).into_bytes()
}
fn item(guid: &str, enclosure: Option<&str>) -> String {
    let enclosure = enclosure.map(|url| format!(r#"<enclosure url="{url}" type="audio/mpeg"/>"#)).unwrap_or_default();
    format!("<item><title>{guid}</title><guid>{guid}</guid>{enclosure}</item>")
}
fn episode_media(guid: &str) -> MediaId {
    MediaId::PodcastEpisode {
        feed: tenuto::media::id::FeedId::new(feeds::FEED_ID.into()).expect("feed"),
        episode: tenuto::media::id::EpisodeKey::resolve(Some(guid), None, None).expect("key"),
    }
}
const FEED_URL: &str = "https://feeds.example/radio-t.xml";
```

Test cases, each asserting the exact variant:
1. `a_rotated_enclosure_under_the_same_key_is_current_and_refreshes_the_fallback`: seed `item("e1", Some("https://cdn.example/v2.mp3"))`; fallback `v1.mp3` → `Current { location: Http(v2), refreshed_fallback: Some(v2) }`; `episode_media("e1")` is unchanged.
2. `a_reordered_feed_still_matches_by_identity`: seed `e2` then `e1` → `e1` resolves to its own URL.
3. `an_absent_episode_uses_the_saved_source`: seed `e2` only → `Saved { location: Http(fallback) }`.
4. `an_absent_subscription_uses_the_saved_source`: save an empty `SubscriptionSnapshot` → `Saved`.
5. `a_missing_cache_file_uses_the_saved_source`: seed, then `rig.cache.remove(&id)` → `Saved`.
6. `a_present_episode_without_an_enclosure_is_not_playable`: seed `item("e1", None)` → `Err(NotPlayable)`.
7. `a_corrupt_cache_is_a_resolution_error_not_absence`: overwrite the cache file with `b"{"` → `Err(Library(FeedError::CacheCorrupt { .. }))`.
8. `episode_candidates_carry_the_enclosure_and_declared_duration`: seed with `<itunes:duration>61</itunes:duration>` (declare the itunes namespace on `<rss>`) → one candidate, enclosure present, `declared_duration == Some(61 s)`.

- [ ] **Step 2: Run `cargo test --locked --test m5_podcast_resolve`.** Expected: FAIL, unresolved `tenuto::application`.
- [ ] **Step 3: Implement** `resolve_podcast` and `episode_candidates` per the rules. `application/mod.rs` declares `pub mod podcast;` only.
- [ ] **Step 4: Run `cargo test --locked --test m5_podcast_resolve --test m4_library_reads`.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add src/application src/lib.rs src/library.rs tests/m5_podcast_resolve.rs
git commit -m "feat: resolve queued podcast episodes from the local cache"
```

---
## Phase E — Shared lifecycle

### Task 11: Exclusive profile lock for playback commands

**Files:**
- Create: `src/lifecycle/mod.rs`, `src/lifecycle/lock.rs`, `tests/m5_profile_lock.rs`, `tests/m5_lock_process.rs`
- Modify: `src/lib.rs` (`pub mod lifecycle;`), `src/error.rs` (`AppError::Lifecycle`), `src/persistence/atomic.rs` (`prepare_directory` → `pub(crate)`), `src/app.rs` (`resolve_source` regular-file check; `run_resolved` lock acquisition; `finish` keeps the guard; drop the no-state-directory fallback)

**Interfaces:**
- Consumes: `StateStore::platform_path`, `atomic::prepare_directory`.
- Produces:

```rust
pub struct ProfileLock { file: std::fs::File, path: PathBuf }
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("Another Tenuto player is using this state profile")]
    Contended,
    #[error("no platform state directory is available")]
    NoStateDirectory,
    #[error("cannot prepare the state directory")]
    Directory(#[source] crate::persistence::PersistenceError),
    #[error("cannot {op} {path:?}")]
    Io { path: PathBuf, op: &'static str, #[source] source: std::io::Error },
}
impl ProfileLock {
    pub fn acquire(state_file: &Path) -> Result<Self, LockError>;
    pub fn path(&self) -> &Path;
}
// src/error.rs
#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error(transparent)] Lock(#[from] LockError),
    #[error("cannot install signal handling")] Signals(#[source] std::io::Error),
    #[error("cannot open the session log")] Log(#[source] std::io::Error),
    #[error("cannot redirect stderr to the session log")] Redirect(#[source] std::io::Error),
    #[error("cannot set up the terminal")] Terminal(#[source] std::io::Error),
}
AppError::Lifecycle(#[from] LifecycleError)   // transparent
```

Rules (§6):
- `acquire` resolves the parent of `state.json`, prepares it with the existing private-directory policy, opens `state.lock` with `read(true).write(true).create(true).truncate(false)`, and calls `File::try_lock()`. `TryLockError::WouldBlock` → `Contended`. Never lock `state.json`, never unlink the lock file; dropping the guard releases the lock.
- Legacy `play` order: resolve the source (now including a regular-file check) → acquire the lock → `StateStore::load` → writer and engine → terminal loop. Keep the guard alive through `finish` until after `report_flush`.
- `StateStore::platform_path()` failure is a startup error (`LockError::NoStateDirectory`), replacing the old unsaved fallback for `play`.
- `resolve_source` for a local path rejects a non-regular file with the same error `DecodedSource::open` produces: `PlaybackError::UnsupportedInput { path, reason: "not a regular file".into() }`.
- `--probe-only` and feed commands never acquire the lock.

- [ ] **Step 1: Write failing unit tests.**

```rust
// tests/m5_profile_lock.rs
use tenuto::lifecycle::lock::{LockError, ProfileLock};

#[test]
fn a_second_acquisition_is_contended_until_the_first_is_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("tenuto").join("state.json");
    let first = ProfileLock::acquire(&state).expect("first");
    assert!(first.path().ends_with("state.lock"));
    assert!(matches!(ProfileLock::acquire(&state), Err(LockError::Contended)));
    drop(first);
    let again = ProfileLock::acquire(&state).expect("released on drop");
    assert!(again.path().exists(), "the lock file is never unlinked");
    assert!(!state.exists(), "acquiring never creates or reads state.json");
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
    assert_eq!(LockError::Contended.to_string(), "Another Tenuto player is using this state profile");
}
```

- [ ] **Step 2: Write failing process tests.**

```rust
// tests/m5_lock_process.rs
#![cfg(target_os = "linux")]
#[path = "support/process.rs"] mod process;
mod support;

use std::process::Stdio;
use std::time::{Duration, Instant};
use support::server::{Script, TestServer};

const CONTENDED: &str = "Another Tenuto player is using this state profile";

fn wait_exit(child: &mut std::process::Child, patience: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + patience;
    loop {
        if let Ok(Some(status)) = child.try_wait() { return Some(status); }
        if Instant::now() >= deadline { let _ = child.kill(); return None; }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_second_play_refuses_while_the_first_holds_the_profile() {
    let profile = process::Profile::new().expect("profile");
    let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
    let mut first = profile.command().args(["play", &server.url("/a.mp3")])
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().expect("spawn");
    // The load starts only after the lock and state load, so a stalled
    // request proves the first process owns the profile.
    assert!(server.wait_until_stalled(Duration::from_secs(10)));

    let second = profile.command()
        .args(["play", concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac")])
        .env("TENUTO_AUDIO_OUTPUT", "null").output().expect("second");
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains(CONTENDED));

    let _ = first.kill();
    let _ = wait_exit(&mut first, Duration::from_secs(10));
    server.shutdown();
}

#[test]
fn source_errors_are_reported_before_profile_contention() {
    let profile = process::Profile::new().expect("profile");
    let _held = tenuto::lifecycle::lock::ProfileLock::acquire(&profile.state_file()).expect("hold");
    let absent = profile.command().args(["play", "/nonexistent/definitely-not-here.flac"]).output().expect("run");
    let text = String::from_utf8_lossy(&absent.stderr);
    assert!(text.contains("definitely-not-here.flac") && !text.contains(CONTENDED), "{text}");
    let directory = profile.command().args(["play", env!("CARGO_MANIFEST_DIR")]).output().expect("run");
    let text = String::from_utf8_lossy(&directory.stderr);
    assert!(text.contains("regular file") && !text.contains(CONTENDED), "{text}");
}

#[test]
fn probe_only_and_feed_listing_ignore_a_held_profile() {
    let profile = process::Profile::new().expect("profile");
    let _held = tenuto::lifecycle::lock::ProfileLock::acquire(&profile.state_file()).expect("hold");
    let probe = profile.command()
        .args(["play", concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac"), "--probe-only"])
        .output().expect("run");
    assert!(probe.status.success());
    assert!(profile.command().arg("feeds").output().expect("run").status.success());
}
```

`TENUTO_AUDIO_OUTPUT=null` has no effect until Task 12; the second process is refused before it would open any output, so this test does not depend on it.

- [ ] **Step 3: Run `cargo test --locked --test m5_profile_lock --test m5_lock_process`.** Expected: FAIL (module missing; second process not refused).
- [ ] **Step 4: Implement** `lifecycle::lock` and the `app.rs` changes described above. `lifecycle/mod.rs` declares `pub mod lock;`.
- [ ] **Step 5: Run.** `cargo test --locked --test m5_profile_lock --test m5_lock_process --test cli_playback --test http_cli --test m4_cli --lib` → PASS.
- [ ] **Step 6: Commit.**

```bash
git add src tests/m5_profile_lock.rs tests/m5_lock_process.rs
git commit -m "feat: lock the playback state profile for play"
```

### Task 12: Signal-driven shutdown for `play`, with a paced virtual output

**Files:**
- Create: `src/lifecycle/signals.rs`, `src/playback/output/null_output.rs`, `tests/m5_signals_play.rs`
- Modify: `Cargo.toml`, `Cargo.lock` (`signal-hook` for `cfg(unix)`), `src/lifecycle/mod.rs` (`RunOutcome`), `src/playback/output/mod.rs` (`pub mod null_output;`), `src/playback/engine.rs` (`spawn_for_environment`), `src/app.rs` (`run` returns `RunOutcome`; loops observe shutdown requests), `src/main.rs` (exit status)

**Interfaces:**
- Consumes: `ProfileLock` (Task 11), `EngineHandle::spawn`.
- Produces:

```rust
// lifecycle/mod.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunOutcome { Completed, Signalled(i32) }
impl RunOutcome { pub fn exit_status(self) -> u8 }  // Completed → 0; Signalled(n) → 128 + n, saturating at 255

// lifecycle/signals.rs
pub struct ShutdownSignals { /* shared state, cfg(unix) signal_hook::iterator::Handle, listener JoinHandle */ }
impl ShutdownSignals {
    pub fn install() -> std::io::Result<Self>;   // SIGINT, SIGHUP, SIGTERM on Unix; inert elsewhere
    pub fn request(&self);                        // `q` / key Ctrl-C: records no signal
    pub fn requested(&self) -> bool;
    pub fn first_signal(&self) -> Option<i32>;
    pub fn wake(&self) -> &crossbeam_channel::Receiver<()>;
    pub fn outcome(&self) -> RunOutcome;          // Signalled(first) if a signal was recorded
    pub fn close(self);                           // closes the iterator handle and joins the listener
}

// playback/output/null_output.rs
pub struct NullOutput;   // NullOutput::new()
// playback/engine.rs
pub fn spawn_for_environment() -> EngineHandle;  // TENUTO_AUDIO_OUTPUT=null → NullOutput, else spawn_cpal
// app.rs
pub fn run(cli: cli::Cli) -> Result<RunOutcome, AppError>;
```

Rules (§4):
- The listener thread iterates `Signals::forever()`, stores the first signal with a compare-exchange from 0, sets `requested`, and `try_send`s the wake channel. It never renders, loads state or flushes. `Drop` closes the handle and joins if `close` was not called.
- Legacy `play` order: resolve source → `ShutdownSignals::install()` (failure is a startup error) → lock → load state → writer/engine → loops. Both loops check `signals.requested()` at the top of each pass and break into `finish`. After `finish`, close the signals and return `signals.outcome()` when a signal was recorded, even if the loop's outcome was a playback error (the signal exit status wins); otherwise `Completed` or the error.
- `main.rs`: `Ok(outcome) => ExitCode::from(outcome.exit_status())`.
- `run_resolved` creates its engine with `EngineHandle::spawn_for_environment()`.
- `NullOutput::negotiate` returns the preferred rate (clamped to 8 000–192 000) and channels (clamped to 1–2), 480 buffer frames, F32. `open` spawns `tenuto-null-output`, which calls `core.fill(&mut buffer, now, now + 20 ms)` once per buffer period against an `Instant` captured at open, sleeping to the next deadline; `now()` reads the last published instant; `close` stops and joins. It allocates its buffer once in `open`.

- [ ] **Step 1: Add the dependency** under `[target.'cfg(unix)'.dependencies]`: `signal-hook = "0.4.4"`. Run `cargo check --locked` after `cargo update -p signal-hook --precise 0.4.4` if the lock file needs the new entry; commit the lock change with this task. Confirm the iterator API (`signal_hook::iterator::Signals::new`, `Signals::handle`, `Handle::close`, `Signals::forever`) in the 0.4.4 docs before writing the listener.

- [ ] **Step 2: Write failing tests.**

```rust
// tests/m5_signals_play.rs
#![cfg(target_os = "linux")]
#[path = "support/process.rs"] mod process;
mod support;

use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use support::server::{Script, TestServer};

const SIGNALS: [(&str, i32); 3] = [("INT", 2), ("HUP", 1), ("TERM", 15)];
const FIXTURE_5S: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");
const FIXTURE_SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");

fn send_signal(child: &Child, name: &str) {
    let status = std::process::Command::new("kill").arg(format!("-{name}")).arg(child.id().to_string())
        .status().expect("kill");
    assert!(status.success());
}

fn wait_exit(child: &mut Child, patience: Duration) -> ExitStatus {
    let deadline = Instant::now() + patience;
    loop {
        if let Ok(Some(status)) = child.try_wait() { return status; }
        assert!(Instant::now() < deadline, "the process did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn next_invocation_acquires_the_profile(profile: &process::Profile) {
    let output = profile.command().args(["play", FIXTURE_SHORT]).env("TENUTO_AUDIO_OUTPUT", "null")
        .output().expect("next");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn every_shutdown_signal_during_stalled_preparation_exits_128_plus_n() {
    for (name, number) in SIGNALS {
        let profile = process::Profile::new().expect("profile");
        let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
        let mut child = profile.command().args(["play", &server.url("/a.mp3")])
            .stdout(Stdio::null()).stderr(Stdio::piped()).spawn().expect("spawn");
        assert!(server.wait_until_stalled(Duration::from_secs(10)));
        send_signal(&child, name);
        let status = wait_exit(&mut child, Duration::from_secs(15));
        assert_eq!(status.code(), Some(128 + number), "SIG{name}");
        next_invocation_acquires_the_profile(&profile);
        server.shutdown();
    }
}

#[test]
fn every_shutdown_signal_during_playback_flushes_a_checkpoint() {
    for (name, number) in SIGNALS {
        let profile = process::Profile::new().expect("profile");
        let mut child = profile.command().args(["play", FIXTURE_5S]).env("TENUTO_AUDIO_OUTPUT", "null")
            .stdout(Stdio::null()).stderr(Stdio::piped()).spawn().expect("spawn");
        std::thread::sleep(Duration::from_millis(1_500));
        send_signal(&child, name);
        let status = wait_exit(&mut child, Duration::from_secs(15));
        assert_eq!(status.code(), Some(128 + number), "SIG{name}");
        let state: serde_json::Value = serde_json::from_slice(&std::fs::read(profile.state_file()).expect("flushed"))
            .expect("json");
        let key = format!("local:{}", std::fs::canonicalize(FIXTURE_5S).expect("fixture").display());
        let secs = state["checkpoints"][&key]["position"]["secs"].as_u64().expect("checkpoint written");
        assert!(secs >= 1, "SIG{name}: position {secs}");
        next_invocation_acquires_the_profile(&profile);
    }
}

#[test]
fn exit_status_arithmetic() {
    use tenuto::lifecycle::RunOutcome;
    assert_eq!(RunOutcome::Completed.exit_status(), 0);
    assert_eq!(RunOutcome::Signalled(15).exit_status(), 143);
    assert_eq!(RunOutcome::Signalled(200).exit_status(), 255);
}
```

- [ ] **Step 3: Run `cargo test --locked --test m5_signals_play`.** Expected: FAIL (the default SIGTERM disposition kills the child without a flush; `lifecycle::RunOutcome` missing).
- [ ] **Step 4: Implement** the listener, `RunOutcome`, `NullOutput`, `spawn_for_environment` and the `play`/`main` wiring per the rules.
- [ ] **Step 5: Run.** `cargo test --locked --test m5_signals_play --test m5_lock_process --test cli_playback --test cli --test m4_cli` → PASS.
- [ ] **Step 6: Commit.**

```bash
git add Cargo.toml Cargo.lock src tests/m5_signals_play.rs
git commit -m "feat: flush and exit 128+n on shutdown signals in play"
```

---

## Phase F — Terminal-free runtime

### Task 13: Extract shared helpers from `app.rs`

**Files:**
- Create: `src/application/source.rs`, `src/application/seek.rs`, `src/media/display.rs`
- Modify: `src/application/mod.rs`, `src/media/mod.rs`, `src/app.rs`

**Interfaces:**
- Produces (moves plus the seek-cancellation correction):
  - `application::source::{resolve_source, is_url_spelling}` (with `resolve_url` private)
  - `application::seek::{KeyRouter, SeekBurst}`; `app.rs` keeps `pub use crate::application::seek::KeyRouter;`. Expose the existing `KeyRouter::cancel(&mut self)` as public so the runtime can discard both an unsubmitted burst and its displayed target on superseding actions. In `observe`, `Loaded`, `LoadCancelled` and `Failed` call `cancel`, rather than merely `release`.
  - `media::display::{display_name, remote_display_name, episode_name, format_hms, fit_to_width}` as `pub fn`
- `app.rs` imports all of them so its in-file tests (`super::*`) compile unchanged. Move each item's unit tests with it when they test only that item (`seek_target`/burst tests to `seek.rs`, URL spelling tests to `source.rs`, name/width tests to `display.rs`); leave `Mirror`/status-line tests in `app.rs`.

- [ ] **Step 1: Record the baseline.** Run `cargo test --locked --lib 2>&1 | grep "test result"` and `cargo test --locked --test app_cli`; note the passing counts.
- [ ] **Step 2: Move the code and tests** as listed, plus the cancellation change above. Add `loaded_and_failed_events_cancel_unsubmitted_bursts`: accumulate a relative seek, observe each superseding outcome, assert `!router.is_seeking()`, advance beyond the coalescing window, and assert `take_due(now) == None` in the module's unit test.
- [ ] **Step 3: Rerun both commands.** Expected: all original tests plus the new cancellation regression pass.
- [ ] **Step 4: Run `cargo clippy --locked --all-targets -- -D warnings`.** Expected: clean.
- [ ] **Step 5: Commit.**

```bash
git add src
git commit -m "refactor: share source resolution, seek routing and display names"
```

### Task 14: Transport decision table

**Files:**
- Create: `src/application/transport.rs`, `tests/m5_transport_rules.rs`
- Modify: `src/application/mod.rs`

**Interfaces:**
- Consumes: `Queue`, `QueueEntryId` (Task 3).
- Produces:

```rust
pub const PLAY_BEFORE_SEEK: &str = "Play a track before seeking";
pub const QUEUE_EMPTY: &str = "Queue is empty";
pub const TRACK_ENDED: &str = "Track ended; press play to replay";
pub const STILL_LOADING: &str = "Still loading";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackPhase { Unloaded, Loading, LoadFailed, Playing, Paused, Stopped, Ended }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportInput { Space, Play, Enter, Home, SeekBy(i64), SeekTo(Duration), Previous, Next }

pub struct TransportSituation<'a> {
    pub queue: &'a Queue,
    pub selected: Option<QueueEntryId>,
    pub phase: PlaybackPhase,
    pub last_requested: Option<QueueEntryId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportDecision { Load(QueueEntryId), TogglePause, Play, SeekBy(i64), SeekTo(Duration), Restart, Notice(&'static str), Nothing }

pub fn decide(input: TransportInput, situation: &TransportSituation<'_>) -> TransportDecision;
```

The table, implemented exactly (§4; "selection" means `selected.or(queue.first())`):

| Phase | Space | Play (`p`) | Enter | Home | SeekBy / SeekTo | Previous / Next anchor |
|---|---|---|---|---|---|---|
| empty queue (any phase) | `Notice(QUEUE_EMPTY)` | same | same | same | same | `Nothing` |
| Unloaded, active entry | `Load(active)` | `Load(active)` | `Load(selection)` | `Notice(PLAY_BEFORE_SEEK)` | same | active |
| Unloaded, no active | `Load(selection)` | `Load(selection)` | `Load(selection)` | `Notice(PLAY_BEFORE_SEEK)` | same | selection |
| LoadFailed | `Load(last_requested)` if still queued, else the Unloaded rule | same | `Load(selection)` | `Notice(PLAY_BEFORE_SEEK)` | same | active, else selection |
| Loading | `Nothing` | `Nothing` | `Load(selection)` | `Notice(STILL_LOADING)` | same | `last_requested`, else active, else selection |
| Playing | `TogglePause` | `Play` | `Load(selection)` | `Restart` | pass through | active |
| Paused, Stopped | `TogglePause` | `Play` | `Load(selection)` | `Restart` | pass through | active |
| Ended | `Load(active)` | `Load(active)` | `Load(selection)` | `Restart` | `Notice(TRACK_ENDED)` | active |

Previous/Next return `Load(neighbor)` or `Nothing` when the anchor has no neighbor (never wraps). A `Load` decision re-issues a full tokenized load; the runtime applies the resume/replay policy.

- [ ] **Step 1: Write failing table tests.**

```rust
// tests/m5_transport_rules.rs
mod support;

use std::time::Duration;
use tenuto::application::transport::*;
use tenuto::media::id::MediaId;
use tenuto::persistence::model::PersistedState;
use tenuto::queue::{DisplayMetadata, NewQueueEntry, Queue, QueueEntryId, QueueSource};
use tenuto::session::{LoadTarget, Session};
use support::media;

fn entry(name: &str) -> NewQueueEntry {
    let MediaId::LocalFile(path) = media(name) else { unreachable!() };
    NewQueueEntry::new(media(name), QueueSource::LocalFile(path), DisplayMetadata::default()).expect("entry")
}

/// A queue of three with the second entry active, built through Session so
/// the active entry is set the only legal way.
fn with_active() -> (Queue, Vec<QueueEntryId>) {
    use tenuto::clock::{Clock, FakeClock};
    use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
    use tenuto::playback::event::{PlaybackEvent, StartDisposition};
    let mut session = Session::new(PersistedState::default());
    let (ids, _) = session.enqueue(vec![entry("a"), entry("b"), entry("c")]).expect("fits");
    let request = session.register_load(LoadTarget::Queue(ids[1]), &media("b")).expect("registered");
    session.observe(&PlaybackEvent::Loaded { session_rev: 1, request, media: media("b"),
        metadata: Default::default(), capabilities: MediaCapabilities { continuity: Continuity::Finite, seek: SeekSupport::Native },
        position: Duration::ZERO, disposition: StartDisposition::Fresh }, FakeClock::new().sample());
    (session.state().queue().clone(), ids)
}

fn without_active() -> (Queue, Vec<QueueEntryId>) {
    let mut queue = Queue::default();
    let ids = queue.enqueue(vec![entry("a"), entry("b"), entry("c")]).expect("fits");
    (queue, ids)
}

fn run(queue: &Queue, selected: Option<QueueEntryId>, phase: PlaybackPhase, last: Option<QueueEntryId>, input: TransportInput) -> TransportDecision {
    decide(input, &TransportSituation { queue, selected, phase, last_requested: last })
}

#[test]
fn a_restored_active_entry_loads_on_space_regardless_of_selection() {
    let (queue, ids) = with_active();
    for input in [TransportInput::Space, TransportInput::Play] {
        assert_eq!(run(&queue, Some(ids[2]), PlaybackPhase::Unloaded, None, input), TransportDecision::Load(ids[1]));
    }
    assert_eq!(run(&queue, Some(ids[2]), PlaybackPhase::Unloaded, None, TransportInput::Enter), TransportDecision::Load(ids[2]));
    for input in [TransportInput::Home, TransportInput::SeekBy(10), TransportInput::SeekBy(-10), TransportInput::SeekTo(Duration::from_secs(3))] {
        assert_eq!(run(&queue, Some(ids[2]), PlaybackPhase::Unloaded, None, input), TransportDecision::Notice(PLAY_BEFORE_SEEK));
    }
}

#[test]
fn without_an_active_entry_the_default_selection_is_the_first_row() {
    let (queue, ids) = without_active();
    assert_eq!(run(&queue, None, PlaybackPhase::Unloaded, None, TransportInput::Space), TransportDecision::Load(ids[0]));
    assert_eq!(run(&queue, Some(ids[2]), PlaybackPhase::Unloaded, None, TransportInput::Play), TransportDecision::Load(ids[2]));
}

#[test]
fn an_empty_queue_answers_every_transport_key_with_a_notice() {
    let queue = Queue::default();
    for input in [TransportInput::Space, TransportInput::Play, TransportInput::Enter, TransportInput::Home, TransportInput::SeekBy(10)] {
        assert_eq!(run(&queue, None, PlaybackPhase::Unloaded, None, input), TransportDecision::Notice(QUEUE_EMPTY));
    }
    assert_eq!(run(&queue, None, PlaybackPhase::Unloaded, None, TransportInput::Next), TransportDecision::Nothing);
}

#[test]
fn an_ended_last_entry_replays_on_play_and_restarts_on_home() {
    let (queue, ids) = with_active();
    assert_eq!(run(&queue, Some(ids[0]), PlaybackPhase::Ended, None, TransportInput::Space), TransportDecision::Load(ids[1]));
    assert_eq!(run(&queue, Some(ids[0]), PlaybackPhase::Ended, None, TransportInput::Enter), TransportDecision::Load(ids[0]));
    assert_eq!(run(&queue, Some(ids[0]), PlaybackPhase::Ended, None, TransportInput::Home), TransportDecision::Restart);
    assert_eq!(run(&queue, Some(ids[0]), PlaybackPhase::Ended, None, TransportInput::SeekBy(10)), TransportDecision::Notice(TRACK_ENDED));
}

#[test]
fn loading_never_accumulates_a_seek() {
    let (queue, ids) = with_active();
    for input in [TransportInput::Home, TransportInput::SeekBy(-10)] {
        assert_eq!(run(&queue, None, PlaybackPhase::Loading, Some(ids[2]), input), TransportDecision::Notice(STILL_LOADING));
    }
}

#[test]
fn playing_uses_engine_semantics() {
    let (queue, ids) = with_active();
    assert_eq!(run(&queue, None, PlaybackPhase::Playing, None, TransportInput::Space), TransportDecision::TogglePause);
    assert_eq!(run(&queue, None, PlaybackPhase::Playing, None, TransportInput::Play), TransportDecision::Play);
    assert_eq!(run(&queue, None, PlaybackPhase::Paused, None, TransportInput::SeekBy(10)), TransportDecision::SeekBy(10));
    assert_eq!(run(&queue, None, PlaybackPhase::Stopped, None, TransportInput::Home), TransportDecision::Restart);
    assert_eq!(run(&queue, Some(ids[0]), PlaybackPhase::Playing, None, TransportInput::Enter), TransportDecision::Load(ids[0]));
}

#[test]
fn a_failed_load_retries_the_last_requested_entry_while_it_is_queued() {
    let (queue, ids) = without_active();
    assert_eq!(run(&queue, Some(ids[0]), PlaybackPhase::LoadFailed, Some(ids[2]), TransportInput::Space), TransportDecision::Load(ids[2]));
    let mut shorter = queue.clone();
    shorter.remove(ids[2]).expect("known");
    assert_eq!(run(&shorter, Some(ids[1]), PlaybackPhase::LoadFailed, Some(ids[2]), TransportInput::Play), TransportDecision::Load(ids[1]));
}

#[test]
fn previous_and_next_anchor_on_the_active_entry_and_never_wrap() {
    let (queue, ids) = with_active();
    assert_eq!(run(&queue, Some(ids[0]), PlaybackPhase::Playing, None, TransportInput::Next), TransportDecision::Load(ids[2]));
    assert_eq!(run(&queue, Some(ids[2]), PlaybackPhase::Unloaded, None, TransportInput::Previous), TransportDecision::Load(ids[0]));
    let (queue, ids) = without_active();
    assert_eq!(run(&queue, Some(ids[0]), PlaybackPhase::Unloaded, None, TransportInput::Previous), TransportDecision::Nothing);
    assert_eq!(run(&queue, Some(ids[2]), PlaybackPhase::Unloaded, None, TransportInput::Next), TransportDecision::Nothing);
}
```

- [ ] **Step 2: Run `cargo test --locked --test m5_transport_rules`.** Expected: FAIL, module missing.
- [ ] **Step 3: Implement `decide`** as one `match` on `(phase, input)` after the empty-queue check, with a `fn anchor(situation) -> Option<QueueEntryId>` for Previous/Next per the table.
- [ ] **Step 4: Run the test.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add src/application tests/m5_transport_rules.rs
git commit -m "feat: encode the pre-load and end-of-queue transport rules"
```

### Task 15: `PlayerRuntime` and `PlayerView`

**Files:**
- Create: `src/application/runtime.rs`, `src/application/view.rs`, `tests/m5_runtime.rs`
- Modify: `src/application/mod.rs`

**Interfaces:**
- Consumes: `Session` (Tasks 8–9), `decide` (Task 14), `resolve_podcast` (Task 10), `resolve_source`, `KeyRouter` (Task 13), `EngineHandle`, `WriterHandle`, `HttpService`.
- Produces:

```rust
// application/runtime.rs
pub type EngineFactory = Box<dyn FnMut() -> EngineHandle + Send>;
pub struct LibraryStores { pub subscriptions: SubscriptionStore, pub cache: CacheStore }
pub struct RuntimeParts {
    pub session: Session, pub writer: WriterHandle, pub persisting: bool,
    pub clock: Arc<dyn Clock>, pub engine_factory: EngineFactory,
    pub library: Option<LibraryStores>, pub http_limits: Limits,
}
#[derive(Clone, Debug)]
pub enum EnqueueItem { Path(PathBuf), Url(String), Episode(EpisodeCandidate) }
impl EnqueueItem { pub fn from_input(text: &str) -> Self }   // http(s) spelling → Url, else Path
#[derive(Clone, Debug)]
pub enum AppCommand {
    PlayPause { selected: Option<QueueEntryId> }, Play { selected: Option<QueueEntryId> }, PlayEntry(QueueEntryId),
    Stop, SeekBy(i64), SeekTo(Duration), Restart, AdjustVolume(f32),
    Previous { selected: Option<QueueEntryId> }, Next { selected: Option<QueueEntryId> },
    Enqueue(Vec<EnqueueItem>), Remove(QueueEntryId), Move(QueueEntryId, Direction), ClearQueue,
}
pub enum FlushReport { Written, Failed(PersistenceError), Unconfirmed, Disabled }
impl PlayerRuntime {
    pub fn new(parts: RuntimeParts) -> Self;
    pub fn handle(&mut self, command: AppCommand);
    pub fn pump(&mut self);
    pub fn view(&self) -> PlayerView;
    pub fn take_selection_hint(&mut self) -> Option<QueueEntryId>;
    pub fn set_status(&mut self, message: impl Into<String>);
    pub fn session(&self) -> &Session;
    pub fn shutdown(self) -> FlushReport;
}

// application/view.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SavedHistory { Position { at: Duration, estimated: bool }, Played, Unknown }
pub fn saved_history(entry: Option<&PersistedCheckpoint>) -> Option<SavedHistory>;
pub fn format_saved(history: SavedHistory) -> String;   // "~01:02 saved" | "01:02 saved" | "played" | "position unknown"
#[derive(Clone, Debug)]
pub struct QueueRow { pub id: QueueEntryId, pub title: String, pub subtitle: Option<String>, pub duration: Option<DisplayDuration>, pub saved: Option<SavedHistory> }
#[derive(Clone, Debug)]
pub struct NowPlaying {
    pub entry: Option<QueueEntryId>, pub title: String, pub artist: Option<String>, pub album: Option<String>,
    pub loaded: bool, pub state: PlaybackState, pub position: Duration, pub duration: Option<DisplayDuration>,
    pub estimated_position: bool, pub degraded: bool, pub buffering: bool, pub seek: Option<SeekSupport>,
    pub saved: Option<SavedHistory>, pub session_rev: u64, pub load: Option<LoadRequestId>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistenceStatus { Saving, Unsaved }
#[derive(Clone, Debug)]
pub struct PlayerView {
    pub rows: Vec<QueueRow>, pub active: Option<QueueEntryId>, pub now_playing: Option<NowPlaying>,
    pub phase: PlaybackPhase, pub volume: Volume, pub status: Option<String>, pub persistence: PersistenceStatus,
    pub last_requested: Option<QueueEntryId>,
}
```

Runtime rules:
- **Lazy engine.** `ensure_engine()` calls the factory on the first load, then sends `SetVolume(session volume)`. Volume changes without an engine go through `Session::set_volume`; with one, through the engine (whose `VolumeChanged` reaches `Session` as today).
- **Commands that touch transport** build a `TransportSituation` and act on `decide`: `Load(id)` → `load_entry(id)`; `TogglePause`/`Play`/`SeekBy`/`Restart` → `KeyRouter::route`; `SeekTo(t)` → cancel the router, then `engine.submit_seek(t)` only when adopted playback has a known duration and seek support is not `Unsupported`, else status `Play a track before seeking`; `Notice(text)` → status; `Stop` → cancel the router, then `engine.interrupt_stop()` when an engine exists. Restart and shutdown also cancel. Discard bursts before active removal/clear, and before submitting an accepted new load. Cancelling must not submit the old target first.
- **`load_entry(id)`**: look up the entry and record `last_requested = Some(id)` before resolving it. Build the location (`LocalFile` → `LocalPath`, `RemoteUrl` → `Http(Url::parse(normalized))`, `Podcast` → `resolve_podcast` when `library` is `Some`, otherwise `Saved`; apply a refreshed fallback through Session; `Saved` sets `SAVED_SOURCE_NOTICE`). Resolution or HTTP-service startup errors set `load_failed = true` and status, then return without adopting. Register the load (`Busy` → status `Too many pending loads`); ensure engine/HTTP service, retracting on setup failure. Submit `Load { request, media, source, resume: session.resume_intent(&media) }`; non-`Accepted` retracts, reports busy and returns without any play command. On acceptance cancel the seek router, clear `load_failed`, then submit `PlayLoaded { request }`. Never queue unrestricted `Play` as part of a load. If automatic-start admission is busy, leave the successfully loaded track paused and report `Player is busy`; do not retry with unrestricted `Play`.
- **`pump()`**: first drain every event: save `session.accepts_media_event(&event)`, then `Session::observe` → submit; `KeyRouter::observe`; update the playback mirror only for events accepted by that pre-observation check (and, for `Loaded`, only when Session adopted its token); update profile-wide volume separately. A current `Failed { request: Some }` sets `load_failed` and status; playback failures set status. Then `take_stop_request()` → cancel router and `interrupt_stop`; `take_advance()` → `Next(id)` calls `load_entry(id)`, `EndOfQueue` does nothing. Flush the router only after those transitions and only when the transport decision table allows seeking. Finally sample progress, submit `Session::tick`, and update the mirror only when revision and token match. Never flush an old burst into a new or still-loading track.
- **`phase()`**: `Loading` while pending loads exist; otherwise `LoadFailed` when the latest requested load failed, even if an older occurrence is still adopted; otherwise `Unloaded` if nothing is adopted; otherwise map mirror state (`Ended`, `Paused`, `Playing` directly, `Stopped`/`Failed` → `Stopped`, `Idle`/`Loading` → `Loading`). A later successfully adopted request clears `load_failed`; an earlier outcome cannot overwrite a later request's success/failure. Store `last_attempt_token: Option<LoadRequestId>` alongside `last_requested`: clear it at the start of each attempt, set it on accepted load admission, and change `load_failed` from an outcome only when its token equals this value. Do not match by media ID. A resolution failure has no token but still takes precedence over older in-flight outcomes. Space/p after failure must retry `last_requested` with a fresh token; Enter chooses selection.
- **Queue commands**: `Enqueue` resolves every item first (paths through `resolve_source`, URLs through `resolve_source`, episodes need an enclosure or report `NotPlayable`), builds `NewQueueEntry`s with display from the item (`Episode` → title and `DurationSource::Declared`), then one `Session::enqueue` (only `QueueError::Capacity` becomes status `Queue is full (256 entries)`; `IdExhausted` and other errors retain their own message); any resolution error rejects the whole batch with that error's text. `Remove` and `ClearQueue` pass the latest `engine.progress()` (or a zero progress with `load: None` when no engine exists), call `interrupt_stop` when `stop_playback`, and store the selection hint. `Move` submits.
- **View**: rows in queue order; titles via `display.title` or `display_name(media)` / `episode_name`, always passed through `commands::displayable`; `saved` from `saved_history(state.entry_for(media))`. `now_playing` follows the active entry; before loading it has `loaded: false` and its saved history. `persistence` is `Unsaved` when `persisting` is false.
- **`shutdown`**: if an engine exists: `interrupt_shutdown`, in-band `Shutdown`, `join`, `reconcile_shutdown`, forced submit. Then `writer.shutdown()`, classified like `app::classify_flush`.

- [ ] **Step 1: Write failing tests** (real-time `NullOutput`, `sine.flac` is 0.5 s).

```rust
// tests/m5_runtime.rs
mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use tenuto::application::runtime::{AppCommand, EnqueueItem, FlushReport, PlayerRuntime, RuntimeParts};
use tenuto::application::transport::PlaybackPhase;
use tenuto::clock::SystemClock;
use tenuto::http::limits::Limits;
use tenuto::persistence::model::PersistedState;
use tenuto::persistence::store::StateStore;
use tenuto::persistence::writer::WriterHandle;
use tenuto::playback::engine::EngineHandle;
use tenuto::playback::output::null_output::NullOutput;
use tenuto::session::Session;

const SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");
const MISSING: &str = "/nonexistent/m5-missing.flac";

struct Rig { _dir: tempfile::TempDir, runtime: PlayerRuntime, state_path: std::path::PathBuf }

fn rig_with(state: PersistedState) -> Rig {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock: Arc<dyn tenuto::clock::Clock> = Arc::new(SystemClock);
    let state_path = dir.path().join("state.json");
    let writer = WriterHandle::spawn(Box::new(StateStore::new(state_path.clone(), clock.clone())), clock.clone());
    let runtime = PlayerRuntime::new(RuntimeParts {
        session: Session::new(state), writer, persisting: true, clock,
        engine_factory: Box::new(|| EngineHandle::spawn(Box::new(NullOutput::new()))),
        library: None, http_limits: Limits::default(),
    });
    // Task 23 adds `metadata_probe` and `hook` fields to this literal.
    Rig { _dir: dir, runtime, state_path }
}

fn pump_until(runtime: &mut PlayerRuntime, what: &str, done: impl Fn(&tenuto::application::view::PlayerView) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        runtime.pump();
        if done(&runtime.view()) { return; }
        assert!(Instant::now() < deadline, "never reached: {what}; view {:?}", runtime.view());
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn enter_plays_the_selected_entry_and_adopts_only_it() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::Enqueue(vec![EnqueueItem::Path(SHORT.into()), EnqueueItem::Path(SHORT.into())]));
    let ids: Vec<_> = rig.runtime.view().rows.iter().map(|row| row.id).collect();
    rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    pump_until(&mut rig.runtime, "second row active", |view| view.active == Some(ids[1]));
    assert!(matches!(rig.runtime.view().phase, PlaybackPhase::Playing | PlaybackPhase::Ended));
}

#[test]
fn completion_advances_once_and_the_last_entry_stays_ended() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::Enqueue(vec![EnqueueItem::Path(SHORT.into()), EnqueueItem::Path(SHORT.into())]));
    let ids: Vec<_> = rig.runtime.view().rows.iter().map(|row| row.id).collect();
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    pump_until(&mut rig.runtime, "advanced to the second row", |view| view.active == Some(ids[1]));
    pump_until(&mut rig.runtime, "queue ended", |view| view.phase == PlaybackPhase::Ended);
    assert_eq!(rig.runtime.view().active, Some(ids[1]), "no wrap back to the first row");
}

#[test]
fn a_failed_load_keeps_the_queue_and_does_not_skip() {
    let mut state = Session::new(PersistedState::default());
    let missing = tenuto::media::id::AbsolutePath::new(MISSING.into()).expect("absolute");
    let (_ids, _) = state.enqueue(vec![
        tenuto::queue::NewQueueEntry::new(tenuto::media::id::MediaId::LocalFile(missing.clone()),
            tenuto::queue::QueueSource::LocalFile(missing), Default::default()).expect("entry"),
    ]).expect("fits");
    let mut rig = rig_with(state.state().clone());
    rig.runtime.handle(AppCommand::Enqueue(vec![EnqueueItem::Path(SHORT.into())]));
    let ids: Vec<_> = rig.runtime.view().rows.iter().map(|row| row.id).collect();
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    pump_until(&mut rig.runtime, "load failed", |view| view.phase == PlaybackPhase::LoadFailed);
    let view = rig.runtime.view();
    assert_eq!(view.rows.len(), 2);
    assert_eq!(view.active, None);
    assert!(view.status.as_deref().is_some_and(|s| !s.is_empty()));
}

#[test]
fn space_before_loading_loads_the_restored_active_entry() {
    let path = std::fs::canonicalize(SHORT).expect("fixture");
    let key = format!("local:{}", path.display());
    let file = serde_json::json!({ "schema_version": 3, "current_media": key, "volume": 1.0, "checkpoints": {},
        "queue": [{ "id": 1, "media": "local:/music/other.flac", "source": { "kind": "local", "path": "/music/other.flac" } },
                  { "id": 2, "media": key, "source": { "kind": "local", "path": path }}],
        "active_entry": 2 });
    let mut rig = rig_with(serde_json::from_value(file).expect("valid"));
    let ids: Vec<_> = rig.runtime.view().rows.iter().map(|row| row.id).collect();
    rig.runtime.handle(AppCommand::PlayPause { selected: Some(ids[0]) });
    pump_until(&mut rig.runtime, "active entry loaded", |view| view.now_playing.as_ref().is_some_and(|now| now.loaded && now.entry == Some(ids[1])));
}

#[test]
fn seeking_before_any_load_is_a_notice_and_opens_nothing() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::Enqueue(vec![EnqueueItem::Path(SHORT.into())]));
    rig.runtime.handle(AppCommand::SeekBy(10));
    assert_eq!(rig.runtime.view().status.as_deref(), Some("Play a track before seeking"));
    assert_eq!(rig.runtime.view().phase, PlaybackPhase::Unloaded);
}

#[test]
fn volume_without_an_engine_is_persisted_at_shutdown() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::AdjustVolume(-0.25));
    let state_path = rig.state_path.clone();
    assert!(matches!(rig.runtime.shutdown(), FlushReport::Written));
    let written: serde_json::Value = serde_json::from_slice(&std::fs::read(state_path).expect("written")).expect("json");
    assert_eq!(written["volume"], 0.75);
}

#[test]
fn an_oversized_enqueue_is_rejected_whole_with_a_visible_message() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::Enqueue((0..257).map(|_| EnqueueItem::Path(SHORT.into())).collect()));
    assert!(rig.runtime.view().rows.is_empty());
    assert_eq!(rig.runtime.view().status.as_deref(), Some("Queue is full (256 entries)"));
}
```

```rust
#[test]
fn two_loads_of_one_media_submitted_together_end_on_the_later_row() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::Enqueue(vec![EnqueueItem::Path(SHORT.into()), EnqueueItem::Path(SHORT.into())]));
    let ids: Vec<_> = rig.runtime.view().rows.iter().map(|row| row.id).collect();
    // Both are submitted before a single event is drained.
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    assert_eq!(rig.runtime.session().pending_load_count(), 2);
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while rig.runtime.view().active != Some(ids[1]) {
        rig.runtime.pump();
        if let Some(active) = rig.runtime.view().active && seen.last() != Some(&active) { seen.push(active); }
        assert!(Instant::now() < deadline, "never adopted the later row");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(rig.runtime.session().pending_load_count(), 0);
    assert!(!seen.windows(2).any(|pair| pair == [ids[1], ids[0]]), "never regressed to the earlier row: {seen:?}");
}

struct FailingSink;
impl tenuto::persistence::writer::StateSink for FailingSink {
    fn write(&self, _: &PersistedState) -> Result<(), tenuto::persistence::PersistenceError> {
        Err(tenuto::persistence::PersistenceError::NoStateDirectory)
    }
}

#[test]
fn a_failed_final_flush_is_reported_not_claimed_as_saved() {
    let clock: Arc<dyn tenuto::clock::Clock> = Arc::new(SystemClock);
    let mut runtime = PlayerRuntime::new(RuntimeParts {
        session: Session::new(PersistedState::default()),
        writer: WriterHandle::spawn(Box::new(FailingSink), clock.clone()), persisting: true, clock,
        engine_factory: Box::new(|| EngineHandle::spawn(Box::new(NullOutput::new()))),
        library: None, http_limits: Limits::default(),
    });
    runtime.handle(AppCommand::AdjustVolume(-0.1));
    assert!(matches!(runtime.shutdown(), FlushReport::Failed(_)));
}
```

Add unit tests in `view.rs`:
- `saved_history`/`format_saved`: completed → `played`; estimated 62 s → `~01:02 saved`; position 62 s → `01:02 saved`; neither → `position unknown`; no entry → `None`.
- `a_title_with_terminal_controls_is_rendered_inert`: a queue entry whose display title is `"evil\u{1b}[2Jtitle"` produces a `QueueRow::title` containing no `'\u{1b}'` character.

Add runtime regressions with these exact sequences:
- `seek_then_load_discards_the_old_target`: enqueue two 5-second fixtures; play A, issue `SeekBy(10)` then `PlayEntry(B)` before the burst deadline, pump for at least 250 ms; B remains playing near zero, no old seek is submitted and its saved position is not advanced to A's target. Repeat with Stop, active removal, clear and mouse `SeekTo` superseding a burst.
- `space_retries_a_failed_switch_with_a_fresh_token`: seed a queue with playable 5-second A and missing local B, play A, request B, wait for `LoadFailed`; active stays A. Create B by copying the fixture, press Space with selection still on A, then assert B is adopted under a fresh token. Repeat with `Play`, and with two duplicate-media occurrences to ensure matching uses tokens.
- `a_resolution_failure_is_retryable_without_losing_the_previous_adoption`: use the podcast resolver's corrupt-cache fixture for B after A is adopted; repair the cache and press p with A selected; B must load. Older pending outcomes cannot clear the later resolution failure.
- `automatic_start_handles_intervening_outcomes`: submit A and B before pumping; both `Loaded` events adopt their own occurrence in order, but a delayed `PlayLoaded(A)` never starts B. Make B fail and verify no implicit retry. On start-command admission failure, the loaded track remains paused with a visible busy status.

- [ ] **Step 2: Run `cargo test --locked --test m5_runtime`.** Expected: FAIL, module missing.
- [ ] **Step 3: Implement** `runtime.rs` and `view.rs` per the rules. Keep the runtime free of Ratatui and crossterm types.
- [ ] **Step 4: Run.** `cargo test --locked --test m5_runtime --test m5_transport_rules --lib` → PASS (run twice to catch timing flakiness).
- [ ] **Step 5: Commit.**

```bash
git add src/application tests/m5_runtime.rs
git commit -m "feat: add the terminal-free player runtime"
```

---
## Phase G — Terminal interface

### Task 16: Panic containment, test hooks and idempotent terminal cleanup

**Files:**
- Create: `src/lifecycle/hooks.rs`, `src/lifecycle/panic.rs`, `src/lifecycle/terminal.rs`, `tests/m5_contained_panic.rs`
- Modify: `src/lifecycle/mod.rs`

**Interfaces:**
- Produces:

```rust
// hooks.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TestHook { None, PanicBeforeRedirect, PanicAfterRedirect, PanicAfterTerminal, StderrProbe, ArtworkJobPanic, ArtworkEncodingPanic, MetadataJobPanic, WorkerPanic }
impl TestHook {
    pub fn parse(value: Option<&str>) -> Self;   // exact kebab-case names from decision 8; anything else → None
    pub fn from_env() -> Self;                   // reads TENUTO_TEST_HOOK once
    pub fn panic_at(self, stage: TestHook);      // panics with "tenuto test hook: <name>" when self == stage
}

// panic.rs
pub fn in_contained_job() -> bool;               // thread-local, `try_with`, never panics
#[derive(Debug, thiserror::Error)] #[error("{label} job panicked")]
pub struct ContainedPanic { pub label: &'static str }
pub fn run_contained<R>(label: &'static str, job: impl FnOnce() -> R) -> Result<R, ContainedPanic>;
pub enum TakeResult<T> { Taken(T), Empty, Busy }
pub struct TakeOnceSlot<T> { /* Arc<Mutex<Option<T>>> */ }
impl<T> TakeOnceSlot<T> { pub fn new() -> Self; pub fn publish(&self, value: T) -> Result<(), T>; pub fn try_take(&self) -> TakeResult<T>; }
impl<T> Clone for TakeOnceSlot<T>;
pub struct FatalCleanup { /* rendering_disabled, fatal_requested: AtomicBool, terminal: TerminalCleanup,
                               stderr: TakeOnceSlot<Box<dyn Send>>, diagnostic_log: TakeOnceSlot<File> via Mutex<Option<File>>, wake: Sender<()> */ }
impl FatalCleanup {
    pub fn new(wake: crossbeam_channel::Sender<()>) -> Self;
    pub fn terminal(&self) -> &TerminalCleanup;
    pub fn stderr_slot(&self) -> &TakeOnceSlot<Box<dyn Send>>;
    pub fn set_diagnostic_log(&self, file: std::fs::File);
    pub fn rendering_disabled(&self) -> bool;
    pub fn fatal_requested(&self) -> bool;
    pub fn restore_now(&self);                   // terminal restore + stderr try_take; used by hook and normal teardown
}
pub fn install_panic_hook(cleanup: Arc<FatalCleanup>);

// terminal.rs
#[derive(Default)]
pub struct TerminalCleanup { raw: AtomicBool, alternate: AtomicBool, mouse: AtomicBool, cursor_hidden: AtomicBool, kitty_images: AtomicBool }
impl TerminalCleanup {
    pub fn mark_raw(&self); pub fn mark_alternate(&self); pub fn set_mouse(&self, on: bool);
    pub fn mark_cursor_hidden(&self); pub fn set_kitty_images(&self, on: bool);
    pub fn restore_into(&self, out: &mut dyn std::io::Write);   // idempotent; ignores write errors
    pub fn restore(&self);                                       // restore_into(stdout) + disable_raw_mode if raw
}
```

Rules (§11):
- `run_contained` sets the thread-local flag through a guard that restores the **previous** value on success or unwind, wraps the job in `catch_unwind(AssertUnwindSafe(job))`, and maps a caught panic to `ContainedPanic`. Never call it around engine or `Session` code.
- The hook, first thing: `if in_contained_job()`, write one line `contained panic in background job: <sanitized message>` to the diagnostic log with `try_lock` (skip on busy or absent) and return. It must not restore the terminal, touch fd 2, disable rendering, call the previous hook or request shutdown.
- Otherwise: set `rendering_disabled` and `fatal_requested`, `try_send` the wake channel, `restore_now()`, then call the previous hook. Never block on application state.
- `TakeOnceSlot::try_take`: `try_lock`; on `Poisoned` recover the guard; `take()` inside a block so the guard drops **before** the value is returned; `WouldBlock` → `Busy`. `publish` returns `Err(value)` if the slot is occupied.
- `restore_into` writes, only for flags that were set (each flag swapped to false first): kitty delete-all `\x1b_Ga=d,d=A\x1b\\`, `DisableMouseCapture`, `Show`, `LeaveAlternateScreen`. `restore` then calls `disable_raw_mode` if the raw flag was set. Every write error is ignored so a vanished PTY cannot stop teardown.
- Sanitize the panic message with `commands::displayable` and cut it to 512 characters.

- [ ] **Step 1: Write failing tests** (in-process; no hook installed here).

```rust
// tests/m5_contained_panic.rs
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use tenuto::lifecycle::hooks::TestHook;
use tenuto::lifecycle::panic::{TakeOnceSlot, TakeResult, in_contained_job, run_contained};
use tenuto::lifecycle::terminal::TerminalCleanup;

#[test]
fn a_contained_panic_becomes_a_failed_job_and_the_flag_resets() {
    assert!(!in_contained_job());
    let failed = run_contained("artwork", || -> u32 { panic!("decoder exploded") });
    assert_eq!(failed.map_err(|e| e.label), Err("artwork"));
    assert!(!in_contained_job(), "flag restored after unwind");
    assert_eq!(run_contained("artwork", || 7).ok(), Some(7), "a later job succeeds");
}

#[test]
fn nested_containment_restores_the_outer_value() {
    let observed = run_contained("outer", || {
        let inner = run_contained("inner", || in_contained_job()).ok();
        (inner, in_contained_job())
    });
    assert_eq!(observed.ok(), Some((Some(true), true)));
    assert!(!in_contained_job());
}

struct Counted(Arc<AtomicUsize>);
impl Drop for Counted { fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); } }

#[test]
fn a_slot_is_taken_once_and_dropped_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let slot = TakeOnceSlot::new();
    assert!(slot.publish(Counted(drops.clone())).is_ok());
    assert!(matches!(slot.try_take(), TakeResult::Taken(_)));
    assert!(matches!(slot.try_take(), TakeResult::Empty));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn a_busy_slot_is_skipped_without_deadlock() {
    let slot: TakeOnceSlot<u8> = TakeOnceSlot::new();
    assert!(slot.publish(1).is_ok());
    let holder = slot.clone();
    let barrier = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let (b, r) = (barrier.clone(), release.clone());
    let thread = std::thread::spawn(move || holder.hold_for_test(|| { b.wait(); r.wait(); }));
    barrier.wait();
    assert!(matches!(slot.try_take(), TakeResult::Busy));
    release.wait();
    thread.join().expect("holder");
    assert!(matches!(slot.try_take(), TakeResult::Taken(1)));
}

#[test]
fn a_poisoned_slot_still_yields_its_value() {
    let slot: TakeOnceSlot<u8> = TakeOnceSlot::new();
    assert!(slot.publish(9).is_ok());
    let poisoner = slot.clone();
    let _ = std::thread::spawn(move || poisoner.hold_for_test(|| panic!("poison the slot"))).join();
    assert!(matches!(slot.try_take(), TakeResult::Taken(9)));
    assert!(matches!(slot.try_take(), TakeResult::Empty));
}

#[test]
fn terminal_restoration_writes_each_undo_once() {
    let cleanup = TerminalCleanup::default();
    cleanup.mark_alternate();
    cleanup.set_mouse(true);
    cleanup.mark_cursor_hidden();
    let mut first = Vec::new();
    cleanup.restore_into(&mut first);
    let text = String::from_utf8_lossy(&first);
    assert!(text.contains("\x1b[?1049l") && text.contains("\x1b[?25h") && text.contains("\x1b[?1000l"), "{text:?}");
    let mut second = Vec::new();
    cleanup.restore_into(&mut second);
    assert!(second.is_empty());
}

#[test]
fn hook_names_parse_exactly() {
    assert_eq!(TestHook::parse(Some("panic-after-terminal")), TestHook::PanicAfterTerminal);
    assert_eq!(TestHook::parse(Some("artwork-job-panic")), TestHook::ArtworkJobPanic);
    assert_eq!(TestHook::parse(Some("PANIC-AFTER-TERMINAL")), TestHook::None);
    assert_eq!(TestHook::parse(None), TestHook::None);
}
```

`TakeOnceSlot::hold_for_test(&self, f: impl FnOnce())` locks the mutex (recovering poison) and runs `f` while holding it. Mark it `#[doc(hidden)]`; it exists so busy and poisoned states are testable.

Confirm the exact mouse-disable sequence crossterm 0.29 emits (`DisableMouseCapture` writes several `?100xl` modes); assert on one it always includes.

- [ ] **Step 2: Run `cargo test --locked --test m5_contained_panic`.** Expected: FAIL, modules missing.
- [ ] **Step 3: Implement** the three modules per the rules. `install_panic_hook` is exercised by process tests in Task 29.
- [ ] **Step 4: Run the test.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add src/lifecycle tests/m5_contained_panic.rs
git commit -m "feat: contain background job panics and make terminal cleanup idempotent"
```

### Task 17: Session log, retention and fd-2 redirection

**Files:**
- Create: `src/lifecycle/stderr.rs`, `tests/m5_session_log.rs`
- Modify: `Cargo.toml`, `Cargo.lock` (`gag = "1.0.0"` for `cfg(unix)`), `src/lifecycle/mod.rs`

**Interfaces:**
- Produces:

```rust
pub const KEPT_PRIOR_LOGS: usize = 5;
pub fn log_dir(state_file: &Path) -> PathBuf;                                  // <state dir>/logs
pub fn retain_recent_logs(dir: &Path, keep: usize) -> std::io::Result<Vec<PathBuf>>;  // removes older tenuto-tui-*.log, returns removed
pub fn open_session_log(state_file: &Path, wall: OffsetDateTime) -> std::io::Result<(std::fs::File, PathBuf)>;
#[cfg(unix)] pub struct StderrRedirect(gag::Redirect<std::fs::File>);
#[cfg(unix)] pub fn redirect_stderr(file: std::fs::File) -> std::io::Result<StderrRedirect>;
```

Rules (§11): `open_session_log` creates `logs/` with the private directory policy, calls `retain_recent_logs(dir, KEPT_PRIOR_LOGS)` **before** creating the new file, then creates `tenuto-tui-<stamp>-<pid>.log` (stamp as in `store::stamp`) with `append(true).create_new(true)` and mode 0600, trying `-<pid>-2` … `-<pid>-100` on `AlreadyExists`. Retention sorts matching names ascending (stamps sort chronologically) and deletes all but the last `keep`; other files are never touched. The redirect is exercised only in subprocess tests (Task 29) because it would capture the test runner's fd 2.

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_session_log.rs
use tenuto::lifecycle::stderr::{KEPT_PRIOR_LOGS, log_dir, open_session_log, retain_recent_logs};
use time::macros::datetime;

#[test]
fn retention_keeps_the_five_most_recent_prior_logs_and_ignores_other_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    for day in 1..=8 {
        std::fs::write(dir.path().join(format!("tenuto-tui-2026090{day}T000000Z-1.log")), b"x").expect("seed");
    }
    std::fs::write(dir.path().join("notes.txt"), b"keep").expect("seed");
    let removed = retain_recent_logs(dir.path(), KEPT_PRIOR_LOGS).expect("retain");
    assert_eq!(removed.len(), 3);
    let mut left: Vec<_> = std::fs::read_dir(dir.path()).expect("dir").map(|e| e.expect("entry").file_name().into_string().expect("utf8")).collect();
    left.sort();
    assert_eq!(left.len(), 6);
    assert!(left.contains(&"notes.txt".to_string()));
    assert!(left.contains(&"tenuto-tui-20260908T000000Z-1.log".to_string()));
    assert!(!left.contains(&"tenuto-tui-20260903T000000Z-1.log".to_string()));
}

#[test]
fn each_session_gets_a_new_unique_log_under_the_profile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path().join("tenuto").join("state.json");
    let wall = datetime!(2026-09-14 12:00:00 UTC);
    let (_a, first) = open_session_log(&state, wall).expect("first");
    let (_b, second) = open_session_log(&state, wall).expect("second");
    assert_ne!(first, second);
    assert!(first.starts_with(log_dir(&state)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&first).expect("meta").permissions().mode() & 0o777, 0o600);
    }
}
```

If `time`'s `macros` feature is not enabled, build the value with `OffsetDateTime::from_unix_timestamp(1_789_387_200)` instead of adding a feature.

- [ ] **Step 2: Run `cargo test --locked --test m5_session_log`.** Expected: FAIL.
- [ ] **Step 3: Add `gag` and implement.** Confirm `gag::Redirect::stderr` in the 1.0.0 docs.
- [ ] **Step 4: Run the test.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add Cargo.toml Cargo.lock src/lifecycle tests/m5_session_log.rs
git commit -m "feat: per-session log files with retention and stderr redirection"
```

### Task 18: `tenuto tui` startup, loop and teardown

**Files:**
- Create: `src/tui/mod.rs`, `tests/support/pty.rs`, `tests/m5_tui_process.rs`
- Modify: `Cargo.toml`, `Cargo.lock` (`ratatui`, `portable-pty` dev), `src/lib.rs` (`pub mod tui;`), `src/cli.rs` (`Tui` subcommand), `src/app.rs` (dispatch; `DisabledSink` moves out), `src/persistence/writer.rs` (`pub struct DisabledSink`), `tests/support/process.rs` (`profile_env`)

**Interfaces:**
- Consumes: every lifecycle piece (Tasks 11, 12, 16, 17), `PlayerRuntime` (Task 15), `StateStore`, `WriterHandle`, `EngineHandle::spawn_for_environment`.
- Produces:

```rust
// cli.rs
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum MouseMode { #[default] On, Off }
CliCommand::Tui { #[arg(long, value_enum, default_value_t = MouseMode::On)] mouse: MouseMode }

// tui/mod.rs
pub struct TuiOptions { pub mouse: MouseMode }
pub fn run(options: TuiOptions) -> Result<RunOutcome, AppError>;

// tests/support/process.rs
pub fn profile_env(root: &Path) -> Vec<(&'static str, std::ffi::OsString)>;  // command_in uses it
// tests/support/pty.rs
pub struct PtyChild;
impl PtyChild {
    pub fn spawn(root: &Path, args: &[&str], env: &[(&str, &str)], cols: u16, rows: u16) -> std::io::Result<Self>;
    pub fn send(&mut self, bytes: &[u8]);
    pub fn wait_for(&self, needle: &str, patience: Duration) -> bool;
    pub fn output(&self) -> String;
    pub fn pid(&self) -> Option<u32>;
    pub fn wait_exit(&mut self, patience: Duration) -> Option<u32>;   // exit code
    pub fn close_master(&mut self);                                    // hang up the PTY
}
```

Startup order, exactly (§11); check `signals.requested()` between stages and jump to teardown with only the resources initialized so far:

1. `TestHook::from_env()`; build `FatalCleanup` (wake channel) and `install_panic_hook`.
2. `ShutdownSignals::install()`.
3. `hook.panic_at(PanicBeforeRedirect)`. `StateStore::platform_path()` → `ProfileLock::acquire` (contention prints the §6 message and exits nonzero before raw mode).
4. `StateStore::load()`; remember `queue_repair` and `writable`.
5. `open_session_log` → `cleanup.set_diagnostic_log(file.try_clone())` → `redirect_stderr(file)` → `cleanup.stderr_slot().publish(Box::new(redirect))` with no fallible step between the redirect and the publish. `hook.panic_at(PanicAfterRedirect)`.
6. Writer (`DisabledSink` when not writable, reusing `app.rs`'s type moved to `persistence::writer` as `pub struct DisabledSink`), library stores from `commands::platform_subscription_stores()` (`None` on error), `PlayerRuntime::new` with `engine_factory = Box::new(EngineHandle::spawn_for_environment)`. Status for a queue repair: `Queue data was reset (<fields>); backup at <path>` or `Queue data was reset (<fields>); this session is not saved`; unwritable state: `This session is not saved`.
7. Terminal: `enable_raw_mode` → `mark_raw`; `EnterAlternateScreen` → `mark_alternate`; `Hide` → `mark_cursor_hidden`; `EnableMouseCapture` when `mouse == On` → `set_mouse(true)`; build `Terminal<CrosstermBackend<Stdout>>`. Failure → `LifecycleError::Terminal` after teardown. `hook.panic_at(PanicAfterTerminal)`; `StderrProbe` spawns `sh -c 'printf tenuto-stderr-probe >&2'` (inheriting fd 2) and `eprintln!("tenuto-stderr-probe-rust")`.

Loop, wrapped in `catch_unwind(AssertUnwindSafe(..))`: poll crossterm events for up to 50 ms; `q` or Ctrl-C → `signals.request()`; `runtime.pump()`; `if signals.requested() || cleanup.fatal_requested() { break }`; draw unless `cleanup.rendering_disabled()`. This task draws only a status row (`tenuto`), the queue titles and `Queue is empty` when empty; Task 19 replaces the drawing.

Teardown (normal, signal, fatal-worker, or after a caught app-thread panic): `runtime.shutdown()` (engine and writer) → `cleanup.restore_now()` (terminal, then fd 2 via the slot) → `signals.close()` → drop the lock → print a final error or `State was not saved: <reason>` for `FlushReport::Failed`/`Unconfirmed` → for a caught app-thread panic, `resume_unwind(payload)`; for a worker fatal, return `AppError` with message `a background worker panicked`. Return `signals.outcome()` otherwise.

- [ ] **Step 1: Add dependencies.** `ratatui = { version = "0.30.2", default-features = false, features = ["crossterm_0_29", "layout-cache"] }`; dev `portable-pty = "0.9.0"`. Run `cargo tree -i crossterm --locked` after updating the lock file and confirm exactly one `crossterm 0.29` in the graph.

- [ ] **Step 2: Write the PTY helper** over `portable_pty::native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })`, `CommandBuilder::new(process::binary())` with `profile_env(root)` applied and `TENUTO_TEST_HOOK`/`TENUTO_AUDIO_OUTPUT` removed unless given in `env`, a reader thread appending to `Arc<Mutex<Vec<u8>>>`, `take_writer()` for `send`, and `Child::wait`/`try_wait` for exit codes. `close_master` drops the master and writer.

- [ ] **Step 3: Write failing process tests.**

```rust
// tests/m5_tui_process.rs
#![cfg(target_os = "linux")]
#[path = "support/process.rs"] mod process;
#[path = "support/pty.rs"] mod pty;
mod support;

use std::time::Duration;
use pty::PtyChild;

const CONTENDED: &str = "Another Tenuto player is using this state profile";
const LEAVE_ALT: &str = "\x1b[?1049l";

#[test]
fn tui_opens_idle_on_an_empty_queue_and_q_restores_the_terminal() {
    let profile = process::Profile::new().expect("profile");
    let mut child = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    assert!(child.wait_for("Queue is empty", Duration::from_secs(10)), "{}", child.output());
    child.send(b"q");
    assert_eq!(child.wait_exit(Duration::from_secs(10)), Some(0));
    assert!(child.output().contains(LEAVE_ALT));
}

#[test]
fn tui_refuses_a_held_profile_before_entering_raw_mode() {
    let profile = process::Profile::new().expect("profile");
    let _held = tenuto::lifecycle::lock::ProfileLock::acquire(&profile.state_file()).expect("hold");
    let mut child = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    let code = child.wait_exit(Duration::from_secs(10));
    assert!(matches!(code, Some(c) if c != 0));
    let output = child.output();
    assert!(output.contains(CONTENDED), "{output}");
    assert!(!output.contains("\x1b[?1049h"), "never entered the alternate screen");
}

#[test]
fn play_refuses_while_tui_holds_the_profile_and_tui_keeps_its_volume() {
    let profile = process::Profile::new().expect("profile");
    let mut tui = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    assert!(tui.wait_for("Queue is empty", Duration::from_secs(10)));
    tui.send(b"-");
    let play = profile.command()
        .args(["play", concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac")])
        .env("TENUTO_AUDIO_OUTPUT", "null").output().expect("play");
    assert!(String::from_utf8_lossy(&play.stderr).contains(CONTENDED));
    tui.send(b"q");
    assert_eq!(tui.wait_exit(Duration::from_secs(10)), Some(0));
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(profile.state_file()).expect("flushed")).expect("json");
    assert_eq!(state["volume"], 0.95);
}

#[test]
fn sigterm_restores_the_terminal_and_exits_143() {
    let profile = process::Profile::new().expect("profile");
    let mut child = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    assert!(child.wait_for("Queue is empty", Duration::from_secs(10)));
    let pid = child.pid().expect("pid");
    assert!(std::process::Command::new("kill").args(["-TERM", &pid.to_string()]).status().expect("kill").success());
    assert_eq!(child.wait_exit(Duration::from_secs(10)), Some(143));
    assert!(child.output().contains(LEAVE_ALT));
}
```

In this task, `-` maps directly to `AppCommand::AdjustVolume(-0.05)` inside the minimal loop; Task 20 replaces the key handling with the full map.

- [ ] **Step 4: Run `cargo test --locked --test m5_tui_process`.** Expected: FAIL, unrecognized subcommand `tui`.
- [ ] **Step 5: Implement** `tui::run` with the order above and the CLI dispatch in `app::run`.
- [ ] **Step 6: Run.** `cargo test --locked --test m5_tui_process --test m5_lock_process --test cli` → PASS.
- [ ] **Step 7: Commit.**

```bash
git add Cargo.toml Cargo.lock src tests/support/process.rs tests/support/pty.rs tests/m5_tui_process.rs
git commit -m "feat: start tenuto tui in the specified lifecycle order"
```

### Task 19: Layout tiers and rendering

**Files:**
- Create: `src/tui/theme.rs`, `src/tui/layout.rs`, `src/tui/render.rs`, `src/tui/state.rs`, `tests/m5_tui_render.rs`
- Modify: `src/tui/mod.rs` (draw through `render::draw`)

**Interfaces:**
- Consumes: `PlayerView`, `NowPlaying`, `QueueRow`, `SavedHistory`, `format_saved` (Task 15); `media::display::format_hms` (Task 13).
- Produces:

```rust
// theme.rs — reference palette (§7)
pub struct Theme { pub background: Color, pub panel: Color, pub line: Color, pub muted: Color, pub text: Color,
                   pub cream: Color, pub green: Color, pub amber: Color, pub cyan: Color }
impl Default for Theme  // #10181a, #191e21, #344145, #93a19f, #dce5df, #e7eddf, #b3d89c, #e7b478, #8bbeb6 as Color::Rgb

// layout.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum Tier { Resize, Minimal, Compact, Normal }
pub fn tier_for(width: u16, height: u16) -> Tier;
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Regions { pub status: Rect, pub cover: Option<Rect>, pub info: Rect, pub spectrum: Option<Rect>,
                     pub transport: Rect, pub progress: Rect, pub queue: Rect, pub footer: Rect }
pub fn regions(area: Rect, tier: Tier) -> Regions;

// state.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum Overlay { None, Help, ConfirmClear, Input, Browser }
#[derive(Clone, Debug)]
pub struct UiState { pub selected: Option<QueueEntryId>, pub overlay: Overlay, pub mouse_capture: bool, pub input: String, pub queue_offset: usize }
impl UiState { pub fn new(mouse_capture: bool) -> Self; pub fn reconcile(&mut self, view: &PlayerView, hint: Option<QueueEntryId>); }

// render.rs
#[derive(Default)] pub struct Visuals<'a> { pub cover: CoverView<'a>, pub spectrum: Option<&'a [f32]> }
#[derive(Default)] pub enum CoverView<'a> { #[default] Placeholder, Image(&'a dyn CoverWidget) }
pub trait CoverWidget { fn render_cover(&self, area: Rect, buffer: &mut Buffer); }
pub fn draw(frame: &mut Frame<'_>, view: &PlayerView, ui: &UiState, visuals: &Visuals<'_>) -> HitMap;
#[derive(Clone, Debug, Default)]
pub struct HitMap { pub rows: Vec<(Rect, QueueEntryId)>, pub queue: Rect, pub progress: Rect,
                    pub buttons: Vec<(Rect, TransportButton)> }
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum TransportButton { Previous, PlayPause, Stop, Next }
```

Rendering rules:
- `tier_for`: `w < 30 || h < 8` → Resize; `w < 50 || h < 18` → Minimal; `w < 80 || h < 28` → Compact; else Normal. `regions` never panics for any `Rect` including zero size (use saturating arithmetic; empty rects are fine).
- **Normal**: status row 1; player block 9 rows high (bordered) with cover `Rect` of 7 rows × 14 columns on the left (square in pixels on typical 1:2 cells); inside the info column: title (cream), artist · album (muted), a 3-row spectrum, transport row, progress row; queue fills the rest (bordered table); footer row 1.
- **Compact**: player 6 rows; cover 4 rows × 8 columns; spectrum 1 row; no artist/album line; single-line queue rows.
- **Minimal**: no cover, no spectrum; player 3 rows (title; state + progress; transport); queue; footer.
- **Resize**: a centered `Terminal too small (need 30×8)` and a last line `space play · q quit`.
- Status row: `tenuto` in green on the left; right side `vol NN%`, `mouse on|off`, and `unsaved` when `persistence == Unsaved`.
- Progress: `mm:ss / mm:ss` via `format_hms` trimmed to `mm:ss` under one hour; unknown duration shows `--:--` and a bar without a filled ratio; an estimated position prefixes `~`; a `Declared` duration is shown in parentheses and never fills the bar; `buffering` appends ` buffering` to the state label; before loading, the progress row shows the saved label (`format_saved`) instead of a position.
- Queue: a playing marker column (`▶` for the active entry, blank otherwise) separate from the selected-row highlight (reversed green background); columns title, duration, saved label. Empty queue: `Queue is empty — press b to browse or a to add`.
- Footer: status message when present, else `space play · enter play selected · b browse · a add · ? help · q quit`.
- Every string reaching the buffer is already sanitized by the view; the renderer never formats raw media identifiers.
- The cover area draws a stable character-block placeholder (`░` fill with a centered `♪`) unless `CoverView::Image`.
- The spectrum area draws flat bars when `visuals.spectrum` is `None` and `▁▂▃▄▅▆▇█` scaled levels otherwise, alternating green and cyan.

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_tui_render.rs
use std::time::Duration;

use tenuto::application::transport::PlaybackPhase;
use tenuto::application::view::{NowPlaying, PersistenceStatus, PlayerView, QueueRow, SavedHistory};
use tenuto::playback::state::PlaybackState;
use tenuto::playback::volume::Volume;
use tenuto::queue::{DisplayDuration, DurationSource};
use tenuto::tui::layout::{Tier, regions, tier_for};
use tenuto::tui::render::{Visuals, draw};
use tenuto::tui::state::UiState;
use ratatui::{Terminal, backend::TestBackend, layout::Rect};

fn text(view: &PlayerView, ui: &UiState, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("backend");
    terminal.draw(|frame| { draw(frame, view, ui, &Visuals::default()); }).expect("draw");
    let buffer = terminal.backend().buffer().clone();
    buffer.content().chunks(usize::from(w.max(1))).map(|row| row.iter().map(|c| c.symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
}

fn ids() -> Vec<tenuto::queue::QueueEntryId> {
    let mut queue = tenuto::queue::Queue::default();
    let entries = ["a", "b", "c"].iter().map(|n| {
        let path = tenuto::media::id::AbsolutePath::new(format!("/music/{n}.flac").into()).expect("abs");
        tenuto::queue::NewQueueEntry::new(tenuto::media::id::MediaId::LocalFile(path.clone()),
            tenuto::queue::QueueSource::LocalFile(path), Default::default()).expect("entry")
    }).collect();
    queue.enqueue(entries).expect("fits")
}

fn view(phase: PlaybackPhase, now: Option<NowPlaying>) -> PlayerView {
    let ids = ids();
    PlayerView {
        rows: vec![
            QueueRow { id: ids[0], title: "Morning Tide".into(), subtitle: Some("Harbor".into()), duration: Some(DisplayDuration { value: Duration::from_secs(185), source: DurationSource::Decoded(Default::default()) }), saved: None },
            QueueRow { id: ids[1], title: "Long Episode".into(), subtitle: None, duration: Some(DisplayDuration { value: Duration::from_secs(3600), source: DurationSource::Declared }), saved: Some(SavedHistory::Position { at: Duration::from_secs(62), estimated: true }) },
            QueueRow { id: ids[2], title: "Done".into(), subtitle: None, duration: None, saved: Some(SavedHistory::Played) },
        ],
        active: now.as_ref().and_then(|n| n.entry), now_playing: now, phase, volume: Volume::new(0.8),
        status: None, persistence: PersistenceStatus::Saving, last_requested: None,
    }
}

fn playing(entry: tenuto::queue::QueueEntryId, loaded: bool, duration: Option<DisplayDuration>, estimated: bool) -> NowPlaying {
    NowPlaying { entry: Some(entry), title: "Morning Tide".into(), artist: Some("Harbor".into()), album: Some("Coast".into()),
        loaded, state: if loaded { PlaybackState::Playing } else { PlaybackState::Idle }, position: Duration::from_secs(62),
        duration, estimated_position: estimated, degraded: false, buffering: false, seek: None,
        saved: Some(SavedHistory::Position { at: Duration::from_secs(62), estimated: false }), session_rev: 1, load: None }
}

#[test]
fn tiers_follow_the_smallest_matching_dimension() {
    assert_eq!(tier_for(100, 20), Tier::Compact);
    assert_eq!(tier_for(60, 40), Tier::Compact);
    assert_eq!(tier_for(80, 28), Tier::Normal);
    assert_eq!(tier_for(49, 40), Tier::Minimal);
    assert_eq!(tier_for(120, 17), Tier::Minimal);
    assert_eq!(tier_for(29, 40), Tier::Resize);
    assert_eq!(tier_for(100, 7), Tier::Resize);
}

#[test]
fn regions_are_valid_for_zero_and_tiny_areas() {
    for tier in [Tier::Resize, Tier::Minimal, Tier::Compact, Tier::Normal] {
        let _ = regions(Rect::new(0, 0, 0, 0), tier);
        let _ = regions(Rect::new(0, 0, 1, 1), tier);
    }
    let ui = UiState::new(true);
    let _ = text(&view(PlaybackPhase::Unloaded, None), &ui, 1, 1);
}

#[test]
fn normal_layout_shows_cover_metadata_and_distinct_playing_and_selected_rows() {
    let v = view(PlaybackPhase::Playing, Some(playing(ids()[0], true, Some(DisplayDuration { value: Duration::from_secs(185), source: DurationSource::Decoded(Default::default()) }), false)));
    let mut ui = UiState::new(true);
    ui.selected = Some(v.rows[2].id);
    let screen = text(&v, &ui, 100, 30);
    assert!(screen.contains("Harbor"), "normal shows secondary metadata\n{screen}");
    assert!(screen.contains("01:02 / 03:05"));
    let playing_line = screen.lines().find(|l| l.contains("Morning Tide") && l.contains('▶')).expect("playing marker");
    assert!(!playing_line.contains("Done"));
    assert!(screen.contains("(1:00:00)"), "declared duration is parenthesized");
    assert!(screen.contains("~01:02 saved") && screen.contains("played"));
}

#[test]
fn compact_drops_secondary_metadata_at_mixed_dimensions() {
    let v = view(PlaybackPhase::Playing, Some(playing(ids()[0], true, None, false)));
    for (w, h) in [(100, 20), (60, 40)] {
        let screen = text(&v, &UiState::new(true), w, h);
        assert!(!screen.contains("Coast"), "{w}x{h}\n{screen}");
        assert!(screen.contains("Morning Tide"));
    }
}

#[test]
fn minimal_hides_cover_and_spectrum_but_keeps_title_state_and_queue() {
    let v = view(PlaybackPhase::Playing, Some(playing(ids()[0], true, None, false)));
    let screen = text(&v, &UiState::new(true), 45, 16);
    assert!(screen.contains("Morning Tide") && screen.contains("playing") && screen.contains("Done"));
    assert!(!screen.contains('░'));
}

#[test]
fn the_resize_tier_keeps_quit_and_play_hints() {
    let screen = text(&view(PlaybackPhase::Unloaded, None), &UiState::new(true), 25, 6);
    assert!(screen.contains("q quit") && screen.contains("space play"));
}

#[test]
fn unknown_duration_and_estimated_position_are_honest() {
    let v = view(PlaybackPhase::Playing, Some(playing(ids()[0], true, None, true)));
    let screen = text(&v, &UiState::new(true), 100, 30);
    assert!(screen.contains("~01:02 / --:--"), "{screen}");
}

#[test]
fn a_saved_but_unloaded_entry_shows_saved_history_not_live_progress() {
    let v = view(PlaybackPhase::Unloaded, Some(playing(ids()[0], false, None, false)));
    let screen = text(&v, &UiState::new(true), 100, 30);
    assert!(screen.contains("01:02 saved"));
    assert!(!screen.contains("01:02 / "));
}

#[test]
fn empty_loading_and_failed_screens() {
    let mut empty = view(PlaybackPhase::Unloaded, None);
    empty.rows.clear();
    assert!(text(&empty, &UiState::new(true), 100, 30).contains("Queue is empty"));
    let loading = view(PlaybackPhase::Loading, None);
    assert!(text(&loading, &UiState::new(true), 100, 30).contains("loading"));
    let mut failed = view(PlaybackPhase::LoadFailed, None);
    failed.status = Some("cannot open media \"/x.flac\"".into());
    assert!(text(&failed, &UiState::new(false), 100, 30).contains("cannot open media"));
    assert!(text(&failed, &UiState::new(false), 100, 30).contains("mouse off"));
}
```

- [ ] **Step 2: Run `cargo test --locked --test m5_tui_render`.** Expected: FAIL, modules missing.
- [ ] **Step 3: Implement** `theme`, `layout`, `state`, `render` per the rules; wire `tui::run` to call `render::draw` and keep the returned `HitMap` for Task 21.
- [ ] **Step 4: Run.** `cargo test --locked --test m5_tui_render --test m5_tui_process` → PASS.
- [ ] **Step 5: Commit.**

```bash
git add src/tui tests/m5_tui_render.rs
git commit -m "feat: render the compact player in four size tiers"
```

### Task 20: Keyboard map, overlays and redraw

**Files:**
- Create: `src/tui/input.rs`, `tests/m5_tui_input.rs`
- Modify: `src/tui/mod.rs`, `src/tui/render.rs` (help and confirm overlays, input line)

**Interfaces:**
- Consumes: `UiState`, `Overlay` (Task 19); `AppCommand`, `EnqueueItem` (Task 15).
- Produces:

```rust
#[derive(Clone, Debug)]
pub enum Effect { App(AppCommand), Quit, SetMouseCapture(bool), FullRedraw, OpenBrowser, CloseBrowser }
pub fn handle_key(key: KeyEvent, ui: &mut UiState, view: &PlayerView) -> Vec<Effect>;
```

Rules (§7 table), for `KeyEventKind::Press` only:
- **Always first:** Ctrl-C → `Quit`, even while typing.
- **`Overlay::Input`:** printable characters append to `ui.input` (so `q`, space, `+` never act as shortcuts); Backspace pops; Enter → `App(Enqueue(vec![EnqueueItem::from_input(trimmed)]))` when nonempty, then closes and clears; Esc cancels and clears.
- **`Overlay::ConfirmClear`:** `y` → `App(ClearQueue)` and close; any other key closes without effect.
- **`Overlay::Help`:** `?` or Esc closes; other keys are ignored.
- **`Overlay::Browser`:** keys go to the browser (Task 22); `b` or Esc → `CloseBrowser`.
- **No overlay:** Space → `PlayPause { selected }`; Enter → `PlayEntry(selected)` when a row is selected; Up/`k` and Down/`j` move `ui.selected` (no app command); `K`/`J` → `Move(selected, Up|Down)`; Left/Right → `SeekBy(-10 | 10)`; Home → `Restart`; `-`/`_` → `AdjustVolume(-0.05)`; `+`/`=` → `AdjustVolume(0.05)`; `s` → `Stop`; `p` → `Play { selected }`; `[`/`]` → `Previous|Next { selected }`; `d` → `Remove(selected)`; `b` → `OpenBrowser`; `a` → open input; `c` → open confirm; `?` → open help; `m` → toggle `ui.mouse_capture` and `SetMouseCapture(new)`; Ctrl-L → `FullRedraw`; Esc → nothing; `q` → `Quit`.
- In `tui::run`: `SetMouseCapture` executes `EnableMouseCapture`/`DisableMouseCapture` and updates `TerminalCleanup::set_mouse`; `FullRedraw` calls `terminal.clear()` and invalidates prepared images (Task 25 hooks in); after every `App` effect, call `ui.reconcile(&runtime.view(), runtime.take_selection_hint())`.
- Confirm overlay text: `Clear the queue? Listening history is kept. y to confirm`. Help overlay lists the §7 table.

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_tui_input.rs
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use tenuto::application::runtime::{AppCommand, EnqueueItem};
use tenuto::tui::input::{Effect, handle_key};
use tenuto::tui::state::{Overlay, UiState};

// `sample_view()` builds a PlayerView with three rows exactly as in tests/m5_tui_render.rs.

fn key(code: KeyCode) -> KeyEvent { KeyEvent::new(code, KeyModifiers::NONE) }
fn ctrl(c: char) -> KeyEvent { KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL) }

fn app(effects: &[Effect]) -> Vec<&AppCommand> {
    effects.iter().filter_map(|e| match e { Effect::App(c) => Some(c), _ => None }).collect()
}

#[test]
fn selection_moves_without_touching_playback() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    ui.reconcile(&view, None);
    assert_eq!(ui.selected, Some(view.rows[0].id), "default selection is the first row");
    let effects = handle_key(key(KeyCode::Down), &mut ui, &view);
    assert!(app(&effects).is_empty());
    assert_eq!(ui.selected, Some(view.rows[1].id));
    handle_key(key(KeyCode::Char('k')), &mut ui, &view);
    assert_eq!(ui.selected, Some(view.rows[0].id));
}

#[test]
fn transport_keys_carry_the_selection_as_an_argument() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    ui.selected = Some(view.rows[2].id);
    let selected = ui.selected;
    assert!(matches!(app(&handle_key(key(KeyCode::Char(' ')), &mut ui, &view))[..], [AppCommand::PlayPause { selected: s }] if *s == selected));
    assert!(matches!(app(&handle_key(key(KeyCode::Enter), &mut ui, &view))[..], [AppCommand::PlayEntry(id)] if Some(*id) == selected));
    assert!(matches!(app(&handle_key(key(KeyCode::Char('p')), &mut ui, &view))[..], [AppCommand::Play { .. }]));
    assert!(matches!(app(&handle_key(key(KeyCode::Left), &mut ui, &view))[..], [AppCommand::SeekBy(-10)]));
    assert!(matches!(app(&handle_key(key(KeyCode::Home), &mut ui, &view))[..], [AppCommand::Restart]));
    assert!(matches!(app(&handle_key(key(KeyCode::Char(']')), &mut ui, &view))[..], [AppCommand::Next { .. }]));
    assert!(matches!(app(&handle_key(key(KeyCode::Char('J')), &mut ui, &view))[..], [AppCommand::Move(_, tenuto::queue::Direction::Down)]));
}

#[test]
fn both_spellings_of_each_volume_key_work() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    for (code, delta) in [('-', -0.05), ('_', -0.05), ('+', 0.05), ('=', 0.05)] {
        let effects = handle_key(key(KeyCode::Char(code)), &mut ui, &view);
        assert!(matches!(app(&effects)[..], [AppCommand::AdjustVolume(d)] if (*d - delta).abs() < f32::EPSILON), "{code}");
    }
}

#[test]
fn typing_a_url_never_triggers_shortcuts_and_enter_enqueues_it() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    handle_key(key(KeyCode::Char('a')), &mut ui, &view);
    assert_eq!(ui.overlay, Overlay::Input);
    for c in "https://q.example/ p+.mp3".chars() {
        assert!(handle_key(key(KeyCode::Char(c)), &mut ui, &view).is_empty(), "{c}");
    }
    let effects = handle_key(key(KeyCode::Enter), &mut ui, &view);
    assert!(matches!(app(&effects)[..], [AppCommand::Enqueue(items)] if matches!(&items[..], [EnqueueItem::Url(u)] if u == "https://q.example/ p+.mp3")));
    assert_eq!(ui.overlay, Overlay::None);
}

#[test]
fn ctrl_c_quits_even_while_typing_and_esc_cancels_input() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    handle_key(key(KeyCode::Char('a')), &mut ui, &view);
    handle_key(key(KeyCode::Char('x')), &mut ui, &view);
    handle_key(key(KeyCode::Esc), &mut ui, &view);
    assert_eq!((ui.overlay, ui.input.as_str()), (Overlay::None, ""));
    handle_key(key(KeyCode::Char('a')), &mut ui, &view);
    assert!(matches!(handle_key(ctrl('c'), &mut ui, &view)[..], [Effect::Quit]));
}

#[test]
fn clearing_requires_confirmation() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    handle_key(key(KeyCode::Char('c')), &mut ui, &view);
    assert!(app(&handle_key(key(KeyCode::Char('n')), &mut ui, &view)).is_empty());
    handle_key(key(KeyCode::Char('c')), &mut ui, &view);
    assert!(matches!(app(&handle_key(key(KeyCode::Char('y')), &mut ui, &view))[..], [AppCommand::ClearQueue]));
}

#[test]
fn mouse_toggle_and_full_redraw() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    assert!(matches!(handle_key(key(KeyCode::Char('m')), &mut ui, &view)[..], [Effect::SetMouseCapture(false)]));
    assert!(!ui.mouse_capture);
    assert!(matches!(handle_key(ctrl('l'), &mut ui, &view)[..], [Effect::FullRedraw]));
    let release = KeyEvent { code: KeyCode::Char('q'), modifiers: KeyModifiers::NONE, kind: KeyEventKind::Release, state: KeyEventState::NONE };
    assert!(handle_key(release, &mut ui, &view).is_empty());
}
```

- [ ] **Step 2: Run `cargo test --locked --test m5_tui_input`.** Expected: FAIL.
- [ ] **Step 3: Implement** `handle_key`, the overlays, and the effects in `tui::run` (replacing Task 18's temporary key handling).
- [ ] **Step 4: Run.** `cargo test --locked --test m5_tui_input --test m5_tui_render --test m5_tui_process` → PASS.
- [ ] **Step 5: Commit.**

```bash
git add src/tui tests/m5_tui_input.rs
git commit -m "feat: map the keyboard, overlays and full redraw"
```

### Task 21: Mouse selection, activation, scrolling and transport hits

**Files:**
- Modify: `src/tui/input.rs`, `src/tui/render.rs` (populate `HitMap`), `src/tui/mod.rs` (`--mouse off`, mouse events)
- Test: `tests/m5_tui_input.rs`

**Interfaces:**
- Consumes: `HitMap`, `TransportButton` (Task 19).
- Produces: `pub fn handle_mouse(event: MouseEvent, hits: &HitMap, ui: &mut UiState, view: &PlayerView) -> Vec<Effect>`.

Rules: ignored entirely when `ui.mouse_capture` is false. Left click on a row selects it; a click on the already-selected row → `App(PlayEntry(id))`. Scroll up/down inside `hits.queue` moves the selection by one. Left click on a transport button → `Previous|Next { selected }`, `PlayPause { selected }`, `Stop`. Left click on `hits.progress` → `App(SeekTo(fraction × duration))` only when `now_playing` is loaded with a `Decoded` duration; otherwise nothing (the runtime's `SeekTo` path still applies its own capability checks). `--mouse off` starts with capture disabled and never sends enable sequences.

- [ ] **Step 1: Write failing tests** appended to `tests/m5_tui_input.rs`: build a `HitMap` by drawing `sample_view()` into a 100×30 `TestBackend` with `render::draw`, then send `MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column, row, modifiers: KeyModifiers::NONE }` at the centre of each rect:
  1. click row 2 selects it with no app command; click it again → `PlayEntry(row 2)`;
  2. `ScrollDown` inside the queue moves the selection down;
  3. click `PlayPause` → `PlayPause { selected }`;
  4. click the middle of `progress` with a loaded 10 s decoded duration → `SeekTo` between 4 s and 6 s; with `duration: None` → no effect;
  5. with `ui.mouse_capture = false`, every event → empty.
- [ ] **Step 2: Run `cargo test --locked --test m5_tui_input`.** Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the test.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add src/tui tests/m5_tui_input.rs
git commit -m "feat: mouse selection, activation, scrolling and transport"
```

### Task 22: On-demand browser and path/URL input

**Files:**
- Create: `src/application/browse.rs`, `src/tui/browser.rs`, `tests/m5_browser.rs`, `tests/m5_no_network.rs`
- Modify: `src/application/mod.rs`, `src/tui/mod.rs`, `src/tui/input.rs` (browser key routing), `src/tui/render.rs` (browser overlay)

**Interfaces:**
- Consumes: `library::{list_feeds, episode_candidates}` (Task 10), `LibraryStores`, `EnqueueItem`.
- Produces:

```rust
// application/browse.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum EntryKind { Directory, Audio, Other }
#[derive(Clone, Debug, Eq, PartialEq)] pub struct DirEntry { pub name: String, pub path: PathBuf, pub kind: EntryKind }
pub fn list_directory(path: &Path) -> std::io::Result<Vec<DirEntry>>;   // one level; directories first, then case-insensitive name
pub enum BrowseRequest { Directory(PathBuf), Feeds, Episodes { slug: String } }
pub enum BrowseResult {
    Directory { path: PathBuf, entries: Result<Vec<DirEntry>, String> },
    Feeds(Result<Vec<FeedSummary>, String>),
    Episodes { slug: String, episodes: Result<Vec<EpisodeCandidate>, String> },
}
pub struct BrowseWorker;  // spawn(library: Option<LibraryStores>) -> Self; request(&self, BrowseRequest); try_result(&self) -> Option<BrowseResult>

// tui/browser.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)] pub enum BrowserTab { Files, Podcasts }
pub struct BrowserState { pub tab: BrowserTab, pub cwd: PathBuf, pub entries: Vec<DirEntry>, pub feeds: Vec<FeedSummary>,
                          pub episodes: Option<(String, Vec<EpisodeCandidate>)>, pub cursor: usize, pub marked: BTreeSet<usize>, pub loading: bool }
pub enum BrowserEffect { Request(BrowseRequest), Enqueue(Vec<EnqueueItem>), Close }
impl BrowserState { pub fn new(cwd: PathBuf) -> Self; pub fn apply(&mut self, result: BrowseResult); pub fn handle_key(&mut self, key: KeyEvent) -> Vec<BrowserEffect>; }
```

Rules (§8):
- Audio extensions (case-insensitive): `mp3`, `flac`, `wav`, `m4a`. Unreadable directories return an error string, never a panic. No recursion, no metadata probing in the listing.
- The worker thread owns its own `LibraryStores` (built by `tui::run` from `commands::platform_subscription_stores()`) and never constructs an `HttpService`. Opening the browser or listing episodes never refreshes a feed.
- Keys in the browser: Up/Down/`j`/`k` move; Tab switches tabs (Podcasts requests `Feeds`); Enter on a directory requests it; Enter on a feed requests its `Episodes`; Enter on an audio file or an episode enqueues the marked items (or the item under the cursor if none are marked) as `EnqueueItem::Path`/`EnqueueItem::Episode`; Space toggles a mark on enqueueable rows; Backspace/Left goes to the parent directory or back to the feed list; `b`/Esc closes. Episodes without an enclosure are shown dimmed and cannot be marked.
- `b` opens the browser at the directory of the active local entry, else the current working directory.

- [ ] **Step 1: Write failing tests.**
  - `tests/m5_browser.rs`:
    1. `a_listing_is_one_level_directories_first_and_classifies_audio`: tempdir with `b.MP3`, `a.flac`, `z/` (containing `deep.flac`), `notes.txt` → `[z (Directory), a.flac (Audio), b.MP3 (Audio), notes.txt (Other)]`; `deep.flac` absent.
    2. `an_unreadable_directory_is_an_error_value`: `list_directory` on a missing path → `Err`.
    3. `marked_files_enqueue_together_in_listing_order`: `BrowserState::apply(Directory { .. })` with the listing above, Down, Space, Down, Space, Enter → `[Enqueue([Path(a.flac), Path(b.MP3)])]`.
    4. `enter_on_a_directory_requests_it_and_backspace_returns_to_the_parent`.
    5. `podcasts_tab_lists_feeds_then_episodes_and_skips_unplayable_marks`.
  - `tests/m5_no_network.rs` (loopback `TestServer` counts requests):
    1. `restoring_enqueueing_and_browsing_remote_entries_make_no_requests`: start a server; build a `PlayerRuntime` (Task 15 rig, `library: None`) whose restored state has a remote entry for `server.url("/a.mp3")` and a podcast entry with fallback `server.url("/ep.mp3")`; `handle(Enqueue(vec![Url(server.url("/b.mp3"))]))`; `view()`; `pump()` for 300 ms; also `BrowseWorker` `Episodes` listing through a feeds `Rig` whose enclosures point at the server → `server.requests()` is empty.
    2. `only_an_explicit_play_prepares_the_remote_source`: `PlayEntry(remote)` → within 5 s `server.requests()` is nonempty.
- [ ] **Step 2: Run `cargo test --locked --test m5_browser --test m5_no_network`.** Expected: FAIL.
- [ ] **Step 3: Implement** the worker, browser state and overlay rendering, and route keys from `input.rs` when `Overlay::Browser`.
- [ ] **Step 4: Run.** `cargo test --locked --test m5_browser --test m5_no_network --test m5_tui_input` → PASS.
- [ ] **Step 5: Commit.**

```bash
git add src/application src/tui tests/m5_browser.rs tests/m5_no_network.rs
git commit -m "feat: browse local directories and cached episodes on demand"
```

### Task 23: Local tag probe and background metadata enrichment

**Files:**
- Modify: `Cargo.toml`, `Cargo.lock` (`image = { version = "0.25.10", default-features = false, features = ["jpeg", "png"] }`)
- Create: `src/media/tags.rs`, `src/application/enrich.rs`, `tests/support/tagged_flac.rs`, `tests/m5_metadata.rs`
- Modify: `src/media/mod.rs`, `src/media/metadata.rs` (`artist`, `album`), `src/playback/decode.rs` (extract artist/album), `src/session.rs` (`absorb_load_metadata` copies artist/album), `src/application/runtime.rs` (request enrichment, apply results), every `MediaMetadata { .. }` literal (`src/app.rs`, `src/playback/engine.rs` tests, `tests/domain_values.rs`)

**Interfaces:**
- Consumes: `run_contained`, `TestHook` (Task 16); `Session::update_display` (Task 9).
- Produces:

```rust
// media/metadata.rs — two new fields
pub artist: Option<String>, pub album: Option<String>,

// media/tags.rs
pub const MAX_EMBEDDED_COVER_BYTES: usize = 10 * 1024 * 1024;
#[derive(Clone, Debug, Eq, PartialEq)] pub struct CoverBytes { pub data: Vec<u8>, pub media_type: Option<String> }
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LocalTags { pub title: Option<String>, pub artist: Option<String>, pub album: Option<String>,
                       pub duration: Option<Duration>, pub duration_provenance: PositionProvenance,
                       pub front_cover: Option<CoverBytes>, pub cover_oversized: bool }
pub fn probe_local_tags(path: &AbsolutePath) -> Result<LocalTags, PlaybackError>;

// application/enrich.rs
pub type TagProbe = Arc<dyn Fn(&AbsolutePath) -> Result<LocalTags, PlaybackError> + Send + Sync>;
pub enum EnrichOutcome { Tags(LocalTags), Failed(String), Panicked }
pub struct EnrichResult { pub media: MediaId, pub generation: u64, pub outcome: EnrichOutcome }
pub struct MetadataWorkers;   // spawn(workers: usize, probe: TagProbe, hook: TestHook) -> Self; request(&self, media: MediaId, path: AbsolutePath); cancel_all(&self); try_result(&self) -> Option<EnrichResult>
pub fn default_probe(hook: TestHook) -> TagProbe;   // probe_local_tags, panicking once first when hook == MetadataJobPanic
```

Rules (§8, §9, §11):
- `probe_local_tags` opens the file and runs symphonia's probe only (no decoder, no audio device), reads `metadata().current()`: `StandardTag::TrackTitle`, `Artist`, `Album`; duration and provenance exactly as `DecodedSource::from_media_source` computes them (share a helper); the first visual with `usage == Some(StandardVisualKey::FrontCover)` becomes `front_cover` if its data is at most `MAX_EMBEDDED_COVER_BYTES`, else `cover_oversized = true`.
- `MetadataWorkers::spawn(2, probe, hook)` starts exactly `workers` threads named `tenuto-metadata-N` reading a bounded channel (capacity 256). Each job runs inside `run_contained("metadata", ..)`; a panic yields `Panicked` and the worker keeps serving. After a job returns normally the worker logs `tracing::debug!("metadata job completed")`. Before its first job, **outside** the contained boundary, the worker calls `hook.panic_at(TestHook::WorkerPanic)` — the uncontained worker panic Task 29 exercises. Tests below pass `TestHook::None`. `cancel_all` increments a shared generation and drains the channel; results from an older generation are discarded by `try_result`.
- The runtime requests enrichment for `LocalFile` entries whose display title is `None`, after `Enqueue` and once at construction for restored entries; results call `Session::update_display` (title/artist/album only when `Some`; duration as `Decoded(provenance)`). `URL` and podcast entries are never probed. Enrichment for a removed media is harmless because `update_display` finds no entry.
- `tests/support/tagged_flac.rs`: `pub fn tagged_flac(dir: &Path, title: &str, artist: &str, album: &str, cover_png: Option<&[u8]>) -> PathBuf` copies `tests/fixtures/sine.flac`, walks its metadata blocks (4-byte header: last-flag bit + 7-bit type, 24-bit big-endian length), clears the last flag on the final block, and appends a `VORBIS_COMMENT` block (type 4: little-endian vendor length + vendor, count, `TITLE=`, `ARTIST=`, `ALBUM=` entries) and, when given, a `PICTURE` block (type 6: big-endian picture type 3, MIME `image/png`, empty description, width, height, depth 32, colors 0, data length, data), marking the new final block as last.

- [ ] **Step 1: Add `image` with the exact dependency line above and update only its required lockfile entries.** `cargo check --locked` must resolve it before the metadata test starts; the test encodes its PNG fixture in this task.

- [ ] **Step 2: Write failing tests.**

```rust
// tests/m5_metadata.rs
#[path = "support/tagged_flac.rs"] mod tagged_flac;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tenuto::application::enrich::{EnrichOutcome, MetadataWorkers, TagProbe};
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::media::tags::{LocalTags, probe_local_tags};

fn png_2x2() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 100, 50, 255]))
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).expect("png");
    bytes
}

#[test]
fn tags_and_the_front_cover_are_read_without_decoding() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = tagged_flac::tagged_flac(dir.path(), "Morning Tide", "Harbor", "Coast", Some(&png_2x2()));
    let tags = probe_local_tags(&AbsolutePath::new(path).expect("abs")).expect("probe");
    assert_eq!((tags.title.as_deref(), tags.artist.as_deref(), tags.album.as_deref()), (Some("Morning Tide"), Some("Harbor"), Some("Coast")));
    assert_eq!(tags.duration, Some(Duration::from_millis(500)));
    assert!(tags.front_cover.is_some_and(|c| c.data.starts_with(b"\x89PNG")));
}

#[test]
fn an_untagged_file_has_no_names_but_a_duration() {
    let path = AbsolutePath::new(std::fs::canonicalize(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac")).expect("fixture")).expect("abs");
    let tags = probe_local_tags(&path).expect("probe");
    assert_eq!(tags.title, None);
    assert_eq!(tags.duration, Some(Duration::from_millis(500)));
}

fn next_result(workers: &MetadataWorkers) -> tenuto::application::enrich::EnrichResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(result) = workers.try_result() { return result; }
        assert!(Instant::now() < deadline, "no enrichment result");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_panicking_probe_is_contained_and_the_worker_serves_the_next_job() {
    let panicked_once = Arc::new(AtomicBool::new(false));
    let flag = panicked_once.clone();
    let probe: TagProbe = Arc::new(move |_path| {
        if !flag.swap(true, Ordering::SeqCst) { panic!("injected decoder panic"); }
        assert!(tenuto::lifecycle::panic::in_contained_job());
        Ok(LocalTags { title: Some("ok".into()), ..LocalTags::default() })
    });
    let workers = MetadataWorkers::spawn(1, probe, tenuto::lifecycle::hooks::TestHook::None);
    let path = AbsolutePath::new("/music/a.flac".into()).expect("abs");
    workers.request(MediaId::LocalFile(path.clone()), path.clone());
    assert!(matches!(next_result(&workers).outcome, EnrichOutcome::Panicked));
    workers.request(MediaId::LocalFile(path.clone()), path);
    assert!(matches!(next_result(&workers).outcome, EnrichOutcome::Tags(t) if t.title.as_deref() == Some("ok")));
}

#[test]
fn cancelled_results_are_discarded() {
    let probe: TagProbe = Arc::new(|_path| { std::thread::sleep(Duration::from_millis(200)); Ok(LocalTags::default()) });
    let workers = MetadataWorkers::spawn(2, probe, tenuto::lifecycle::hooks::TestHook::None);
    let path = AbsolutePath::new("/music/a.flac".into()).expect("abs");
    workers.request(MediaId::LocalFile(path.clone()), path);
    workers.cancel_all();
    std::thread::sleep(Duration::from_millis(400));
    assert!(workers.try_result().is_none());
}
```

Add one runtime test to `tests/m5_runtime.rs`: `enqueueing_a_tagged_local_file_fills_title_and_artist_in_the_background` (enqueue the generated tagged FLAC, pump until the row title is `Morning Tide` and the row subtitle contains `Harbor`).

- [ ] **Step 3: Run `cargo test --locked --test m5_metadata`.** Expected: FAIL.
- [ ] **Step 4: Implement** the probe, fields, workers and runtime wiring. `tui::run` passes `default_probe(hook)`; `RuntimeParts` gains `metadata_probe: Option<TagProbe>` and `hook: TestHook` (`None` disables enrichment); add `metadata_probe: None, hook: TestHook::None` to every `RuntimeParts` literal in `tests/m5_runtime.rs` and `tests/m5_no_network.rs`, except in the tagged-file test, which passes `Some(default_probe(TestHook::None))`.
- [ ] **Step 5: Run.** `cargo test --locked --test m5_metadata --test m5_runtime --test domain_values --test decode_fixtures --lib` → PASS.
- [ ] **Step 6: Commit.**

```bash
git add Cargo.toml Cargo.lock src tests/support/tagged_flac.rs tests/m5_metadata.rs tests/m5_runtime.rs tests/domain_values.rs
git commit -m "feat: enrich local queue entries with tags in contained workers"
```

---
## Phase H — Artwork

### Task 24: Artwork lookup, bounded decode and a contained worker

**Files:**
- Create: `src/artwork/mod.rs`, `src/artwork/resolve.rs`, `src/artwork/decode.rs`, `src/artwork/worker.rs`, `tests/m5_artwork.rs`
- Modify: `src/lib.rs` (`pub mod artwork;`). The `image` dependency and lockfile entries already arrive in Task 23.

**Interfaces:**
- Consumes: `probe_local_tags`, `CoverBytes` (Task 23); `run_contained`, `TestHook` (Task 16).
- Produces:

```rust
// artwork/resolve.rs
pub const SIBLING_NAMES: [&str; 4] = ["cover.jpg", "cover.png", "folder.jpg", "folder.png"];
#[derive(Clone, Debug, Eq, PartialEq)] pub enum ArtworkSource { Embedded(Vec<u8>), Sibling(PathBuf) }
pub fn find_artwork(track: &Path, embedded: Option<CoverBytes>) -> Option<ArtworkSource>;

// artwork/decode.rs
pub const MAX_ENCODED_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_PIXELS: u64 = 16_000_000;
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ArtworkError {
    #[error("artwork file is larger than 10 MiB")] TooLarge,
    #[error("artwork is larger than 16 million pixels")] TooManyPixels,
    #[error("artwork is not JPEG or PNG")] Unsupported,
    #[error("artwork could not be decoded")] Corrupt,
    #[error("artwork could not be read")] Io,
    #[error("artwork decoding panicked")] Panicked,
    #[error("no artwork")] Missing,
}
pub fn read_limited(path: &Path) -> Result<Vec<u8>, ArtworkError>;        // size check before reading, then File::take(MAX + 1)
pub fn decode_limited(bytes: &[u8]) -> Result<image::DynamicImage, ArtworkError>;

// artwork/worker.rs
pub type CoverLoader = Arc<dyn Fn(&AbsolutePath) -> Result<image::DynamicImage, ArtworkError> + Send + Sync>;
pub fn default_loader(hook: TestHook) -> CoverLoader;   // probe tags → find_artwork → read/decode; panics once first when hook == ArtworkJobPanic
pub struct ArtworkResult { pub media: MediaId, pub image: Result<Arc<image::DynamicImage>, ArtworkError> }
pub struct ArtworkWorker;   // spawn(loader: CoverLoader) -> Self; request(&self, media: MediaId, path: AbsolutePath); try_result(&self) -> Option<ArtworkResult>
```

Rules (§9): embedded front cover first, then the sibling names in order, only regular files. `decode_limited`: reject `bytes.len() > MAX_ENCODED_BYTES` → `TooLarge`; `ImageReader::new(Cursor).with_guessed_format()`; format other than JPEG/PNG → `Unsupported`; `into_dimensions()` on a second reader and reject `w × h > MAX_PIXELS` → `TooManyPixels` **before** decoding; decode with `image::Limits { max_image_width: Some(w), max_image_height: Some(h), max_alloc: Some(512 MiB) }`; any decode error → `Corrupt`. The worker is one thread with a latest-wins request slot (`bounded(1)`, replacing a pending request); every job runs inside `run_contained("artwork", ..)`, mapping a panic to `Panicked`; a job that returns normally logs `tracing::debug!("artwork job completed")`. Remote and podcast entries never reach the worker (the TUI shows the placeholder).

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_artwork.rs
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tenuto::artwork::decode::{ArtworkError, MAX_ENCODED_BYTES, decode_limited, read_limited};
use tenuto::artwork::resolve::{ArtworkSource, find_artwork};
use tenuto::artwork::worker::{ArtworkWorker, CoverLoader};
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::media::tags::CoverBytes;

fn encoded(format: image::ImageFormat, w: u32, h: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb([10, 20, 30])))
        .write_to(&mut Cursor::new(&mut bytes), format).expect("encode");
    bytes
}

#[test]
fn embedded_art_wins_then_siblings_in_their_documented_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let track = dir.path().join("track.flac");
    for name in ["folder.png", "folder.jpg", "cover.png"] {
        std::fs::write(dir.path().join(name), encoded(image::ImageFormat::Png, 2, 2)).expect("write");
    }
    let embedded = CoverBytes { data: vec![1, 2, 3], media_type: None };
    assert_eq!(find_artwork(&track, Some(embedded)), Some(ArtworkSource::Embedded(vec![1, 2, 3])));
    assert_eq!(find_artwork(&track, None), Some(ArtworkSource::Sibling(dir.path().join("cover.png"))));
    std::fs::write(dir.path().join("cover.jpg"), encoded(image::ImageFormat::Jpeg, 2, 2)).expect("write");
    assert_eq!(find_artwork(&track, None), Some(ArtworkSource::Sibling(dir.path().join("cover.jpg"))));
    std::fs::remove_file(dir.path().join("cover.jpg")).expect("rm");
    std::fs::remove_file(dir.path().join("cover.png")).expect("rm");
    assert_eq!(find_artwork(&track, None), Some(ArtworkSource::Sibling(dir.path().join("folder.jpg"))));
}

#[test]
fn missing_art_is_none() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("cover.jpg")).expect("a directory is not artwork");
    assert_eq!(find_artwork(&dir.path().join("t.flac"), None), None);
}

#[test]
fn supported_formats_decode_and_everything_else_is_bounded() {
    assert!(decode_limited(&encoded(image::ImageFormat::Png, 3, 2)).is_ok());
    assert!(decode_limited(&encoded(image::ImageFormat::Jpeg, 3, 2)).is_ok());
    assert_eq!(decode_limited(b"\x89PNG\r\n\x1a\ngarbage").err(), Some(ArtworkError::Corrupt));
    assert_eq!(decode_limited(b"GIF89a\x01\x00\x01\x00").err(), Some(ArtworkError::Unsupported));

    // A valid PNG signature and IHDR declaring 5000×4000, with no pixel data.
    let mut huge = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = b"IHDR".to_vec();
    ihdr.extend_from_slice(&5000u32.to_be_bytes());
    ihdr.extend_from_slice(&4000u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    huge.extend_from_slice(&13u32.to_be_bytes());
    huge.extend_from_slice(&ihdr);
    huge.extend_from_slice(&crc32(&ihdr).to_be_bytes());
    assert_eq!(decode_limited(&huge).err(), Some(ArtworkError::TooManyPixels));
}

/// PNG chunk CRC (ISO 3309), so the header parses as valid.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 { crc = if crc & 1 == 1 { 0xedb8_8320 ^ (crc >> 1) } else { crc >> 1 }; }
    }
    !crc
}

#[test]
fn an_oversized_file_is_refused_before_reading() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cover.jpg");
    std::fs::File::create(&path).expect("create").set_len(MAX_ENCODED_BYTES + 1).expect("sparse");
    assert_eq!(read_limited(&path).err(), Some(ArtworkError::TooLarge));
}

#[test]
fn a_panicking_decode_is_a_placeholder_and_the_next_job_succeeds() {
    let first = Arc::new(AtomicBool::new(true));
    let flag = first.clone();
    let loader: CoverLoader = Arc::new(move |_| {
        if flag.swap(false, Ordering::SeqCst) { panic!("injected artwork panic"); }
        Ok(image::DynamicImage::new_rgb8(1, 1))
    });
    let worker = ArtworkWorker::spawn(loader);
    let path = AbsolutePath::new("/music/a.flac".into()).expect("abs");
    let media = MediaId::LocalFile(path.clone());
    let wait = |worker: &ArtworkWorker| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = worker.try_result() { return result; }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    worker.request(media.clone(), path.clone());
    assert_eq!(wait(&worker).image.err(), Some(ArtworkError::Panicked));
    worker.request(media, path);
    assert!(wait(&worker).image.is_ok());
}
```

- [ ] **Step 2: Run `cargo test --locked --test m5_artwork`.** Expected: FAIL.
- [ ] **Step 3: Implement** per the rules. `ArtworkError` derives `Eq` so tests compare it.
- [ ] **Step 4: Run the test.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add src/artwork src/lib.rs tests/m5_artwork.rs
git commit -m "feat: resolve and decode local cover art within fixed limits"
```

### Task 25: Terminal image protocols, cover cache and cleanup

**Files:**
- Create: `src/tui/images.rs`, `tests/m5_tui_images.rs`
- Modify: `Cargo.toml`, `Cargo.lock` (`ratatui-image = { version = "11.0.8", default-features = false, features = ["crossterm"] }`), `src/cli.rs` (`--artwork`), `src/tui/mod.rs`, `src/tui/render.rs` (`CoverView::Image`)

**Interfaces:**
- Consumes: `ArtworkWorker`, `ArtworkError` (Task 24), `Regions::cover` (Task 19), `run_contained` and `TerminalCleanup::set_kitty_images` (Task 16).
- Produces:

```rust
// cli.rs
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum)]
pub enum ArtworkMode { #[default] Auto, Blocks, Off }
CliCommand::Tui { mouse, #[arg(long, value_enum, default_value_t = ArtworkMode::Auto)] artwork: ArtworkMode }

// tui/images.rs
pub const DETECTION_TIMEOUT: Duration = Duration::from_millis(250);
pub fn prepare_contained<T>(job: impl FnOnce() -> Result<T, ArtworkError>) -> Result<T, ArtworkError>;
pub fn picker_for(mode: ArtworkMode, query: impl FnOnce(Duration) -> Option<Picker>) -> Option<Picker>;
    // Off → None; Blocks → Picker::halfblocks(); Auto → query(DETECTION_TIMEOUT) or Picker::halfblocks()
#[derive(Clone, Debug, Eq, PartialEq)] pub struct CoverKey { pub media: MediaId, pub mode: ArtworkMode, pub area: Rect }
pub struct CoverCache { /* key, prepared Protocol, image Arc, placement_dirty: bool, panic_next_encoding: bool */ }
impl CoverCache {
    pub fn new(hook: TestHook) -> Self;  // records whether to inject one ArtworkEncodingPanic
    pub fn set_image(&mut self, media: MediaId, image: Option<Arc<DynamicImage>>);
    pub fn prepare(&mut self, picker: Option<&Picker>, mode: ArtworkMode, area: Option<Rect>) -> bool;  // true when it (re)encoded
    pub fn invalidate(&mut self);                     // Ctrl-L and resize
    pub fn take_placement_cleanup(&mut self) -> bool; // true once after a replacement, resize or invalidate
    pub fn widget(&self) -> Option<&dyn CoverWidget>;
}
```

Rules (§9, §11):
- Detection runs once, after entering the alternate screen and before reading input: `Picker::from_query_stdio_with_options` with a `QueryStdioOptions` timeout of `DETECTION_TIMEOUT` (confirm field names in ratatui-image 11.0.8 docs). Any error or timeout → halfblocks. When the detected protocol is Kitty, call `TerminalCleanup::set_kitty_images(true)` so exit deletes placements.
- Preparation happens in the loop **before** `terminal.draw`, never inside the draw closure. When `CoverKey` changes, build `picker.new_protocol(image, area, Resize::Fit(None))` inside `prepare_contained`, including all resizing/encoding. Build a disposable candidate without mutating the cache or holding shared locks; install it only on success and log `tracing::debug!("artwork encoding completed")`. Any error or unwind clears the prepared image, requests placement cleanup, remembers the failed key to avoid retrying every draw, and shows the placeholder. Another media/mode/area or explicit invalidation permits a new attempt. Rendering, Session and worker orchestration remain outside containment.

```rust
pub fn prepare_contained<T>(job: impl FnOnce() -> Result<T, ArtworkError>) -> Result<T, ArtworkError> {
    crate::lifecycle::panic::run_contained("artwork encoding", job)
        .map_err(|_| ArtworkError::Panicked)?
}
```
- A replacement, resize or `invalidate` sets `placement_dirty`; the loop answers `take_placement_cleanup()` with `terminal.clear()` before the next draw so stale placements cannot persist. Resize events and Ctrl-L both call `invalidate`.
- `tui::run` constructs `CoverCache::new(hook)`. On preparation, take `panic_next_encoding` before entering `prepare_contained`; if true, panic inside the contained job before encoding. The flag is consumed once, so the next media/key can prepare successfully. This implements the `artwork-encoding-panic` process hook without wrapping cache mutation or rendering in containment.
- The loop requests artwork for the active local entry when its media changes; remote and podcast entries and every error show the placeholder. `--artwork off` never starts the worker.

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_tui_images.rs
use std::sync::Arc;
use tenuto::cli::ArtworkMode;
use tenuto::media::id::{AbsolutePath, MediaId};
use tenuto::tui::images::{CoverCache, picker_for};
use ratatui::layout::Rect;
use ratatui_image::picker::Picker;

fn media(name: &str) -> MediaId { MediaId::LocalFile(AbsolutePath::new(format!("/music/{name}.flac").into()).expect("abs")) }

#[test]
fn off_disables_images_blocks_never_queries_and_auto_falls_back() {
    assert!(picker_for(ArtworkMode::Off, |_| panic!("off never queries")).is_none());
    assert!(picker_for(ArtworkMode::Blocks, |_| panic!("blocks never queries")).is_some());
    assert!(picker_for(ArtworkMode::Auto, |_| None).is_some(), "timeout falls back to half-blocks");
}

#[test]
fn a_prepared_cover_is_reused_until_media_mode_or_area_changes() {
    let picker = Picker::halfblocks();
    let mut cache = CoverCache::new(tenuto::lifecycle::hooks::TestHook::None);
    let area = Some(Rect::new(0, 0, 14, 7));
    cache.set_image(media("a"), Some(Arc::new(image::DynamicImage::new_rgb8(8, 8))));
    assert!(cache.prepare(Some(&picker), ArtworkMode::Blocks, area));
    assert!(!cache.prepare(Some(&picker), ArtworkMode::Blocks, area), "cached");
    assert!(cache.prepare(Some(&picker), ArtworkMode::Blocks, Some(Rect::new(0, 0, 8, 4))), "resize re-encodes");
    assert!(cache.take_placement_cleanup());
    assert!(!cache.take_placement_cleanup(), "cleanup is requested once");
    cache.set_image(media("b"), Some(Arc::new(image::DynamicImage::new_rgb8(8, 8))));
    assert!(cache.prepare(Some(&picker), ArtworkMode::Blocks, Some(Rect::new(0, 0, 8, 4))), "replacement re-encodes");
    assert!(cache.take_placement_cleanup());
    cache.invalidate();
    assert!(cache.take_placement_cleanup());
    assert!(cache.prepare(Some(&picker), ArtworkMode::Blocks, Some(Rect::new(0, 0, 8, 4))), "full redraw re-encodes");
}

#[test]
fn no_image_or_no_area_renders_the_placeholder() {
    let picker = Picker::halfblocks();
    let mut cache = CoverCache::new(tenuto::lifecycle::hooks::TestHook::None);
    cache.set_image(media("a"), None);
    assert!(!cache.prepare(Some(&picker), ArtworkMode::Blocks, Some(Rect::new(0, 0, 14, 7))));
    assert!(cache.widget().is_none());
    cache.set_image(media("a"), Some(Arc::new(image::DynamicImage::new_rgb8(8, 8))));
    assert!(!cache.prepare(Some(&picker), ArtworkMode::Blocks, None), "minimal tier has no cover area");
    assert!(cache.widget().is_none());
}
```

Add this test and a cache unit test using an injected encoder in the same preparation path: seed a prepared cover, panic on replacement, assert `widget().is_none()` and one placement-cleanup request, then succeed on the next key. In the PTY containment suite, run an encoding panic inside this boundary while the terminal is active and assert continued rendering, unchanged stderr redirection and successful later preparation.

```rust
#[test]
fn an_encoding_panic_is_contained_and_a_later_preparation_succeeds() {
    use tenuto::artwork::decode::ArtworkError;
    use tenuto::lifecycle::panic::in_contained_job;
    use tenuto::tui::images::prepare_contained;
    let failed = prepare_contained(|| -> Result<(), ArtworkError> {
        assert!(in_contained_job());
        panic!("encoding panic");
    });
    assert_eq!(failed, Err(ArtworkError::Panicked));
    assert!(!in_contained_job());
    assert_eq!(prepare_contained(|| Ok(7)), Ok(7));
}
```

- [ ] **Step 2: Run `cargo test --locked --test m5_tui_images`.** Expected: FAIL.
- [ ] **Step 3: Implement** `images.rs` and the loop wiring; confirm `Picker::halfblocks`, `Picker::new_protocol`, `Resize::Fit`, `Image::new` signatures in ratatui-image 11.0.8.
- [ ] **Step 4: Run.** `cargo test --locked --test m5_tui_images --test m5_tui_render --test m5_tui_process` → PASS.
- [ ] **Step 5: Commit.**

```bash
git add Cargo.toml Cargo.lock src/cli.rs src/tui tests/m5_tui_images.rs
git commit -m "feat: draw cover art through detected terminal image protocols"
```

---

## Phase I — Frequency spectrum

### Task 26: Band geometry and the analyzer

**Files:**
- Create: `src/playback/spectrum/mod.rs`, `src/playback/spectrum/bands.rs`, `src/playback/spectrum/analyzer.rs`, `tests/m5_spectrum_bands.rs`
- Modify: `Cargo.toml`, `Cargo.lock` (`rustfft = "6.4.1"`), `src/playback/mod.rs` (`pub mod spectrum;`)

**Interfaces:**
- Produces:

```rust
// bands.rs
pub const WINDOW: usize = 2048;
pub const NOMINAL_BANDS: usize = 24;
pub const LOW_HZ: f64 = 40.0;
pub const HIGH_HZ: f64 = 16_000.0;
#[derive(Clone, Debug, PartialEq)]
pub struct Band { pub low_hz: f64, pub high_hz: f64, pub bins: std::ops::Range<usize> }  // FFT bin indices k (center k·rate/WINDOW)
pub fn layout_bands(sample_rate: u32) -> Vec<Band>;   // empty = unavailable

// analyzer.rs
pub const HOP: usize = WINDOW / 2;
pub struct SpectrumAnalyzer;
impl SpectrumAnalyzer {
    pub fn new(sample_rate: u32, channels: u16) -> Self;
    pub fn bands(&self) -> &[Band];
    pub fn reset(&mut self);
    /// Interleaved samples, whole frames. Returns levels in 0.0..=1.0, one per band,
    /// each time a full window is available (then advances by HOP).
    pub fn push_interleaved(&mut self, samples: &[f32]) -> Option<Vec<f32>>;
}
```

Geometry, exactly (§10): `top = min(HIGH_HZ, rate / 2)`; if `top <= LOW_HZ` return empty. Edges `e_i = LOW_HZ · (top / LOW_HZ)^(i / 24)` for `i = 0..=24` in `f64`. Bin centers `c_k = k · rate / WINDOW` for `k = 1..=WINDOW/2`. Interval `i` owns centers with `e_i <= c < e_{i+1}`, the last interval also owning `c == e_24`. Walk intervals low→high, extending the current band until it owns ≥ 2 centers, then emit it. Merge any underfilled tail into the preceding band; if no band was emitted, return empty. Emitted bands therefore own contiguous, non-overlapping bin ranges covering every eligible center exactly once.

Analyzer: per channel, a `WINDOW`-sample Hann window (`0.5 − 0.5·cos(2πn/(N−1))`), `rustfft` forward transform (planner created once), power `|X_k|²·4/(Σw)²`. Band power = mean over its bins; average band power across **all** channels (never sum samples across channels first). Level = `clamp((10·log10(power + 1e-12) + 80) / 80, 0, 1)`. `reset` clears accumulated samples. `push_interleaved` ignores trailing partial frames.

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_spectrum_bands.rs
use tenuto::playback::spectrum::analyzer::SpectrumAnalyzer;
use tenuto::playback::spectrum::bands::{WINDOW, layout_bands};

#[test]
fn the_band_table_matches_the_spec_at_ordinary_and_high_rates() {
    for (rate, count, first_high) in [(44_100, 21, 65.9), (48_000, 21, 84.6), (96_000, 19, 108.6), (192_000, 16, 229.6)] {
        let bands = layout_bands(rate);
        assert_eq!(bands.len(), count, "{rate}");
        assert!((bands[0].low_hz - 40.0).abs() < 1e-9);
        assert!((bands[0].high_hz - first_high).abs() < 0.05, "{rate}: {}", bands[0].high_hz);
    }
    let sizes: Vec<_> = layout_bands(44_100).iter().map(|b| b.bins.len()).collect();
    assert_eq!(sizes, [2, 2, 3, 2, 3, 4, 5, 6, 9, 10, 14, 17, 22, 29, 37, 47, 60, 78, 99, 128, 165]);
}

#[test]
fn every_band_owns_two_or_more_centers_and_every_center_is_assigned_once() {
    for rate in [44_100u32, 48_000, 96_000, 192_000] {
        let bands = layout_bands(rate);
        let top = (rate as f64 / 2.0).min(16_000.0);
        let eligible: Vec<usize> = (1..=WINDOW / 2)
            .filter(|k| { let c = *k as f64 * rate as f64 / WINDOW as f64; c >= 40.0 && c <= top })
            .collect();
        let assigned: Vec<usize> = bands.iter().flat_map(|b| b.bins.clone()).collect();
        assert!(bands.iter().all(|b| b.bins.len() >= 2), "{rate}");
        assert!(bands.windows(2).all(|p| p[0].bins.end == p[1].bins.start && p[0].high_hz <= p[1].low_hz + 1e-9), "{rate}: contiguous, no overlap");
        assert_eq!(assigned, eligible, "{rate}: each center exactly once");
    }
}

#[test]
fn a_rate_with_no_usable_range_is_unavailable() {
    assert!(layout_bands(64).is_empty());
}

fn tone(rate: u32, hz: f32, frames: usize, channels: usize, phase_flip: bool) -> Vec<f32> {
    (0..frames).flat_map(|n| {
        let s = (2.0 * std::f32::consts::PI * hz * n as f32 / rate as f32).sin() * 0.5;
        (0..channels).map(move |ch| if phase_flip && ch % 2 == 1 { -s } else { s })
    }).collect()
}

fn loudest(levels: &[f32]) -> usize {
    levels.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, _)| i).expect("bands")
}

#[test]
fn a_known_tone_peaks_in_its_own_band_for_mono_stereo_and_multichannel() {
    for channels in [1u16, 2, 6] {
        let mut analyzer = SpectrumAnalyzer::new(48_000, channels);
        let levels = analyzer.push_interleaved(&tone(48_000, 1_000.0, WINDOW, usize::from(channels), false)).expect("window");
        let band = &analyzer.bands()[loudest(&levels)];
        assert!(band.low_hz <= 1_000.0 && 1_000.0 < band.high_hz, "{channels} ch: {band:?}");
    }
}

#[test]
fn the_merged_low_band_reacts_to_a_low_tone() {
    let mut analyzer = SpectrumAnalyzer::new(48_000, 1);
    let levels = analyzer.push_interleaved(&tone(48_000, 60.0, WINDOW, 1, false)).expect("window");
    assert_eq!(loudest(&levels), 0);
}

#[test]
fn opposite_phase_channels_do_not_cancel() {
    let mut in_phase = SpectrumAnalyzer::new(48_000, 2);
    let mut opposite = SpectrumAnalyzer::new(48_000, 2);
    let a = in_phase.push_interleaved(&tone(48_000, 1_000.0, WINDOW, 2, false)).expect("window");
    let b = opposite.push_interleaved(&tone(48_000, 1_000.0, WINDOW, 2, true)).expect("window");
    for (x, y) in a.iter().zip(&b) { assert!((x - y).abs() < 1e-4); }
}

#[test]
fn silence_is_silent_and_reset_discards_a_partial_window() {
    let mut analyzer = SpectrumAnalyzer::new(44_100, 2);
    let levels = analyzer.push_interleaved(&vec![0.0; WINDOW * 2]).expect("window");
    assert!(levels.iter().all(|l| *l < 0.01));
    let mut analyzer = SpectrumAnalyzer::new(44_100, 1);
    assert!(analyzer.push_interleaved(&vec![0.1; 1_000]).is_none());
    analyzer.reset();
    assert!(analyzer.push_interleaved(&vec![0.1; WINDOW - 1_000]).is_none(), "reset dropped the first 1000");
}
```

- [ ] **Step 2: Run `cargo test --locked --test m5_spectrum_bands`.** Expected: FAIL.
- [ ] **Step 3: Implement** `bands.rs` and `analyzer.rs`.
- [ ] **Step 4: Run the test.** Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add Cargo.toml Cargo.lock src/playback tests/m5_spectrum_bands.rs
git commit -m "feat: merged logarithmic spectrum bands over a 2048-sample Hann window"
```

### Task 27: Allocation-free output tap

**Files:**
- Create: `src/playback/spectrum/tap.rs`, `tests/m5_spectrum_tap.rs`
- Modify: `src/playback/spectrum/mod.rs`, `src/playback/callback.rs` (`with_tap`, offer after gain)

**Interfaces:**
- Consumes: `rtrb`, `Nanos`.
- Produces:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TapDescriptor { pub instance: u64, pub generation: u16, pub epoch: u32, pub channels: u16,
                           pub sample_rate: u32, pub discontinuity: u32, pub predicted: Nanos, pub samples: u32 }
pub const DESCRIPTOR_CAPACITY: usize = 256;
pub struct TapWriter;   // offer(&mut self, samples: &[f32], channels: u16, sample_rate: u32, generation: u16, epoch: u32, predicted: Nanos)
pub struct TapReader;   // next_block(&mut self, out: &mut Vec<f32>) -> Option<TapDescriptor>; appends the block's samples to `out`
pub fn tap_pair(instance: u64, channels: u16, sample_rate: u32, enabled: Arc<AtomicBool>) -> (TapWriter, TapReader);
// callback.rs
impl CallbackCore { pub fn with_tap(self, tap: TapWriter) -> Self }
```

Rules (§10), for `offer` (runs on the audio callback: no allocation, no lock, no I/O, never blocks):
1. If `enabled` is false: mark `lost` and return.
2. `whole = samples.len() / channels × channels`; return if zero.
3. If `descriptors.slots() == 0` or `pcm.slots() < whole`: mark `lost` and return (drop the whole block).
4. `pcm.write_chunk(whole)`, copy into both slices of `as_mut_slices()`, `commit_all()` — PCM first.
5. If `lost`: `discontinuity = discontinuity.wrapping_add(1)`, clear `lost`.
6. `descriptors.push(descriptor)` — guaranteed to succeed after step 3 on this single producer.

The PCM ring holds half a second rounded down to whole frames (`rate / 2 × channels`, minimum one `WINDOW` of frames); both rings are allocated in `tap_pair`. `next_block` pops a descriptor, then reads exactly `samples` values with `read_chunk` and `commit_all`; it never reads PCM without a descriptor. `CallbackCore::run` calls `offer(&out[..frames × channels], ..)` **after** `apply_gain`, only when `frames > 0`, with the published `control.generation`, `control.epoch` and the callback's `playback` instant.

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_spectrum_tap.rs
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use tenuto::playback::output::Nanos;
use tenuto::playback::spectrum::tap::tap_pair;

fn pair(enabled: bool) -> (tenuto::playback::spectrum::tap::TapWriter, tenuto::playback::spectrum::tap::TapReader) {
    tap_pair(7, 2, 48_000, Arc::new(AtomicBool::new(enabled)))
}

#[test]
fn only_whole_frames_are_committed_with_a_matching_descriptor() {
    let (mut writer, mut reader) = pair(true);
    writer.offer(&[1.0, 2.0, 3.0, 4.0, 5.0], 2, 48_000, 3, 9, Nanos(100));
    let mut out = Vec::new();
    let descriptor = reader.next_block(&mut out).expect("described");
    assert_eq!((descriptor.samples, descriptor.instance, descriptor.generation, descriptor.epoch), (4, 7, 3, 9));
    assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    assert!(reader.next_block(&mut out).is_none(), "no orphan PCM");
}

#[test]
fn a_block_that_does_not_fit_is_dropped_whole_and_marks_a_discontinuity() {
    let (mut writer, mut reader) = pair(true);
    let big = vec![0.25; 48_000];            // exactly the half-second ring for stereo
    writer.offer(&big, 2, 48_000, 1, 1, Nanos(0));
    writer.offer(&[0.5, 0.5], 2, 48_000, 1, 1, Nanos(1));   // no room: dropped
    let mut out = Vec::new();
    let first = reader.next_block(&mut out).expect("first block");
    assert_eq!(first.discontinuity, 0);
    assert!(reader.next_block(&mut out).is_none(), "the dropped block left nothing behind");
    out.clear();
    writer.offer(&[0.75, 0.75], 2, 48_000, 1, 1, Nanos(2));
    let next = reader.next_block(&mut out).expect("accepted");
    assert_eq!(next.discontinuity, 1);
    assert_eq!(out, [0.75, 0.75]);
}

#[test]
fn descriptor_exhaustion_also_drops_whole_blocks() {
    let (mut writer, mut reader) = pair(true);
    for n in 0..300 { writer.offer(&[n as f32, n as f32], 2, 48_000, 1, 1, Nanos(n)); }
    let mut out = Vec::new();
    let mut blocks = 0;
    while reader.next_block(&mut out).is_some() { blocks += 1; }
    assert_eq!(blocks, 256);
    assert_eq!(out.len(), 512);
}

#[test]
fn data_survives_ring_wraparound_in_order() {
    // 480 frames per block against a 24 000-frame ring: 2 000 rounds wrap it
    // about forty times.
    let (mut writer, mut reader) = pair(true);
    let mut block = vec![0.0f32; 960];
    let mut out = Vec::new();
    for round in 0..2_000u64 {
        for (index, sample) in block.iter_mut().enumerate() {
            *sample = ((round as usize * 960 + index) % 16_777_216) as f32;
        }
        writer.offer(&block, 2, 48_000, 1, 1, Nanos(round));
        out.clear();
        let descriptor = reader.next_block(&mut out).expect("block");
        assert_eq!(descriptor.samples, 960);
        assert_eq!(out, block, "round {round}");
    }
}

#[test]
fn a_disabled_or_unread_tap_never_blocks_the_writer() {
    let (mut disabled, mut reader) = pair(false);
    disabled.offer(&[1.0, 1.0], 2, 48_000, 1, 1, Nanos(0));
    assert!(reader.next_block(&mut Vec::new()).is_none());

    let (mut writer, _unread) = pair(true);
    let block = vec![0.1f32; 960];
    let started = Instant::now();
    for n in 0..20_000 { writer.offer(&block, 2, 48_000, 1, 1, Nanos(n)); }
    assert!(started.elapsed().as_millis() < 500, "saturation must stay cheap");
}
```

Add to `callback.rs` tests: `the_tap_sees_post_gain_samples` — `link.set_gain(0.5)` **before** constructing the core (so no ramp), push frames of `1.0`, attach `tap_pair(..)` via `with_tap`, run one `Phase::Run` period, and assert every tapped sample is `0.5`.

- [ ] **Step 2: Run `cargo test --locked --test m5_spectrum_tap`.** Expected: FAIL.
- [ ] **Step 3: Implement** `tap.rs` and the callback hook.
- [ ] **Step 4: Run.** `cargo test --locked --test m5_spectrum_tap --lib --test engine_contract` → PASS.
- [ ] **Step 5: Commit.**

```bash
git add src/playback tests/m5_spectrum_tap.rs
git commit -m "feat: tap post-gain output PCM into bounded rings"
```

### Task 28: Transport mappings, analysis worker and the on-screen spectrum

**Files:**
- Create: `src/playback/spectrum/registry.rs`, `src/playback/spectrum/worker.rs`, `tests/m5_spectrum_engine.rs`
- Modify: `src/playback/handshake.rs` (`upcoming_epoch`), `src/playback/engine.rs` (`assemble`, `open_transport`, `prime_and_run`, `play`'s release, `teardown`, `TransportCore`), `src/playback/wait.rs` (thaw path through `TransportCore::release`), `src/tui/mod.rs`, `src/tui/render.rs` (spectrum levels)

**Interfaces:**
- Consumes: `tap_pair`, `TapReader`, `TapDescriptor` (Task 27); `SpectrumAnalyzer` (Task 26).
- Produces:

```rust
// handshake.rs
pub fn upcoming_epoch(&self) -> u32;   // self.epoch.wrapping_add(1), the epoch the next publication will use

// registry.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TapMapping { pub instance: u64, pub generation: u16, pub epoch: u32, pub session_rev: u64, pub sample_rate: u32, pub channels: u16 }
#[derive(Clone, Default)] pub struct TapRegistry;   // Arc<Mutex<..>>, never touched by the callback
impl TapRegistry {
    pub fn publish(&self, mapping: TapMapping);          // retires older generations of the same instance
    pub fn retire_instance(&self, instance: u64);
    pub fn lookup(&self, instance: u64, generation: u16, epoch: u32) -> Option<TapMapping>;
}

// worker.rs
pub const MAX_FRAMES_PER_SECOND: u32 = 20;
pub const FRAME_MAX_AGE: Duration = Duration::from_millis(150);
pub fn frame_is_fresh(frame: &SpectrumFrame, now: Instant) -> bool;
#[derive(Clone, Debug, PartialEq)]
pub struct SpectrumFrame { pub session_rev: u64, pub bands: Vec<(f64, f64)>, pub levels: Vec<f32>, pub at: Nanos, pub published_at: Instant }
#[derive(Clone)]
pub struct SpectrumHandle;   // set_enabled(&self, bool); latest(&self) -> Option<SpectrumFrame>; registry(&self) -> &TapRegistry
impl EngineHandle { pub fn spectrum(&self) -> SpectrumHandle }
```

Rules (§10):
- `EngineHandle::assemble` creates a `TapRegistry`, an `enabled` flag (initially `false`), a latest-frame slot, an attach channel and one `tenuto-spectrum` thread; the worker owns the receiving side and the device clock `Arc<AtomicU64>` the engine already shares.
- `open_transport` assigns `instance = next_tap_instance` (monotonic `u64`, never reused), builds `tap_pair`, sends the reader over the attach channel, attaches the writer with `CallbackCore::with_tap`, and stores `instance` and `session_rev` in `TransportCore`.
- **Before** every `Run` publication — `prime_and_run`'s `start_running`, `play`'s `release`, and `TransportCore::release` on the hook's thaw path — publish `TapMapping { instance, generation, epoch: handshake.upcoming_epoch(), session_rev, sample_rate, channels }`.
- `teardown` retires the instance. A load, stop or shutdown therefore invalidates every mapping of the old transport; a seek's reinstall publishes a newer generation, which retires the older one.
- The worker loop: when disabled, clear the latest/pending frames and analyzer, drain and discard readers' blocks and sleep 50 ms. Otherwise read blocks; `lookup` failure → discard. Reset the analyzer when the mapping key `(instance, generation, epoch)`, sample rate, channel count, or `discontinuity` changes (build a new analyzer when rate/channels change). Each newly analyzed window becomes a pending frame stamped with the block's `predicted` instant; publish it into the latest slot once the device clock reaches that instant and at least `1 s / MAX_FRAMES_PER_SECOND` has passed since the last publication. Smooth as `level = max(new, previous × 0.85)`. At least every 50 ms, even when no block arrives, look up the mapping of the frame in the latest slot and clear the slot when that mapping has been retired, so a stopped or replaced transport's spectrum disappears without new audio. Stamp `published_at = Instant::now()` only when publishing a new audible frame; never refresh it while returning/reusing the latest value. Clear expired frames after `FRAME_MAX_AGE` using monotonic wall time even if the output clock stops. A pending frame also expires 150 ms after its predicted output time is first reached; replacing/reusing a pending frame must not make old PCM fresh. Track that ready instant separately from its device timestamp.
- TUI: enable analysis only while the spectrum rectangle exists and the phase is `Playing`. Draw levels only when the phase is still `Playing`, the revision/token match adopted playback and `frame_is_fresh(frame, Instant::now())`. Otherwise decay displayed levels by 0.85 per draw, including pause, stop and starvation with an unchanged valid mapping. The view's clock check prevents a delayed worker cleanup from freezing the display.

```rust
pub fn frame_is_fresh(frame: &SpectrumFrame, now: Instant) -> bool {
    now.checked_duration_since(frame.published_at).is_some_and(|age| age < FRAME_MAX_AGE)
}
```

- [ ] **Step 1: Write failing tests.**

```rust
// tests/m5_spectrum_engine.rs
mod support;

use std::time::{Duration, Instant};
use tenuto::playback::spectrum::registry::{TapMapping, TapRegistry};
use support::TestEngine;

fn mapping(instance: u64, generation: u16, epoch: u32, rev: u64) -> TapMapping {
    TapMapping { instance, generation, epoch, session_rev: rev, sample_rate: 48_000, channels: 2 }
}

#[test]
fn a_newer_generation_retires_the_older_one_and_a_reused_generation_never_matches_another_instance() {
    let registry = TapRegistry::default();
    registry.publish(mapping(1, 5, 10, 3));
    assert!(registry.lookup(1, 5, 10).is_some());
    registry.publish(mapping(1, 6, 11, 3));
    assert!(registry.lookup(1, 5, 10).is_none(), "seek retired generation 5");
    registry.publish(mapping(2, 6, 11, 4));
    assert_eq!(registry.lookup(2, 6, 11).map(|m| m.session_rev), Some(4));
    assert_eq!(registry.lookup(1, 6, 11).map(|m| m.session_rev), Some(3), "instances are distinct");
    registry.retire_instance(1);
    assert!(registry.lookup(1, 6, 11).is_none());
    assert!(registry.lookup(2, 6, 12).is_none(), "unknown epoch");
}

#[test]
fn playback_publishes_frames_labelled_with_the_current_revision_only() {
    let mut engine = TestEngine::start("sine-5s.flac");
    let spectrum = engine.handle().spectrum();
    spectrum.set_enabled(true);
    engine.play_for(Duration::from_millis(600));
    let deadline = Instant::now() + Duration::from_secs(10);
    let frame = loop {
        engine.play_for(engine.position() + Duration::from_millis(50));
        if let Some(frame) = spectrum.latest() { break frame; }
        assert!(Instant::now() < deadline, "no spectrum frame");
    };
    assert_eq!(frame.session_rev, engine.progress().session_rev);
    assert!(!frame.levels.is_empty() && frame.levels.len() <= 24);

    engine.interrupt_stop();
    engine.await_state(tenuto::playback::state::PlaybackState::Stopped);
    let deadline = Instant::now() + Duration::from_secs(5);
    while spectrum.latest().is_some() {
        assert!(Instant::now() < deadline, "a retired transport's frame must be cleared");
        std::thread::sleep(Duration::from_millis(10));
    }
    engine.finish();
}

#[test]
fn device_recreation_gets_a_new_instance_and_drops_old_mappings() {
    let mut engine = TestEngine::start("sine-5s.flac");
    let spectrum = engine.handle().spectrum();
    spectrum.set_enabled(true);
    engine.play_for(Duration::from_millis(300));
    engine.force_device_loss();
    engine.await_recovery_capture();
    engine.play_for(engine.position() + Duration::from_millis(600));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(frame) = spectrum.latest() && frame.session_rev == engine.progress().session_rev { break; }
        assert!(Instant::now() < deadline, "no frame for the recovered revision");
        engine.play_for(engine.position() + Duration::from_millis(50));
    }
    engine.finish();
}
```

Confirm `force_device_loss` and `await_recovery_capture` behave as their names suggest in `tests/support/mod.rs` before relying on them; if recovery needs a different helper sequence, use the one `tests/engine_contract.rs` uses for its device-recovery test.

Add to `tests/m5_tui_render.rs`: `the_spectrum_row_draws_levels_and_nothing_in_minimal` — `Visuals { spectrum: Some(&[1.0; 12]), .. }` at 100×30 renders at least one `█`; at 45×16 renders none.

Add the deterministic freshness test below. Also exercise `pause_without_transport_retirement_decays` and `starvation_without_new_pcm_expires_the_latest_frame`: publish a nonzero frame, retain its mapping, stop providing PCM while advancing the injected monotonic time past 150 ms, and assert no fresh levels are drawn; a new window resumes the display. Repeat with analysis disabled/re-enabled and ensure the old latest/pending frame is never revived.

```rust
#[test]
fn an_unchanged_frame_expires_even_when_its_revision_still_matches() {
    use tenuto::playback::output::Nanos;
    use tenuto::playback::spectrum::worker::{SpectrumFrame, FRAME_MAX_AGE, frame_is_fresh};
    let now = Instant::now();
    let frame = SpectrumFrame { session_rev: 1, bands: vec![(40.0, 85.0)],
        levels: vec![1.0], at: Nanos(0), published_at: now };
    assert!(frame_is_fresh(&frame, now));
    assert!(!frame_is_fresh(&frame, now + FRAME_MAX_AGE));
    assert!(!frame_is_fresh(&frame, now + Duration::from_secs(10)));
}
```

- [ ] **Step 2: Run `cargo test --locked --test m5_spectrum_engine`.** Expected: FAIL.
- [ ] **Step 3: Implement** the registry, worker, engine wiring and TUI enabling/decay.
- [ ] **Step 4: Run the engine suites for regressions.**
Run: `cargo test --locked --test m5_spectrum_engine --test m5_spectrum_tap --test engine_contract --test engine_shutdown --test engine_remote --test wait_service --test m5_tui_render --lib`
Expected: PASS.
- [ ] **Step 5: Commit.**

```bash
git add src/playback src/tui tests/m5_spectrum_engine.rs tests/m5_tui_render.rs
git commit -m "feat: analyze the output tap per transport and draw the spectrum"
```

---

## Phase J — Acceptance

### Task 29: Process-level lifecycle suite, documentation and manual terminal checks

**Files:**
- Modify: `tests/m5_tui_process.rs`, `README.md`, `docs/architecture.md`
- Create: `docs/m5-acceptance.md`

**Interfaces:**
- Consumes: everything above; `TestHook` values; `TENUTO_AUDIO_OUTPUT=null`.

Each case below is its own subprocess, with its own `Profile`, so a panic, a signal or a redirect cannot affect the test runner. Seed queues by writing a schema-3 `state.json` into `profile.state_dir()` before spawning.

- [ ] **Step 1: Write the failing process tests** in `tests/m5_tui_process.rs`.
  1. `tui_signals_during_playback_flush_and_exit_128_plus_n` — for INT, HUP, TERM: queue `[sine-5s.flac]` active; spawn with `TENUTO_AUDIO_OUTPUT=null`; send `" "`; wait 1.5 s; `kill -<SIG>`; exit code `128 + n`; `state.json` has a checkpoint ≥ 1 s for the fixture; a fresh `tui` in a PTY starts and exits 0 on `q` (lock released).
  2. `tui_signals_during_stalled_http_preparation_exit_128_plus_n` — queue a remote entry pointing at a `stall_headers` server; send `" "`; `server.wait_until_stalled`; for each signal the exit code is `128 + n` and a fresh invocation acquires the profile.
  3. `a_direct_fd2_write_reaches_the_log_not_the_pty` — hook `stderr-probe`; wait for the idle screen; `q`; the newest file in `profile.state_dir().join("logs")` contains `tenuto-stderr-probe` and `tenuto-stderr-probe-rust`; PTY output contains neither.
  4. `a_hung_up_pty_still_flushes_and_releases_the_profile` — press `-`, then `close_master()`; the child exits (SIGHUP path: code 129, or 0 if it read EOF first — accept either, but it must exit within 10 s); `state.json` volume is `0.95`; a fresh `tui` acquires the profile.
  5. `a_panic_before_redirection_reaches_the_terminal` — hook `panic-before-redirect`; exit code 101; PTY output contains `tenuto test hook: panic-before-redirect`; no `logs/` file contains it.
  6. `a_panic_right_after_redirection_is_printed_on_restored_stderr` — hook `panic-after-redirect`; exit code 101; PTY output contains the hook message.
  7. `a_panic_after_terminal_entry_leaves_the_alternate_screen_first` — hook `panic-after-terminal`; exit 101; in PTY output the last `\x1b[?1049l` appears before the hook message.
  8. `contained_artwork_and_metadata_panics_keep_the_player_running` — for hooks `artwork-job-panic`, `artwork-encoding-panic` and `metadata-job-panic`, `RUST_LOG=tenuto=debug`: queue two tagged FLACs with covers (Task 23 helper), restore the first active entry and wait for its contained failure; press Down then Enter to activate the second entry and wait for its successful job; send `q`; exit 0; the log contains `contained panic in background job` and a later successful-job debug line (`artwork job completed` / `artwork encoding completed` / `metadata job completed`, emitted at debug level by the corresponding successful job); PTY output does not contain the hook message.
  9. `an_uncontained_worker_panic_restores_the_terminal_and_fails` — hook `worker-panic` (the metadata worker panics outside `run_contained` on its first job); exit code nonzero; `\x1b[?1049l` precedes `a background worker panicked` in PTY output.
  10. `a_play_without_a_terminal_still_honours_signals` is already covered by Task 12; do not duplicate it.
- [ ] **Step 2: Run `cargo test --locked --test m5_tui_process`.** Expected: the new cases FAIL only where wiring is missing (for example the `worker-panic` hook site or the job-completed debug lines). Implement those missing pieces in the owning modules, keeping each one's own tests green.
- [ ] **Step 3: Update the docs.**
  - `README.md`: new `## Terminal player` section — `tenuto tui [--mouse on|off] [--artwork auto|blocks|off]`, the §7 key table, queue persistence and its 256-entry limit, the `saved`/`~`/`played`/`position unknown` labels, the lock message, logs under `$XDG_STATE_HOME/tenuto/logs/`, manual feed refresh. Replace "One process at a time" with the lock behavior for `play`/`tui` (feed commands still do not lock). Add `state.lock` and `logs/` to "Where the files live".
  - `docs/architecture.md`: add execution contexts (TUI application thread, signal listener, metadata workers ×2, artwork worker, spectrum worker, browse worker) with ownership/"must not" columns; the token correlation contract and protected outcomes (§6 of the spec, decisions 2–5); schema 3 and queue-only recovery; the lock, startup order and panic containment; `TENUTO_AUDIO_OUTPUT=null` and `TENUTO_TEST_HOOK` as diagnostic switches.
- [ ] **Step 4: Manual terminal checks.** Build `cargo build --release --locked` and run `tenuto tui` with a local album that has embedded art and a folder with `cover.jpg`, one podcast episode, and one direct URL, in: Ghostty directly; Ghostty with Zellij; Ghostty with Herdr. In each, check and record in `docs/m5-acceptance.md` (a table: environment × check → pass/fail + note): image protocol used or half-block fallback; `--artwork blocks` and `off`; resize through all four tiers; pane switch and return; keyboard map; mouse selection/scroll/transport and `m` toggle with text selection working while off; Ctrl-L; `q`, Ctrl-C, closing the pane; terminal state after exit (cursor, raw mode, no stray images). Browser checks do not count. Record failures honestly; do not mark a row passed without doing it.
- [ ] **Step 5: Final gates.**

```bash
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
```

Expected: all three succeed. Fix any failure in the task that owns the code, then rerun all three.

- [ ] **Step 6: Commit.**

```bash
git add tests/m5_tui_process.rs README.md docs/architecture.md docs/m5-acceptance.md src
git commit -m "test: prove M5 lifecycle guarantees and document the terminal player"
```

---

## Spec coverage map

| Spec section | Tasks |
|---|---|
| §1 Product decisions (layout A, local-first, no server/Tauri) | 18–22, 24–25, 28; Global Constraints |
| §2 Existing foundations kept | 6–9 preserve engine/session contracts; every task reruns existing suites |
| §3 Ownership and module boundaries | 3, 8–9 (Session owns queue and tokens), 13–15 (runtime), 19–22 (TUI), 23–24 (workers), 26–28 (spectrum) |
| §4 Entry points, pre-load input, shutdown, signals, exit codes | 2, 11, 12, 14, 15, 18, 29 |
| §5 Queue behavior, podcast resolution, cap, eviction | 3, 8, 9, 10, 14, 15 |
| §6 Schema 3, migration, queue-only recovery, backup, tokens, protected outcomes, lock, source-before-lock | 4, 5, 6, 7, 8, 11 |
| §7 Layout tiers, palette, key and mouse map, honest duration | 19, 20, 21 |
| §8 Browser, metadata, no implicit network | 10, 22, 23 |
| §9 Artwork order, limits, protocols, cleanup, containment | 23, 24, 25, 29 |
| §10 Spectrum tap, mapping, band merging, rate, decay | 26, 27, 28 |
| §11 Failure behavior, fd-2 redirect, startup order, panic hook, terminal cleanup | 16, 17, 18, 29 |
| §12 Validation list and delivery order | 1 (subprocess isolation), phase order A–J, 29 (manual checks, final gates) |
| §13 Scope changes | Global Constraints "Never" list |

## Planning self-review

- **Coverage:** every §12 evidence bullet maps to a named test: queue (Tasks 3, 8, 9, 15), state (4, 5, 9, 11), engine/session tokens (7, 8), Ratatui buffers and input (19–21), podcast resolver and no-network (10, 22), artwork (24, 25, 29), spectrum (26–28), subprocess/PTY/signal/redirect/panic (12, 18, 29), manual terminals (29), gates (29).
- **Placeholders:** no step defers behavior to "later" without naming the task that owns it. Tasks 21, 22 and 29 list their test cases as exact inputs and expected outputs rather than full listings; they reuse helpers defined in Tasks 18–20.
- **Names checked across tasks:** `LoadRequestId`, `Progress::load`, `LoadTarget`, `RegisterLoadError`, `Advance`, `Removal`, `DisplayUpdate`, `PlaybackPhase`, `TransportDecision`, `AppCommand`, `EnqueueItem`, `PlayerView`, `NowPlaying`, `SavedHistory`, `UiState`, `Overlay`, `Effect`, `HitMap`, `TakeOnceSlot`, `TestHook`, `ArtworkMode`, `CoverCache`, `prepare_contained`, `TapWriter`, `TapRegistry`, `SpectrumHandle`, `PlayLoaded`, `accepts_media_event`, `frame_is_fresh` are defined once and used with the same signatures.
- **Review regressions:** Tasks 1 (verified-platform launch isolation), 3–4 (ID exhaustion), 6 and 15 (guarded automatic start and retries), 8 (atomic adoption snapshots and checkpoint ownership), 13 and 15 (seek cancellation), 23 (dependency order), 25 (encoding containment), and 28 (frame expiry) carry the ten review fixes and their checks.
- **Known judgment calls** are listed as Implementation decisions 1–25; any change to them needs a note in the executing task's review.
