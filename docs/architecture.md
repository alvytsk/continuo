# Continuo architecture

This document publishes the contracts approved in the [Continuo foundation spec](superpowers/specs/2026-09-07-continuo-foundation-design.md). Milestone 0 implements the domain values, canonical identities, checkpoint value, tracing setup, and repository checks. The playback engine and the other runtime behavior described below bind future milestones; they do not work in M0.

## 1. Scope and invariant

Continuo is a keyboard-first terminal audio player for local files, finite remote audio over HTTP, and podcasts delivered through RSS or Atom feeds. Transport and media semantics are modeled separately: HTTP does not by itself make media finite, seekable, resumable, or live.

> Stopping or recreating the audio transport pipeline must not implicitly reset the logical playback position.

The canonical position contract is:

> **Position** is the session's logical resume point. It advances from estimated playback of media frames. Stop and transport recreation preserve it; restoration, media selection, explicit restart, and successful seeks establish a new position.

A position also carries **provenance** (`PositionProvenance`, `src/playback/provenance.rs`): whether its absolute media time was decoder-confirmed (`Established`) or derived from a byte-offset estimate (`Estimated`). This is a second axis alongside `PositionQuality` (§4), never merged into it and never a fourth quality state — quality reports how precisely *heard* playback is known, reconstructed from the output callback's spans; provenance reports whether the media time itself is trustworthy. The two compose independently: a `Degraded` position (a timing base that jumped) whose media time was decoder-established is still `Established`, and an estimated landing that is playing normally is `Estimated` regardless of quality. Provenance is sticky — decoding forward from an estimated landing keeps reporting `Estimated`; no amount of elapsed playback converts it to `Established`, only an event that independently re-establishes the absolute position does (a confirmed seek landing, an established restart, or a fresh load). An estimated position may drive display and resume (§6), but must never replace an established checkpoint for the same media, and no accuracy figure is promised for it — "estimated" means the true media time may be in a substantially different part of the recording, not "approximately right."

M0 ships values, tracing, repository checks, and this documentation. M1 ships the in-session position behavior described below; M2 persists it. M3 ships finite HTTP media, and M4 feeds and subscriptions. All three transports named above now work: local files (M1), finite remote audio over HTTP given as a direct URL (M3), and podcast episodes discovered from an RSS or Atom feed (M4). M4 changed nothing about the third one's playback — an episode is played by handing the existing engine the enclosure URL the feed named, under the episode's own identity.

M5 ships `continuo tui`, a Ratatui terminal player over the same engine, `Session` and `library` functions, designed in [the M5 spec](superpowers/specs/2026-09-14-continuo-ratatui-design.md). It adds a persistent queue, a correlated load contract between the application and the engine, a profile lock, an fd-2 log redirect, contained background jobs, cover art and a spectrum analyzer. Legacy `continuo play` keeps its interface and gains the lock and the signal contract. The sections below are extended where M5 changes them.

## 2. Execution contexts

The runtime has these execution contexts, with strict ownership:

