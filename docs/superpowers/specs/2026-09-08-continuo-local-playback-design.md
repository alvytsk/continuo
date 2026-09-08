# Continuo local playback (Milestone 1)

**Date:** 2026-09-08
**Status:** Proposed for review; implementation has not started.
**Predecessor:** [Approved foundation design](2026-09-07-continuo-foundation-design.md).

## 1. Scope and starting point

M0 is merged as `b04ca96`: domain contracts, typed errors, telemetry, and a thin binary. M1 delivers the first audible slice — decode, output, pause, stop, seek, explicit restart, volume, position reporting, device-failure recovery, and graceful shutdown.

The binding constraint from M0 is unchanged and governs this entire document:

> Stopping or recreating the audio transport pipeline must not implicitly reset the logical playback position.
>
> Position is the session's logical resume point. It advances from estimated playback of media frames. Stop and transport recreation preserve it; restoration, media selection, explicit restart, and successful seeks establish a new position.

**Surface.** `continuo play <path>` starts playback immediately and enters crossterm raw mode:

```
ep.flac [playing] 00:04:12 / 01:02:30  vol 80%
space pause · ←/→ seek 10s · Home restart · -/+ volume · s stop · p play · q quit
```

The session stays open after stop or end of track so resume and restart are exercisable. `q`, EOF, and Ctrl-C shut down gracefully. No arguments prints help; invalid arguments exit nonzero with a concise message.

**Formats.** Local regular files, mono or stereo. MP3, FLAC, and PCM WAV are required; M4A/ALAC ride along via the enabled features. Other layouts produce contextual unsupported-input errors. Surround downmixing is out of scope.

**In scope beyond the core slice** (selected during design): volume control, metadata read from the file, output device failure handling.

**Out of scope:** multi-file queue, HTTP sources, feeds, `PlaybackCheckpoint` persistence, played/completed policy, gapless, cross-fade, TUI.

## 2. Decisions

| Decision | Choice | Consequence |
|---|---|---|
| Runtime | **No Tokio until M3** | Orchestration is the main thread; `docs/architecture.md` §2 needs amending |
| Interface | **Interactive keys, raw mode** | crossterm read loop on the main thread, 100 ms render tick |
| Test seam | **Narrow output-device seam** | `AudioOutput` with `CpalOutput` and `TestOutput`; internal, `pub(crate)` |
| Structure | **Worker-owned state machine** | App holds a mirror it never writes back |

Approach A (worker-authoritative) was chosen over a shared `Mutex<EngineState>` and over splitting source-reader from decode. The mutex variant demotes M0's "one coordinated transition" from a structural property to a locking convention adjacent to a real-time thread. The reader/decode split earns its keep in M3 for HTTP prefetch and can be introduced behind the same worker without disturbing this design; adding it now inserts another handoff for position accounting to cross.

**Pinned candidate versions**, all verified to build together under Rust 1.98.1: symphonia 0.6.1 (features `mp3`, `aac`, `isomp4`, `alac` added — `mp3` is *not* a default), cpal 0.18.2, rubato 5.0.0, rtrb 0.4.0, crossbeam-channel 0.5.17, crossterm 0.29.0, clap 4.6.6.

## 3. Threads and ownership

| Context | Owns | Must never |
|---|---|---|
| Main thread | terminal raw mode, render, `Sender<Command>`, `Receiver<Event>`, read-only mirror | write engine state |
| Decode worker | `FormatReader`, decoder, resampler, `Producer<f32>`, output handle, **state machine, anchor, generation** | block un-wakeably on the audio path |
| CPAL callback | `Consumer<f32>`, span publication, gain ramp | allocate, lock, log, or perform I/O |

Tokio is absent. The worker creates, parks, and destroys the CPAL stream; the callback drains preallocated PCM using bounded operations only. No persistence writer before M2.

## 4. The shared output link

`Arc<OutputLink>` is the only state shared between worker and callback. It contains no locks reachable from the callback.

