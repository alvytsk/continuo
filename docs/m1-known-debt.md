# Milestone 1 — known debt

Findings raised during M1 review that were deliberately deferred rather than
fixed. None blocks correctness of the shipped slice; each was judged against the
milestone's binding invariant (stop and transport recreation preserve the logical
playback position) and found not to threaten it. Milestone 2's carried debt is
recorded in the second half of this file.

## Error modelling

- **`PlaybackError::Failed(String)` is stringly-typed**, which the project's error
  policy forbids. The stringiness originates upstream: `Worker::fail(message: String)`
  and `PlaybackEvent::Failed { message: String }` already collapse every worker fault
  to a display string, so the CLI has no typed cause left to preserve. Fixing it
  honestly means giving the event protocol a typed cause.
- **`PlaybackError::Io` and `Failed` live in `src/playback/error.rs`** although the
  M1 plan's file list for that task did not include it. Recorded as a plan amendment.

## Decoder

- `seek_refined` reports `refinement_truncated: false` when refinement hits end of
  media, so the flag alone cannot distinguish "landed exactly" from "ran out". The
  engine compensates at the call site by comparing `actual` against the target.
- After `PlaybackError::Cancelled`, the source sits at an arbitrary mid-refinement
  position. No documented post-cancel contract; callers must re-establish position.
- `duration_to_frames` floors via `as u64`, so a sub-frame target lands one frame early.
- `self.pending = !self.planes[0].is_empty()` is a dead guard — the overshoot is
  always at least one frame inside that branch.

## Resampling

- `Converter::finish` carries an unexplained `MAX_FLUSH_CHUNKS = 8` cap. If
  `output_delay()` ever exceeded it, `finish` would return short with no error.

## Output and device handling

- `Phase::from_u8` / `Adopted::from_u8` map an unrecognised tag to `Run`/`Running` —
  fail-open rather than fail-safe. Unreachable today: only `as_u8` output is ever
  packed and a `u64` atomic cannot tear.
- The fault-classification fallback absorbs any variant cpal adds in future releases.
  Unavoidable given `#[non_exhaustive]`; a new variant should be classified explicitly.
- `NegotiatedOutput` fabricates `buffer_frames: 1024` while opening with
  `BufferSize::Default`. (Sample format is now negotiated: if the device's default
  config is not `f32`, its supported configurations are searched for one that is,
  preferring the default's sample rate, before refusing.)
- Integer-only devices remain unsupported. The engine writes `f32` buffers throughout;
  the negotiation guard only makes the refusal legible instead of an obscure failure
  when the stream is built. Supporting them means a format conversion at the callback.
- `OutputFault::Rebuild(DeviceBusy)` is the only Rebuild classification without a test.

## Engine

- Two owners call `link.take_diagnostics()` — `Worker::collect_diagnostics` and
  `Handshake::drain_spans`. No double-count (it is a swap), but xruns consumed inside
  `freeze_and_capture` never reach the aggregated warning, which therefore under-reports.
- A `Failed` event can carry a newer `session_rev` than the keep-latest `Progress`
  snapshot published a tick earlier. The app's `session_rev` guard correctly skips the
  stale snapshot; costs at most one render tick.
- After a discard timeout during a seek, `seek_to` still emits `SeekCompleted { actual }`
  even though `rebuild` has restored the pre-seek position, so the event and the position
  disagree. The position is correct, so the invariant holds, but the event carries the
  post-rebuild `session_rev` and cannot be filtered as stale. Fix before anything treats
  `actual` as authoritative (a checkpoint writer, a UI seek bar).
- A panic on the decode thread leaves the worker dead while the UI keeps rendering its
  last state: no `Failed` is emitted and the app appears frozen rather than reporting the
  fault. A dying worker should surface as `Failed`.
- `capture_and_teardown`'s rescue fallback reports frames *submitted* rather than *heard*,
  so a device loss taken while the callback holds an unpublished span reports a position up
  to one output latency ahead. Bounded by one output buffer and flagged `Degraded`.
- The playback path opens and probes the file twice: once to validate before entering raw
  mode, once in the worker. Eliminating it means threading an opened source through `Load`.
- Dead API: `EngineHandle::spawn`, `CpalOutput::drain_faults`, `DecodedSource::time_base`,
  `CallbackCore::sample_rate`, `TestOutput::buffer_frames`.

## CLI

- `--help` and `--version` exit non-zero to stderr, because every `try_parse` error is
  treated as a failure. The bare-invocation behaviour is deliberate and tested; the
  explicit-flag case likely is not.

## Untested paths

- The fault **deferral** gate (holding a fault back when the event backlog has no room)
  has no end-to-end test. Command admission closes as soon as one event is pending, so a
  harness cannot build the ~121-deep backlog deferral needs by sending commands; reaching
  it requires many asynchronous events. The *retirement* rule that pairs with it is unit
  tested (`retire_faults`), and the gate arithmetic is documented beside the constant, but
  neither is evidence the gate fires correctly in a running engine.
- F1's whole-frame ring atomicity is structural (one commit per batch) and has no test:
  the race needs a consumer observing a partially written batch, which the sequential
  harness cannot stage.
- Integer-only and multichannel-only device refusals are unverified without such hardware.

## Not measured

- Span-ring capacity (64) and the PCM ring target (250 ms) remain the spec's starting
  values. Validating them needs a full track on real hardware with `spans_dropped` and
  xrun counts logged — part of the outstanding manual acceptance.

# Milestone 2 — carried debt

Findings from the M2 review that were judged fine to carry. None threatens the
milestone's invariant (a position the listener reached is never overwritten by
one the engine never validated); each is here so the next milestone inherits the
reasoning rather than rediscovering the finding.