| Context | Owns | Must not |
|---|---|---|
| Application (main thread) | The application context is the main thread: it reads keys, renders status, and owns the command sender and event receiver. It also owns an `HttpService` (`src/http/service.rs`) whose Tokio runtime has exactly one worker thread — built only when the source is an HTTP URL, not on the `--probe-only` flag: any local-file session runs with no Tokio runtime at all, `--probe-only` included, while probing a URL spawns the runtime the same as playing one would. | Hold a decoder or a CPAL stream |
| Decode thread (`std::thread`) | Symphonia demux/decode, resampling, command processing, PCM production, position anchoring, **the CPAL stream's full lifecycle** | Block on the Tokio runtime |
| CPAL callback | Drain a bounded SPSC ring buffer, emit silence on underrun, publish a frame counter | Lock, allocate, wait, or perform I/O |
| Persistence writer thread | Serialize and atomically write state snapshots | Run on Tokio's executor |
| HTTP source adapter (`HttpMediaSource`, `src/http/source.rs`, invoked from the decode thread) | Implements the pinned Symphonia `MediaSource` contract as synchronous, cancellable reads and seeks over the shared byte channel (`src/http/channel.rs`) | Call into the Tokio runtime, or block on it, directly |
| Fetch task (spawned by `HttpService` onto its own runtime) | One task per source generation: issues the request, follows redirects, streams the response body into the bounded byte channel | Own or touch decoder, resampler, or CPAL state; run more than one active fetch per source generation, or accumulate an unbounded body |
| Feed application (`src/library.rs`, M4) | The seven functions a listing, a subscription change or an episode resolution is made of: reads `subscriptions.json`, the feed cache and the checkpoint snapshot, decides what to commit, returns values | Construct an `EngineHandle`, an `AudioOutput` or a terminal; contain a `block_on`; print or format anything |
| Feed presentation (`src/commands.rs`, M4) | The only place a feed command's columns, its `—`/`never`/`played` spellings and its exit status are decided, and the **one** synchronous bridge into the HTTP runtime | Decide what to fetch or commit; open a device |
| Document fetch (spawned by `HttpService` onto the same runtime, M4) | One bounded whole-document GET per call, alongside — never instead of — the streaming media fetch above: its own manual redirect loop, its own conditional headers, and a body capped by `Limits::document_bytes` that is buffered entirely in memory before it is parsed | Stream into the byte channel, outlive its caller's `await`, or accumulate a body past the cap |
| TUI application thread (`src/tui/mod.rs` over `PlayerRuntime`, `src/application/runtime.rs`, M5) | Under `continuo tui` the main thread is this context: it reads terminal input, turns it into `AppCommand`s, pumps the runtime (finished enrichment, then engine events, then progress), and draws. `PlayerRuntime` owns the `EngineHandle` (created on the first load), `Session`, the writer handle, the `HttpService` and the metadata workers; the TUI owns the artwork, browse and spectrum front ends and the terminal. It resolves a podcast entry against the local feed cache just before loading it — a bounded local read, never a fetch. | Mutate `PersistedState` except through `Session`; decode media, read directories, probe tags or decode artwork inline; block on a worker's result |
| Signal listener (`continuo-signal-listener`, `src/lifecycle/signals.rs`, M5) | The `signal-hook` iterator for SIGINT, SIGHUP and SIGTERM: records the first signal number, sets the shutdown flag, and wakes the application. Installed before the lock and state load; closed and joined in teardown. Shared by `tui` and `play`. | Render, load state, flush, or run anything inside a raw signal handler |
| Metadata workers ×2 (`continuo-metadata-1`, `-2`, `src/application/enrich.rs`, M5) | Tag probes for untitled local queue entries, each probe a disposable job inside `run_contained`; results go back over a bounded channel with a cancellation generation | Touch `Session`, the terminal or the network; probe a remote or podcast entry; build a snapshot |
| Artwork worker (`continuo-artwork`, `src/artwork/worker.rs`, M5) | One latest-wins request slot: resolves and decodes the active entry's cover — a local file's embedded front cover, then `cover.jpg`, `cover.png`, `folder.jpg`, `folder.png`; a podcast episode's cached `itunes:image` URL, fetched as one whole document through the `HttpService` playback already opened (`application::podcast::podcast_artwork`); failing that, and for a plain URL, the front cover the decoder read from the loaded stream's tag (`MediaMetadata::front_cover`, on `Loaded`) — within 10 MiB encoded and 16 million decoded pixels, each job inside `run_contained`. Known limitation: Symphonia 0.6.1 reads an embedded picture into memory in full while probing the container, before Continuo can apply the 10 MiB check, so that limit bounds what is kept and decoded, not what the probe reads | Encode for the terminal (the application thread does that, also contained), open its own network connection, read a stream's tag itself, or touch `Session` |
| Spectrum worker (`continuo-spectrum`, `src/playback/spectrum/worker.rs`, M5) | One per `EngineHandle`: reads the output taps' bounded rings, labels blocks through the transport mapping registry, runs the 2048-sample Hann FFT, and publishes at most 20 frames per second into a latest-value slot. Blocks on its control channel while analysis is disabled. | Block or allocate on the callback's behalf, publish a frame under an unknown or retired mapping, or catch its own panics |
| Browse worker (`continuo-browse`, `src/application/browse.rs`, M5) | The browser's reads, one request at a time: one directory level, the subscription list, one feed's cached episodes | Refresh or fetch a feed, recurse into a library, or touch `Session` |

The decode worker creates, starts, pauses, and destroys the CPAL stream. The callback owns only the ring-buffer consumer and progress counter. Tokio tasks never touch decoder or output-device state, and writer filesystem work stays off Tokio.

M5's callback also writes post-gain PCM into an optional output tap: a preallocated ring for whole channel frames plus a descriptor ring, filled by reserving capacity in both first and dropping the whole block when either is short. Writing a tap stays allocation-free, lock-free and I/O-free, and nothing on that path ever waits for the spectrum worker.

**Only artwork and metadata jobs are contained** (spec §9, §11). A contained job runs inside `lifecycle::panic::run_contained`, which sets a thread-local flag under a scoped guard around a `catch_unwind`; the panic hook sees the flag, writes one sanitized `contained panic in background job` line to the session log, and returns without touching the terminal, fd 2 or the shutdown flag, and the job reports an ordinary failure (a placeholder cover, or an entry that keeps its file name). Every other panic — on the application thread, the signal listener, the browse worker, the decode thread, or in the metadata workers' own loop outside a job — is fatal. **The spectrum worker deliberately has no containment.** It belongs to the engine, and the spec reserves containment for disposable decode/probe jobs: it forbids applying the flag to the audio engine or to workers' shared-state orchestration, and §12 requires that panics outside those job boundaries still take the fatal path. A panic anywhere in the spectrum worker, the FFT included, therefore ends that thread and fails the terminal player through the panic hook. The engine's own shutdown still completes: stopping the spectrum thread only sets a flag, sends on a channel whose failure is ignored, and joins a thread that has already exited, logging rather than propagating its panic.

M4 adds the two feed contexts without adding a thread. `library.rs` is the application seam M5 reuses: it returns values and never prints, and its three public network functions (`subscribe`, `refresh`, `refresh_all`) are `async` so that a caller decides where they run. `commands.rs` is the only caller today, and it enters the runtime in exactly one place (`wait_http`), so `block_on` exists at one line of the program rather than being spread through the layer that decides what to fetch.