| Field | Writer | Purpose |
|---|---|---|
| `control: AtomicU64` | worker | `(gen:u16 \| epoch:u32 \| phase:u8)` |
| `ack: AtomicU64` | callback | echoes the exact `(gen, epoch)` plus adopted state |
| `spans: Producer<SpanRecord>` | callback | bounded history ring (rtrb, initial capacity 64) |
| `rescue: [AtomicU64; 3] + AtomicU8` | callback | last unpublished span, readable only after teardown |
| `gain: AtomicU32` | worker | target gain, `f32` bit pattern |
| `xruns`, `spans_dropped`, `faults` | callback | aggregated diagnostics |

Phases are `Run`, `Freeze`, `Discard`, `Park`. Acknowledged states are `Running`, `Frozen`, `Parked`. **Every control publication bumps `epoch`**, and the worker waits for that exact epoch, so repeated same-generation `Park`/`Run` transitions can never accept a stale acknowledgment.

The generation is a **handshake nonce with a lifetime of exactly one handshake**, not an identifier. Nothing outside the worker/callback pair retains it, and no `PlaybackEvent` carries it. Retirement is proven by acknowledgment at both ends, and transport recreation allocates a fresh `OutputLink`, so the 16-bit wrap cannot alias.

## 5. Position: media spans, not a frame counter

A callback pops the ring once, so media always occupies a **prefix** of the output buffer: `k` media frames at offsets `[0, k)`, silence at `[k, N)`. CPAL supplies `playback`, the predicted instant of frame 0. Each callback that moves media publishes one record:

```
SpanRecord { gen, media_total_after, t0: Nanos, k }
```

Media frames `[media_total_after - k, media_total_after)` map linearly onto instants `[t0, t0 + k/rate)`. Records carry **cumulative** totals, not deltas, which is what makes a dropped record non-corrupting: a loss costs timing granularity only, never frame counts.

Position, with `now = output.now()`:

```
walk retained records, retiring those whose t0 + k/rate has passed (keeping the last as floor)
if now < t0                 -> played = floor                  // span has not begun
else if now >= t0 + k/rate  -> played = media_total_after
else                        -> played = media_total_after - k + clamp((now - t0) * rate, 0, k)
position = anchor.media_ts + played / rate
```

All timestamp arithmetic uses `checked_duration_since`. **`now < t0` is the normal case for the newest span, not an anomaly**: CPAL's PulseAudio backend returns bare elapsed time from `now()` (`host/pulseaudio/stream.rs:128`) while constructing `playback` as elapsed **plus** queued latency (`stream.rs:210`). The `StreamTrait::now()` doc claim that the clock is never earlier than a delivered `playback` instant is contradicted by that implementation and is not relied upon. This is also why a single latest-span slot is insufficient and a retained history is required.

No latency value is ever subtracted from a frame counter. That formulation fails across silence: one second of media followed by a long underrun leaves `submitted` at one second while a persistent 100 ms latency drags the reported position to 900 ms forever. Silence is simply absent from the media timeline.

**Recorded silence in the source counts as media; injected underrun and park silence does not.**

On drain the worker validates monotonic totals and non-overlapping intervals. A discontinuity disables interpolation and marks the position `Degraded` rather than being assumed away. Capacity 64 (~640 ms at 10 ms buffers against a 100 ms render tick) is a starting size to be validated by measurement, not a time guarantee.

**Dropped-span recovery.** On push failure the callback replaces a callback-local `pending` record and bumps `spans_dropped`. Every subsequent callback republishes `pending` first — **including while `Frozen` or `Parked`**, which is exactly when the ring is draining. The rule that closes the capture hole: **the callback may not acknowledge `Frozen` or `Parked` while `pending` is `Some`.** The acknowledgment therefore proves the *latest cumulative* record arrived; intermediate records may still have been dropped, which is compatible with degraded timing but is not a completeness claim.

Because dropping the stream destroys the closure and any state inside it, `pending` is mirrored into the link's `rescue` slot. It is written by the callback with a `Release` validity flag and read **only after teardown has joined the backend thread**, so the read is not concurrent and needs no sequence protocol — the ordering obligation is discharged by the join inside `Drop`. If teardown itself fails and the record cannot be recovered, the worker **preserves the last validated position and marks it `Degraded`**. It never fabricates a position.