## Writer thread

- The writer wakes ten times a second even with an empty slot, because
  `IDLE_WAIT` caps every wait in `src/persistence/writer.rs`. The principled fix
  is to read `closing` inside the slot's critical section, so an empty slot can
  wait unbounded and still be woken by the shutdown.
- A detach after the D10 timeout can leave an orphan
  `state.json.tmp-<pid>-<seq>` behind if the process dies mid-write
  (`src/persistence/store.rs`). It partly self-heals: a later run that reuses the
  pid hits `create_new` → `AlreadyExists`, removes the stale temp, and succeeds
  on the next sequence.
- `submits_after_shutdown_are_rejected` in `tests/persistence_writer.rs` passes
  for the wrong reason — by the time `shutdown()` has returned the writer thread
  has already exited, so the test would pass without the `closing` check it
  claims to pin.

## Untested paths

- D11's three-failure warning, D3's disable reasons and the honest-flush log are
  all policy expressed through `tracing` (`src/persistence/writer.rs`,
  `src/app.rs`), and none of them is assertable. A small
  `tracing_subscriber::Layer` collecting `(level, message)` pairs would close the
  whole class at once.
- D19's backlog half is covered only by a race
  (`an_event_racing_the_shutdown_interrupt_still_arrives` in
  `tests/engine_shutdown.rs`), which passes whichever side of the race wins. The
  deterministic companion it wants would overflow the worker past
  `EVENT_CAPACITY - RESERVED_EVENT_SLOTS`, so that `flush_events` provably breaks,
  and then assert that the overflow events arrive after the channel's.
- The atomic-replace test in `tests/persistence_store.rs` proves that two
  successive writes leave no stale tail, but it cannot simulate a crash. Crash
  safety rests on the mechanism being inspectable — `create_new`, `fsync`,
  `rename` — rather than on a test.

## Session policy

- `record_outgoing`'s "call this before the per-media fields reset" contract
  (`src/session.rs`) is documentary. Taking the three fields it reads as
  parameters would make the ordering a fact the borrow checker enforces.
- `playback` still describes the previous media for one iteration after a
  `Loaded` (`src/session.rs`), which is safe only because the establishment gate
  covers that window. The invariant it rests on — `playback == Playing` implies
  `established` — is asserted nowhere.