The document fetch is a second shape of HTTP work, not a second HTTP stack: it reuses the same `HttpService`, the same `Limits`, and the same `accept_redirect` policy (hop cap, loop detection, scheme check, HTTPS→HTTP downgrade refusal) as streaming media. The difference is what it does with the body — a feed is small, is needed whole before it can be parsed, and is therefore read to a capped buffer instead of into `ByteChannel`. Nothing about the streaming path changed to make room for it.

**`library.rs`'s `async` functions still do synchronous filesystem work.** Reading `subscriptions.json`, reading and atomically replacing a cache file, and `fsync`ing both happen on whatever thread polls the future, between `await` points. Today that is harmless: `commands.rs` blocks a `main` that has nothing else to do. It would not be harmless in M5's event loop, which must stay responsive to keystrokes. M5 resolves this by not calling them there: the terminal player never subscribes or refreshes (feed refresh stays a manual CLI command), the browser's subscription and episode listings run on the browse worker, and the only feed read on the application thread is the pre-load podcast lookup noted in the table above.

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

**M5 correlates every load with a caller-supplied token** (spec §6, implementation decisions 2–5). `PlaybackCommand::Load` carries a required `LoadRequestId`, echoed unchanged in `Loaded`, in `StateChanged { state: Loading, .. }`, and in `Failed { request: Some(_) }` for a load's own failure; general state and failure events carry `request: None`. `Session` allocates the tokens (process-lifetime unique, never derived from `MediaId`, `session_rev` or a queue index) and registers `(token, queue entry or legacy-play target, MediaId)` before submission, withdrawing the registration when admission answers `Busy` or `Gone`. At most `MAX_PENDING_LOADS` (16) registrations may be outstanding; the application reports busy beyond that, independently of engine admission.

Adoption happens **only** on a `Loaded` for a registered token whose entry still exists and matches the event's media. Two pending loads of one `MediaId` from different rows therefore adopt their own rows in engine event order, a reorder while loading cannot change the target, and a removed pending target is invalidated so its late outcome cannot resurrect the entry (any resulting playback is stopped). `Progress` carries `load: Option<LoadRequestId>` — set when the worker emits `Loaded`, kept across stop, pause and device recovery, cleared when the next load starts — and `Session::tick` ignores progress whose token is not the adopted one, so progress for an unadopted load never updates the previous checkpoint. Completion advances the queue once per `(adopted token, session_rev)`, and only when the latest `Loaded` the session observed carried the adopted token. Automatic start is scoped the same way: `PlaybackCommand::PlayLoaded { request }` plays only while the worker still owns that token and is paused after a successful open, so a failed load never triggers an implicit reopen and an older start cannot affect a newer load.

The protected outcomes are `Loaded`, `LoadCancelled` (an accepted load interrupted before `Loaded` or failure, including loads discarded during shutdown) and `Failed { request: Some(_) }`. Each accepted load has exactly one of them. They bypass `PENDING_CAP`, may occupy the reserved tail, and survive in the shutdown report. The backlog stays bounded because admission closes while `pending_events` is nonempty, so one pass dispatches at most one load. The reserve therefore only has to hold the **terminal-or-protected** share of a pass, and that is what `engine.rs`'s comment block now tabulates:

```text
  stop interrupt        1  StateChanged{Stopped}
  a fatal fault         2  Failed + StateChanged{Failed}
  a dispatched load     3  Loaded + Failed + StateChanged{Failed}
  end of track          2  EndOfTrack + StateChanged{Ended}
                       --
                        8  <= RESERVED_EVENT_SLOTS (9)
```

Ordinary events that do not fit (a `Loading` notification, `CapabilitiesChanged`, a `Paused` after a successful open) wait in `pending_events` until the reserve clears, under the existing `PENDING_CAP` drop-and-displace policy for non-protected events; a cancelled load emits only `LoadCancelled`. `tests/m5_engine_load_outcomes.rs` saturates the channel and asserts exactly one ordered outcome per accepted load, and `tests/m5_session_adoption.rs` covers the adoption rules.

## 4. Position accounting

M1 will derive logical position from a coherent `(media_timestamp, output_frame_count, generation)` anchor and callback-submitted media frames since that anchor. Frame counts are converted at the output sample rate after resampling, with resampler delay and padding accounted for. Available CPAL output latency is subtracted. Submitted audio is not necessarily audible audio, so without latency data the reported position remains an estimate.

Only media frames advance position. Silence inserted during underrun or pause does not; silence that is part of the recording does. A successful seek anchors at the actual resulting media position. A failed seek preserves the logical position even if the decoder becomes unusable; recovery reopens the source at that position or enters an error state. The requested target is never persisted after a failed seek.

