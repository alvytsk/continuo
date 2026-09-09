# Continuo architecture

This document publishes the contracts approved in the [Continuo foundation spec](superpowers/specs/2026-09-07-continuo-foundation-design.md). Milestone 0 implements the domain values, canonical identities, checkpoint value, tracing setup, and repository checks. The playback engine and the other runtime behavior described below bind future milestones; they do not work in M0.

## 1. Scope and invariant

Continuo is a keyboard-first terminal audio player for local files, finite remote audio over HTTP, and podcasts delivered through RSS or Atom feeds. Transport and media semantics are modeled separately: HTTP does not by itself make media finite, seekable, resumable, or live.

> Stopping or recreating the audio transport pipeline must not implicitly reset the logical playback position.

The canonical position contract is:

> **Position** is the session's logical resume point. It advances from estimated playback of media frames. Stop and transport recreation preserve it; restoration, media selection, explicit restart, and successful seeks establish a new position.

M0 ships values, tracing, repository checks, and this documentation. M1 ships the in-session position behavior described below; M2 persists it. M3 ships finite HTTP media. Of the three transports named above, two work today: local files (M1) and finite remote audio over HTTP (M3) both play, and seek and resume where the server's capabilities support it. The third, podcasts delivered through RSS or Atom feeds, is not implemented until M4 — a remote source is given today as a direct HTTP(S) URL rather than discovered from a feed.

## 2. Execution contexts

The future runtime has four execution contexts with strict ownership:

| Context | Owns | Must not |
|---|---|---|
| Application (main thread) | The application context is the main thread: it reads keys, renders status, and owns the command sender and event receiver. It also owns an `HttpService` (`src/http/service.rs`) whose Tokio runtime has exactly one worker thread — built only when the source is an HTTP URL, not on the `--probe-only` flag: any local-file session runs with no Tokio runtime at all, `--probe-only` included, while probing a URL spawns the runtime the same as playing one would. | Hold a decoder or a CPAL stream |
| Decode thread (`std::thread`) | Symphonia demux/decode, resampling, command processing, PCM production, position anchoring, **the CPAL stream's full lifecycle** | Block on the Tokio runtime |
| CPAL callback | Drain a bounded SPSC ring buffer, emit silence on underrun, publish a frame counter | Lock, allocate, wait, or perform I/O |
| Persistence writer thread | Serialize and atomically write state snapshots | Run on Tokio's executor |
| HTTP source adapter (`HttpMediaSource`, `src/http/source.rs`, invoked from the decode thread) | Implements the pinned Symphonia `MediaSource` contract as synchronous, cancellable reads and seeks over the shared byte channel (`src/http/channel.rs`) | Call into the Tokio runtime, or block on it, directly |
| Fetch task (spawned by `HttpService` onto its own runtime) | One task per source generation: issues the request, follows redirects, streams the response body into the bounded byte channel | Own or touch decoder, resampler, or CPAL state; run more than one active fetch per source generation, or accumulate an unbounded body |

The decode worker creates, starts, pauses, and destroys the CPAL stream. The callback owns only the ring-buffer consumer and progress counter. Tokio tasks never touch decoder or output-device state, and writer filesystem work stays off Tokio.

## 3. Cancellation and channel backpressure

The rule against blocking on Tokio still permits cancellable synchronous waits on an HTTP byte buffer and PCM backpressure. Stop, seek, and shutdown must wake those waits directly. A command queued behind a blocking read cannot provide cancellation.

M3's `HttpService` bounds the application's own encoded-byte buffer (`Limits::buffer_bytes`) plus one transfer chunk on top of it (`Limits::chunk_bytes`), and that is the total this document claims. For HTTP/2, reqwest 0.13.5's `ClientBuilder::http2_initial_stream_window_size` and `http2_initial_connection_window_size` are set from `Limits::chunk_bytes` and `Limits::buffer_bytes` respectively (with `http2_adaptive_window` left at its default of `false`), so a peer cannot buffer further ahead of the client than those two caps already promise. HTTP/1.1 has no equivalent read-buffer-size knob in reqwest 0.13.5 (no `http1_max_buf_size` or similar), so on that path the HTTP/TLS library's own transport buffering is not bounded by Continuo and is not part of the stated cap — consistent with the design's requirement to document library buffering separately rather than claim a total over process memory.