`PositionQuality` is `Exact`, `Estimated` (the normal case), or `Degraded`.

## 6. The transition handshake

Every step carries a deadline of `max(250 ms, 4 output periods)`.

1. **Freeze** — the worker stops producing, publishes `(g, e, Freeze)`. The callback stops popping, emits silence, republishes any `pending`, then acknowledges `(g, e, Frozen)`. The worker **acquire-loads that exact acknowledgment before reading spans**, so an in-flight callback cannot still be submitting media.
2. **Capture** — the worker derives the final position. Stop, pause and failed seek all land here.
3. **Discard** — the callback takes `read_chunk(slots())`, `commit_all()` — an O(1) index advance, since `f32` is `Copy` with no `Drop` — then acknowledges `(g, e, Parked)` and **consumes nothing further until it adopts a new phase**. The worker waits on that acknowledgment, never on `slots()`. Waiting on an empty ring alone is unsound: the callback could observe `Discard`, the worker observe an empty ring and refill, and the callback then discard the new PCM.
4. **Install silently** — the worker sets the anchor, refills, and publishes `(g+1, e', Park)`. The callback zeroes counters, resets span state, acknowledges adoption.
5. **Release** — `Run` is published **only when the desired state is playing**. A paused install never passes through `Run`, so no audio escapes.

**The CPAL stream is started once and never hardware-paused.** Pause is `Park`, so the callback is always live and every deadline signals real device trouble rather than a self-inflicted deadlock; it also avoids depending on `pause()` support, which CPAL documents as backend-dependent. The cost is an idle stream's power draw, accepted for v0.1. Pause still requires the `Parked` acknowledgment before submission counts as frozen, and the consequence is documented behaviour: **position continues to advance through the already-submitted tail before settling.**

The deadline bounds the **handshake wait**, not transport destruction: the ALSA, PipeWire and PulseAudio streams join their backend threads during `Drop` with no timeout. The specified behaviour is therefore *on handshake timeout, attempt teardown and recovery* — no claim of bounded end-to-end recovery. Failed recreation preserves the captured position and enters `Failed`. Shutdown tears down without rebuilding.

## 7. State machine and the position contract

States: `Idle` → `Loading` → `Playing` ⇄ `Paused` → `Ended`, with `Stopped` and `Failed` reachable from any live state.

| Transition | Position | Mechanism |
|---|---|---|
| `Load{start_at}` | **establishes** (media selection / restoration) | new generation, silent install |
| Seek success | **establishes** at refined `actual_ts` | full handshake |
| Seek failure | **preserves** the captured value | see below |
| `Pause` / `Play` | **preserves**, continues | `Park` / `Run`, same generation, new epoch |
| `Stop` | **preserves** (the M0 invariant) | Freeze → capture → teardown |
| Transport recreation | **preserves** | fresh `OutputLink`, re-anchored |
| `Ended` | pins at last media timestamp | — |
| `Failed` | **preserves** the last captured value | teardown, no rebuild |

**Seek failure is not a transactional rollback.** Symphonia seeking mutates reader state and requires a decoder reset, so the rule is: *preserve the captured logical position; retain the old pipeline only if still valid, otherwise reopen at that position using a fresh generation, or enter `Failed`.* The rejected target is never published as progress.

**Boundary outcomes.** `Play` from `Stopped` reopens at the preserved position and never resets it. `Play` from `Ended` is a no-op plus a warning — **explicit restart is the documented sequence `SeekTo(0)` then `Play`**, bound to `Home`, with `Play` **sequenced on a successful `SeekCompleted`, not enqueued unconditionally**, so a failed restart cannot start playback at the old position. `Play` from `Failed` is rejected; retry is a fresh `Load` at the preserved position. Seek while `Paused` uses the silent install. Seek from `Ended` re-enters `Paused` at the target; from `Idle` it is rejected. Load failure enters `Failed` with the position pinned at the requested `start_at`. Interruption during `Loading` yields `Idle`, or `Stopped` when media was previously loaded; during seeking it yields the captured position under the rule above. Repeated pause and stop requests are idempotent. Negative, non-finite, or overflowing seek input is rejected before dispatch; clamping to duration happens only when duration is known, and the decoder's actual result is still reported.