`DecodedSource::seek_refined` (`src/playback/decode.rs`) requests `SeekMode::Coarse` uniformly, for every format and every seek. What that changes depends on which demuxer runs: across this crate's whole dependency tree, exactly one `FormatReader::seek` reads its `mode` argument at all — MP3's `MpaReader` (`symphonia-bundle-mp3-0.6.1/src/demuxer.rs:232`). FLAC, WAV, ISO-BMFF/AAC, OGG and MKV all take `_mode` and ignore it, so a seek on any of those formats runs identical code and lands exactly as decoder-confirmed as it always did — its provenance is `Established`. MP3 frames carry no timestamp of their own, so `MpaReader`'s `Coarse` branch (`preseek_coarse`) divides a byte ratio from the track's declared-or-estimated frame count and lands on an estimate; FLAC frames carry absolute sample numbers, so FLAC's own seek binary-searches against real timestamps and needs no estimate at all. A landing produced by MP3's `Coarse` branch reports `PositionProvenance::Estimated`, unconditionally — never conditioned on whether a Xing/Info/VBRI tag was present, since nothing observable at seek time distinguishes a file where the resulting estimate happens to be exact from one where it is not.

The reason every format is asked for `Coarse` rather than only MP3: before this change, every seek requested `SeekMode::Accurate`, whose `preseek_accurate` rewinds to the first packet and rescans forward whenever a seek looks backward relative to the demuxer's own *read* position (`next_packet_ts`) — which runs ahead of audible playback by the PCM ring plus `MediaSourceStream`'s own read-ahead, so a seek that is forward in audible terms can still be backward in demuxer terms and trigger the rewind. That rescan parses frame headers without decoding them, executes inside one uncancellable, unbounded `FormatReader::seek()` call, and blocks the worker from dispatching any command while it runs — which is what made pause and resume appear dead. `Coarse` computes a byte offset directly instead of rewinding to the start, which is what removes the rescan: measured against the file that reproduced this, `Coarse` consumed 3,072 bytes against the 983,040 bytes the `Accurate` rescan it replaces consumed for the same seek — roughly a 320× reduction.

A seek's deadline bounds source **I/O**: reads `HttpMediaSource` issues against the network while a seek is in flight. It does not bound demuxer work performed over bytes the stream has already buffered above it — `MediaSourceStream` holds a 64 KiB ring, and MP3 frame headers can be parsed out of that buffer with no read reaching the source at all. `Coarse` still calls `FormatReader::seek()`; it is not eliminated, and the deadline still cannot see inside that call. What `Coarse` changes is how much work the call does — a resync-and-walk over a few kilobytes rather than a rescan over the whole prefix — which shrinks the exposure by roughly the same 320× measured above, but does not remove it: a call that touches only buffered bytes is invisible to a read-level deadline regardless of how short that call now is.

Buffer invalidation, callback handoff, and progress publication form one coordinated transition. Generation tags alone do not make that transition safe, and progress is accepted only from a coherent anchor/counter snapshot for the active generation. Buffering reads ring occupancy directly. `decoded_position` and occupancy are diagnostics rather than domain state.

Decoder EOF does not mean output has drained: buffered audio can remain after decoding finishes. Completion is recorded only after output drain.

M1 derives position from media spans rather than a shared counter. The realtime CPAL callback never locks or allocates; instead, on every fill it pushes a `SpanRecord { generation, media_total_after, t0, frames }` — the media-frame total reached and the predicted device playback instant `t0` it starts at — into a lock-free SPSC ring (`rtrb`) read by the worker. The worker's `Timeline` accepts spans tagged with the current generation, discards spans from a retired generation (a stale callback finishing after a rebuild cannot rewind position), and reconstructs `played_frames` at a queried instant by locating the span that instant falls in and interpolating within it; queried before the first span or past the last known span it clamps rather than extrapolating. An interpolated read reports `PositionQuality::Estimated`; a dropped span (the ring was full) or overlapping spans (the callback's clock moved non-monotonically) cannot be interpolated safely and report `PositionQuality::Degraded` until the next clean span arrives. Position is `PositionQuality::Exact` only immediately after an event that establishes it outright — load, restart, or a completed seek — before any span has been read back.

A burst of arrow-key presses is coalesced into one seek before any of it reaches the engine (`KeyRouter`, `src/app.rs`). Each press resolves an absolute target on the application thread, and the mirror it resolves against only advances when the worker publishes progress — which the worker does not do while it is inside a seek, reopening a range request and buffering. Resolving each press independently against that frozen position therefore produced N identical `SeekTo` commands: the listener moved one step however many times they pressed, and each command retired the fetch its predecessor had just started. `KeyRouter` accumulates onto the target already displayed rather than re-reading the mirror, so presses compose, and holds the burst for a 250 ms quiet window, so one fetch is spent on the burst rather than one per press. Key repeat delivers a held arrow well inside that window, making hold-to-scrub the same mechanism as press-to-step.

The displayed position jumps to the accumulated target on the press itself, marked `PositionProvenance::Estimated` because it is a prediction until the seek lands. It is display state only: the checkpoint path reads `Progress`, never the mirror (§6), so an optimistic target cannot reach a checkpoint and the rule that an estimated position must never replace an established checkpoint is unaffected. While a target stands the application stops copying `Progress`'s position into the mirror — the worker is still reporting where playback actually is, and copying it would snap the display back. The hold spans the press *and* the wait for the landing, not merely the quiet window: the target is released when the worker accounts for it (`SeekCompleted`, `SeekRejected`, `SeekCancelled`, `SeekTargetStored`, `RestartEstablished`, `EndOfTrack`, `Loaded`, `Failed`), or immediately if the command queue refuses the submission, since a refused seek reports no outcome to wait for. A landing that misses the prediction — an estimated MP3 seek with no Xing/Info/VBRI tag can miss it by a lot — corrects the display when it arrives.

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

