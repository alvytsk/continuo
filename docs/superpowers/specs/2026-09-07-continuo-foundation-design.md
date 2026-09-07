# Continuo — Foundation Design (Milestone 0)

**Date:** 2026-09-07
**Status:** Approved for implementation
**Scope:** Milestone 0 implementation, plus the binding architectural contracts that later milestones must honor.

---

## 1. What Continuo is

Continuo is a keyboard-first terminal audio player for local files, finite remote audio over HTTP, and podcasts delivered through RSS/Atom feeds.

Its organizing motivation is correct playback-state handling. A podcast episode fetched over HTTP is finite, seekable when the server supports byte ranges, and resumable. Internet radio fetched over the same protocol is none of those things. Transport does not determine media semantics, and Continuo models the two separately rather than collapsing them into a single boolean.

### The central invariant

> Stopping or recreating the audio transport pipeline must not implicitly reset the logical playback position.

Everything in this document exists to make that invariant structural rather than conventional.

### Out of scope for v0.1

Spotify, YouTube/yt-dlp, SoundCloud, Jellyfin/Plex/Navidrome, visualizer, equalizer/DSP, themes, plugin system, daemon/client split, remote control. MPRIS and media keys are deferred until the core is stable.

---

## 2. Layering and ownership

Four execution contexts with strict responsibilities.

| Context | Owns | Must not |
|---|---|---|
| Tokio tasks | Application orchestration, HTTP fetching, feed parsing, timers | Touch decoder or output-device state |
| Decode thread (`std::thread`) | Symphonia demux/decode, resampling, command processing, PCM production, position anchoring, **the CPAL stream's full lifecycle** | Block on the Tokio runtime |
| CPAL callback | Drain a bounded SPSC ring buffer, emit silence on underrun, publish a frame counter | Lock, allocate, wait, or perform I/O |
| Persistence writer thread | Serialize and atomically write state snapshots | Run on Tokio's executor |

### Device ownership

The decode thread creates, controls (play/pause), and destroys the CPAL stream. The callback owns only its ring-buffer consumer endpoint and its progress counter. No other context holds a handle to the stream.

### Blocking rules

"The decode thread never blocks on the Tokio runtime" permits **cancellable synchronous waits** on the HTTP byte buffer and on PCM backpressure. This distinction is load-bearing: stop, seek, and shutdown must *wake* those waits directly, because a command sitting in a queue cannot interrupt a thread already blocked inside a read.

Persistence performs filesystem work — including `fsync` — on a dedicated writer thread, never on a Tokio executor thread.

### Channels

Control and audio use different transports. Bounded command/event channels connect the application to the decode worker. A real-time-suitable SPSC ring buffer connects the worker to the callback. No bridge task is mandated; channel types are chosen to fit their endpoints, and a forwarding task is added only if one proves necessary.

Because the event channel is bounded, it is a third blocking point alongside source reads and PCM backpressure, and it needs its own contract:

- **The worker never performs an unbounded blocking send.** Event publication must be non-blocking or cancellable by the shutdown signal. A full event channel must never prevent the worker from processing stop or shutdown.
- **Progress events may coalesce.** Position updates are keep-latest: dropping intermediate values is harmless, because position is recomputed from the anchor rather than accumulated from the event stream.
- **Lifecycle and error events retain ordering and are not dropped.** State transitions, seek results, end-of-track, and errors carry information that cannot be reconstructed from a later event. If backpressure threatens these, the correct response is to widen the channel or coalesce progress harder, never to drop them.
- **A disconnected receiver means shutdown, not failure.** The worker treats it as a shutdown signal and unwinds, rather than retrying or blocking.

---

## 3. The canonical position contract

> **Position** is the session's logical resume point. It advances from estimated playback of media frames. Stop and transport recreation preserve it; restoration, media selection, explicit restart, and successful seeks establish a new position.

This position — and only this position — is reported in ordinary playback events and written to checkpoints.

### Accounting rules

