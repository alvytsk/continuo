# Milestone 1 — known debt

Findings raised during M1 review that were deliberately deferred rather than
fixed. None blocks correctness of the shipped slice; each was judged against the
milestone's binding invariant (stop and transport recreation preserve the logical
playback position) and found not to threaten it.

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
  `BufferSize::Default`, and carries no sample format although the spec calls for one;
  `f32` is hardcoded at `build_output_stream`.
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

## Not measured

- Span-ring capacity (64) and the PCM ring target (250 ms) remain the spec's starting
  values. Validating them needs a full track on real hardware with `spans_dropped` and
  xrun counts logged — part of the outstanding manual acceptance.