M4 implements this without changing any of it. `EpisodeKey::resolve` is called once per item by `bind_feed` (`src/feed/episode.rs`) with the item's GUID, enclosure URL and link in that order, and its result is stored in the cache as the episode's `media_id`; nothing later re-derives an identity from a URL. `resolve_source` (`src/app.rs`) — the function that turns a *command-line argument* into a `(MediaId, SourceLocation)` pair — gained a sibling rather than a branch: `library::resolve_episode` produces the pair for `play <slug> <index>`, and `run_resolved` plays whichever pair it is handed without being able to tell which of the two produced it. The engine, `Session`, the resume decision and the checkpoint writer are M1–M3 code, reused unchanged.

The consequence is that **what is played and what is checkpointed are deliberately different values.** A podcast episode's `SourceLocation` is the enclosure URL; its `MediaId` is `podcast:<feed-id>/<episode-key>`. Playing the same audio as a direct URL produces `remote:<normalized-url>` instead, and the two never share a checkpoint. That is what lets an episode's position survive the show moving its audio to another CDN, and it is asserted end to end — a real cached feed, a real loopback fetch through the virtual output, a real `state.json` — by `tests/m4_playback_identity.rs`.

`--probe-only` on an episode is the one place a second identity is derived: the probe re-resolves the enclosure URL as a `RemoteUrl` to open it. It never persists one, because a probe reads no playback state and writes none.

## 6. Durable state (M2)

Persistence honors XDG environment variables and their standard defaults:

| Location | Contents |
|---|---|
| `$XDG_CONFIG_HOME/continuo/config.toml` | User preferences |
| `$XDG_STATE_HOME/continuo/state.json` | Current media, queue, volume, checkpoints, played status — **one atomic snapshot** |
| `$XDG_DATA_HOME/continuo/subscriptions.json` | Podcast subscriptions (durable user data) |
| `$XDG_CACHE_HOME/continuo/` | Refetchable feed data |
| `$XDG_STATE_HOME/continuo/state.lock` | The player profile lock (M5): locked, never written or unlinked |
| `$XDG_STATE_HOME/continuo/logs/` | One fd-2 log per `tui` run, the five most recent prior ones kept (M5) |

Current media references one checkpoint per media identity inside the single atomic playback snapshot. Keeping these together prevents disagreement after a crash between writes. Subscriptions remain separate durable user data. Storage is a small concrete module with typed load/save operations; there is no repository trait.

A single writer's accepted update sequence orders snapshots after generation validation. Neither timestamps nor maximum positions order updates: clocks can move backward, and a deliberate backward seek supersedes an earlier larger position. `updated_at` exists only for human inspection. There is no merge algorithm.

The application captures checkpoints periodically while playing and on pause, stop, track change, and successful seek, then flushes pending state during graceful shutdown. The capture interval and writer's maximum coalescing interval are each bounded to single-digit seconds, which bounds worst-case loss end to end. The writer creates a temporary file in the destination directory, writes and `fsync`s it, renames it over the destination, then `fsync`s the parent directory where supported.

Every snapshot carries `schema_version` from its first write. Malformed and unsupported-version files are preserved and reported rather than overwritten. `schema_version` became 2 in M3.1 (M5 raises it to 3, below): `PersistedCheckpoint` carries `estimated: Option<Duration>` beside `position`, recording where a byte-offset seek left the listener without ever substituting for the established `position` field — an estimated location may drive a restart's resume point, but writing it never overwrites an established checkpoint for the same media. A v1 file's `position` deserializes straight into `Some`, so the schema bump costs an upgrade nothing; a v2 file read by a v1 build hits the unsupported-version path above and is preserved unwritten. `Unsupported` or `Undetermined` resume capability never deletes a checkpoint. Completed status is separate from position and is set after output drains. Replay-from-beginning requires explicit completed-status policy in M2. There is no near-end reset: stopping near the end preserves that logical position.

`docs/superpowers/specs/2026-09-08-continuo-durable-state-design.md` carries the
decisions behind all of this — completion semantics, the per-identity map and its
cap, rejected-file handling, the checkpoint triggers — and its §19 records where
the implementation amended them.

M3 adds one protection rule on top of this. When a positive checkpoint exists but capability resolution conclusively rules out resuming it, sequential playback may begin at zero instead of failing outright — and that fallback load marks the existing entry protected: periodic, pause, stop, outgoing-media and shutdown captures do not overwrite it merely because this run is playing from zero. Protection ends only on a successfully established explicit restart, a successfully established seek, or verified completion. This deliberately favors recovering the earlier resume point over saving progress from a fallback run — it is not a maximum-position merge rule, and even later progress that exceeds the protected position does not replace it while protection holds.

M4 adds a second durable file and a disposable one, and keeps them apart on purpose.