Position derives from an anchor `(media_timestamp, output_frame_count, generation)` plus the frames the callback has submitted since that anchor, converted at the **output** sample rate after resampling, with resampler delay and padding accounted for.

- **Submitted is not audible.** The callback's counter measures audio handed to the device. Output latency is compensated where CPAL reports it; otherwise the position is documented as an estimate. Continuo makes no claim of sample-accurate audible position.
- **Only media frames advance position.** Silence inserted during underrun or pause does not. Silence recorded within the media does.
- **A successful seek anchors at the actual resulting media position**, not the requested target.
- **A failed seek preserves the logical position** but may leave the decoder unusable. Recovery reopens the source at that position or transitions to an error state. The position survives either outcome; the requested target is never persisted.
- **Generation tagging alone is not the safety mechanism.** Buffer invalidation, callback handoff, and progress publication form one coordinated transition. Progress is accepted only for the active generation, read as a coherent anchor/counter snapshot.

### Diagnostics

`decoded_position` and ring-buffer occupancy are engine diagnostics, not domain state. Buffering decisions read occupancy **directly** rather than differencing decoded and played timestamps, since that difference does not reliably represent queued audio.

Implemented and tested in M1. Binding as a contract from M0.

---

## 4. Domain model

Type shapes below are provisional; the semantics they encode are binding.

### Source location and media semantics are orthogonal

```rust
enum SourceLocation {
    LocalPath(PathBuf),
    Http(Url),
}
```

HTTP is a transport. It establishes neither continuity nor seekability on its own.

```rust
enum Continuity {
    Unresolved,   // not yet determined — HTTP alone cannot decide this
    Finite,
    Indefinite,
}

enum SeekSupport {
    Unknown,            // not yet probed
    Native,             // decoder seek, byte-range backed where remote
    RestartAndDiscard,  // reopen from zero and discard
    Unsupported,
}
```

`Unknown` (not yet probed) is distinct from `Unsupported` (probed, cannot seek). `Continuity::Finite` is independent of whether duration is known — duration lives in metadata as `Option<Duration>`, so "finite but duration unknown" is representable.

### Resume capability is derived, not stored

```rust
enum ResumeCapability { Supported, Unsupported, Undetermined }
```

`ResumeCapability` answers exactly one question: *can this source currently restore a saved position?* It is a method on capabilities, never a stored field, so it cannot contradict the values it derives from:

| Continuity | SeekSupport | ResumeCapability |
|---|---|---|
| `Indefinite` | any | `Unsupported` |
| `Unresolved` | any | `Undetermined` |
| `Finite` | `Unknown` | `Undetermined` |
| `Finite` | `Unsupported` | `Unsupported` |
| `Finite` | `Native` \| `RestartAndDiscard` | `Supported` |

**Binding invariant:** a checkpoint is never discarded because `ResumeCapability` is `Unsupported` or `Undetermined`. In M0 this holds structurally — `PlaybackCheckpoint` carries no capability-dependent field, so capability has no path by which to affect it. The behavioral test arrives with checkpoint storage in M2. Durable progress is independent of any transport's current opinion about it. A source that cannot seek today may be seekable tomorrow, and the recorded position remains meaningful either way.

### Media identity

```rust
enum MediaId {
    LocalFile(AbsolutePath),
    PodcastEpisode { feed: FeedId, episode: EpisodeKey },
    RemoteUrl(NormalizedUrl),
}
```

Identity requirements:

- **Canonical string form**, percent-encoding each component so `podcast:<feed>/<guid>` is unambiguous regardless of what characters appear in a feed URL or GUID.
- **Serde uses an explicit string representation** via `#[serde(into = "String", try_from = "String")]`. `Display`/`FromStr` alone do not give JSON map-key serialization; this does.
- **GUIDs are opaque.** They are never parsed, normalized, or interpreted — only escaped and compared byte-for-byte.
- **Identity normalization is never applied to the fetch URL.** The enclosure URL used to retrieve bytes preserves its query string exactly as parsed — parameter order and percent-encoding included — because CDN signatures are computed over the query. Note the precise claim: host lowercasing and default-port removal do not generally invalidate signed URLs; query rewriting does. The rule is that the fetch URL is never routed through identity normalization, not that identity normalization is inherently destructive.
- **"Not normalized" is not "byte-identical to the feed."** `Url` stores a parsed serialization, which may differ from the feed's original text (notably in percent-encoding). The contract is that Continuo applies no normalization of its own. If M4 encounters a real signed URL that `Url::parse` round-trips lossily, the remedy is to store the original string alongside the parsed value — deferred until such a case actually appears.
- **Non-UTF-8 paths are rejected** with a domain error rather than lossily converted. Documented v0.1 limitation.
- **M0 accepts a validated absolute path and performs no I/O.** `fs::canonicalize` is I/O and belongs at the source-opening boundary in M1.
- **`AbsolutePath` never collapses `..` lexically.** Collapsing `/a/link/../episode.mp3` to `/a/episode.mp3` is wrong whenever `link` is a symlink, and would attach the checkpoint to the wrong file. Rather than normalize, the constructor **rejects** paths containing `.` or `..`, so the hazard cannot be represented. M1 feeds it the output of `fs::canonicalize`, which resolves symlinks and by construction contains no parent components.

The three inner types are validating newtypes, each rejecting invalid input at construction so that an existing value is always well-formed:

- `AbsolutePath` — a UTF-8, absolute path containing no `.` or `..` components. Rejects relative input, non-UTF-8 input, and unresolved parent components. Performs no filesystem access.
- `EpisodeKey` — an opaque escaped identifier produced by the resolution order below. Never parsed or interpreted after construction.
- `NormalizedUrl` — a URL normalized for identity purposes only, per the rules below. Never used to fetch bytes.

Episode identity resolves in priority order:

1. RSS/Atom GUID (scoped to its feed)
2. Enclosure URL, normalized
3. Item `<link>`, normalized

**Identity does not imply playability.** An item with no enclosure still has stable identity and appears in listings, so `Episode { id: MediaId, source: Option<SourceLocation> }` — the absence of a playable source is representable.

URL normalization for identity lowercases scheme and host, drops the default port and the fragment, and **preserves the query string**. That preservation is precisely why GUID outranks enclosure URL: signed URLs change between fetches while the GUID does not.

### Feed identity

`FeedId` is assigned at subscription time and is immutable. The feed's fetch URL is a separate, mutable field. A permanent redirect updates the fetch URL while the `FeedId` — and therefore every episode checkpoint hanging off it — stays intact.

### Checkpoints

```rust
struct PlaybackCheckpoint {
    media: MediaId,
    position: Duration,
    updated_at: OffsetDateTime,  // RFC 3339, for human inspection only
}
```

**No merge algorithm exists, in M0 or later.** Ordering comes from the single persistence writer's accepted update sequence, with generation validation before persistence. Timestamps are not used to order updates: wall clocks move backward and collide, and a deliberate backward seek must supersede an older, larger position. `updated_at` is written for humans reading the state file.

**No near-end reset in M0.** Stopping three seconds before the end preserves those three seconds. Completion is recorded **after output drains**, as a status field separate from position — decoder EOF alone is insufficient, because audio remains buffered when the decoder finishes. Replay-from-beginning follows explicit completed status. That policy and its tests belong to M2, where a real pipeline can validate them.

---

## 5. Persistence contract

Documented in M0; implemented in M2. No filesystem I/O ships in M0.

| Location | Contents |
|---|---|
| `$XDG_CONFIG_HOME/continuo/config.toml` | User preferences |
| `$XDG_STATE_HOME/continuo/state.json` | Current media, queue, volume, checkpoints, played status — **one atomic snapshot** |
| `$XDG_DATA_HOME/continuo/subscriptions.json` | Podcast subscriptions (durable user data) |
| `$XDG_CACHE_HOME/continuo/` | Refetchable feed data |

XDG environment variables and their standard defaults are honored.