Command and event channels will be bounded. The worker must never block indefinitely while publishing an event. Progress is keep-latest and intermediate progress may be coalesced. Lifecycle events, seek results, end-of-track events, and errors remain ordered and lossless. Pressure is handled by widening the bounded channel or coalescing progress more aggressively, never by dropping lifecycle or error events. A disconnected event receiver means shutdown, so the worker unwinds instead of retrying.

M1 ships this as two bounded `crossbeam-channel` queues plus a keep-latest snapshot, wired by `EngineHandle`. `PlaybackCommand` (`Load`, `Play`, `Pause`, `TogglePause`, `SeekTo`, `SeekBy`, `Restart`, `SetVolume`, `Stop`, `Shutdown`) is the application-to-worker protocol; `PlaybackEvent` (`Loaded`, `StateChanged`, `SeekCompleted`, `SeekTargetStored`, `SeekRejected`, `VolumeChanged`, `EndOfTrack`, `DeviceRecovered`, `Warning`, `Failed`) is the worker-to-application protocol, and every variant carries the `session_rev` the mirror keys its rendering on. The event channel reserves a fixed tail (`RESERVED_EVENT_SLOTS`) that ordinary events may not occupy but terminal outcomes may — `Failed`, `EndOfTrack`, and any `StateChanged` into `Stopped`, `Ended`, or `Failed` (`PlaybackEvent::is_terminal`) — so a full backlog of ordinary events can never starve the outcome the application is waiting to react to. Diagnostics (dropped spans, output xruns) do not enter this channel as individual events; they accumulate and are periodically coalesced into a single aggregated `Warning`, keeping the lossless guarantee scoped to lifecycle and error events rather than per-occurrence noise. `Progress` (`session_rev`, `media`, `position`, `quality`) is a separate `Mutex`-guarded keep-latest snapshot read with `EngineHandle::progress()`; because it is refreshed independently of the event stream, a consumer must accept a snapshot only when its `session_rev` matches the session it believes is current (see §4) rather than assuming the two stay in lockstep. No bridge task was needed: the application thread owns both endpoints directly.

