# Continuo architecture

This document publishes the contracts approved in the [Continuo foundation spec](superpowers/specs/2026-09-07-continuo-foundation-design.md). Milestone 0 implements the domain values, canonical identities, checkpoint value, tracing setup, and repository checks. The playback engine and the other runtime behavior described below bind future milestones; they do not work in M0.

## 1. Scope and invariant

Continuo is a keyboard-first terminal audio player for local files, finite remote audio over HTTP, and podcasts delivered through RSS or Atom feeds. Transport and media semantics are modeled separately: HTTP does not by itself make media finite, seekable, resumable, or live.

> Stopping or recreating the audio transport pipeline must not implicitly reset the logical playback position.

The canonical position contract is:

> **Position** is the session's logical resume point. It advances from estimated playback of media frames. Stop and transport recreation preserve it; restoration, media selection, explicit restart, and successful seeks establish a new position.

M0 ships values, tracing, repository checks, and this documentation. M1 will implement the in-session position behavior, and M2 will persist it.

## 2. Execution contexts

The future runtime has four execution contexts with strict ownership:

| Context | Owns | Must not |
|---|---|---|
| Tokio tasks | Application orchestration, HTTP fetching, feed parsing, timers | Touch decoder or output-device state |
| Decode thread (`std::thread`) | Symphonia demux/decode, resampling, command processing, PCM production, position anchoring, **the CPAL stream's full lifecycle** | Block on the Tokio runtime |
| CPAL callback | Drain a bounded SPSC ring buffer, emit silence on underrun, publish a frame counter | Lock, allocate, wait, or perform I/O |
| Persistence writer thread | Serialize and atomically write state snapshots | Run on Tokio's executor |

The decode worker creates, starts, pauses, and destroys the CPAL stream. The callback owns only the ring-buffer consumer and progress counter. Tokio tasks never touch decoder or output-device state, and writer filesystem work stays off Tokio.

## 3. Cancellation and channel backpressure

The rule against blocking on Tokio still permits cancellable synchronous waits on an HTTP byte buffer and PCM backpressure. Stop, seek, and shutdown must wake those waits directly. A command queued behind a blocking read cannot provide cancellation.

Command and event channels will be bounded. The worker must never block indefinitely while publishing an event. Progress is keep-latest and intermediate progress may be coalesced. Lifecycle events, seek results, end-of-track events, and errors remain ordered and lossless. Pressure is handled by widening the bounded channel or coalescing progress more aggressively, never by dropping lifecycle or error events. A disconnected event receiver means shutdown, so the worker unwinds instead of retrying.

M1 will choose concrete channels and protocol enums. No bridge task is assumed; one is added only if the chosen endpoints require it.

## 4. Position accounting

M1 will derive logical position from a coherent `(media_timestamp, output_frame_count, generation)` anchor and callback-submitted media frames since that anchor. Frame counts are converted at the output sample rate after resampling, with resampler delay and padding accounted for. Available CPAL output latency is subtracted. Submitted audio is not necessarily audible audio, so without latency data the reported position remains an estimate.

Only media frames advance position. Silence inserted during underrun or pause does not; silence that is part of the recording does. A successful seek anchors at the actual resulting media position. A failed seek preserves the logical position even if the decoder becomes unusable; recovery reopens the source at that position or enters an error state. The requested target is never persisted after a failed seek.

Buffer invalidation, callback handoff, and progress publication form one coordinated transition. Generation tags alone do not make that transition safe, and progress is accepted only from a coherent anchor/counter snapshot for the active generation. Buffering reads ring occupancy directly. `decoded_position` and occupancy are diagnostics rather than domain state.

Decoder EOF does not mean output has drained: buffered audio can remain after decoding finishes. Completion is recorded only after output drain.

## 5. Identity and capabilities

Source location, continuity, and seek support are orthogonal. `SourceLocation` identifies a local path or HTTP URL. `Continuity::Unresolved` means continuity is not known yet; it differs from `Indefinite`. `SeekSupport::Unknown` means no conclusive probe has happened; it differs from `Unsupported`, which records a negative result. `Continuity::Finite` remains valid when `MediaMetadata::duration` is `None`.