`$XDG_DATA_HOME/continuo/subscriptions.json` is durable user data: the slug, the assigned `FeedId`, the feed's last known title, its current fetch URL, and when it was added. `$XDG_CACHE_HOME/continuo/feeds/<feed-id>.json` is refetchable: the parsed episodes, the URL that representation was actually retrieved from, its `ETag`/`Last-Modified` validators, and both timestamps. The cache is keyed by `FeedId` and never by slug, so renaming a subscription cannot orphan it, and it carries a `parser_version` beside its `schema_version` because a later parser fix would not otherwise reach data already cached.

**One cache file is one atomic write, which is what keeps a validator honest.** The episodes, `fetched_from` and the validators live in the same file and are replaced by the same `rename(2)`, so no interleaving can leave an `ETag` describing a representation other than the episodes stored beside it. That matters more than it looks: a validator that outran its episodes would make the *next* conditional refresh answer 304 and preserve a list the server no longer has.

Across the two files there is no such guarantee, and M4 does not pretend otherwise. A single atomic commit would need a journal or a combined file, and combining them would put refetchable data inside durable user data. Instead the commit **order** is fixed so that every interruption leaves a recoverable state, and the incomplete outcome is *reported* rather than hidden:

| Command | First | Then | If the second step fails |
|---|---|---|---|
| `subscribe` | write the cache | add the subscription | an unreferenced cache file remains; **nothing is subscribed**, and the command says so and exits nonzero |
| `unsubscribe` | remove the subscription | delete the cache | the subscription really is gone; a stale cache file remains, is reported, and exits nonzero |
| `refresh` | write the cache | update title / redirected URL | the episodes really were saved; the metadata was not, is reported, and exits nonzero |

This is why §6.4's "a partial failure cannot exit successfully" is a rule about *exit status*, not about atomicity: the work is genuinely half-done, the half that landed is genuinely useful, and the command's job is to name which half that was. `FollowupFailure` carries the failed step and its cause rather than a bool, precisely so the message can.

Recovery from an unusable cache is a refetch, never a repair. Missing, corrupt and `parser_version`-mismatched are all treated as "nothing to revalidate against", so `refresh` fetches unconditionally in each case; a corrupt file is left exactly where it is, and no listing ever quarantines, rewrites or deletes one. A `304` answered when there was no usable cache to revalidate is refused (`UnsolicitedNotModified`) rather than turned into a fabricated cache entry.

Concurrency is out of scope for M4 and documented rather than solved: each file write is atomic, but nothing serializes one whole process against another, so two simultaneous mutating commands can lose one of the two subscription writes. One process at a time.

Checkpoints are the third file and are never touched by a feed command. `unsubscribe` takes no `StateStore` argument and deletes nothing; resubscribing mints a fresh `FeedId`, so checkpoints keyed to the old one are orphaned rather than reattached. Reattaching would mean trusting a feed URL to name the same feed forever.

**M5 moves the playback snapshot to schema 3**, adding `queue` (entries with a stable occurrence `id`, the `MediaId`, a `source` and display metadata) and `active_entry` beside the existing fields, in the same atomic file. A local source duplicates its `path` and a remote one its `url` on disk, so a source that no longer matches its identity is detectable; a podcast source stores only `fallback_url`, the last resolved enclosure, since its identity is the `MediaId`. Schemas 1 and 2 load with an empty queue and no active entry, keeping `current_media`. The 256-occurrence cap is an admission rule, not a validity rule, and queue membership does not pin a checkpoint against the 512-entry eviction.

**Recovery is queue-only.** `PersistedState` is decoded through a private `RawState` that holds `queue` and `active_entry` as raw JSON, and the pure `queue_codec::recover_queue` turns them into a `Queue` plus an optional `QueueReset`, so a wrong type or a malformed entry can never fail the file's checkpoints, volume or `current_media`. A malformed, dangling or media-mismatched active reference clears only that reference; a malformed entry, duplicate IDs, a source/identity mismatch or an over-capacity queue clears the whole queue; an active occurrence is never inferred from a matching `MediaId`. Before any writer starts after a repair, `StateStore::load` copies the exact original bytes to a create-new `state.json.queue-recovery-<stamp>` sibling and syncs it; if that fails, the session keeps the recovered state in memory with persistence disabled and leaves the original untouched. Whole-file quarantine and the unsupported-version path are unchanged, and read-only snapshot readers (feed listings) never examine queue data.

`Session` remains the sole owner of `PersistedState` and the sole builder of `Action::Submit` snapshots. Queue mutations, enrichment results, podcast fallback updates, playback events and checkpoint ticks all go through it on the application thread, so a periodic checkpoint carries the latest queue and a queue mutation carries the latest accepted checkpoint; background workers only send results. An adoption snapshot is built last, after the outgoing history is captured and the new media, active occurrence and metadata are in place.

**The profile lock replaces "one process at a time" for players.** `tui` and `play` take a nonblocking exclusive lock on a sibling `state.lock` (`lifecycle::lock::ProfileLock`) before any state read, migration, quarantine or writer creation, and hold it through the final flush and teardown; the lock file is never unlinked or recreated, and the atomically replaced `state.json` inode is never what is locked. Contention refuses to start with `Another Continuo player is using this state profile`, before any audio device or raw mode — never by falling back to `writable = false`. `play` resolves its source before the lock, so source errors win over contention. Feed commands and `--probe-only` take no lock, and the M4 caveat about two concurrent subscription writers still applies to them.