Playback state and checkpoints live in **one** snapshot rather than separate files, because separate files can disagree after a crash between writes. Subscriptions are durable user data and live separately from playback history.

Rules:

- **One writer serializes snapshots.** Updates coalesce, but a maximum write interval is enforced so continuous playback cannot postpone persistence indefinitely.
- **Atomic replacement:** write a temporary file in the destination directory, `fsync` it, `rename` it, then `fsync` the parent directory where supported. Rename alone does not guarantee durability across power loss.
- **Checkpoints are captured periodically while playing**, on pause, on stop, on track change, and after successful seeks, with pending state flushed during graceful shutdown. Periodic capture is required, not optional: without it an uninterrupted episode triggers no discrete action for hours and a crash loses all of its progress.
- **Worst-case loss is bounded end to end.** The capture interval and the writer's maximum coalescing interval together bound how much progress a crash can destroy. Both are bounded to single-digit seconds. Bounding only the writer is insufficient, since a snapshot written promptly still holds a stale position if capture never ran.
- **`schema_version` is present from the first write.** Malformed or unsupported-version files are preserved and reported, never silently overwritten.
- **Position is persisted once per media identity**; the current-media field references that identity.
- Storage lives behind a small concrete persistence module with typed load/save operations. **No repository trait.** Domain types stay free of filesystem paths and serialization details, which is separation enough for a later SQLite migration.

---

## 6. Milestone 0 deliverables

### Module layout

```
src/
  main.rs              # tracing init, minimal entry point
  lib.rs               # module declarations
  error.rs             # domain error types
  telemetry.rs         # tracing subscriber setup
  media/
    mod.rs
    id.rs              # MediaId, FeedId, EpisodeKey, AbsolutePath,
                       #   NormalizedUrl, escaping, serde
    source.rs          # SourceLocation
    capabilities.rs    # Continuity, SeekSupport, MediaCapabilities, ResumeCapability
    metadata.rs        # title, Option<Duration> for unknown-duration media
  playback/
    mod.rs
    checkpoint.rs      # PlaybackCheckpoint
docs/architecture.md
README.md
```

The crate splits into a library and a thin binary so integration tests can use it.

No `audio/`, `http/`, `podcast/`, `tui/`, or `persistence/` directories are created in M0. Each appears when the milestone that needs it arrives. Modules are not created to mirror a suggested structure.

### Dependencies

`thiserror`, `serde` (derive), `tracing`, `tracing-subscriber` (env-filter), `url`, `percent-encoding`, `time` (serde + RFC 3339).

Dev-dependencies: `serde_json`.

No `clap` — argument parsing arrives with the M1 CLI. No `anyhow` — `thiserror` throughout, to keep error modeling honest rather than stringly-typed.

### Error handling

Domain errors carry context: path or URL, media id, operation. `unsafe_code` is forbidden crate-wide. `clippy::unwrap_used` is denied, with `clippy.toml` setting `allow-unwrap-in-tests` and `allow-expect-in-tests` so the rule holds in runtime paths without poisoning test code.

At the application boundary, errors present concisely to the user while the full chain goes to structured logs.

### Logging

`tracing`, initialized in M0. Later milestones must make these diagnosable: source opened, HTTP redirect, range support, selected decoder, known duration, seek requested, actual seek result, playback state transition, checkpoint persisted, end of track, output failure. No per-frame logging.

### Tooling

- `rust-toolchain.toml` pinning Rust **1.98.1** with `rustfmt` and `clippy`. A specific release, not `channel = "stable"` — edition 2024 alone does not select a compiler version.
- `Cargo.lock` committed; Continuo is an application.
- One CI workflow on `ubuntu-latest`, triggered on push and pull request:
  - `cargo fmt --check`
  - `cargo clippy --locked --all-targets --all-features -- -D warnings`
  - `cargo test --locked`
- No `libasound2-dev` yet — M0 has no audio dependency. It is added when M1 introduces CPAL.
- No matrix, coverage service, or release automation.

### Explicitly deferred from M0