M3 extends this protocol rather than replacing it. `EngineHandle::submit_*` return `Admission` (`Accepted`, `Busy`, `Gone`) instead of blocking, so a saturated command queue is visible to the caller rather than stalling the application thread. `submit_seek` publishes a `SEEK` bit in the out-of-band interrupt word alongside the existing stop and shutdown bits — a distinct bit, not a third value of the same one, because a blocked read must wake for a seek while a seek must not cancel its own first attempt; `open_transport`, ordinary reads, and `ensure_source_open` check only stop/shutdown, never `SEEK`. `SourceInterrupt` (`src/http/channel.rs`) adds `frozen`, a *level* rather than an edge: a pause persists until a play, and a read that blocks after the edge already passed must still observe it, so every wait re-tests the flag on each wake rather than trusting the one that woke it. `PlaybackEvent` gains three variants: `SeekCancelled` (an accepted seek a stop or shutdown retired before it committed — terminal, distinct from `SeekRejected`'s validation failure, so §8's "every accepted seek receives an outcome" survives a shutdown backlog), `CapabilitiesChanged` (evidence resolved after `Loaded`, most often a stopped seek's on-demand trial resolving `Unknown`; ordered and revision-keyed like `Loaded`, and never establishes or clears checkpoint protection), and `RestartEstablished` (an explicit restart that landed — `SeekCompleted` deliberately does not cover this, because `restart()` discards a stored target and seeks to zero without emitting one). `RESERVED_EVENT_SLOTS` rises from 8 to 9. The bound is the union, not the maximum, of what one worker-loop iteration can emit:

```text
  stop interrupt        1  StateChanged{Stopped}
  a serviced fault      2  Failed + StateChanged, or DeviceRecovered + StateChanged
  a dispatched command  4  Load is the widest: StateChanged{Loading}, Loaded,
                           CapabilitiesChanged, StateChanged{Paused}
  end of track          2  EndOfTrack + StateChanged{Ended}
                       --
                        9  <= RESERVED_EVENT_SLOTS
```

`CapabilitiesChanged` is what raises `Load`'s worst case from three events to four; `SeekCancelled` and `RestartEstablished` both belong to commands narrower than `Load` and do not raise the bound.

## 4. Position accounting

M1 will derive logical position from a coherent `(media_timestamp, output_frame_count, generation)` anchor and callback-submitted media frames since that anchor. Frame counts are converted at the output sample rate after resampling, with resampler delay and padding accounted for. Available CPAL output latency is subtracted. Submitted audio is not necessarily audible audio, so without latency data the reported position remains an estimate.

Only media frames advance position. Silence inserted during underrun or pause does not; silence that is part of the recording does. A successful seek anchors at the actual resulting media position. A failed seek preserves the logical position even if the decoder becomes unusable; recovery reopens the source at that position or enters an error state. The requested target is never persisted after a failed seek.

Buffer invalidation, callback handoff, and progress publication form one coordinated transition. Generation tags alone do not make that transition safe, and progress is accepted only from a coherent anchor/counter snapshot for the active generation. Buffering reads ring occupancy directly. `decoded_position` and occupancy are diagnostics rather than domain state.

Decoder EOF does not mean output has drained: buffered audio can remain after decoding finishes. Completion is recorded only after output drain.

M1 derives position from media spans rather than a shared counter. The realtime CPAL callback never locks or allocates; instead, on every fill it pushes a `SpanRecord { generation, media_total_after, t0, frames }` — the media-frame total reached and the predicted device playback instant `t0` it starts at — into a lock-free SPSC ring (`rtrb`) read by the worker. The worker's `Timeline` accepts spans tagged with the current generation, discards spans from a retired generation (a stale callback finishing after a rebuild cannot rewind position), and reconstructs `played_frames` at a queried instant by locating the span that instant falls in and interpolating within it; queried before the first span or past the last known span it clamps rather than extrapolating. An interpolated read reports `PositionQuality::Estimated`; a dropped span (the ring was full) or overlapping spans (the callback's clock moved non-monotonically) cannot be interpolated safely and report `PositionQuality::Degraded` until the next clean span arrives. Position is `PositionQuality::Exact` only immediately after an event that establishes it outright — load, restart, or a completed seek — before any span has been read back.

Transitions between callback phases (`Run`, `Freeze`, `Discard`, `Park`) are coordinated by an acknowledged handshake (`Handshake` over `OutputLink`) rather than by hoping the callback notices new state on its own. The worker publishes one `Control { generation, epoch, phase }` word; the callback reads it, acts, and hands back an acknowledgment carrying the same epoch. Every transition increments the epoch first, so a stale acknowledgment from a phase the worker already moved past is distinguishable from the current one and cannot be mistaken for it. Every wait on an acknowledgment carries a deadline (250 ms); a timeout does not mean the request failed outright, it means *attempt teardown and recovery* — the worker tears the transport down and reopens it at the preserved position, because the underlying ALSA/PipeWire/PulseAudio stream joins its backend thread on `Drop` with no timeout of its own, so the deadline bounds only the handshake wait, not end-to-end recovery.

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