`tui`'s startup order is fixed (spec §11): cleanup state and the panic hook; the signal listener; the profile path and lock; state load and recovery; the session log and the fd-2 redirect; the writer and the runtime's workers; the terminal, then artwork detection. A recorded shutdown request or a fatal worker panic is checked between stages, and every exit — `q`, a signal, a worker fatal, a startup failure or a panic on the application thread — runs the same teardown over whatever was initialized: the engine and writer (while fd 2 still points at the log), then the terminal and fd 2, then the signal listener, then the lock, and only then any message for the user. OS-signal shutdown exits `128 + signal_number` of the first recorded signal; `q` and the Ctrl-C key exit 0.

## 7. Diagnostics and errors

Tracing is implemented in M0 and controlled through `RUST_LOG`, for example `RUST_LOG=continuo=debug cargo run --locked`. Logs go to stderr. Without `RUST_LOG` the default filter is `continuo=info`. An invalid filter, or a `RUST_LOG` value that is not valid Unicode, is reported as a startup failure and exits nonzero rather than being silently ignored. Later milestones must log source opened, redirects, range support, selected decoder, known duration, requested and actual seek results, playback state transitions, checkpoint writes, end of track, and output failures. Per-frame logging is forbidden.

Errors carry typed context such as the path or URL, media identity, and operation. The application boundary presents concise text to the user while the full error chain goes to structured logs. Position remains estimated when the device cannot report output latency.

M3 gives `PlaybackEvent::Failed` a typed `cause: Option<RemoteFailure>` beside its existing `message: String`. `RemoteFailure` (`src/http/error.rs`) is one variant per §11 category — invalid source, HTTP status, redirect rejection, timeout, invalid range, resource change, truncated body, probe limit, unresolved continuity, unsupported live media, seek/resume unavailable, non-identity content encoding, transport, and cancellation — so a policy can act on what went wrong rather than only read a string a human wrote. `Display` on every variant is third-party-safe: every URL reaching it has already passed through `redact_url`, so no signed query or userinfo can reach a status line, a log line, or a `Failed` message. This narrows rather than closes M1's known-debt finding about stringly-typed failures: `cause` is `None` for every local (non-remote) fault, so `message` remains the only thing those carry. See `docs/m1-known-debt.md`'s Milestone 3 section for the remaining rough edges in `RemoteFailure` itself.

M4 adds `FeedError` (`src/feed/error.rs`) as the single type every feed and subscription operation returns, and `AppError` (`src/error.rs`) as the two-armed union `app::run` hands to `main`. Both `AppError` arms are `#[error(transparent)]`, so the concrete failure keeps printing rather than a wrapper that says nothing.

`FeedError` follows the same redaction discipline as `RemoteFailure`, and for the same reason: `main.rs` prints `{error}` **and** logs `?error`, and a derived `Debug` prints field values verbatim, so a redaction applied only to `Display` would leak straight through the log line. Every URL-bearing field therefore holds already-redacted text rather than a live `Url`. Three distinct rules are in force, and they are not the same rule:

- **Transport URLs are secret.** Query and userinfo are where a bearer token hides; every URL reaching a message has passed `redact_url` first, and text that does not parse as a URL is replaced by `<unparseable URL>` rather than echoed, since the unparseable text may itself be the secret.
- **File content is never quoted back.** A `serde_json::Error`'s own `Display` can echo the offending bytes, and a checkpoint key or a cached `media_id` *is* an untrusted identity that may carry a URL. Both the cache decoder and the subscription decoder reduce it to a category plus line and column; `PersistenceError::Deserialize` deliberately carries no `#[source]` for the same reason.
- **Titles and explicitly requested aliases are content, not transport.** They are what the listener asked to see, so they are escaped at the formatting boundary (control characters, U+2028/U+2029, and the bidi overrides U+202A–U+202E become visible escapes) rather than hidden. Nothing is transliterated: a Cyrillic or CJK title prints as stored.

The same rules bind logging. No `ParsedItem`, `BoundItem`, `CachedFeed`, `CachedEpisode`, `DocumentRequest` or validator record is ever logged whole — any of them under `Debug` would carry a GUID, a title and a URL at once. Parse and bind warnings carry only the item's document ordinal and a warning category; resolution diagnostics carry only the slug, the index and the two declared enclosure fields. `tests/m4_diagnostics.rs` audits this through the real constructors and through the real binary at `RUST_LOG=continuo=debug`.

One accepted inaccuracy: `main.rs` labels its error log line `"playback failed"` for every command, including the feed commands, which is wrong for `subscribe`, `feeds`, `episodes`, `refresh` and `unsubscribe`. The text of the error itself is correct; only the label is not. M4 left `main.rs` untouched on purpose, and this is recorded debt rather than an oversight.

**M5's terminal player redirects fd 2 for its whole lifetime.** Under `tui`, stderr — `tracing` output, direct writes from ALSA or other C libraries, and contained-panic diagnostics — goes to a new append-only `continuo-tui-<stamp>-<pid>.log` under `<state dir>/logs/`, created under the profile lock after deleting all but the five most recent prior logs. The redirect is `gag::Redirect` behind a take-once slot shared by normal teardown and the panic hook, published into the slot with nothing fallible in between. The active log has no size cap. Plain CLI commands, legacy `play` included, keep writing to stderr.