**Seek while `Stopped` has no transport but still requires validation.** It stores a `requested_target` and emits only a state change; `SeekCompleted` is deferred until the next `Play` opens the decoder, seeks, and refines. An unvalidated target is never reported as an achieved position.

M1 guarantees preservation for the current session only. It writes no `PlaybackCheckpoint`.

## 8. Command and event protocol

**Commands:** `Load{media, source, start_at}`, `Play`, `Pause`, `TogglePause`, `SeekTo`, `SeekBy`, `SetVolume`, `Stop`, `Shutdown`.

**Events:** `Loaded{metadata, capabilities}`, `StateChanged`, `SeekCompleted{requested, actual, refinement_truncated}`, `SeekRejected{reason}`, `VolumeChanged`, `EndOfTrack{position}`, `DeviceRecovered`, `Warning`, `Failed`. Every event carries `session_rev`.

**Two paths.** Lifecycle and error events use a bounded crossbeam channel — lossless and ordered. Progress is **pulled, not pushed**: the worker replaces a keep-latest `Mutex<Progress>` snapshot (main thread only, never the callback; the critical section copies and returns), and the render tick reads it. Coalescing is structural, so no progress event can displace an ordered lifecycle event.

`Progress { session_rev, media, position, quality }`. **The app renders progress only when `session_rev` matches its mirror**, because a mutex snapshot can otherwise overtake queued lifecycle events and show media B's position under media A's title. `session_rev` bumps on load, stop and recreation, and is deliberately distinct from the callback's generation and epoch.

**Bounded lossless delivery.** A `pending_events: VecDeque` with a hard cap holds events that could not be sent. **Command admission stops while it is non-empty** — the worker selects only on `send` until the backlog drains — which structurally bounds further command-driven event generation. Backlog processing continues to drain the span ring and service device faults; it is not a bare send loop. Reserved tail slots hold terminal outcomes (`Failed`, `EndOfTrack`, `Stopped`, shutdown acknowledgment) and ordinary events may not occupy them. The implementation plan must enumerate each operation's maximum event count and prove the reserve budget, including simultaneous EOF and output fault.

**Asynchronous generation is bounded too.** An `Xrun` storm cannot exhaust storage: repeating faults increment aggregated counters in the link, and the worker emits at most one rate-limited `Warning` per second carrying the aggregate count.

**Stop and shutdown travel out of band.** A sticky `interrupt: AtomicU8` (`STOP`, `SHUTDOWN`) is paired with a **bounded wake channel of capacity 1** that is included in every `select!`; the setter sets bits then `try_send(())`, and a full channel is harmless because one pending wake suffices. An atomic alone cannot wake a blocked `select!`, so the channel is required, not decorative. Shutdown dominates stop. Every worker wait is a `select!` with a timeout — never a bare sleep. Event-receiver disconnection is shutdown: the worker stops retrying delivery and exits.

## 9. Decode, seek, and conversion

Resolve the path with `fs::canonicalize` at the worker's source-opening boundary, then build the existing `AbsolutePath` and `MediaId`. **Reject non-regular files** so pipes and device inputs cannot introduce an unbounded read into this local-file slice. Local reads are not cancellable mid-syscall; the engine checks cancellation *between* packets and makes no claim of hard syscall cancellation.

Probe the selected track; derive `MediaMetadata` and `MediaCapabilities` from the source. Unknown duration stays `None`.