`SeekSupport::RestartAndDiscard` is part of this matrix — it resolves to `ResumeCapability::Supported`, the same as `Native` — but M3 never constructs it. The evidence M3 actually combines (`SourceEvidence`, `DemuxerSeek`, both in `src/media/capabilities.rs`) is byte-seekability crossed with whether the demuxer's own seek has been demonstrated, and that combination only ever yields `Native`, `Unknown`, or `Unsupported`. Restart-and-discard — treating a byte-seekable-but-not-time-seekable demuxer as seekable by reopening from zero and discarding audio up to the target — is a real strategy, but it was scoped out of M3 (R1) rather than built and left unused: no code path attempts it, and the variant exists so a later milestone can decide whether it is worth building rather than needing a new one added to the type.

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

Persistence honors XDG environment variables and their standard defaults:

| Location | Contents |
|---|---|
| `$XDG_CONFIG_HOME/continuo/config.toml` | User preferences |
| `$XDG_STATE_HOME/continuo/state.json` | Current media, queue, volume, checkpoints, played status — **one atomic snapshot** |
| `$XDG_DATA_HOME/continuo/subscriptions.json` | Podcast subscriptions (durable user data) |
| `$XDG_CACHE_HOME/continuo/` | Refetchable feed data |

Current media references one checkpoint per media identity inside the single atomic playback snapshot. Keeping these together prevents disagreement after a crash between writes. Subscriptions remain separate durable user data. Storage is a small concrete module with typed load/save operations; there is no repository trait.

A single writer's accepted update sequence orders snapshots after generation validation. Neither timestamps nor maximum positions order updates: clocks can move backward, and a deliberate backward seek supersedes an earlier larger position. `updated_at` exists only for human inspection. There is no merge algorithm.

The application captures checkpoints periodically while playing and on pause, stop, track change, and successful seek, then flushes pending state during graceful shutdown. The capture interval and writer's maximum coalescing interval are each bounded to single-digit seconds, which bounds worst-case loss end to end. The writer creates a temporary file in the destination directory, writes and `fsync`s it, renames it over the destination, then `fsync`s the parent directory where supported.

Every snapshot carries `schema_version` from its first write. Malformed and unsupported-version files are preserved and reported rather than overwritten. `Unsupported` or `Undetermined` resume capability never deletes a checkpoint. Completed status is separate from position and is set after output drains. Replay-from-beginning requires explicit completed-status policy in M2. There is no near-end reset: stopping near the end preserves that logical position.

`docs/superpowers/specs/2026-09-08-continuo-durable-state-design.md` carries the
decisions behind all of this — completion semantics, the per-identity map and its
cap, rejected-file handling, the checkpoint triggers — and its §19 records where
the implementation amended them.

M3 adds one protection rule on top of this. When a positive checkpoint exists but capability resolution conclusively rules out resuming it, sequential playback may begin at zero instead of failing outright — and that fallback load marks the existing entry protected: periodic, pause, stop, outgoing-media and shutdown captures do not overwrite it merely because this run is playing from zero. Protection ends only on a successfully established explicit restart, a successfully established seek, or verified completion. This deliberately favors recovering the earlier resume point over saving progress from a fallback run — it is not a maximum-position merge rule, and even later progress that exceeds the protected position does not replace it while protection holds.

## 7. Diagnostics and errors

Tracing is implemented in M0 and controlled through `RUST_LOG`, for example `RUST_LOG=continuo=debug cargo run --locked`. Logs go to stderr. Without `RUST_LOG` the default filter is `continuo=info`. An invalid filter, or a `RUST_LOG` value that is not valid Unicode, is reported as a startup failure and exits nonzero rather than being silently ignored. Later milestones must log source opened, redirects, range support, selected decoder, known duration, requested and actual seek results, playback state transitions, checkpoint writes, end of track, and output failures. Per-frame logging is forbidden.

Errors carry typed context such as the path or URL, media identity, and operation. The application boundary presents concise text to the user while the full error chain goes to structured logs. Position remains estimated when the device cannot report output latency.