- `PlaybackCommand` and `PlaybackEvent` enums → M1
- The executable playback state machine → M1
- Channels, worker threads, callback accounting, buffer management → M1
- Detailed decoder and device errors → M1
- Checkpoint storage and completion policy → M2
- HTTP buffering adapter and capability probing → M3

Implementing the pipeline will reveal protocol details that cannot be guessed — particularly seek acknowledgment, cancellation, and output drain behavior. Testing speculative enums now would provide little assurance.

### Tests

- `ResumeCapability` across the full capability matrix, including both unresolved states
- `MediaId` string round-trip with adversarial components: `#`, `/`, `:`, `?`, and spaces in feed URLs and GUIDs
- `MediaId` serializes as a JSON **map key** and round-trips
- Episode identity priority: GUID → enclosure URL → `<link>`
- URL normalization preserves the query string, drops the fragment, strips the default port, lowercases scheme and host
- The fetch URL retains query parameter order and percent-encoding, and does not pass through identity normalization
- `AbsolutePath` rejects non-UTF-8 input, relative paths, and any path containing a `.` or `..` component
- `AbsolutePath` does not collapse `..`: `/a/link/../episode.mp3` is rejected rather than silently rewritten to `/a/episode.mp3`

---

## 7. Milestone map

| Milestone | Delivers |
|---|---|
| **M0** | Repository foundation, domain types, contracts, CI |
| **M1** | Local playback vertical slice: Symphonia + CPAL, decode thread, ring buffer, position accounting, command/event protocol, state machine, resampler choice |
| **M2** | Checkpoint persistence, stop/resume and restart/resume semantics, completion policy |
| **M3** | Finite HTTP media, capability probing, range-based seek, `RemoteFile` vs `LiveStream` |
| **M4** | RSS/Atom feeds, subscriptions, episode listing and progress |
| **M5** | Ratatui TUI over the existing application interfaces |

### Audio backend decision

M1 implements Symphonia + CPAL directly. The reason is control over buffering, cancellation, and position accounting — not a categorical limitation of Rodio, whose Symphonia backend does implement accurate seek refinement. HTTP range support belongs to the source layer and is not something CPAL provides.

No `PlaybackBackend` trait is written speculatively. Commands and events already provide the application boundary; a trait waits until a concrete testing or implementation need establishes its shape. Rodio remains a contingency if M1 reveals a real blocker, rather than a scheduled rewrite.

---

## 8. Acceptance case: Radio-T

The reference manual acceptance scenario, exercised from M3 onward. Radio-T episodes are finite MP3 files served over HTTP at URLs shaped like `https://cdn.radio-t.com/rt_podcastNNNN.mp3`.

1. Open a Radio-T RSS feed
2. Select an episode
3. Play it
4. Seek successfully, given server range support
5. Play for some time
6. Stop playback
7. Start the same episode again
8. Resume from the previous logical position, not `00:00`
9. Exit Continuo
10. Start Continuo again
11. Restore the episode and resume close to the last saved checkpoint

The episode must be treated as finite remote media, never as live radio merely because the transport is HTTP.

Automated HTTP integration tests use a local test server — never public internet connectivity — covering a range-capable finite file, a server without range support, redirects, invalid range responses, and reconnect-after-stop.

---

## 9. Known limitations for v0.1

- **Non-UTF-8 paths are unsupported.** Media identity requires UTF-8 so it can serve as a readable state-file key. Such paths produce a domain error rather than a lossy conversion.
- **Position is an estimate.** Where CPAL does not report output latency, reported position reflects audio submitted to the device rather than audio confirmed heard.
- **Seek support may remain `Unknown`** for sources that are never probed. This is represented explicitly rather than assumed.

---

## 10. Environment prerequisite

`libasound.so.2` is present on the development machine via PipeWire, but the ALSA development headers are not. `cargo build` will fail on `alsa-sys` once CPAL is introduced in M1 until `libasound2-dev` is installed. M0 has no audio dependency and is unaffected.