**Seek refinement.** Symphonia's accurate seek lands at or before the target and returns `SeekedTo{required_ts, actual_ts}`, so the worker resets the decoder and decodes-and-discards forward to the exact frame. For an **explicit** seek this runs under a budget (~5 s of media); exhaustion anchors where it actually arrived and reports it, with `refinement_truncated` set. Five seconds of media is not a bound on elapsed time, so cancellation is checked between decoding steps. **`Stop`→`Play` and transport recovery promise preservation and therefore refine without a frame budget**, bounded instead by a wall-clock deadline and cancellation checks; a missed deadline is `Failed` with the position intact, never a silent resume elsewhere. `SeekSupport::Unsupported` is rejected before the transport is touched — no freeze, no generation burned.

**Channels.** Duplicate mono to stereo or average stereo to mono as the negotiated configuration requires.

**Resampling.** rubato converts only when rates differ. `output_delay()` frames are trimmed **once per resampler creation or reset — not on every logical anchor update**; a seek resets the resampler, so trimming legitimately recurs there. Pause and play retain resampler state along with buffered PCM. At EOF the final chunk is processed with `Indexing::partial_len` set to the remaining frames, flushing delayed valid output, and the synthetic padding is then trimmed to the exact expected output extent — otherwise startup trimming shortens playback. The valid-output extent travels with the PCM so padding never advances position.

**Buffering.** The ring is bounded by output frames, initially 250 ms. The worker reads occupancy directly for backpressure and waits interruptibly.

## 10. Output seam, volume, and device failure

The seam is internal (`pub(crate)`) and limited to what production and tests both exercise. `cpal::StreamInstant` must not leak, so `output::Nanos(u64)` — from the public `as_nanos() -> u128`, saturating — is the clock type. `timeline.rs` therefore performs all position math **with no dependency on cpal**, which is what makes every clock pathology a plain unit test.

Negotiation is two-phase, because the ring's capacity depends on the negotiated rate:

```rust
pub(crate) trait AudioOutput: Send {
    fn negotiate(&mut self, request: &OutputRequest) -> Result<NegotiatedOutput, OutputError>;
    fn open(&mut self, cfg: &NegotiatedOutput, link: Arc<OutputLink>, ring: Consumer<f32>)
        -> Result<(), OutputError>;
    fn now(&self) -> Nanos;
    fn close(&mut self);
}
```

`NegotiatedOutput` reports the actual sample rate, channel count, sample format, and buffer frames, and the stream opens **parked**, so the worker configures resampling and frame accounting before any audio flows. `open() -> Result` covers synchronous failure only; **CPAL's error callback pushes an `OutputFault` into a bounded channel held in the link and signals the same wake channel from §8**, which is how asynchronous errors reach the worker.

`TestOutput` drives a virtual clock: `advance(d)` synthesizes callbacks with `playback = now + fake_latency` and runs **the production callback core**, making handshake ordering, stale-epoch rejection, silent installs, and deadline-to-teardown deterministic without a device.

**Volume** is applied in the callback: an `AtomicU32` bit-cast `f32` target with a per-buffer linear ramp, **one gain value per frame applied across all channels**. Worker-side gain would delay volume changes by a full ring (~250 ms), which is worse than a few allocation-free lines in the callback. Gain is post-ring, so it never touches the media timeline.

**Device failure**, classified on `cpal::ErrorKind`:

| Kind | Handling |
|---|---|
| `DeviceChanged` | automatically rerouted, no rebuild; `Warning`, mark `Degraded` — the timing base may jump. Fires only for default-device streams |
| `DeviceNotAvailable`, `StreamInvalidated` | capture, rebuild at the preserved position, bounded retries with backoff and interrupt checks; `DeviceRecovered` or `Failed` |
| `Xrun` | aggregated counter, rate-limited `Warning`, no rebuild |
| `RealtimeDenied` | `Warning`; playback continues. Its *absence* proves nothing about real-time quality |
| `DeviceBusy` | retry with backoff at open, then `Failed` |
| `PermissionDenied`, `HostUnavailable`, `UnsupportedConfig` | `Failed` at open with a legible message |
| `ResourceExhausted`, `UnsupportedOperation`, `InvalidInput`, `BackendError` | fallback: one rebuild attempt, then `Failed` |