An uncontained panic takes the fatal path: the hook marks rendering disabled, requests shutdown, restores the terminal (raw mode, mouse capture, cursor, Kitty image placements, alternate screen) and fd 2, and only then runs the previous hook, so the diagnostic appears on the primary screen. A worker's fatal panic makes the run fail with `a background worker panicked`; a panic on the application thread unwinds through teardown and resumes. `tests/m5_tui_process.rs` checks each stage, and the contained artwork and metadata cases, in a subprocess under a PTY.

Two environment variables are **diagnostic switches for process tests**, not user configuration:

- `CONTINUO_AUDIO_OUTPUT=null` makes `EngineHandle::spawn_for_environment` use `NullOutput` (`src/playback/output/null_output.rs`), a paced virtual output thread (`continuo-null-output`) that drives the real callback core against a wall clock and discards the samples. It lets `play` and `tui` really decode, advance and checkpoint on a machine with no sound device. Any other value, or none, opens the default CPAL device.
- `CONTINUO_TEST_HOOK` (`lifecycle::hooks::TestHook`) is read once at startup. Its exact values — `panic-before-redirect`, `panic-after-redirect`, `panic-after-terminal`, `stderr-probe`, `artwork-job-panic`, `artwork-encoding-panic`, `metadata-job-panic`, `worker-panic` — trigger a panic or an fd-2 probe at one fixed stage. Anything else, including a differently cased name, means no hook.

The subprocess suites run on Linux only, with `XDG_STATE_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` and `XDG_CONFIG_HOME` set per child by `tests/support/process.rs`, which every binary launch goes through (`tests/launch_audit.rs` enforces this). There is no production profile override.

Runtime code forbids unsafe code and denies `unwrap` and `expect`. Tests may use them for assertions and fixtures; where Clippy does not recognize a bare integration-test helper as test code, its exception is scoped to that fixed fixture helper rather than weakening runtime lint policy.

## 8. Milestones and backend

| Milestone | Delivers |
|---|---|
| **M0** | Repository foundation, domain types, contracts, CI |
| **M1** | Local playback vertical slice: Symphonia + CPAL, decode thread, ring buffer, position accounting, command/event protocol, state machine, resampler choice |
| **M2** | Checkpoint persistence, stop/resume and restart/resume semantics, completion policy |
| **M3** | Finite HTTP media, capability probing, range-based seek, `RemoteFile` vs `LiveStream` — shipped |
| **M4** | RSS/Atom feeds, subscriptions, episode listing and progress — shipped |
| **M5** | Ratatui TUI over the existing application interfaces: queue, browser, artwork, spectrum, profile lock, signal contract — implemented; manual terminal acceptance pending (`docs/m5-acceptance.md`) |

M0 explicitly defers `PlaybackCommand` and `PlaybackEvent`, the executable state machine, channels, worker threads, callback accounting, buffer management, detailed decoder and device errors, checkpoint storage, completion policy, HTTP buffering, and capability probing. All of those deferrals have since been implemented: playback (M1), persistence (M2), HTTP fetching (M3), feeds and subscriptions (M4), and the TUI (M5).

M1 used Symphonia and CPAL directly to control buffering, cancellation, and position accounting. This choice does not claim that Rodio cannot seek; Rodio's Symphonia backend implements accurate seek refinement. HTTP range support belongs to the source layer, not CPAL. Commands and events will form the application boundary, so no speculative backend trait is introduced. Rodio remains a contingency if M1 uncovers a concrete blocker.

The v0.1 scope excludes Spotify, YouTube/yt-dlp, SoundCloud, Jellyfin, Plex, Navidrome, equalizer or DSP, themes, plugins, a daemon/client split, and remote control. The foundation scope also excluded a visualizer; the M5 spec explicitly adds the frequency spectrum, and nothing else from this list. MPRIS and media keys are deferred until the core is stable. Known limitations are non-UTF-8 paths, estimated position where device latency is unavailable, and seek support that can remain `Unknown` until probed.

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

M4 makes this executable end to end: steps 1–2 are `subscribe` and `episodes`, step 3 is `play <slug> <index>`, and steps 4–11 are M1–M3 behavior the feed layer does not touch. The episode remains finite remote media rather than becoming live radio merely because it uses HTTP, and steps 9–11 restore it under its **podcast** identity rather than its enclosure URL — `tests/m4_playback_identity.rs` asserts exactly that against a written `state.json`.

M4 adds two dependencies and no more: `quick-xml` for the pull parser, and `getrandom` for the 128 bits of OS randomness a `FeedId` is minted from. `url`'s `serde` feature is activated for the subscription and cache DTOs.

Automated HTTP integration tests use a local test server and have no public-network dependency. They cover range-capable finite files, servers without range support, redirects, invalid range responses, reconnect-after-stop, and in-place resume of a ranged body that ended short.

`docs/m5-acceptance.md` maps the M5 spec's §12 evidence to the tests that discharge it and records the manual terminal checks.

`docs/m3-acceptance.md` carries the full H1–H18 coverage map: which test discharges each of the design's acceptance items, and, for the two that are only partially discharged, exactly why.