Resume capability is derived from continuity and seek support rather than stored:

| Continuity | Seek support | Resume capability |
|---|---|---|
| `Indefinite` | any | `Unsupported` |
| `Unresolved` | any | `Undetermined` |
| `Finite` | `Unknown` | `Undetermined` |
| `Finite` | `Unsupported` | `Unsupported` |
| `Finite` | `Native` or `RestartAndDiscard` | `Supported` |

M0 implements this matrix as `MediaCapabilities::resume_capability`.

M0 implements the validating identity types. `FeedId` is a nonempty opaque string assigned by the future subscription layer; it is immutable and is never derived from the mutable feed fetch URL. `EpisodeKey` distinguishes an opaque nonempty GUID from an identity-normalized enclosure or item-link URL. GUIDs are preserved and compared byte-for-byte, including whitespace. Resolution prefers GUID, then enclosure URL, then item link. Enclosure and item-link fallback share the URL namespace, while a GUID that happens to equal a URL cannot collide with them.

`NormalizedUrl` accepts HTTP or HTTPS with a host. The URL parser normalizes scheme, host, and default port; identity normalization removes fragments and preserves query serialization. It is used only for identity. `SourceLocation::Http` retains a separate parsed fetch `Url`, including its fragment and query order and percent-encoding as serialized by `url`; Continuo does not route it through identity normalization. Parsed serialization can differ from the original feed text. If a real signed URL later proves lossy through parsing, M4 may retain the original string alongside the parsed value.

`AbsolutePath` validates without filesystem I/O. It accepts UTF-8 absolute paths and rejects raw `.` or `..` segments, repeated separators, and trailing separators except filesystem roots. It never collapses or canonicalizes paths, because lexical collapse can cross a symlink incorrectly. M1 supplies output from `fs::canonicalize` at the source-opening boundary. Non-UTF-8 paths are an explicit v0.1 limitation.

Canonical `MediaId` strings use this readable grammar:

```text
local:<escaped-path>
remote:<escaped-url>
podcast:<escaped-feed>/<escaped-episode-key>

<episode-key-before-outer-escaping> = guid:<opaque-guid> | url:<normalized-url>
```

Controls, spaces, double quotes, backslashes, percent signs, and non-ASCII UTF-8 bytes are percent-encoded in every component. Podcast components also encode `/`, their separator; local and remote bodies extend to the end and keep `/` readable. Ordinary ASCII punctuation stays readable. Parsing rejects invalid UTF-8 and noncanonical encodings by parsing and requiring serialization to reproduce the input exactly. Explicit string serde supports identity values and JSON map keys.

`Episode` holds a `MediaId` and an optional source, so a feed item without an enclosure can retain stable identity and appear in listings without claiming playability. Feed redirects may change a subscription's fetch URL without changing its assigned `FeedId` or episode checkpoints.

## 6. Durable state (M2)

Persistence is a future M2 subsystem. It will honor XDG environment variables and their standard defaults:

| Location | Contents |
|---|---|
| `$XDG_CONFIG_HOME/continuo/config.toml` | User preferences |
| `$XDG_STATE_HOME/continuo/state.json` | Current media, queue, volume, checkpoints, played status — **one atomic snapshot** |
| `$XDG_DATA_HOME/continuo/subscriptions.json` | Podcast subscriptions (durable user data) |
| `$XDG_CACHE_HOME/continuo/` | Refetchable feed data |

Current media references one checkpoint per media identity inside the single atomic playback snapshot. Keeping these together prevents disagreement after a crash between writes. Subscriptions remain separate durable user data. Storage will be a small concrete module with typed load/save operations; there is no repository trait.

A single writer's accepted update sequence orders snapshots after generation validation. Neither timestamps nor maximum positions order updates: clocks can move backward, and a deliberate backward seek supersedes an earlier larger position. `updated_at` exists only for human inspection. There is no merge algorithm.