M3 gives `PlaybackEvent::Failed` a typed `cause: Option<RemoteFailure>` beside its existing `message: String`. `RemoteFailure` (`src/http/error.rs`) is one variant per §11 category — invalid source, HTTP status, redirect rejection, timeout, invalid range, resource change, truncated body, probe limit, unresolved continuity, unsupported live media, seek/resume unavailable, non-identity content encoding, transport, and cancellation — so a policy can act on what went wrong rather than only read a string a human wrote. `Display` on every variant is third-party-safe: every URL reaching it has already passed through `redact_url`, so no signed query or userinfo can reach a status line, a log line, or a `Failed` message. This narrows rather than closes M1's known-debt finding about stringly-typed failures: `cause` is `None` for every local (non-remote) fault, so `message` remains the only thing those carry. See `docs/m1-known-debt.md`'s Milestone 3 section for the remaining rough edges in `RemoteFailure` itself.

Runtime code forbids unsafe code and denies `unwrap` and `expect`. Tests may use them for assertions and fixtures; where Clippy does not recognize a bare integration-test helper as test code, its exception is scoped to that fixed fixture helper rather than weakening runtime lint policy.

## 8. Milestones and backend

| Milestone | Delivers |
|---|---|
| **M0** | Repository foundation, domain types, contracts, CI |
| **M1** | Local playback vertical slice: Symphonia + CPAL, decode thread, ring buffer, position accounting, command/event protocol, state machine, resampler choice |
| **M2** | Checkpoint persistence, stop/resume and restart/resume semantics, completion policy |
| **M3** | Finite HTTP media, capability probing, range-based seek, `RemoteFile` vs `LiveStream` — shipped |
| **M4** | RSS/Atom feeds, subscriptions, episode listing and progress |
| **M5** | Ratatui TUI over the existing application interfaces |

M0 explicitly defers `PlaybackCommand` and `PlaybackEvent`, the executable state machine, channels, worker threads, callback accounting, buffer management, detailed decoder and device errors, checkpoint storage, completion policy, HTTP buffering, and capability probing. Playback, persistence, HTTP fetching, feeds, subscriptions, and the TUI are not implemented today.

M1 used Symphonia and CPAL directly to control buffering, cancellation, and position accounting. This choice does not claim that Rodio cannot seek; Rodio's Symphonia backend implements accurate seek refinement. HTTP range support belongs to the source layer, not CPAL. Commands and events will form the application boundary, so no speculative backend trait is introduced. Rodio remains a contingency if M1 uncovers a concrete blocker.

The v0.1 scope excludes Spotify, YouTube/yt-dlp, SoundCloud, Jellyfin, Plex, Navidrome, a visualizer, equalizer or DSP, themes, plugins, a daemon/client split, and remote control. MPRIS and media keys are deferred until the core is stable. Known limitations are non-UTF-8 paths, estimated position where device latency is unavailable, and seek support that can remain `Unknown` until probed.

M1 requires ALSA development headers (`libasound2-dev`) on Linux when CPAL is introduced; the runtime `libasound.so.2` alone is insufficient. M0 has no audio dependency and does not require them.

### M3's fixed limits

`src/http/limits.rs`'s `Limits::default()` fixes the numbers M3 ships with: 10 s to connect, 15 s awaiting response headers, 15 s with no data while actively demanding it, 30 s for the whole of opening and probing, an 8 MiB cap on what a probe may consume, a 1 MiB encoded-byte buffer, one 64 KiB application transfer chunk on top of it, and at most 5 redirects.

The 1 MiB buffer plus the one 64 KiB chunk is the *application's* bound, not a claim about total process memory. reqwest and rustls hold buffering of their own on top of it — see §3 above for exactly what that library buffering does and does not have a corresponding cap in this version of reqwest — and this number does not include theirs.

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

`docs/m3-acceptance.md` carries the full H1–H18 coverage map: which test discharges each of the design's acceptance items, and, for the two that are only partially discharged, exactly why.