## 11. Module layout and errors

```
src/playback/
  checkpoint.rs   (M0, unchanged)      link.rs      control/ack atomics, SpanRecord, span ring, rescue slot
  command.rs  event.rs  state.rs       timeline.rs  spans -> position   (pure: no audio, no threads, no cpal)
  engine.rs   worker loop              decode.rs    symphonia open / decode / seek refinement
  error.rs    PlaybackError            resample.rs  rubato + delay trim + EOF flush
  volume.rs   Volume newtype           output/{mod,cpal,test}.rs
src/app.rs    key loop, render, mirror
src/cli.rs    clap
```

`PlaybackError` (thiserror) covers `Open`, `Decode`, `UnsupportedInput`, `Output`, `SeekFailed`, `Timeout`, `Cancelled`. M0's `DomainError` and `TelemetryError` are unchanged. The existing lint policy holds: `unsafe_code = "forbid"`, `unwrap_used` and `expect_used` denied outside tests.

## 12. Validation

Tests require no network and no audio hardware. Fixtures are generated locally and committed: a short sine as PCM WAV, MP3, and FLAC, with provenance recorded — the formats M1 promises must be exercised, not only WAV.

- **`timeline.rs` units:** `now < t0`, dropped spans, overlapping or non-monotonic records, underrun silence, recorded silence, EOF offset within the final buffer, clamping to `[0, k]`.
- **Production callback core through `TestOutput`:** span publication, gain ramp across stereo, `pending` republication while parked, **dropped-final-span teardown recovery via the rescue slot**.
- **Handshake:** freeze-then-capture ordering, refill-after-`Parked` only, stale-epoch rejection, paused install never entering `Run`, deadline-to-teardown.
- **Position contract**, worded from M0: stop preserves, recreation preserves, seek establishes, load establishes; backward seek, failed seek, failed `Home` restart not starting playback.
- **Conversion:** differing rates, mono/stereo, partial final blocks, startup delay trim, **EOF tail trimming at a differing rate**.
- **Backpressure:** saturated command, event, and PCM capacity; command admission; **interrupting a saturated event send** via the wake channel; receiver disconnection; bounded memory.
- **CLI:** help, invalid input, key handling, interrupt-driven shutdown, all device-free.
- **Real device:** `#[ignore]`d, opt-in, never run in CI.

Manual acceptance: play a local file, pause and resume, seek both directions, stop and resume at the preserved position, restart from zero, reach drain, quit. Repeat at a rate requiring conversion, and with the device removed mid-playback. Passing simulated tests alone does not establish audible playback.

## 13. Amendments this milestone forces

- `docs/architecture.md` §2: the application context is the **main thread**, not Tokio, until M3.
- `.github/workflows/ci.yml`: install `libasound2-dev` (present locally, 1.2.15.3).
- `Cargo.toml`: symphonia with `mp3`/`aac`/`isomp4`/`alac`, cpal, rtrb, rubato, crossbeam-channel, crossterm, clap.

## 14. Implementation sequence after approval

1. Protocol, state, and the pure `timeline` module with its full pathology suite.
2. `OutputLink`, callback core, `TestOutput`, and the handshake tests.
3. Decoding, metadata, capabilities, and accurate seek refinement against real fixtures.
4. Conversion, resampler delay and tail handling, bounded ring, `CpalOutput` negotiation.
5. Worker state machine, event admission, device-failure classification, shutdown.
6. CLI, key loop, render, CI prerequisite, acceptance checks.

Each step must produce an exercised behaviour. The implementation plan specifies concrete APIs, dependency features, the concurrency proof obligations, and the commands that verify each task.

## 15. Known limitations recorded for v0.1

Position is an estimate: CPAL's `playback` timestamp is a prediction on every backend. Seek and stop leave up to one output latency of previously submitted audio audible — an audible splice, not a defect; no cross-fade. The output stream runs continuously while paused, trading idle power for callback liveness. Span-ring capacity and the 250 ms buffer target are starting values to be validated by measurement.