The application will capture checkpoints periodically while playing and on pause, stop, track change, and successful seek, then flush pending state during graceful shutdown. The capture interval and writer's maximum coalescing interval are each bounded to single-digit seconds, which bounds worst-case loss end to end. The writer will create a temporary file in the destination directory, write and `fsync` it, rename it over the destination, then `fsync` the parent directory where supported.

Every snapshot will have `schema_version` from its first write. Malformed and unsupported-version files are preserved and reported rather than overwritten. `Unsupported` or `Undetermined` resume capability never deletes a checkpoint. Completed status is separate from position and is set after output drains. Replay-from-beginning requires explicit completed-status policy in M2. There is no near-end reset: stopping near the end preserves that logical position.

## 7. Diagnostics and errors

Tracing is implemented in M0 and controlled through `RUST_LOG`, for example `RUST_LOG=continuo=debug cargo run --locked`. Logs go to stderr. Without `RUST_LOG` the default filter is `continuo=info`. An invalid filter, or a `RUST_LOG` value that is not valid Unicode, is reported as a startup failure and exits nonzero rather than being silently ignored. Later milestones must log source opened, redirects, range support, selected decoder, known duration, requested and actual seek results, playback state transitions, checkpoint writes, end of track, and output failures. Per-frame logging is forbidden.

Errors carry typed context such as the path or URL, media identity, and operation. The application boundary presents concise text to the user while the full error chain goes to structured logs. Position remains estimated when the device cannot report output latency.

Runtime code forbids unsafe code and denies `unwrap` and `expect`. Tests may use them for assertions and fixtures; where Clippy does not recognize a bare integration-test helper as test code, its exception is scoped to that fixed fixture helper rather than weakening runtime lint policy.

## 8. Milestones and backend

| Milestone | Delivers |
|---|---|
| **M0** | Repository foundation, domain types, contracts, CI |
| **M1** | Local playback vertical slice: Symphonia + CPAL, decode thread, ring buffer, position accounting, command/event protocol, state machine, resampler choice |
| **M2** | Checkpoint persistence, stop/resume and restart/resume semantics, completion policy |
| **M3** | Finite HTTP media, capability probing, range-based seek, `RemoteFile` vs `LiveStream` |
| **M4** | RSS/Atom feeds, subscriptions, episode listing and progress |
| **M5** | Ratatui TUI over the existing application interfaces |

M0 explicitly defers `PlaybackCommand` and `PlaybackEvent`, the executable state machine, channels, worker threads, callback accounting, buffer management, detailed decoder and device errors, checkpoint storage, completion policy, HTTP buffering, and capability probing. Playback, persistence, HTTP fetching, feeds, subscriptions, and the TUI are not implemented today.

M1 will use Symphonia and CPAL directly to control buffering, cancellation, and position accounting. This choice does not claim that Rodio cannot seek; Rodio's Symphonia backend implements accurate seek refinement. HTTP range support belongs to the source layer, not CPAL. Commands and events will form the application boundary, so no speculative backend trait is introduced. Rodio remains a contingency if M1 uncovers a concrete blocker.

The v0.1 scope excludes Spotify, YouTube/yt-dlp, SoundCloud, Jellyfin, Plex, Navidrome, a visualizer, equalizer or DSP, themes, plugins, a daemon/client split, and remote control. MPRIS and media keys are deferred until the core is stable. Known limitations are non-UTF-8 paths, estimated position where device latency is unavailable, and seek support that can remain `Unknown` until probed.

M1 requires ALSA development headers (`libasound2-dev`) on Linux when CPAL is introduced; the runtime `libasound.so.2` alone is insufficient. M0 has no audio dependency and does not require them.

## 9. Future acceptance

The reference manual Radio-T scenario is:

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

The complete feed-driven scenario becomes executable in M4. M3 exercises its finite HTTP playback portion. The episode must remain finite remote media rather than becoming live radio merely because it uses HTTP.

Automated HTTP integration tests use a local test server and have no public-network dependency. They cover range-capable finite files, servers without range support, redirects, invalid range responses, and reconnect-after-stop.
