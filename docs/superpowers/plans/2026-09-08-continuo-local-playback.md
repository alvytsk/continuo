# Continuo Local Playback (Milestone 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver the first audible slice — `continuo play <path>` decodes a local MP3/FLAC/WAV file through CPAL with keyboard control, and preserves its logical position across pause, stop, seek, and device recreation.

**Architecture:** A decode worker thread owns the entire audio pipeline and is the sole authority for playback state, the position anchor, and the transport generation. The main thread reads keys, renders, and holds a mirror it never writes back. The CPAL callback publishes *media spans* — cumulative frame totals tagged with CPAL's predicted playback instant — into a lock-free ring, from which the worker reconstructs position without ever subtracting a latency estimate from a frame counter.

**Tech Stack:** Rust 1.98.1, edition 2024; symphonia 0.6.1, cpal 0.18.2, rubato 5.0.0, rtrb 0.4.0, crossbeam-channel 0.5.17, crossterm 0.29.0, clap 4; existing thiserror 2, tracing 0.1, time 0.3.

**Spec:** `docs/superpowers/specs/2026-09-08-continuo-local-playback-design.md` (approved; read alongside this plan).

## Global Constraints

- Rust **1.98.1**, pinned by `rust-toolchain.toml`. Edition 2024. `Cargo.lock` committed.
- `unsafe_code = "forbid"` crate-wide. **No task may relax this.** Every design element here is achievable in safe Rust.
- `clippy::unwrap_used` and `clippy::expect_used` are **denied**. `clippy.toml` already sets `allow-unwrap-in-tests` / `allow-expect-in-tests`, which covers `#[test]` fns and `#[cfg(test)]` modules but **not bare helper fns in `tests/`** — scope any `#[allow(clippy::unwrap_used)]` to the helper, never to runtime code.
- `thiserror` throughout. No `anyhow`.
- Verification command for every task: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked`.
- **symphonia's `mp3` feature is NOT a default.** It must be listed explicitly or MP3 playback silently fails to probe.
- **No Tokio.** Orchestration is the main thread until M3. Do not add an async runtime.
- The CPAL data callback must not allocate, lock, log, or perform I/O. Bounded operations only.
- The CPAL stream is started once and **never hardware-paused**. Pause is the `Park` phase.
- No `PlaybackCheckpoint` is written. No queue, no HTTP, no persistence — those are M2+.
- Tests must pass with no audio device and no network. Real-device tests are `#[ignore]`d.
- M0 domain types are consumed, not modified: `MediaId`, `AbsolutePath`, `MediaMetadata`, `MediaCapabilities`, `Continuity`, `SeekSupport`, `SourceLocation`.

---

## Starting point

`b04ca96` and `0adbb6d`/`602630e` are merged. `src/` contains `error.rs`, `lib.rs`, `main.rs`, `media/{capabilities,id,metadata,source}.rs`, `playback/{mod,checkpoint}.rs`, `telemetry.rs`. `src/playback/mod.rs` currently contains only `pub mod checkpoint;`. There is no audio code, no CLI parsing, and no `src/app.rs`.

Local ALSA development headers are present (`pkg-config --modversion alsa` → 1.2.15.3). CI does **not** yet install them; Task 1 fixes that.

**Verified third-party signatures** used by this plan (read from the vendored sources, not from memory — 0.6 differs substantially from symphonia 0.5):

```rust
// symphonia 0.6.1
symphonia::default::get_probe().probe(&Hint, MediaSourceStream<'s>, FormatOptions, MetadataOptions)
    -> Result<Box<dyn FormatReader + 's>>
symphonia::default::get_codecs().make_audio_decoder(&AudioCodecParameters, &AudioDecoderOptions)
    -> Result<Box<dyn AudioDecoder>>
FormatReader::default_track(&self, TrackType) -> Option<&Track>
FormatReader::next_packet(&mut self) -> Result<Option<Packet>>   // Ok(None) == end of media
FormatReader::seek(&mut self, SeekMode, SeekTo) -> Result<SeekedTo>  // { track_id, required_ts, actual_ts }
AudioDecoder::decode(&mut self, &Packet) -> Result<GenericAudioBufferRef<'_>>
AudioDecoder::reset(&mut self)
GenericAudioBufferRef::copy_to_vecs_planar::<f32>(&self, &mut Vec<Vec<f32>>)
GenericAudioBufferRef::copy_to_vec_interleaved::<f32>(&self, &mut Vec<f32>)
Track { id, codec_params: Option<CodecParameters>, time_base: Option<TimeBase>,
        num_frames: Option<u64>, duration: Option<Duration>, delay, padding, .. }
AudioCodecParameters { codec, sample_rate: Option<u32>, channels: Option<Channels>, .. }

// cpal 0.18.2
Device::build_output_stream(&SupportedStreamConfig|&StreamConfig, data_cb, err_cb, timeout: Option<Duration>)
OutputCallbackInfo::timestamp() -> OutputStreamTimestamp { callback, playback }  // playback is a PREDICTION
StreamInstant::as_nanos(&self) -> u128
StreamInstant::checked_duration_since(&self, earlier) -> Option<Duration>  // None only when self < earlier
StreamTrait::now(&self) -> StreamInstant  // NOT reliably >= a delivered `playback`; see Task 2
ErrorKind::{ DeviceBusy, DeviceChanged, DeviceNotAvailable, HostUnavailable, InvalidInput,
             PermissionDenied, RealtimeDenied, ResourceExhausted, StreamInvalidated,
             UnsupportedConfig, UnsupportedOperation, Xrun, BackendError }

// rtrb 0.4.0 — Producer<T> and Consumer<T> are Send but NOT Sync; push/pop take &mut self.
// Neither may be placed in an Arc. Ownership is split, never shared.
Producer::{push, slots, buffer().capacity(), write_chunk_uninit, is_abandoned}
Consumer::{pop, slots, read_chunk, is_abandoned}
ReadChunk::commit_all(self)     // O(1) index advance; f32 is Copy with no Drop

// rubato 5.0.0
Resampler::{ process_into_buffer(&dyn Adapter, &mut dyn AdapterMut, Option<&Indexing>)
             -> ResampleResult<(usize, usize)>, output_delay, reset,
             input_frames_next, output_frames_max }
Indexing { input_offset, output_offset, partial_len: Option<usize>, active_channels_mask }
```

## File map and ownership

| File | Responsibility |
|---|---|
| `Cargo.toml`, `.github/workflows/ci.yml` | Audio dependency set, ALSA build prerequisite |
| `src/playback/mod.rs` | Module declarations and public re-exports |
| `src/playback/error.rs` | `PlaybackError`, `OutputError` |
| `src/playback/output/mod.rs` | `Nanos`, `SpanRecord`, `AudioOutput` trait, `OutputRequest`, `NegotiatedOutput`, `OutputFault` |
| `src/playback/timeline.rs` | Spans → played frames. **Pure: no cpal, no threads, no audio.** |
| `src/playback/link.rs` | `OutputLink` atomics: control/ack/epoch, phases, rescue slot, diagnostics |
| `src/playback/callback.rs` | The production callback core, shared by `CpalOutput` and `TestOutput` |
| `src/playback/output/test_output.rs` | Virtual-clock driver for the callback core |
| `src/playback/output/cpal_output.rs` | Device negotiation, stream construction, fault reporting |
| `src/playback/handshake.rs` | Worker side of freeze/capture/discard/park/install |
| `src/playback/decode.rs` | Open, metadata, capabilities, decode to planar f32, seek + refinement |
| `src/playback/resample.rs` | rubato wrapper, startup delay trim, EOF flush |
| `src/playback/volume.rs` | `Volume` newtype |
| `src/playback/command.rs`, `event.rs`, `state.rs` | Protocol and state enums |
| `src/playback/engine.rs` | Worker loop: state machine, admission control, diagnostics, shutdown |
| `src/app.rs`, `src/cli.rs` | Key loop, status render, mirror; clap |
| `tests/fixtures/` | Generated `sine.wav`, `sine.mp3`, `sine.flac` |

**Natural split point:** Tasks 1–5 deliver a complete, tested, device-free playback core. Tasks 6–10 attach real decoding, a real device, and the CLI. If this plan is executed across sessions, that is the seam.

---

### Task 1: Audio dependencies, fixtures, and build prerequisites

**Files:**
- Modify: `Cargo.toml`
- Modify: `.github/workflows/ci.yml:12-16`
- Modify: `docs/architecture.md` (§2 execution contexts)
- Create: `tests/fixtures/README.md`, `tests/fixtures/sine.wav`, `tests/fixtures/sine.mp3`, `tests/fixtures/sine.flac`
- Create: `tests/fixtures_and_features.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: the dependency set every later task compiles against; fixture paths `tests/fixtures/sine.{wav,mp3,flac}` used by Tasks 6, 7, 10.

- [ ] **Step 1: Generate the fixtures**

A 0.5 s 440 Hz sine at 44100 Hz, stereo, encoded three ways. Requires `ffmpeg` locally (fixtures are committed, so this runs once — CI never regenerates them).

```bash
mkdir -p tests/fixtures
ffmpeg -y -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=0.5" \
  -ac 2 -c:a pcm_s16le tests/fixtures/sine.wav
ffmpeg -y -i tests/fixtures/sine.wav -c:a libmp3lame -b:a 128k tests/fixtures/sine.mp3
ffmpeg -y -i tests/fixtures/sine.wav -c:a flac tests/fixtures/sine.flac
ls -l tests/fixtures/
```

If `ffmpeg` is unavailable, stop and report it rather than substituting a downloaded file — provenance must stay self-generated.

- [ ] **Step 2: Record fixture provenance**

```bash
cat > tests/fixtures/README.md <<'EOF'
# Test fixtures

Self-generated, no third-party content, no licensing constraints.

`sine.wav` — 0.5 s, 440 Hz, 44100 Hz, stereo, PCM s16le:

    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=0.5" \
      -ac 2 -c:a pcm_s16le sine.wav

`sine.mp3` and `sine.flac` are transcoded from `sine.wav` with
`-c:a libmp3lame -b:a 128k` and `-c:a flac` respectively.

MP3 and FLAC are present because M1 promises those formats. Testing WAV alone
would leave both promised codecs unexercised, and symphonia's `mp3` feature is
not enabled by default.
EOF
```

- [ ] **Step 3: Write the failing test**

This test guards the single highest-risk dependency mistake in the milestone: a missing `mp3` feature, which fails at probe time rather than at compile time.

```rust
// tests/fixtures_and_features.rs
use std::fs::File;
use std::path::Path;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

fn probe_fixture(name: &str, extension: &str) -> u32 {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    let file = File::open(&path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(extension);
    let reader = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .unwrap();
    let track = reader.default_track(TrackType::Audio).unwrap();
    track.codec_params.as_ref().unwrap().audio().unwrap().sample_rate.unwrap()
}

#[test]
fn every_promised_format_probes() {
    assert_eq!(probe_fixture("sine.wav", "wav"), 44100);
    assert_eq!(probe_fixture("sine.flac", "flac"), 44100);
    // Fails with a probe error unless Cargo.toml enables symphonia's non-default `mp3` feature.
    assert_eq!(probe_fixture("sine.mp3", "mp3"), 44100);
}
```

- [ ] **Step 4: Run it and confirm it fails**

Run: `cargo test --locked --test fixtures_and_features`
Expected: FAIL — `symphonia` is not yet a dependency, so the test does not compile.

- [ ] **Step 5: Add the dependencies**

```toml
# Cargo.toml — add to [dependencies]
symphonia = { version = "0.6.1", features = ["mp3", "aac", "isomp4", "alac"] }
cpal = "0.18.2"
rtrb = "0.4.0"
rubato = "5.0.0"
crossbeam-channel = "0.5.17"
crossterm = "0.29"
clap = { version = "4", features = ["derive"] }
```

`mp3`, `aac`, `isomp4` and `alac` are additive: symphonia's defaults already supply flac, wav/riff, ogg, vorbis, pcm and mkv. Do **not** pass `default-features = false`.

- [ ] **Step 6: Run the test and confirm it passes**

Run: `cargo test --locked --test fixtures_and_features`
Expected: PASS — all three formats probe at 44100 Hz.

If `sine.mp3` fails while the others pass, the `mp3` feature did not take effect. Check `cargo tree -i symphonia-bundle-mp3`.

- [ ] **Step 7: Add the CI build prerequisite**

CPAL's ALSA backend needs `libasound2-dev` headers. Insert before the toolchain step in `.github/workflows/ci.yml`:

```yaml
      - name: Install ALSA development headers
        run: sudo apt-get update && sudo apt-get install -y libasound2-dev
```

- [ ] **Step 8: Amend the architecture document**

`docs/architecture.md` §2 lists Tokio tasks as the application execution context. M1 has no Tokio. Replace that row's description with:

```
The application context is the main thread: it reads keys, renders status, and
owns the command sender and event receiver. Tokio arrives with M3's networking,
not before. This context never holds a decoder or a CPAL stream.
```

- [ ] **Step 9: Verify the whole build**

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked`
Expected: PASS. The new dependencies must not introduce clippy warnings at this stage because no code uses them yet.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml Cargo.lock .github/workflows/ci.yml docs/architecture.md tests/fixtures tests/fixtures_and_features.rs
git commit -m "Add audio dependencies, generated fixtures, and ALSA CI prerequisite"
```

---

### Task 2: The pure timeline

**Files:**
- Create: `src/playback/output/mod.rs` (partial: `Nanos`, `SpanRecord` only)
- Create: `src/playback/timeline.rs`
- Modify: `src/playback/mod.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub struct Nanos(pub u64)` with `checked_sub(self, Nanos) -> Option<Duration>` and `from_stream_nanos(u128) -> Nanos`
  - `pub struct SpanRecord { pub generation: u16, pub media_total_after: u64, pub t0: Nanos, pub frames: u32 }`
  - `pub enum PositionQuality { Exact, Estimated, Degraded }`
  - `pub struct Timeline` with `new(sample_rate: u32) -> Self`, `reset(&mut self, generation: u16)`, `accept(&mut self, SpanRecord)`, `note_dropped(&mut self, u32)`, `played_frames(&mut self, now: Nanos) -> u64`, `quality(&self) -> PositionQuality`

This is the highest-value task in the milestone: it holds every subtle correctness property, and it depends on nothing.

- [ ] **Step 1: Write the failing tests**

```rust
// src/playback/timeline.rs — tests module at the bottom of the file
#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    fn ms(n: u64) -> Nanos { Nanos(n * 1_000_000) }
    // 480 frames == 10 ms at 48 kHz.
    fn span(gen: u16, total: u64, t0_ms: u64, frames: u32) -> SpanRecord {
        SpanRecord { generation: gen, media_total_after: total, t0: ms(t0_ms), frames }
    }

    #[test]
    fn a_span_that_has_not_begun_contributes_nothing() {
        // The normal case: cpal's `playback` is a prediction, and `now()` on the
        // PulseAudio backend is bare elapsed time, so now < t0 routinely.
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 480, 100, 480));
        assert_eq!(t.played_frames(ms(50)), 0);
    }

    #[test]
    fn a_fully_elapsed_span_is_wholly_played() {
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 480, 100, 480));
        assert_eq!(t.played_frames(ms(200)), 480);
    }

    #[test]
    fn an_in_flight_span_interpolates_and_clamps() {
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 480, 100, 480));
        assert_eq!(t.played_frames(ms(105)), 240);
        assert_eq!(t.played_frames(ms(109)), 432);
        assert_eq!(t.played_frames(ms(110)), 480);
    }

    #[test]
    fn earlier_queued_spans_are_retained_not_overwritten() {
        // Two 10 ms spans at [100,110) and [110,120). At now = 105 the newest
        // span has not started while the first is halfway played. A single
        // latest-span slot reports 0 here; a history reports 240.
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 480, 100, 480));
        t.accept(span(0, 960, 110, 480));
        assert_eq!(t.played_frames(ms(105)), 240);
        assert_eq!(t.played_frames(ms(115)), 720);
    }

    #[test]
    fn underrun_silence_never_advances_or_rewinds_position() {
        // One second of media, then a long gap of injected silence. Position
        // must stay at one second - not drift, and not be dragged backward by
        // any latency subtraction.
        let mut t = Timeline::new(RATE);
        t.accept(SpanRecord { generation: 0, media_total_after: 48_000, t0: ms(0), frames: 48_000 });
        assert_eq!(t.played_frames(ms(1_000)), 48_000);
        assert_eq!(t.played_frames(ms(5_000)), 48_000);
        assert_eq!(t.played_frames(ms(60_000)), 48_000);
    }

    #[test]
    fn a_dropped_span_costs_granularity_not_frame_counts() {
        // Records carry absolute cumulative totals, so a lost record cannot
        // corrupt the count. Frames in the gap are credited only once the
        // surviving span's start instant has passed - under-reporting briefly,
        // never over-reporting.
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 480, 100, 480));
        t.note_dropped(1);                       // the [110,120) record was lost
        t.accept(span(0, 1440, 120, 480));       // totals still absolute and correct
        assert_eq!(t.played_frames(ms(115)), 480);
        assert_eq!(t.played_frames(ms(130)), 1440);
        assert_eq!(t.quality(), PositionQuality::Degraded);
    }

    #[test]
    fn overlapping_spans_disable_interpolation_and_degrade() {
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 480, 100, 480));
        t.accept(span(0, 960, 105, 480));        // starts before its predecessor ends
        assert_eq!(t.quality(), PositionQuality::Degraded);
        assert_eq!(t.played_frames(ms(107)), 480); // floor only, no interpolation
    }

    #[test]
    fn spans_from_a_retired_generation_are_ignored() {
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 48_000, 0, 48_000));
        t.reset(1);
        assert_eq!(t.played_frames(ms(10_000)), 0);
        t.accept(span(0, 96_000, 0, 48_000));    // stale generation
        assert_eq!(t.played_frames(ms(10_000)), 0);
        t.accept(span(1, 480, 0, 480));
        assert_eq!(t.played_frames(ms(10_000)), 480);
    }

    #[test]
    fn end_of_media_waits_for_the_last_frames_predicted_play_time() {
        // EOF must not fire when the ring merely empties. The final span's last
        // frame is predicted at t0 + frames/rate, offset within its own buffer.
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 960, 100, 480));
        assert!(t.played_frames(ms(105)) < 960);
        assert_eq!(t.played_frames(ms(110)), 960);
    }
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --locked timeline`
Expected: FAIL — `src/playback/timeline.rs` does not exist.

- [ ] **Step 3: Write `Nanos` and `SpanRecord`**

```rust
// src/playback/output/mod.rs
use std::time::Duration;

/// A point on the output device's clock, in nanoseconds.
///
/// This exists so `timeline` never depends on cpal: `TestOutput` synthesizes
/// these from a virtual clock, `CpalOutput` converts them from `StreamInstant`.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Nanos(pub u64);

impl Nanos {
    /// `cpal::StreamInstant::as_nanos` returns `u128`; saturate rather than wrap.
    /// The clamp is unreachable in practice (u64 nanoseconds is 585 years).
    pub fn from_stream_nanos(value: u128) -> Self {
        Self(u64::try_from(value).unwrap_or(u64::MAX))
    }

    pub fn checked_sub(self, earlier: Self) -> Option<Duration> {
        self.0.checked_sub(earlier.0).map(Duration::from_nanos)
    }

    pub fn saturating_add_frames(self, frames: u64, sample_rate: u32) -> Self {
        let rate = u64::from(sample_rate.max(1));
        Self(self.0.saturating_add(frames.saturating_mul(1_000_000_000) / rate))
    }
}

/// One callback's contribution of media to the output timeline.
///
/// A callback pops the ring once, so media always occupies a prefix of the
/// output buffer: `frames` media frames at offsets `[0, frames)`, injected
/// silence afterwards. `t0` is cpal's predicted instant for offset 0.
///
/// `media_total_after` is **cumulative and absolute**, not a delta. That is what
/// makes a dropped record cost timing granularity without corrupting counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpanRecord {
    pub generation: u16,
    pub media_total_after: u64,
    pub t0: Nanos,
    pub frames: u32,
}

impl SpanRecord {
    pub fn end(&self, sample_rate: u32) -> Nanos {
        self.t0.saturating_add_frames(u64::from(self.frames), sample_rate)
    }

    pub fn media_total_before(&self) -> u64 {
        self.media_total_after.saturating_sub(u64::from(self.frames))
    }
}
```

- [ ] **Step 4: Write the timeline**

```rust
// src/playback/timeline.rs
use std::collections::VecDeque;

use super::output::{Nanos, SpanRecord};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositionQuality {
    Exact,
    Estimated,
    Degraded,
}

/// Reconstructs played media frames from the spans the output callback publishes.
///
/// Deliberately does **not** subtract an output latency from a submitted-frame
/// counter. That formulation fails across silence: one second of media followed
/// by a long underrun leaves the counter at one second while a persistent
/// latency drags the reported position backward indefinitely. Injected silence
/// is simply absent from this timeline.
#[derive(Debug)]
pub struct Timeline {
    sample_rate: u32,
    generation: u16,
    /// Media frames known to have finished playing.
    floor: u64,
    /// End instant of the most recently accepted span, for overlap validation.
    last_end: Nanos,
    last_total: u64,
    pending: VecDeque<SpanRecord>,
    dropped: u32,
    degraded: bool,
}

impl Timeline {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            generation: 0,
            floor: 0,
            last_end: Nanos(0),
            last_total: 0,
            pending: VecDeque::new(),
            dropped: 0,
            degraded: false,
        }
    }

    /// Adopt a new transport generation. Everything from the old one is void.
    pub fn reset(&mut self, generation: u16) {
        self.generation = generation;
        self.floor = 0;
        self.last_end = Nanos(0);
        self.last_total = 0;
        self.pending.clear();
        self.dropped = 0;
        self.degraded = false;
    }

    pub fn note_dropped(&mut self, count: u32) {
        if count > 0 {
            self.dropped = self.dropped.saturating_add(count);
            self.degraded = true;
        }
    }

    pub fn accept(&mut self, record: SpanRecord) {
        if record.generation != self.generation {
            return;
        }
        let contiguous = record.media_total_after >= self.last_total
            && record.t0 >= self.last_end
            && record.media_total_before() >= self.last_total;
        if !contiguous && self.last_total > 0 {
            self.degraded = true;
        }
        self.last_end = record.end(self.sample_rate);
        self.last_total = record.media_total_after;
        self.pending.push_back(record);
    }

    pub fn played_frames(&mut self, now: Nanos) -> u64 {
        while let Some(front) = self.pending.front() {
            if now >= front.end(self.sample_rate) {
                self.floor = front.media_total_after;
                self.pending.pop_front();
            } else {
                break;
            }
        }
        let Some(front) = self.pending.front() else {
            return self.floor;
        };
        // `now < t0` is the normal case for the newest span, not an anomaly:
        // cpal's `playback` is a prediction ahead of the callback instant.
        if now < front.t0 || self.degraded {
            return self.floor;
        }
        let Some(elapsed) = now.checked_sub(front.t0) else {
            return self.floor;
        };
        let elapsed_frames = elapsed.as_nanos() * u128::from(self.sample_rate) / 1_000_000_000;
        let elapsed_frames = u64::try_from(elapsed_frames).unwrap_or(u64::from(front.frames));
        front.media_total_before() + elapsed_frames.min(u64::from(front.frames))
    }

    pub fn quality(&self) -> PositionQuality {
        if self.degraded { PositionQuality::Degraded } else { PositionQuality::Estimated }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}
```

- [ ] **Step 5: Declare the modules**

```rust
// src/playback/mod.rs
pub mod checkpoint;
pub mod output;
pub mod timeline;
```

- [ ] **Step 6: Run the tests and confirm they pass**

Run: `cargo test --locked timeline`
Expected: PASS — all nine tests.

If `earlier_queued_spans_are_retained_not_overwritten` fails, the retirement loop is discarding a span before its end instant. If `underrun_silence_never_advances_or_rewinds_position` fails, a latency subtraction has crept in.

- [ ] **Step 7: Verify and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked
git add src/playback/mod.rs src/playback/output/mod.rs src/playback/timeline.rs
git commit -m "Add pure media-span timeline for position reconstruction"
```

---

### Task 3: The shared output link

**Files:**
- Create: `src/playback/link.rs`
- Modify: `src/playback/mod.rs`

**Interfaces:**
- Consumes: `SpanRecord`, `Nanos` from Task 2.
- Produces:
  - `pub enum Phase { Run, Freeze, Discard, Park }` and `pub enum Adopted { Running, Frozen, Parked }`, both `#[repr(u8)]`-convertible via `as_u8`/`from_u8`
  - `pub struct Control { pub generation: u16, pub epoch: u32, pub phase: Phase }`
  - `pub struct OutputLink` with `new() -> Self`, `publish_control(&self, Control)`, `load_control(&self) -> Control`, `acknowledge(&self, generation: u16, epoch: u32, Adopted)`, `load_ack(&self) -> (u16, u32, Adopted)`, `set_gain(&self, f32)`, `gain(&self) -> f32`, `note_xrun(&self)`, `note_dropped_span(&self)`, `take_diagnostics(&self) -> Diagnostics`, `stash_rescue(&self, SpanRecord)`, `clear_rescue(&self)`, `take_rescue_after_teardown(&self) -> Option<SpanRecord>`
  - `pub struct Diagnostics { pub xruns: u32, pub spans_dropped: u32 }`

`OutputLink` holds **atomics only**. It must not contain an `rtrb` endpoint: `Producer<T>` and `Consumer<T>` are `Send` but not `Sync` (they cache positions in a `Cell`) and their methods take `&mut self`, so neither can live behind an `Arc`.

- [ ] **Step 1: Write the failing tests**

```rust
// src/playback/link.rs — tests module at the bottom
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn control_round_trips_through_one_atomic_word() {
        let link = OutputLink::new();
        let control = Control { generation: 7, epoch: 300_000, phase: Phase::Discard };
        link.publish_control(control);
        assert_eq!(link.load_control(), control);
    }

    #[test]
    fn a_stale_epoch_acknowledgment_is_distinguishable() {
        // Repeated same-generation Park/Run transitions must not accept an old
        // acknowledgment, which is why every publication bumps the epoch.
        let link = OutputLink::new();
        link.acknowledge(3, 10, Adopted::Parked);
        assert_eq!(link.load_ack(), (3, 10, Adopted::Parked));
        link.acknowledge(3, 11, Adopted::Running);
        assert_ne!(link.load_ack(), (3, 10, Adopted::Parked));
        assert_eq!(link.load_ack(), (3, 11, Adopted::Running));
    }

    #[test]
    fn the_rescue_slot_survives_the_callback_being_dropped() {
        // Dropping a cpal stream destroys the callback closure and any span it
        // had not yet published. The rescue slot lives in the link instead.
        let link = Arc::new(OutputLink::new());
        let record = SpanRecord {
            generation: 2, media_total_after: 9_600, t0: Nanos(1_000), frames: 480,
        };
        let callback = {
            let link = Arc::clone(&link);
            move || link.stash_rescue(record)
        };
        callback();
        drop(callback);
        assert_eq!(link.take_rescue_after_teardown(), Some(record));
        assert_eq!(link.take_rescue_after_teardown(), None);
    }

    #[test]
    fn diagnostics_accumulate_and_drain_to_zero() {
        let link = OutputLink::new();
        link.note_xrun();
        link.note_xrun();
        link.note_dropped_span();
        assert_eq!(link.take_diagnostics(), Diagnostics { xruns: 2, spans_dropped: 1 });
        assert_eq!(link.take_diagnostics(), Diagnostics { xruns: 0, spans_dropped: 0 });
    }

    #[test]
    fn gain_round_trips_as_a_bit_pattern() {
        let link = OutputLink::new();
        assert_eq!(link.gain(), 1.0);
        link.set_gain(0.375);
        assert_eq!(link.gain(), 0.375);
    }
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --locked link`
Expected: FAIL — `src/playback/link.rs` does not exist.

- [ ] **Step 3: Write the link**

```rust
// src/playback/link.rs
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use super::output::{Nanos, SpanRecord};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Run,
    Freeze,
    Discard,
    Park,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Adopted {
    Running,
    Frozen,
    Parked,
}

impl Phase {
    fn as_u8(self) -> u8 {
        match self {
            Self::Run => 0,
            Self::Freeze => 1,
            Self::Discard => 2,
            Self::Park => 3,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Freeze,
            2 => Self::Discard,
            3 => Self::Park,
            _ => Self::Run,
        }
    }
}

impl Adopted {
    fn as_u8(self) -> u8 {
        match self {
            Self::Running => 0,
            Self::Frozen => 1,
            Self::Parked => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Frozen,
            2 => Self::Parked,
            _ => Self::Running,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Control {
    pub generation: u16,
    pub epoch: u32,
    pub phase: Phase,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Diagnostics {
    pub xruns: u32,
    pub spans_dropped: u32,
}

fn pack(generation: u16, epoch: u32, tag: u8) -> u64 {
    (u64::from(generation) << 48) | (u64::from(epoch) << 8) | u64::from(tag)
}

fn unpack(word: u64) -> (u16, u32, u8) {
    (
        (word >> 48) as u16,
        ((word >> 8) & 0xFFFF_FFFF) as u32,
        (word & 0xFF) as u8,
    )
}

/// Atomics shared between the decode worker and the output callback.
///
/// Contains no locks and no ring endpoints. `rtrb::Producer`/`Consumer` are
/// `Send` but not `Sync` and need `&mut self`, so ring ownership is split
/// between the two contexts rather than shared through this struct.
#[derive(Debug)]
pub struct OutputLink {
    /// Worker-written: `(generation, epoch, phase)`.
    control: AtomicU64,
    /// Callback-written: `(generation, epoch, adopted)`.
    ack: AtomicU64,
    /// Worker-written target gain, as `f32` bits.
    gain: AtomicU32,
    xruns: AtomicU32,
    spans_dropped: AtomicU32,
    /// Callback-written last-unpublished span; read only after teardown.
    rescue_generation: AtomicU32,
    rescue_total: AtomicU64,
    rescue_t0: AtomicU64,
    rescue_frames: AtomicU32,
    rescue_valid: AtomicU8,
}

impl OutputLink {
    pub fn new() -> Self {
        Self {
            control: AtomicU64::new(pack(0, 0, Phase::Park.as_u8())),
            ack: AtomicU64::new(pack(0, 0, Adopted::Parked.as_u8())),
            gain: AtomicU32::new(1.0f32.to_bits()),
            xruns: AtomicU32::new(0),
            spans_dropped: AtomicU32::new(0),
            rescue_generation: AtomicU32::new(0),
            rescue_total: AtomicU64::new(0),
            rescue_t0: AtomicU64::new(0),
            rescue_frames: AtomicU32::new(0),
            rescue_valid: AtomicU8::new(0),
        }
    }

    pub fn publish_control(&self, control: Control) {
        self.control.store(
            pack(control.generation, control.epoch, control.phase.as_u8()),
            Ordering::Release,
        );
    }

    pub fn load_control(&self) -> Control {
        let (generation, epoch, tag) = unpack(self.control.load(Ordering::Acquire));
        Control { generation, epoch, phase: Phase::from_u8(tag) }
    }

    pub fn acknowledge(&self, generation: u16, epoch: u32, adopted: Adopted) {
        self.ack
            .store(pack(generation, epoch, adopted.as_u8()), Ordering::Release);
    }

    pub fn load_ack(&self) -> (u16, u32, Adopted) {
        let (generation, epoch, tag) = unpack(self.ack.load(Ordering::Acquire));
        (generation, epoch, Adopted::from_u8(tag))
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    pub fn note_xrun(&self) {
        self.xruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_dropped_span(&self) {
        self.spans_dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn take_diagnostics(&self) -> Diagnostics {
        Diagnostics {
            xruns: self.xruns.swap(0, Ordering::Relaxed),
            spans_dropped: self.spans_dropped.swap(0, Ordering::Relaxed),
        }
    }

    /// Mirror the callback's unpublished span so it survives teardown.
    pub fn stash_rescue(&self, record: SpanRecord) {
        self.rescue_generation
            .store(u32::from(record.generation), Ordering::Relaxed);
        self.rescue_total.store(record.media_total_after, Ordering::Relaxed);
        self.rescue_t0.store(record.t0.0, Ordering::Relaxed);
        self.rescue_frames.store(record.frames, Ordering::Relaxed);
        self.rescue_valid.store(1, Ordering::Release);
    }

    pub fn clear_rescue(&self) {
        self.rescue_valid.store(0, Ordering::Release);
    }

    /// Read the stashed span. Sound only once the callback is provably stopped:
    /// the ordering obligation is discharged by the backend thread join inside
    /// the stream's `Drop`, so this read is not concurrent.
    pub fn take_rescue_after_teardown(&self) -> Option<SpanRecord> {
        if self.rescue_valid.swap(0, Ordering::Acquire) == 0 {
            return None;
        }
        Some(SpanRecord {
            generation: self.rescue_generation.load(Ordering::Relaxed) as u16,
            media_total_after: self.rescue_total.load(Ordering::Relaxed),
            t0: Nanos(self.rescue_t0.load(Ordering::Relaxed)),
            frames: self.rescue_frames.load(Ordering::Relaxed),
        })
    }
}

impl Default for OutputLink {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 4: Declare the module and run the tests**

Add `pub mod link;` to `src/playback/mod.rs`.

Run: `cargo test --locked link`
Expected: PASS — all five tests.

- [ ] **Step 5: Verify and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked
git add src/playback/mod.rs src/playback/link.rs
git commit -m "Add shared output link with epoch-tagged control and rescue slot"
```

---

### Task 4: The callback core and virtual-clock test output

**Files:**
- Create: `src/playback/callback.rs`
- Create: `src/playback/output/test_output.rs`
- Modify: `src/playback/mod.rs`, `src/playback/output/mod.rs`

**Interfaces:**
- Consumes: `OutputLink`, `Control`, `Phase`, `Adopted` (Task 3); `SpanRecord`, `Nanos` (Task 2).
- Produces:
  - `pub struct CallbackCore` with `new(link: Arc<OutputLink>, pcm: Consumer<f32>, spans: Producer<SpanRecord>, channels: u16, sample_rate: u32) -> Self` and `fill(&mut self, out: &mut [f32], playback: Nanos)`
  - `pub struct TestOutput` with `new(channels: u16, sample_rate: u32, buffer_frames: u32, latency: Duration) -> Self`, `attach(&mut self, CallbackCore)`, `advance(&mut self, Duration)`, `now(&self) -> Nanos`, `captured(&self) -> &[f32]`

The core owns `Consumer<f32>` and `Producer<SpanRecord>` outright — this is the split ownership Task 3's link deliberately does not provide.

- [ ] **Step 1: Write the failing tests**

```rust
// src/playback/callback.rs — tests module at the bottom
#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::output::test_output::TestOutput;
    use std::time::Duration;

    const RATE: u32 = 48_000;

    fn harness(pcm_frames: usize) -> (Arc<OutputLink>, rtrb::Producer<f32>,
                                      rtrb::Consumer<SpanRecord>, TestOutput) {
        let link = Arc::new(OutputLink::new());
        let (pcm_tx, pcm_rx) = rtrb::RingBuffer::<f32>::new(pcm_frames * 2);
        let (span_tx, span_rx) = rtrb::RingBuffer::<SpanRecord>::new(64);
        let core = CallbackCore::new(Arc::clone(&link), pcm_rx, span_tx, 2, RATE);
        let mut output = TestOutput::new(2, RATE, 480, Duration::from_millis(20));
        output.attach(core);
        (link, pcm_tx, span_rx, output)
    }

    fn push_frames(tx: &mut rtrb::Producer<f32>, frames: usize, value: f32) {
        for _ in 0..frames * 2 {
            tx.push(value).unwrap();
        }
    }

    #[test]
    fn running_publishes_a_span_whose_start_is_the_predicted_playback_instant() {
        let (link, mut pcm, mut spans, mut out) = harness(4_800);
        push_frames(&mut pcm, 480, 0.5);
        link.publish_control(Control { generation: 1, epoch: 1, phase: Phase::Run });
        out.advance(Duration::from_millis(10));
        let record = spans.pop().unwrap();
        assert_eq!(record.generation, 1);
        assert_eq!(record.frames, 480);
        assert_eq!(record.media_total_after, 480);
        // 20 ms of virtual latency ahead of the callback instant.
        assert_eq!(record.t0, Nanos(20_000_000));
    }

    #[test]
    fn an_underrun_emits_silence_and_publishes_only_the_media_prefix() {
        let (link, mut pcm, mut spans, mut out) = harness(4_800);
        push_frames(&mut pcm, 200, 0.5);           // less than one 480-frame buffer
        link.publish_control(Control { generation: 1, epoch: 1, phase: Phase::Run });
        out.advance(Duration::from_millis(10));
        let record = spans.pop().unwrap();
        assert_eq!(record.frames, 200);
        assert_eq!(record.media_total_after, 200);
        let captured = out.captured();
        assert!(captured[..400].iter().all(|s| *s != 0.0));
        assert!(captured[400..960].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn park_emits_silence_consumes_nothing_and_acknowledges() {
        let (link, mut pcm, mut spans, mut out) = harness(4_800);
        push_frames(&mut pcm, 480, 0.5);
        link.publish_control(Control { generation: 1, epoch: 4, phase: Phase::Park });
        out.advance(Duration::from_millis(10));
        assert_eq!(link.load_ack(), (1, 4, Adopted::Parked));
        assert!(spans.pop().is_err());
        assert!(out.captured().iter().all(|s| *s == 0.0));
    }

    #[test]
    fn freeze_acknowledges_only_after_the_pending_span_is_published() {
        // The callback may not acknowledge while a span is unpublished, so a
        // saturated span ring holds the acknowledgment back until the worker
        // drains. Task 5 relies on this being the only thing that gates it.
        let link = Arc::new(OutputLink::new());
        let (mut pcm_tx, pcm_rx) = rtrb::RingBuffer::<f32>::new(48_000);
        let (span_tx, mut span_rx) = rtrb::RingBuffer::<SpanRecord>::new(1);
        let core = CallbackCore::new(Arc::clone(&link), pcm_rx, span_tx, 2, RATE);
        let mut out = TestOutput::new(2, RATE, 480, Duration::from_millis(20));
        out.attach(core);
        for _ in 0..480 * 2 * 3 {
            pcm_tx.push(0.5).unwrap();
        }
        link.publish_control(Control { generation: 1, epoch: 1, phase: Phase::Run });
        // Three callbacks: the first publishes, the second is retained as
        // `pending`, the third displaces that retained record - the first
        // actual loss. A retained span is not a lost one.
        out.advance(Duration::from_millis(30));
        assert!(link.take_diagnostics().spans_dropped >= 1);

        link.publish_control(Control { generation: 1, epoch: 2, phase: Phase::Freeze });
        out.advance(Duration::from_millis(10));
        assert_ne!(link.load_ack(), (1, 2, Adopted::Frozen)); // still holding a pending span

        span_rx.pop().unwrap();                       // the worker drains
        out.advance(Duration::from_millis(10));
        assert_eq!(link.load_ack(), (1, 2, Adopted::Frozen));
    }

    #[test]
    fn discard_drops_the_whole_ring_without_advancing_progress() {
        let (link, mut pcm, mut spans, mut out) = harness(4_800);
        push_frames(&mut pcm, 480, 0.5);
        link.publish_control(Control { generation: 1, epoch: 1, phase: Phase::Run });
        out.advance(Duration::from_millis(10));
        let before = spans.pop().unwrap().media_total_after;

        push_frames(&mut pcm, 2_000, 0.5);
        link.publish_control(Control { generation: 1, epoch: 2, phase: Phase::Discard });
        out.advance(Duration::from_millis(10));
        assert_eq!(link.load_ack(), (1, 2, Adopted::Parked));
        assert!(spans.pop().is_err());               // discarded frames publish nothing
        assert_eq!(before, 480);
    }

    #[test]
    fn adopting_a_new_generation_zeroes_the_media_counter() {
        let (link, mut pcm, mut spans, mut out) = harness(4_800);
        push_frames(&mut pcm, 480, 0.5);
        link.publish_control(Control { generation: 1, epoch: 1, phase: Phase::Run });
        out.advance(Duration::from_millis(10));
        assert_eq!(spans.pop().unwrap().media_total_after, 480);

        push_frames(&mut pcm, 480, 0.5);
        link.publish_control(Control { generation: 2, epoch: 2, phase: Phase::Run });
        out.advance(Duration::from_millis(10));
        let record = spans.pop().unwrap();
        assert_eq!(record.generation, 2);
        assert_eq!(record.media_total_after, 480);
    }

    #[test]
    fn gain_ramps_once_per_frame_across_both_channels() {
        let (link, mut pcm, _spans, mut out) = harness(4_800);
        push_frames(&mut pcm, 480, 1.0);
        link.set_gain(0.0);
        link.publish_control(Control { generation: 1, epoch: 1, phase: Phase::Run });
        out.advance(Duration::from_millis(10));
        let captured = out.captured();
        for frame in captured.chunks_exact(2) {
            assert_eq!(frame[0], frame[1], "one gain value per frame, across channels");
        }
        assert!(captured[0].abs() < captured[958].abs(), "gain ramps rather than stepping");
    }
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --locked callback`
Expected: FAIL — `src/playback/callback.rs` does not exist.

- [ ] **Step 3: Write the callback core**

```rust
// src/playback/callback.rs
use std::sync::Arc;

use rtrb::{Consumer, Producer};

use super::link::{Adopted, Control, OutputLink, Phase};
use super::output::{Nanos, SpanRecord};

/// The production output callback, shared by `CpalOutput` and `TestOutput`.
///
/// Allocation-free, lock-free, and I/O-free. Owns its ring endpoints outright
/// because `rtrb`'s `Producer`/`Consumer` are not `Sync`.
pub struct CallbackCore {
    link: Arc<OutputLink>,
    pcm: Consumer<f32>,
    spans: Producer<SpanRecord>,
    channels: u16,
    sample_rate: u32,
    generation: u16,
    media_total: u64,
    /// A span that could not be published because the ring was full. Republished
    /// on every later invocation, including while frozen or parked, and it gates
    /// the freeze and park acknowledgments.
    pending: Option<SpanRecord>,
    current_gain: f32,
}

impl CallbackCore {
    pub fn new(
        link: Arc<OutputLink>,
        pcm: Consumer<f32>,
        spans: Producer<SpanRecord>,
        channels: u16,
        sample_rate: u32,
    ) -> Self {
        let gain = link.gain();
        Self {
            link,
            pcm,
            spans,
            channels: channels.max(1),
            sample_rate: sample_rate.max(1),
            generation: 0,
            media_total: 0,
            pending: None,
            current_gain: gain,
        }
    }

    pub fn fill(&mut self, out: &mut [f32], playback: Nanos) {
        self.flush_pending();
        let control = self.link.load_control();
        if control.generation != self.generation {
            self.generation = control.generation;
            self.media_total = 0;
        }
        match control.phase {
            Phase::Run => self.run(out, playback, control),
            Phase::Freeze => {
                silence(out);
                self.acknowledge_when_drained(control, Adopted::Frozen);
            }
            Phase::Discard => {
                silence(out);
                self.discard_all();
                self.acknowledge_when_drained(control, Adopted::Parked);
            }
            Phase::Park => {
                silence(out);
                self.acknowledge_when_drained(control, Adopted::Parked);
            }
        }
    }

    fn run(&mut self, out: &mut [f32], playback: Nanos, control: Control) {
        let channels = usize::from(self.channels);
        let wanted = out.len();
        let mut written = 0;
        while written < wanted {
            match self.pcm.pop() {
                Ok(sample) => {
                    out[written] = sample;
                    written += 1;
                }
                Err(_) => break,
            }
        }
        if written < wanted {
            silence(&mut out[written..]);
            self.link.note_xrun();
        }
        // Media always occupies a prefix, so partial frames cannot straddle.
        let frames = (written / channels) as u32;
        self.apply_gain(out);
        if frames > 0 {
            self.media_total += u64::from(frames);
            let record = SpanRecord {
                generation: control.generation,
                media_total_after: self.media_total,
                t0: playback,
                frames,
            };
            self.publish(record);
        }
        self.link
            .acknowledge(control.generation, control.epoch, Adopted::Running);
    }

    fn publish(&mut self, record: SpanRecord) {
        if self.spans.push(record).is_err() {
            // Absolute cumulative totals make this a granularity loss only.
            if self.pending.is_some() {
                self.link.note_dropped_span();
            }
            self.pending = Some(record);
            self.link.stash_rescue(record);
        }
    }

    fn flush_pending(&mut self) {
        if let Some(record) = self.pending {
            if self.spans.push(record).is_ok() {
                self.pending = None;
                self.link.clear_rescue();
            }
        }
    }

    fn acknowledge_when_drained(&mut self, control: Control, adopted: Adopted) {
        if self.pending.is_none() {
            self.link.acknowledge(control.generation, control.epoch, adopted);
        }
    }

    fn discard_all(&mut self) {
        let available = self.pcm.slots();
        if available > 0 {
            if let Ok(chunk) = self.pcm.read_chunk(available) {
                // O(1): f32 is Copy with no Drop, so this is an index advance.
                chunk.commit_all();
            }
        }
    }

    fn apply_gain(&mut self, out: &mut [f32]) {
        let channels = usize::from(self.channels);
        let target = self.link.gain();
        let frames = out.len() / channels;
        if frames == 0 {
            return;
        }
        let step = (target - self.current_gain) / frames as f32;
        let mut gain = self.current_gain;
        for frame in out.chunks_exact_mut(channels) {
            gain += step;
            for sample in frame.iter_mut() {
                *sample *= gain;
            }
        }
        self.current_gain = target;
    }
}

fn silence(out: &mut [f32]) {
    for sample in out.iter_mut() {
        *sample = 0.0;
    }
}
```

- [ ] **Step 4: Write the virtual-clock test output**

```rust
// src/playback/output/test_output.rs
use std::time::Duration;

use super::Nanos;
use crate::playback::callback::CallbackCore;

/// Drives the production `CallbackCore` on a deterministic virtual clock, with
/// no device and no threads.
pub struct TestOutput {
    channels: u16,
    sample_rate: u32,
    buffer_frames: u32,
    latency: Duration,
    now: Nanos,
    core: Option<CallbackCore>,
    scratch: Vec<f32>,
    captured: Vec<f32>,
}

impl TestOutput {
    pub fn new(channels: u16, sample_rate: u32, buffer_frames: u32, latency: Duration) -> Self {
        let samples = buffer_frames as usize * channels as usize;
        Self {
            channels: channels.max(1),
            sample_rate: sample_rate.max(1),
            buffer_frames: buffer_frames.max(1),
            latency,
            now: Nanos(0),
            core: None,
            scratch: vec![0.0; samples],
            captured: Vec::new(),
        }
    }

    pub fn attach(&mut self, core: CallbackCore) {
        self.core = Some(core);
    }

    pub fn now(&self) -> Nanos {
        self.now
    }

    pub fn captured(&self) -> &[f32] {
        &self.captured
    }

    pub fn clear_captured(&mut self) {
        self.captured.clear();
    }

    /// Invoke the callback as many whole buffer periods as fit in `duration`.
    pub fn advance(&mut self, duration: Duration) {
        let period_nanos =
            u64::from(self.buffer_frames) * 1_000_000_000 / u64::from(self.sample_rate);
        let mut remaining = duration.as_nanos() as u64;
        while remaining >= period_nanos {
            // `playback` is a prediction ahead of the callback instant, exactly
            // as cpal reports it, so `now < t0` holds for the newest span.
            let playback = Nanos(self.now.0 + self.latency.as_nanos() as u64);
            if let Some(core) = self.core.as_mut() {
                core.fill(&mut self.scratch, playback);
                self.captured.extend_from_slice(&self.scratch);
            }
            self.now = Nanos(self.now.0 + period_nanos);
            remaining -= period_nanos;
        }
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }
}
```

- [ ] **Step 5: Declare the modules and run the tests**

Add `pub mod callback;` to `src/playback/mod.rs` and `pub mod test_output;` to `src/playback/output/mod.rs`.

Run: `cargo test --locked callback`
Expected: PASS — all seven tests.

- [ ] **Step 6: Verify and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked
git add src/playback/mod.rs src/playback/callback.rs src/playback/output/mod.rs src/playback/output/test_output.rs
git commit -m "Add allocation-free callback core and virtual-clock test output"
```

---

### Task 5: The transition handshake

**Files:**
- Create: `src/playback/handshake.rs`
- Modify: `src/playback/mod.rs`

**Interfaces:**
- Consumes: `OutputLink`, `Control`, `Phase`, `Adopted` (Task 3); `Timeline`, `SpanRecord` (Tasks 2–3); `TestOutput` (Task 4).
- Produces:
  - `pub struct Handshake` with `new(link: Arc<OutputLink>, spans: Consumer<SpanRecord>) -> Self`
  - `pub enum HandshakeError { Timeout }`
  - `pub fn freeze_and_capture(&mut self, timeline: &mut Timeline, clock: &mut dyn FnMut() -> Nanos, pump: &mut dyn FnMut(), deadline: Duration) -> Result<u64, HandshakeError>`
  - `pub fn discard(&mut self, pump: &mut dyn FnMut(), deadline: Duration) -> Result<(), HandshakeError>`
  - `pub fn install(&mut self, generation: u16, playing: bool, pump: &mut dyn FnMut(), deadline: Duration) -> Result<(), HandshakeError>`
  - `pub fn park(&mut self, pump: &mut dyn FnMut(), deadline: Duration) -> Result<(), HandshakeError>`
  - Private `await_ack(&mut self, epoch, expected, timeline: Option<&mut Timeline>, pump, deadline)` — the timeline is `Some` only for the freeze wait
  - `pub fn start_running(&mut self, generation: u16)`, `pub fn drain_spans(&mut self, &mut Timeline)`, `pub fn generation(&self) -> u16`, `pub fn next_epoch(&mut self) -> u32`

`pump` is the caller-supplied "let the output make progress" closure — in tests it advances `TestOutput`; in production it is a short `select!` on the wake channel, so every wait is interruptible by stop and shutdown.

- [ ] **Step 1: Write the failing tests**

```rust
// src/playback/handshake.rs — tests module at the bottom
#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::callback::CallbackCore;
    use crate::playback::output::test_output::TestOutput;
    use std::cell::RefCell;
    use std::rc::Rc;

    const RATE: u32 = 48_000;
    const DEADLINE: Duration = Duration::from_millis(250);

    struct Rig {
        link: Arc<OutputLink>,
        pcm: rtrb::Producer<f32>,
        output: Rc<RefCell<TestOutput>>,
        handshake: Handshake,
        timeline: Timeline,
    }

    fn rig(span_capacity: usize) -> Rig {
        let link = Arc::new(OutputLink::new());
        let (pcm_tx, pcm_rx) = rtrb::RingBuffer::<f32>::new(48_000);
        let (span_tx, span_rx) = rtrb::RingBuffer::<SpanRecord>::new(span_capacity);
        let core = CallbackCore::new(Arc::clone(&link), pcm_rx, span_tx, 2, RATE);
        let mut output = TestOutput::new(2, RATE, 480, Duration::from_millis(20));
        output.attach(core);
        Rig {
            link: Arc::clone(&link),
            pcm: pcm_tx,
            output: Rc::new(RefCell::new(output)),
            handshake: Handshake::new(link, span_rx),
            timeline: Timeline::new(RATE),
        }
    }

    fn push(rig: &mut Rig, frames: usize) {
        for _ in 0..frames * 2 {
            rig.pcm.push(0.5).unwrap();
        }
    }

    #[test]
    fn freeze_captures_the_final_position_before_anything_resets() {
        let mut rig = rig(64);
        push(&mut rig, 960);
        rig.handshake.start_running(1);
        let out = Rc::clone(&rig.output);
        out.borrow_mut().advance(Duration::from_millis(20));

        let out_clock = Rc::clone(&rig.output);
        let out_pump = Rc::clone(&rig.output);
        let played = rig
            .handshake
            .freeze_and_capture(
                &mut rig.timeline,
                &mut || out_clock.borrow().now(),
                &mut || out_pump.borrow_mut().advance(Duration::from_millis(10)),
                DEADLINE,
            )
            .unwrap();
        assert!(played > 0, "frames submitted before the freeze must be captured");
        assert_eq!(rig.link.load_ack().2, Adopted::Frozen);
    }

    #[test]
    fn freeze_completes_against_a_saturated_span_ring() {
        // Regression: the callback cannot acknowledge while a span is pending,
        // and cannot publish into a full ring. A worker that waits without
        // draining deadlocks and times out on a perfectly healthy device.
        let mut rig = rig(1);
        push(&mut rig, 4_800);
        rig.handshake.start_running(1);
        let out = Rc::clone(&rig.output);
        out.borrow_mut().advance(Duration::from_millis(50)); // saturates the 1-slot ring

        let out_clock = Rc::clone(&rig.output);
        let out_pump = Rc::clone(&rig.output);
        let result = rig.handshake.freeze_and_capture(
            &mut rig.timeline,
            &mut || out_clock.borrow().now(),
            &mut || out_pump.borrow_mut().advance(Duration::from_millis(10)),
            DEADLINE,
        );
        assert!(result.is_ok(), "worker must drain spans while awaiting the ack");
    }

    #[test]
    fn discard_waits_for_the_parked_acknowledgment_not_an_empty_ring() {
        let mut rig = rig(64);
        push(&mut rig, 4_800);
        rig.handshake.start_running(1);
        let out = Rc::clone(&rig.output);
        out.borrow_mut().advance(Duration::from_millis(10));

        let out_pump = Rc::clone(&rig.output);
        rig.handshake
            .discard(&mut || out_pump.borrow_mut().advance(Duration::from_millis(10)), DEADLINE)
            .unwrap();
        assert_eq!(rig.link.load_ack().2, Adopted::Parked);
        assert_eq!(rig.link.load_control().phase, Phase::Discard);
    }

    #[test]
    fn a_paused_install_never_passes_through_run() {
        let mut rig = rig(64);
        let out_pump = Rc::clone(&rig.output);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let seen_probe = Rc::clone(&seen);
        let link_probe = Arc::clone(&rig.link);
        rig.handshake
            .install(
                2,
                false, // desired state is paused
                &mut || {
                    seen_probe.borrow_mut().push(link_probe.load_control().phase);
                    out_pump.borrow_mut().advance(Duration::from_millis(10));
                },
                DEADLINE,
            )
            .unwrap();
        assert!(!seen.borrow().contains(&Phase::Run), "no audio may escape while paused");
        assert_eq!(rig.link.load_control().phase, Phase::Park);
    }

    #[test]
    fn a_playing_install_ends_in_run() {
        let mut rig = rig(64);
        let out_pump = Rc::clone(&rig.output);
        rig.handshake
            .install(2, true, &mut || out_pump.borrow_mut().advance(Duration::from_millis(10)), DEADLINE)
            .unwrap();
        assert_eq!(rig.link.load_control().phase, Phase::Run);
    }

    #[test]
    fn a_stale_epoch_acknowledgment_does_not_satisfy_a_wait() {
        let mut rig = rig(64);
        rig.handshake.start_running(1);
        let stale = rig.link.load_control();
        rig.link.acknowledge(stale.generation, stale.epoch, Adopted::Parked);
        // A fresh Park publication bumps the epoch, so the pre-existing ack must
        // not satisfy it. With no pump the wait can only time out.
        let result = rig.handshake.discard(&mut || {}, Duration::from_millis(20));
        assert!(matches!(result, Err(HandshakeError::Timeout)));
    }

    #[test]
    fn a_silent_output_times_out_rather_than_hanging() {
        let mut rig = rig(64);
        rig.handshake.start_running(1);
        let out_clock = Rc::clone(&rig.output);
        let result = rig.handshake.freeze_and_capture(
            &mut rig.timeline,
            &mut || out_clock.borrow().now(),
            &mut || {}, // the device never runs
            Duration::from_millis(20),
        );
        assert!(matches!(result, Err(HandshakeError::Timeout)));
    }
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --locked handshake`
Expected: FAIL — `src/playback/handshake.rs` does not exist.

- [ ] **Step 3: Write the handshake**

```rust
// src/playback/handshake.rs
use std::sync::Arc;
use std::time::{Duration, Instant};

use rtrb::Consumer;

use super::link::{Adopted, Control, OutputLink, Phase};
use super::output::{Nanos, SpanRecord};
use super::timeline::Timeline;

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("the output callback did not acknowledge within the deadline")]
    Timeout,
}

/// Worker side of the transition protocol.
///
/// Every wait carries a deadline, and a timeout means *attempt teardown and
/// recovery* — it does not bound end-to-end recovery, because the ALSA,
/// PipeWire and PulseAudio streams join their backend threads during `Drop`
/// with no timeout of their own.
pub struct Handshake {
    link: Arc<OutputLink>,
    spans: Consumer<SpanRecord>,
    epoch: u32,
    generation: u16,
}

impl Handshake {
    pub fn new(link: Arc<OutputLink>, spans: Consumer<SpanRecord>) -> Self {
        Self { link, spans, epoch: 0, generation: 0 }
    }

    pub fn next_epoch(&mut self) -> u32 {
        self.epoch = self.epoch.wrapping_add(1);
        self.epoch
    }

    pub fn generation(&self) -> u16 {
        self.generation
    }

    /// Publish `Run` for an already-installed generation (used by tests and by
    /// resuming from `Park` without a flush).
    pub fn start_running(&mut self, generation: u16) {
        self.generation = generation;
        let epoch = self.next_epoch();
        self.link
            .publish_control(Control { generation, epoch, phase: Phase::Run });
    }

    pub fn park(&mut self, pump: &mut dyn FnMut(), deadline: Duration) -> Result<(), HandshakeError> {
        let epoch = self.next_epoch();
        self.link.publish_control(Control {
            generation: self.generation,
            epoch,
            phase: Phase::Park,
        });
        self.await_ack(epoch, Adopted::Parked, None, pump, deadline)
    }

    /// Step 1 and 2: freeze submission, then capture the final played position.
    ///
    /// Drains spans **while** waiting: the callback cannot acknowledge until its
    /// pending span is published, and cannot publish into a full ring, so a
    /// non-draining wait would deadlock on a healthy device.
    pub fn freeze_and_capture(
        &mut self,
        timeline: &mut Timeline,
        clock: &mut dyn FnMut() -> Nanos,
        pump: &mut dyn FnMut(),
        deadline: Duration,
    ) -> Result<u64, HandshakeError> {
        let epoch = self.next_epoch();
        self.link.publish_control(Control {
            generation: self.generation,
            epoch,
            phase: Phase::Freeze,
        });
        // The wait must ACCEPT the spans it drains, not discard them: draining
        // is what frees the slot the callback needs, and those same records are
        // the ones the capture is computed from.
        self.await_ack(epoch, Adopted::Frozen, Some(timeline), pump, deadline)?;
        // A final drain after the acknowledgment, so no published span is missed.
        self.drain_spans(timeline);
        Ok(timeline.played_frames(clock()))
    }

    /// Step 3: the callback discards the ring in one O(1) commit and parks.
    pub fn discard(&mut self, pump: &mut dyn FnMut(), deadline: Duration) -> Result<(), HandshakeError> {
        let epoch = self.next_epoch();
        self.link.publish_control(Control {
            generation: self.generation,
            epoch,
            phase: Phase::Discard,
        });
        self.await_ack(epoch, Adopted::Parked, None, pump, deadline)
    }

    /// Steps 4 and 5: adopt the new generation parked, and release to `Run` only
    /// when the desired state is playing.
    pub fn install(
        &mut self,
        generation: u16,
        playing: bool,
        pump: &mut dyn FnMut(),
        deadline: Duration,
    ) -> Result<(), HandshakeError> {
        self.generation = generation;
        let epoch = self.next_epoch();
        self.link
            .publish_control(Control { generation, epoch, phase: Phase::Park });
        self.await_ack(epoch, Adopted::Parked, None, pump, deadline)?;
        if playing {
            let epoch = self.next_epoch();
            self.link
                .publish_control(Control { generation, epoch, phase: Phase::Run });
        }
        Ok(())
    }

    pub fn drain_spans(&mut self, timeline: &mut Timeline) {
        let diagnostics = self.link.take_diagnostics();
        timeline.note_dropped(diagnostics.spans_dropped);
        while let Ok(record) = self.spans.pop() {
            timeline.accept(record);
        }
    }

    /// `timeline` is `Some` only while freezing, where the drained records are
    /// the ones the capture is computed from. Every other transition is about to
    /// void this generation, so its records are discarded.
    fn await_ack(
        &mut self,
        epoch: u32,
        expected: Adopted,
        mut timeline: Option<&mut Timeline>,
        pump: &mut dyn FnMut(),
        deadline: Duration,
    ) -> Result<(), HandshakeError> {
        let started = Instant::now();
        loop {
            // Draining is what frees the slot the callback needs to publish its
            // pending span, which is what unblocks the acknowledgment.
            while let Ok(record) = self.spans.pop() {
                if let Some(timeline) = timeline.as_deref_mut() {
                    timeline.accept(record);
                }
            }
            let (generation, acked_epoch, adopted) = self.link.load_ack();
            if generation == self.generation && acked_epoch == epoch && adopted == expected {
                return Ok(());
            }
            if started.elapsed() >= deadline {
                return Err(HandshakeError::Timeout);
            }
            pump();
        }
    }
}
```

Note the deliberate asymmetry: `await_ack` discards drained spans because it runs during a transition whose position was already captured or is about to be reset, while `freeze_and_capture` calls `drain_spans` (which feeds the timeline) *after* the acknowledgment so the captured position is complete.

- [ ] **Step 4: Declare the module and run the tests**

Add `pub mod handshake;` to `src/playback/mod.rs`.

Run: `cargo test --locked handshake`
Expected: PASS — all seven tests.

- [ ] **Step 5: Verify and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked
git add src/playback/mod.rs src/playback/handshake.rs
git commit -m "Add acknowledged freeze/discard/install handshake with draining waits"
```

---

### Task 6: Decoding, metadata, and seek refinement

**Files:**
- Create: `src/playback/decode.rs`
- Create: `src/playback/error.rs`
- Modify: `src/playback/mod.rs`
- Create: `tests/decode_fixtures.rs`

**Interfaces:**
- Consumes: M0's `AbsolutePath`, `MediaMetadata`, `MediaCapabilities`, `Continuity`, `SeekSupport`; fixtures from Task 1.
- Produces:
  - `pub enum PlaybackError { Open, UnsupportedInput, Decode, SeekFailed, Timeout, Cancelled, Output }`
  - `pub struct DecodedSource` with `open(&AbsolutePath) -> Result<Self, PlaybackError>`, `metadata(&self) -> &MediaMetadata`, `capabilities(&self) -> MediaCapabilities`, `sample_rate(&self) -> u32`, `channels(&self) -> u16`, `next_planar(&mut self) -> Result<Option<&[Vec<f32>]>, PlaybackError>`, `seek_refined(&mut self, target: Duration, budget: Option<Duration>, cancelled: &mut dyn FnMut() -> bool) -> Result<SeekOutcome, PlaybackError>`
  - `pub struct SeekOutcome { pub actual: Duration, pub refinement_truncated: bool }`

- [ ] **Step 1: Write the failing tests**

```rust
// tests/decode_fixtures.rs
use std::path::Path;
use std::time::Duration;

use continuo::media::capabilities::{Continuity, SeekSupport};
use continuo::media::id::AbsolutePath;
use continuo::playback::decode::DecodedSource;

fn fixture(name: &str) -> AbsolutePath {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    AbsolutePath::new(path.canonicalize().unwrap()).unwrap()
}

fn open(name: &str) -> DecodedSource {
    DecodedSource::open(&fixture(name)).unwrap()
}

#[test]
fn every_promised_format_decodes_to_planar_f32() {
    for name in ["sine.wav", "sine.flac", "sine.mp3"] {
        let mut source = open(name);
        assert_eq!(source.sample_rate(), 44_100, "{name}");
        assert_eq!(source.channels(), 2, "{name}");
        let mut frames = 0usize;
        while let Some(planes) = source.next_planar().unwrap() {
            assert_eq!(planes.len(), 2, "{name}");
            frames += planes[0].len();
        }
        // 0.5 s at 44100 Hz, allowing for codec delay and padding.
        assert!((20_000..30_000).contains(&frames), "{name} decoded {frames} frames");
    }
}

#[test]
fn metadata_and_capabilities_come_from_the_file() {
    let source = open("sine.flac");
    let duration = source.metadata().duration.unwrap();
    assert!(duration >= Duration::from_millis(450) && duration <= Duration::from_millis(550));
    let capabilities = source.capabilities();
    assert_eq!(capabilities.continuity, Continuity::Finite);
    assert_eq!(capabilities.seek, SeekSupport::Native);
}

#[test]
fn refined_seek_lands_at_the_requested_frame_not_the_packet_boundary() {
    // Symphonia's accurate seek lands at or before the target, so the decoder
    // must decode and discard forward to the exact frame.
    let mut source = open("sine.flac");
    let outcome = source
        .seek_refined(Duration::from_millis(250), None, &mut || false)
        .unwrap();
    assert!(!outcome.refinement_truncated);
    let delta = outcome.actual.as_millis().abs_diff(250);
    assert!(delta <= 2, "landed at {:?}, wanted 250 ms", outcome.actual);
}

#[test]
fn an_exhausted_refinement_budget_reports_where_it_actually_arrived() {
    let mut source = open("sine.flac");
    let outcome = source
        .seek_refined(Duration::from_millis(400), Some(Duration::ZERO), &mut || false)
        .unwrap();
    assert!(outcome.refinement_truncated);
    assert!(outcome.actual <= Duration::from_millis(400));
}

#[test]
fn cancellation_is_observed_between_decode_steps() {
    let mut source = open("sine.flac");
    let mut calls = 0;
    let result = source.seek_refined(Duration::from_millis(400), None, &mut || {
        calls += 1;
        true
    });
    assert!(matches!(result, Err(continuo::playback::error::PlaybackError::Cancelled)));
}

#[test]
fn a_missing_file_reports_a_contextual_error() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/absent.wav");
    let error = DecodedSource::open(&AbsolutePath::new(path).unwrap()).unwrap_err();
    assert!(error.to_string().contains("absent.wav"), "got: {error}");
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --locked --test decode_fixtures`
Expected: FAIL — `continuo::playback::decode` does not exist.

- [ ] **Step 3: Write the error type**

```rust
// src/playback/error.rs
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum PlaybackError {
    #[error("cannot open media {path:?}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot play {path:?}: {reason}")]
    UnsupportedInput { path: PathBuf, reason: String },
    #[error("cannot decode media")]
    Decode(#[source] symphonia::core::errors::Error),
    #[error("cannot seek to {target:?}")]
    SeekFailed {
        target: std::time::Duration,
        #[source]
        source: symphonia::core::errors::Error,
    },
    #[error("the audio output did not respond within the deadline")]
    Timeout,
    #[error("the operation was cancelled")]
    Cancelled,
    #[error("audio output failure")]
    Output(#[source] cpal::Error),
}
```

- [ ] **Step 4: Write the decoder**

```rust
// src/playback/decode.rs
use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::TimeBase;

use crate::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use crate::media::id::AbsolutePath;
use crate::media::metadata::MediaMetadata;

use super::error::PlaybackError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeekOutcome {
    pub actual: Duration,
    pub refinement_truncated: bool,
}

pub struct DecodedSource {
    path: PathBuf,
    reader: Box<dyn FormatReader + 'static>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    sample_rate: u32,
    channels: u16,
    metadata: MediaMetadata,
    planes: Vec<Vec<f32>>,
    /// Media timestamp of the next frame `next_planar` will return.
    cursor: u64,
}

impl DecodedSource {
    pub fn open(path: &AbsolutePath) -> Result<Self, PlaybackError> {
        let owned = path.as_path().to_path_buf();
        let metadata_fs = std::fs::metadata(&owned)
            .map_err(|source| PlaybackError::Open { path: owned.clone(), source })?;
        // Reject pipes and device files so this local-file slice cannot acquire
        // an unbounded read.
        if !metadata_fs.is_file() {
            return Err(PlaybackError::UnsupportedInput {
                path: owned,
                reason: "not a regular file".into(),
            });
        }
        let file = File::open(&owned)
            .map_err(|source| PlaybackError::Open { path: owned.clone(), source })?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(extension) = owned.extension().and_then(|e| e.to_str()) {
            hint.with_extension(extension);
        }
        let reader = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(PlaybackError::Decode)?;

        let track = reader.default_track(TrackType::Audio).ok_or_else(|| {
            PlaybackError::UnsupportedInput { path: owned.clone(), reason: "no audio track".into() }
        })?;
        let track_id = track.id;
        let time_base = track.time_base;
        let duration = track
            .num_frames
            .zip(track.time_base)
            .and_then(|(frames, base)| base.calc_time(frames).into());
        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .ok_or_else(|| PlaybackError::UnsupportedInput {
                path: owned.clone(),
                reason: "no audio codec parameters".into(),
            })?;
        let sample_rate = params.sample_rate.ok_or_else(|| PlaybackError::UnsupportedInput {
            path: owned.clone(),
            reason: "unknown sample rate".into(),
        })?;
        let channels = params
            .channels
            .as_ref()
            .map(|channels| channels.count() as u16)
            .unwrap_or(0);
        if channels == 0 || channels > 2 {
            return Err(PlaybackError::UnsupportedInput {
                path: owned,
                reason: format!("{channels} channels; M1 supports mono and stereo"),
            });
        }
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(PlaybackError::Decode)?;
        let title = reader
            .metadata()
            .current()
            .and_then(|revision| {
                revision
                    .tags()
                    .iter()
                    .find(|tag| tag.std.is_some_and(|std| format!("{std:?}") == "TrackTitle"))
                    .map(|tag| tag.value.to_string())
            });

        Ok(Self {
            path: owned,
            reader,
            decoder,
            track_id,
            time_base,
            sample_rate,
            channels,
            metadata: MediaMetadata { title, duration },
            planes: vec![Vec::new(); usize::from(channels)],
            cursor: 0,
        })
    }

    pub fn metadata(&self) -> &MediaMetadata {
        &self.metadata
    }

    /// Local files are finite and, for every format M1 ships, natively seekable.
    pub fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            continuity: if self.metadata.duration.is_some() {
                Continuity::Finite
            } else {
                Continuity::Unresolved
            },
            seek: SeekSupport::Native,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn position(&self) -> Duration {
        self.frames_to_duration(self.cursor)
    }

    /// Decode the next packet into planar `f32`. `Ok(None)` means end of media.
    pub fn next_planar(&mut self) -> Result<Option<&[Vec<f32>]>, PlaybackError> {
        loop {
            let packet = match self.reader.next_packet().map_err(PlaybackError::Decode)? {
                Some(packet) => packet,
                None => return Ok(None),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = self.decoder.decode(&packet).map_err(PlaybackError::Decode)?;
            let frames = decoded.frames() as u64;
            if frames == 0 {
                continue;
            }
            copy_planar(&decoded, &mut self.planes);
            self.cursor += frames;
            return Ok(Some(&self.planes));
        }
    }

    /// Seek, then decode-and-discard forward to the exact frame.
    ///
    /// Symphonia's accurate seek lands at or before the target because the
    /// reader seeks to a packet boundary, so refinement is required for an exact
    /// landing. `budget` bounds refinement for an *explicit* seek; pass `None`
    /// for stop-resume and device recovery, which promise preservation.
    pub fn seek_refined(
        &mut self,
        target: Duration,
        budget: Option<Duration>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<SeekOutcome, PlaybackError> {
        let seeked = self
            .reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Time { time: duration_to_time(target), track_id: Some(self.track_id) },
            )
            .map_err(|source| PlaybackError::SeekFailed { target, source })?;
        self.decoder.reset();
        self.cursor = seeked.actual_ts.into();

        let target_frames = self.duration_to_frames(target);
        let budget_frames = budget.map(|b| self.duration_to_frames(b));
        let start = self.cursor;
        let mut truncated = false;
        while self.cursor < target_frames {
            if cancelled() {
                return Err(PlaybackError::Cancelled);
            }
            if let Some(limit) = budget_frames {
                if self.cursor.saturating_sub(start) >= limit {
                    truncated = true;
                    break;
                }
            }
            match self.next_planar()? {
                Some(_) => {}
                None => break,
            }
        }
        Ok(SeekOutcome { actual: self.position(), refinement_truncated: truncated })
    }

    fn duration_to_frames(&self, value: Duration) -> u64 {
        (value.as_secs_f64() * f64::from(self.sample_rate)) as u64
    }

    fn frames_to_duration(&self, frames: u64) -> Duration {
        Duration::from_secs_f64(frames as f64 / f64::from(self.sample_rate))
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn time_base(&self) -> Option<TimeBase> {
        self.time_base
    }
}

fn copy_planar(decoded: &GenericAudioBufferRef<'_>, planes: &mut Vec<Vec<f32>>) {
    decoded.copy_to_vecs_planar::<f32>(planes);
}

fn duration_to_time(value: Duration) -> symphonia::core::units::Time {
    symphonia::core::units::Time::new(value.as_secs(), f64::from(value.subsec_nanos()) / 1e9)
}
```

The `title` extraction and `duration` derivation are the two places most likely to need adjustment against symphonia 0.6's exact metadata and `TimeBase` API. If a signature mismatch appears, fix it by reading `symphonia-core-0.6.1/src/meta.rs` and `src/units.rs` — do not delete the field.

- [ ] **Step 5: Declare the modules and run the tests**

Add `pub mod decode;` and `pub mod error;` to `src/playback/mod.rs`.

Run: `cargo test --locked --test decode_fixtures`
Expected: PASS — all six tests.

- [ ] **Step 6: Verify and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked
git add src/playback/mod.rs src/playback/decode.rs src/playback/error.rs tests/decode_fixtures.rs
git commit -m "Add symphonia decoding, metadata, and refined accurate seek"
```

---

### Task 7: Resampling with delay trim and EOF flush

**Files:**
- Create: `src/playback/resample.rs`
- Modify: `src/playback/mod.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks (operates on planar `f32`).
- Produces:
  - `pub struct Converter` with `new(source_rate: u32, target_rate: u32, source_channels: u16, target_channels: u16) -> Result<Self, PlaybackError>`, `push(&mut self, planes: &[Vec<f32>], out: &mut Vec<f32>)`, `finish(&mut self, out: &mut Vec<f32>)`, `reset(&mut self)`, `expected_output_frames(&self, input_frames: u64) -> u64`

- [ ] **Step 1: Write the failing tests**

```rust
// src/playback/resample.rs — tests module at the bottom
#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frames: usize, channels: usize) -> Vec<Vec<f32>> {
        (0..channels)
            .map(|_| (0..frames).map(|i| (i as f32 * 0.05).sin()).collect())
            .collect()
    }

    #[test]
    fn a_matching_rate_passes_through_interleaved_without_a_resampler() {
        let mut converter = Converter::new(48_000, 48_000, 2, 2).unwrap();
        let mut out = Vec::new();
        converter.push(&sine(480, 2), &mut out);
        assert_eq!(out.len(), 960);
    }

    #[test]
    fn mono_sources_are_duplicated_into_a_stereo_device() {
        let mut converter = Converter::new(48_000, 48_000, 1, 2).unwrap();
        let mut out = Vec::new();
        converter.push(&sine(100, 1), &mut out);
        assert_eq!(out.len(), 200);
        for frame in out.chunks_exact(2) {
            assert_eq!(frame[0], frame[1]);
        }
    }

    #[test]
    fn stereo_sources_are_averaged_into_a_mono_device() {
        let mut converter = Converter::new(48_000, 48_000, 2, 1).unwrap();
        let mut out = Vec::new();
        converter.push(&vec![vec![1.0; 10], vec![-1.0; 10]], &mut out);
        assert_eq!(out.len(), 10);
        assert!(out.iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn startup_delay_is_trimmed_exactly_once_per_reset() {
        // Trimming on every logical anchor update would repeatedly swallow audio.
        let mut converter = Converter::new(44_100, 48_000, 2, 2).unwrap();
        let mut first = Vec::new();
        for _ in 0..20 {
            converter.push(&sine(1024, 2), &mut first);
        }
        let mut second = Vec::new();
        for _ in 0..20 {
            converter.push(&sine(1024, 2), &mut second);
        }
        // The second run trims nothing, so it yields at least as much output.
        assert!(second.len() >= first.len());
    }

    #[test]
    fn eof_flush_recovers_the_delayed_tail_without_synthetic_padding() {
        // Startup trimming must not shorten playback: the flush returns the
        // delayed valid output, and padding is trimmed to the expected extent.
        let mut converter = Converter::new(44_100, 48_000, 2, 2).unwrap();
        let mut out = Vec::new();
        let input_frames = 44_100u64;
        for _ in 0..43 {
            converter.push(&sine(1024, 2), &mut out);
        }
        converter.push(&sine(44_100 - 43 * 1024, 2), &mut out);
        let before_flush = out.len();
        converter.finish(&mut out);
        assert!(out.len() > before_flush, "the flush must recover delayed output");
        let expected = converter.expected_output_frames(input_frames) as usize * 2;
        let delta = out.len().abs_diff(expected);
        assert!(delta <= 2 * 2, "got {} samples, expected about {expected}", out.len());
    }

    #[test]
    fn reset_discards_resampler_state_for_a_seek() {
        let mut converter = Converter::new(44_100, 48_000, 2, 2).unwrap();
        let mut out = Vec::new();
        converter.push(&sine(1024, 2), &mut out);
        converter.reset();
        let mut after = Vec::new();
        converter.push(&sine(1024, 2), &mut after);
        assert!(!after.is_empty());
    }
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --locked resample`
Expected: FAIL — `src/playback/resample.rs` does not exist.

- [ ] **Step 3: Write the converter**

```rust
// src/playback/resample.rs
use rubato::{Indexing, Resampler, SincFixedIn, SincInterpolationParameters,
             SincInterpolationType, WindowFunction};

use super::error::PlaybackError;

const CHUNK: usize = 1024;

/// Channel mapping plus optional band-limited rate conversion.
///
/// Resampler startup delay is trimmed **once per creation or reset** — not on
/// every anchor update — and the EOF flush recovers the delayed tail so that
/// startup trimming does not shorten playback.
pub struct Converter {
    source_channels: u16,
    target_channels: u16,
    ratio: f64,
    resampler: Option<SincFixedIn<f32>>,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    /// Output frames still to be discarded from the resampler's priming delay.
    trim_remaining: usize,
    input_frames_seen: u64,
    output_frames_emitted: u64,
}

impl Converter {
    pub fn new(
        source_rate: u32,
        target_rate: u32,
        source_channels: u16,
        target_channels: u16,
    ) -> Result<Self, PlaybackError> {
        let ratio = f64::from(target_rate) / f64::from(source_rate);
        let channels = usize::from(source_channels.max(1));
        let resampler = if source_rate == target_rate {
            None
        } else {
            let params = SincInterpolationParameters {
                sinc_len: 256,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 256,
                window: WindowFunction::BlackmanHarris2,
            };
            Some(
                SincFixedIn::<f32>::new(ratio, 2.0, params, CHUNK, channels)
                    .map_err(|_| PlaybackError::UnsupportedInput {
                        path: Default::default(),
                        reason: format!("cannot resample {source_rate} Hz to {target_rate} Hz"),
                    })?,
            )
        };
        let trim_remaining = resampler.as_ref().map_or(0, |r| r.output_delay());
        let output_capacity = resampler.as_ref().map_or(CHUNK, |r| r.output_frames_max());
        Ok(Self {
            source_channels: source_channels.max(1),
            target_channels: target_channels.max(1),
            ratio,
            resampler,
            input: vec![Vec::new(); channels],
            output: vec![vec![0.0; output_capacity]; channels],
            trim_remaining,
            input_frames_seen: 0,
            output_frames_emitted: 0,
        })
    }

    /// Discard resampler state at a discontinuity, and re-arm the startup trim.
    pub fn reset(&mut self) {
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.reset();
            self.trim_remaining = resampler.output_delay();
        }
        for plane in &mut self.input {
            plane.clear();
        }
        self.input_frames_seen = 0;
        self.output_frames_emitted = 0;
    }

    pub fn expected_output_frames(&self, input_frames: u64) -> u64 {
        (input_frames as f64 * self.ratio).round() as u64
    }

    pub fn push(&mut self, planes: &[Vec<f32>], out: &mut Vec<f32>) {
        let frames = planes.first().map_or(0, Vec::len);
        self.input_frames_seen += frames as u64;
        if self.resampler.is_none() {
            self.interleave_planes(planes, frames, out);
            self.output_frames_emitted += frames as u64;
            return;
        }
        for (plane, buffered) in planes.iter().zip(self.input.iter_mut()) {
            buffered.extend_from_slice(plane);
        }
        while self.buffered_frames() >= CHUNK {
            self.process_chunk(CHUNK, None, out);
            for plane in &mut self.input {
                plane.drain(..CHUNK);
            }
        }
    }

    /// Feed the final partial chunk with `partial_len` so the resampler emits
    /// its delayed valid output, then trim back to the expected extent.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        if self.resampler.is_none() {
            return;
        }
        let remaining = self.buffered_frames();
        if remaining > 0 {
            for plane in &mut self.input {
                plane.resize(CHUNK, 0.0);
            }
            self.process_chunk(CHUNK, Some(remaining), out);
            for plane in &mut self.input {
                plane.clear();
            }
        }
        // One more silent chunk pushes out whatever delay still holds valid audio.
        for plane in &mut self.input {
            plane.resize(CHUNK, 0.0);
        }
        self.process_chunk(CHUNK, Some(0), out);
        for plane in &mut self.input {
            plane.clear();
        }
        let expected = self.expected_output_frames(self.input_frames_seen) as usize;
        let wanted = expected * usize::from(self.target_channels);
        if out.len() > wanted {
            out.truncate(wanted);
        }
    }

    fn buffered_frames(&self) -> usize {
        self.input.first().map_or(0, Vec::len)
    }

    fn process_chunk(&mut self, len: usize, partial: Option<usize>, out: &mut Vec<f32>) {
        let Some(resampler) = self.resampler.as_mut() else { return };
        let mut indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: partial,
            active_channels_mask: None,
        };
        indexing.partial_len = partial;
        let input: Vec<&[f32]> = self.input.iter().map(|plane| &plane[..len]).collect();
        let Ok((_, produced)) =
            resampler.process_into_buffer(&input, &mut self.output, Some(&indexing))
        else {
            return;
        };
        let start = self.trim_remaining.min(produced);
        self.trim_remaining -= start;
        let usable = produced - start;
        if usable == 0 {
            return;
        }
        let planes: Vec<Vec<f32>> = self
            .output
            .iter()
            .map(|plane| plane[start..start + usable].to_vec())
            .collect();
        self.interleave_planes(&planes, usable, out);
        self.output_frames_emitted += usable as u64;
    }

    fn interleave_planes(&self, planes: &[Vec<f32>], frames: usize, out: &mut Vec<f32>) {
        match (self.source_channels, self.target_channels) {
            (1, 2) => {
                for i in 0..frames {
                    let sample = planes[0][i];
                    out.push(sample);
                    out.push(sample);
                }
            }
            (2, 1) => {
                for i in 0..frames {
                    out.push((planes[0][i] + planes[1][i]) * 0.5);
                }
            }
            _ => {
                let channels = usize::from(self.target_channels).min(planes.len());
                for i in 0..frames {
                    for plane in planes.iter().take(channels) {
                        out.push(plane[i]);
                    }
                }
            }
        }
    }
}
```

`SincFixedIn::new`'s exact parameter struct is the most likely mismatch against rubato 5.0.0. If it fails to compile, read `rubato-5.0.0/src/sinc_resampler.rs` for the current constructor and keep the surrounding trim/flush logic unchanged.

- [ ] **Step 4: Declare the module and run the tests**

Add `pub mod resample;` to `src/playback/mod.rs`.

Run: `cargo test --locked resample`
Expected: PASS — all six tests.

- [ ] **Step 5: Verify and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked
git add src/playback/mod.rs src/playback/resample.rs
git commit -m "Add channel mapping and resampling with delay trim and EOF flush"
```

---

### Task 8: The CPAL output

**Files:**
- Create: `src/playback/output/cpal_output.rs`
- Modify: `src/playback/output/mod.rs`
- Create: `tests/device_smoke.rs`

**Interfaces:**
- Consumes: `CallbackCore` (Task 4), `OutputLink` (Task 3), `Nanos` (Task 2).
- Produces:
  - `pub struct OutputRequest { pub preferred_rate: u32, pub preferred_channels: u16 }`
  - `pub struct NegotiatedOutput { pub sample_rate: u32, pub channels: u16, pub buffer_frames: u32 }`
  - `pub enum OutputFault { Recoverable(cpal::ErrorKind), Rebuild(cpal::ErrorKind), Fatal(cpal::ErrorKind) }` with `pub fn classify(kind: cpal::ErrorKind) -> OutputFault`
  - `pub trait AudioOutput` with `negotiate`, `open`, `now`, `close`
  - `pub struct CpalOutput`

Negotiation is two-phase because the PCM ring's capacity depends on the negotiated rate: `negotiate` reports the device's actual configuration, the worker sizes the ring, then `open` receives the ring's consumer.

- [ ] **Step 1: Write the failing tests**

```rust
// src/playback/output/cpal_output.rs — tests module at the bottom
#[cfg(test)]
mod tests {
    use super::*;
    use cpal::ErrorKind;

    #[test]
    fn a_rerouted_device_is_recoverable_without_a_rebuild() {
        // cpal documents DeviceChanged as "automatically rerouted; the stream
        // remains active and no rebuild is required".
        assert!(matches!(
            OutputFault::classify(ErrorKind::DeviceChanged),
            OutputFault::Recoverable(_)
        ));
    }

    #[test]
    fn an_xrun_is_recoverable_and_never_triggers_a_rebuild() {
        assert!(matches!(OutputFault::classify(ErrorKind::Xrun), OutputFault::Recoverable(_)));
    }

    #[test]
    fn refused_realtime_scheduling_does_not_stop_playback() {
        assert!(matches!(
            OutputFault::classify(ErrorKind::RealtimeDenied),
            OutputFault::Recoverable(_)
        ));
    }

    #[test]
    fn a_lost_device_requires_rebuilding_at_the_preserved_position() {
        for kind in [ErrorKind::DeviceNotAvailable, ErrorKind::StreamInvalidated] {
            assert!(matches!(OutputFault::classify(kind), OutputFault::Rebuild(_)), "{kind:?}");
        }
    }

    #[test]
    fn authorization_and_host_failures_are_fatal() {
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::HostUnavailable,
            ErrorKind::UnsupportedConfig,
        ] {
            assert!(matches!(OutputFault::classify(kind), OutputFault::Fatal(kind2) if kind2 == kind));
        }
    }

    #[test]
    fn unclassifiable_backend_errors_fall_back_to_one_rebuild_attempt() {
        for kind in [
            ErrorKind::ResourceExhausted,
            ErrorKind::UnsupportedOperation,
            ErrorKind::InvalidInput,
            ErrorKind::BackendError,
        ] {
            assert!(matches!(OutputFault::classify(kind), OutputFault::Rebuild(_)), "{kind:?}");
        }
    }
}
```

```rust
// tests/device_smoke.rs
//! Real-device tests. Never run in CI; run locally with
//! `cargo test --locked --test device_smoke -- --ignored --nocapture`.

use std::time::Duration;

use continuo::playback::output::cpal_output::CpalOutput;
use continuo::playback::output::{AudioOutput, OutputRequest};

#[test]
#[ignore = "requires a real audio device"]
fn a_real_device_negotiates_a_playable_configuration() {
    let mut output = CpalOutput::default();
    let negotiated = output
        .negotiate(&OutputRequest { preferred_rate: 44_100, preferred_channels: 2 })
        .unwrap();
    assert!(negotiated.sample_rate >= 8_000);
    assert!(negotiated.channels >= 1);
    std::thread::sleep(Duration::from_millis(10));
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --locked cpal_output`
Expected: FAIL — `src/playback/output/cpal_output.rs` does not exist.

- [ ] **Step 3: Write the fault classification and the trait**

```rust
// src/playback/output/mod.rs — append
use std::sync::Arc;

use crate::playback::callback::CallbackCore;
use crate::playback::error::PlaybackError;
use crate::playback::link::OutputLink;

// `pub mod test_output;` was added by Task 4 - do not re-declare it here.
pub mod cpal_output;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputRequest {
    pub preferred_rate: u32,
    pub preferred_channels: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiatedOutput {
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_frames: u32,
}

/// The internal device seam. Narrow by design: exactly the operations both
/// `CpalOutput` and `TestOutput` exercise, and nothing speculative.
pub trait AudioOutput: Send {
    /// Report the device's actual configuration, before any ring is sized.
    fn negotiate(&mut self, request: &OutputRequest) -> Result<NegotiatedOutput, PlaybackError>;
    /// Build the stream **parked**, so no audio flows until the worker releases it.
    fn open(
        &mut self,
        config: &NegotiatedOutput,
        link: Arc<OutputLink>,
        core: CallbackCore,
    ) -> Result<(), PlaybackError>;
    fn now(&self) -> Nanos;
    fn close(&mut self);
}
```

```rust
// src/playback/output/cpal_output.rs
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender};

use super::{AudioOutput, NegotiatedOutput, Nanos, OutputRequest};
use crate::playback::callback::CallbackCore;
use crate::playback::error::PlaybackError;
use crate::playback::link::OutputLink;

/// How a device error must be handled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFault {
    /// Playback continues; report and carry on.
    Recoverable(cpal::ErrorKind),
    /// The transport must be rebuilt at the preserved position.
    Rebuild(cpal::ErrorKind),
    /// Unrecoverable; enter `Failed`.
    Fatal(cpal::ErrorKind),
}

impl OutputFault {
    pub fn classify(kind: cpal::ErrorKind) -> Self {
        use cpal::ErrorKind::*;
        match kind {
            // Automatically rerouted; the stream stays active. The timing base
            // may jump, so the caller marks position degraded.
            DeviceChanged | Xrun | RealtimeDenied => Self::Recoverable(kind),
            DeviceNotAvailable | StreamInvalidated | DeviceBusy => Self::Rebuild(kind),
            PermissionDenied | HostUnavailable | UnsupportedConfig => Self::Fatal(kind),
            // Fallback: one rebuild attempt, then the caller gives up.
            _ => Self::Rebuild(kind),
        }
    }

    pub fn kind(self) -> cpal::ErrorKind {
        match self {
            Self::Recoverable(kind) | Self::Rebuild(kind) | Self::Fatal(kind) => kind,
        }
    }
}

pub struct CpalOutput {
    device: Option<cpal::Device>,
    stream: Option<cpal::Stream>,
    faults_tx: Sender<OutputFault>,
    faults_rx: Receiver<OutputFault>,
    wake: Option<Sender<()>>,
}

impl Default for CpalOutput {
    fn default() -> Self {
        let (faults_tx, faults_rx) = crossbeam_channel::bounded(16);
        Self { device: None, stream: None, faults_tx, faults_rx, wake: None }
    }
}

impl CpalOutput {
    /// Asynchronous device errors arrive here; `open()`'s Result covers only
    /// synchronous failures.
    pub fn faults(&self) -> Receiver<OutputFault> {
        self.faults_rx.clone()
    }

    /// The wake channel from the engine, so a fault interrupts a blocked wait.
    pub fn set_wake(&mut self, wake: Sender<()>) {
        self.wake = Some(wake);
    }
}

impl AudioOutput for CpalOutput {
    fn negotiate(&mut self, request: &OutputRequest) -> Result<NegotiatedOutput, PlaybackError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| PlaybackError::UnsupportedInput {
                path: Default::default(),
                reason: "no default audio output device".into(),
            })?;
        let config = device.default_output_config().map_err(PlaybackError::Output)?;
        let negotiated = NegotiatedOutput {
            sample_rate: config.sample_rate().0,
            channels: config.channels(),
            buffer_frames: 1024,
        };
        let _ = request;
        self.device = Some(device);
        Ok(negotiated)
    }

    fn open(
        &mut self,
        config: &NegotiatedOutput,
        link: Arc<OutputLink>,
        mut core: CallbackCore,
    ) -> Result<(), PlaybackError> {
        let device = self.device.as_ref().ok_or(PlaybackError::Timeout)?;
        let stream_config = cpal::StreamConfig {
            channels: config.channels,
            sample_rate: cpal::SampleRate(config.sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };
        let faults = self.faults_tx.clone();
        let wake = self.wake.clone();
        let _ = link;
        let stream = device
            .build_output_stream(
                &stream_config,
                move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    let playback = Nanos::from_stream_nanos(info.timestamp().playback.as_nanos());
                    core.fill(out, playback);
                },
                move |error: cpal::Error| {
                    // Never blocks: a full fault queue means the worker already
                    // has one to act on.
                    let _ = faults.try_send(OutputFault::classify(error.kind()));
                    if let Some(wake) = wake.as_ref() {
                        let _ = wake.try_send(());
                    }
                },
                None,
            )
            .map_err(PlaybackError::Output)?;
        // The stream runs continuously; pause is the Park phase, so the callback
        // stays live and every handshake deadline signals real device trouble.
        stream.play().map_err(PlaybackError::Output)?;
        self.stream = Some(stream);
        Ok(())
    }

    fn now(&self) -> Nanos {
        match self.stream.as_ref() {
            Some(stream) => Nanos::from_stream_nanos(stream.now().as_nanos()),
            None => Nanos(0),
        }
    }

    fn close(&mut self) {
        // Dropping joins the backend thread, which is what makes the link's
        // rescue slot safe to read afterwards. The join has no timeout.
        self.stream = None;
    }
}

impl CpalOutput {
    pub fn drain_faults(&self, timeout: Duration) -> Option<OutputFault> {
        self.faults_rx.recv_timeout(timeout).ok()
    }
}
```

Task 4 created `TestOutput` before this trait existed, so implement the trait for it here — both implementations belong beside the trait, and Task 9's `TestEngine` needs it:

```rust
// src/playback/output/test_output.rs — append
impl super::AudioOutput for TestOutput {
    fn negotiate(&mut self, _request: &super::OutputRequest)
        -> Result<super::NegotiatedOutput, crate::playback::error::PlaybackError> {
        Ok(super::NegotiatedOutput {
            sample_rate: self.sample_rate,
            channels: self.channels,
            buffer_frames: self.buffer_frames,
        })
    }

    fn open(
        &mut self,
        _config: &super::NegotiatedOutput,
        _link: std::sync::Arc<crate::playback::link::OutputLink>,
        core: crate::playback::callback::CallbackCore,
    ) -> Result<(), crate::playback::error::PlaybackError> {
        self.attach(core);
        Ok(())
    }

    fn now(&self) -> super::Nanos {
        self.now
    }

    fn close(&mut self) {
        self.core = None;
    }
}
```

`TestOutput` must be `Send` for `Box<dyn AudioOutput>`; it already is, since `CallbackCore` owns only `Send` ring endpoints and an `Arc<OutputLink>`.

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --locked cpal_output`
Expected: PASS — all six classification tests.

Run: `cargo test --locked --test device_smoke`
Expected: PASS with the device test reported as ignored (`0 passed; 0 failed; 1 ignored`).

- [ ] **Step 5: Confirm the real device works, locally only**

Run: `cargo test --locked --test device_smoke -- --ignored --nocapture`
Expected: PASS on a machine with audio hardware. If it fails in a headless environment, that is expected — record the result and move on; CI never runs it.

- [ ] **Step 6: Verify and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked
git add src/playback/output tests/device_smoke.rs
git commit -m "Add CPAL output with two-phase negotiation and fault classification"
```

---

### Task 9: The engine

**Files:**
- Create: `src/playback/command.rs`, `src/playback/event.rs`, `src/playback/state.rs`, `src/playback/volume.rs`, `src/playback/engine.rs`
- Modify: `src/playback/mod.rs`
- Create: `tests/engine_contract.rs`

**Interfaces:**
- Consumes: everything from Tasks 2–8.
- Produces:
  - `pub enum PlaybackCommand { Load{..}, Play, Pause, TogglePause, SeekTo(Duration), SeekBy(i64), Restart, SetVolume(Volume), Stop, Shutdown }`
  - `pub enum PlaybackEvent { Loaded{..}, StateChanged{..}, SeekCompleted{..}, SeekTargetStored{..}, SeekRejected{..}, VolumeChanged{..}, EndOfTrack{..}, DeviceRecovered{..}, Warning{..}, Failed{..} }`, each carrying `session_rev: u64`
  - `pub enum PlaybackState { Idle, Loading, Playing, Paused, Stopped, Ended, Failed }`
  - `pub struct Progress { pub session_rev: u64, pub media: Option<MediaId>, pub position: Duration, pub quality: PositionQuality }`
  - `pub struct EngineHandle` with `spawn(output: Box<dyn AudioOutput>) -> Self`, `commands() -> &Sender<PlaybackCommand>`, `events() -> &Receiver<PlaybackEvent>`, `progress() -> Progress`, `interrupt_stop()`, `interrupt_shutdown()`, `join(self)`

- [ ] **Step 1: Write the failing tests**

```rust
// tests/engine_contract.rs
//! Contract tests worded directly from the M0 invariant:
//! "Stop and transport recreation preserve position; restoration, media
//! selection, explicit restart, and successful seeks establish a new position."

use std::time::Duration;

use continuo::playback::command::PlaybackCommand;
use continuo::playback::event::PlaybackEvent;
use continuo::playback::state::PlaybackState;

mod support;
use support::TestEngine;

#[test]
fn stop_preserves_the_logical_position() {
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(200));
    let before = engine.position();
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    assert_eq!(engine.position(), before, "stop must not reset position");
}

#[test]
fn play_from_stopped_resumes_at_the_preserved_position_without_resetting() {
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(200));
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    let preserved = engine.position();
    engine.send(PlaybackCommand::Play);
    engine.await_state(PlaybackState::Playing);
    assert!(engine.position() >= preserved, "resume must not rewind to zero");
}

#[test]
fn transport_recreation_preserves_position() {
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(200));
    let before = engine.position();
    engine.force_device_loss();
    engine.await_event(|e| matches!(e, PlaybackEvent::DeviceRecovered { .. }));
    assert!(engine.position() >= before, "recreation must not reset position");
}

#[test]
fn a_successful_seek_establishes_a_new_position() {
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(100));
    engine.send(PlaybackCommand::SeekTo(Duration::from_millis(300)));
    let event = engine.await_event(|e| matches!(e, PlaybackEvent::SeekCompleted { .. }));
    let PlaybackEvent::SeekCompleted { actual, .. } = event else { unreachable!() };
    assert!(actual.as_millis().abs_diff(300) <= 5);
}

#[test]
fn a_seek_while_stopped_stores_a_target_and_does_not_claim_completion() {
    // Regression: emitting SeekCompleted only after Play made Restart stall.
    let mut engine = TestEngine::start("sine.flac");
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    engine.send(PlaybackCommand::SeekTo(Duration::from_millis(300)));
    let event = engine.await_event(|e| {
        matches!(e, PlaybackEvent::SeekTargetStored { .. } | PlaybackEvent::SeekCompleted { .. })
    });
    assert!(matches!(event, PlaybackEvent::SeekTargetStored { .. }));
}

#[test]
fn restart_works_from_stopped_and_from_ended() {
    for setup in [PlaybackState::Stopped, PlaybackState::Ended] {
        let mut engine = TestEngine::start("sine.flac");
        match setup {
            PlaybackState::Stopped => {
                engine.send(PlaybackCommand::Stop);
                engine.await_state(PlaybackState::Stopped);
            }
            _ => engine.play_to_end(),
        }
        engine.send(PlaybackCommand::Restart);
        engine.await_state(PlaybackState::Playing);
        assert!(engine.position() < Duration::from_millis(100));
    }
}

#[test]
fn play_from_ended_does_not_restart_implicitly() {
    let mut engine = TestEngine::start("sine.flac");
    engine.play_to_end();
    engine.send(PlaybackCommand::Play);
    engine.await_event(|e| matches!(e, PlaybackEvent::Warning { .. }));
    assert_eq!(engine.state(), PlaybackState::Ended);
}

#[test]
fn end_of_track_waits_for_the_final_frames_predicted_play_time() {
    let mut engine = TestEngine::start("sine.flac");
    engine.drain_ring_without_advancing_clock();
    assert!(engine.try_event().is_none(), "EOF must not fire when the ring merely empties");
    engine.advance_past_output_latency();
    engine.await_event(|e| matches!(e, PlaybackEvent::EndOfTrack { .. }));
}

#[test]
fn progress_carries_the_session_revision_it_belongs_to() {
    let mut engine = TestEngine::start("sine.flac");
    engine.play_for(Duration::from_millis(100));
    let first = engine.progress();
    engine.send(PlaybackCommand::Stop);
    engine.await_state(PlaybackState::Stopped);
    assert_ne!(engine.progress().session_rev, first.session_rev);
}

#[test]
fn a_saturated_event_channel_stops_admitting_commands_but_not_stop() {
    // Command admission halts while a backlog exists, which bounds further
    // event generation. Stop travels out of band on the interrupt flag plus
    // wake channel, so a full queue cannot delay it.
    let mut engine = TestEngine::start("sine.flac");
    engine.stop_draining_events();
    for _ in 0..256 {
        engine.send(PlaybackCommand::TogglePause);
    }
    engine.interrupt_stop();
    engine.resume_draining_events();
    engine.await_state(PlaybackState::Stopped);
}

#[test]
fn diagnostics_aggregate_rather_than_accumulating_events() {
    let mut engine = TestEngine::start("sine.flac");
    engine.stop_draining_events();
    engine.inject_xruns(10_000);
    engine.resume_draining_events();
    let warnings = engine.count_events(|e| matches!(e, PlaybackEvent::Warning { .. }));
    assert!(warnings < 100, "diagnostics must coalesce, got {warnings} warnings");
}

#[test]
fn a_disconnected_event_receiver_terminates_the_worker() {
    let engine = TestEngine::start("sine.flac");
    engine.drop_event_receiver();
    assert!(engine.join_within(Duration::from_secs(2)));
}
```

`tests/support/mod.rs` provides `TestEngine`, which wires the engine to a `TestOutput` and a virtual clock. Write it as part of this task; scope `#[allow(clippy::unwrap_used)]` to its helper functions, since clippy's test exemption does not cover bare helpers in `tests/`.

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --locked --test engine_contract`
Expected: FAIL — the engine modules do not exist.

- [ ] **Step 3: Write the protocol types**

```rust
// src/playback/volume.rs
/// Linear gain in `[0.0, 1.0]`.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Volume(f32);

impl Volume {
    pub const FULL: Self = Self(1.0);

    pub fn new(value: f32) -> Self {
        Self(if value.is_finite() { value.clamp(0.0, 1.0) } else { 0.0 })
    }

    pub fn as_gain(self) -> f32 {
        self.0
    }

    pub fn percent(self) -> u8 {
        (self.0 * 100.0).round() as u8
    }

    pub fn adjusted(self, delta: f32) -> Self {
        Self::new(self.0 + delta)
    }
}
```

```rust
// src/playback/state.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackState {
    Idle,
    Loading,
    Playing,
    Paused,
    Stopped,
    Ended,
    Failed,
}

impl PlaybackState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Loading => "loading",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Ended => "ended",
            Self::Failed => "failed",
        }
    }
}
```

```rust
// src/playback/command.rs
use std::time::Duration;

use crate::media::id::MediaId;
use crate::media::source::SourceLocation;

use super::volume::Volume;

#[derive(Clone, Debug)]
pub enum PlaybackCommand {
    Load { media: MediaId, source: SourceLocation, start_at: Duration },
    Play,
    Pause,
    TogglePause,
    SeekTo(Duration),
    /// Signed seconds; the engine clamps at zero and at a known duration.
    SeekBy(i64),
    /// Validate first, start output only on success. One command rather than an
    /// app-sequenced `SeekTo(0)` + `Play`, because a stopped seek never emits
    /// `SeekCompleted` for the app to sequence on.
    Restart,
    SetVolume(Volume),
    Stop,
    Shutdown,
}
```

```rust
// src/playback/event.rs
use std::time::Duration;

use crate::media::capabilities::MediaCapabilities;
use crate::media::id::MediaId;
use crate::media::metadata::MediaMetadata;

use super::state::PlaybackState;
use super::timeline::PositionQuality;
use super::volume::Volume;

#[derive(Clone, Debug)]
pub enum PlaybackEvent {
    Loaded { session_rev: u64, media: MediaId, metadata: MediaMetadata,
             capabilities: MediaCapabilities },
    StateChanged { session_rev: u64, state: PlaybackState },
    SeekCompleted { session_rev: u64, requested: Duration, actual: Duration,
                    refinement_truncated: bool },
    /// A seek accepted while stopped. Deliberately not `SeekCompleted`: the
    /// target is unvalidated until the decoder opens.
    SeekTargetStored { session_rev: u64, target: Duration },
    SeekRejected { session_rev: u64, reason: String },
    VolumeChanged { session_rev: u64, volume: Volume },
    EndOfTrack { session_rev: u64, position: Duration },
    DeviceRecovered { session_rev: u64 },
    Warning { session_rev: u64, message: String },
    Failed { session_rev: u64, message: String },
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub session_rev: u64,
    pub media: Option<MediaId>,
    pub position: Duration,
    pub quality: PositionQuality,
}
```

- [ ] **Step 4: Write the engine**

`src/playback/engine.rs` holds the worker loop. Write this skeleton first, then fill each `fn` in against the tests:

```rust
// src/playback/engine.rs
const STOP: u8 = 1;
const SHUTDOWN: u8 = 2;
const TICK: Duration = Duration::from_millis(10);
const DEADLINE: Duration = Duration::from_millis(250);
/// Ordinary events may not occupy these; terminal outcomes may.
const RESERVED_EVENT_SLOTS: usize = 8;
const EVENT_CAPACITY: usize = 64;
const PENDING_CAP: usize = 128;

struct Worker {
    output: Box<dyn AudioOutput>,
    link: Arc<OutputLink>,
    handshake: Handshake,
    timeline: Timeline,
    source: Option<DecodedSource>,
    converter: Option<Converter>,
    pcm: Option<rtrb::Producer<f32>>,
    state: PlaybackState,
    session_rev: u64,
    /// The logical resume point. Stop and recreation preserve it; load,
    /// restart and successful seeks establish a new one.
    position: Duration,
    requested_target: Option<Duration>,
    media: Option<MediaId>,
    volume: Volume,
    pushed_total: u64,
    decoder_drained: bool,
    receivers_gone: bool,
    pending_events: VecDeque<PlaybackEvent>,
    progress: Arc<Mutex<Progress>>,
    commands: Receiver<PlaybackCommand>,
    events: Sender<PlaybackEvent>,
    wake: Receiver<()>,
    interrupt: Arc<AtomicU8>,
}

impl Worker {
    fn run(mut self) {
        loop {
            // 1. Out-of-band interrupts. Shutdown dominates stop.
            let flags = self.interrupt.swap(0, Ordering::Acquire);
            if flags & SHUTDOWN != 0 {
                self.shutdown();
                return;
            }
            if flags & STOP != 0 {
                self.do_stop();
            }

            // 2. Spans -> timeline -> keep-latest progress snapshot.
            self.handshake.drain_spans(&mut self.timeline);
            self.publish_progress();

            // 3. Asynchronous device faults.
            self.service_faults();

            // 4. Flush the event backlog. Admission stays closed while it is
            //    non-empty, which bounds further command-driven generation.
            self.flush_events();

            // 5. Diagnostics coalesce; they never enter pending_events.
            self.emit_aggregated_warning_if_slot_free();

            // 6. Wait. Never a bare sleep - stop and shutdown must wake it.
            let admit = self.pending_events.is_empty();
            crossbeam_channel::select! {
                recv(self.commands) -> command if admit => match command {
                    Ok(command) => self.dispatch(command),
                    Err(_) => { self.shutdown(); return; }
                },
                recv(self.wake) -> _ => {}
                default(TICK) => {}
            }

            // 7. Decode -> convert -> ring, with interruptible backpressure.
            if self.state == PlaybackState::Playing {
                self.pump_audio();
            }
            self.check_end_of_track();

            // A disconnected event receiver is shutdown. Detected from the
            // channel's own errors - `SendError` when flushing events, and
            // `RecvError` above when the command sender is gone - never from
            // channel occupancy, which says nothing about connectedness.
            if self.receivers_gone {
                self.shutdown();
                return;
            }
        }
    }

    fn dispatch(&mut self, command: PlaybackCommand) { /* transition table below */ }
    fn do_stop(&mut self) { /* freeze, capture, teardown; position preserved */ }
    fn publish_progress(&mut self) { /* replace the snapshot; nothing else in the lock */ }
    fn service_faults(&mut self) { /* classify; recover, rebuild, or fail */ }
    fn flush_events(&mut self) { /* honour RESERVED_EVENT_SLOTS; SendError sets receivers_gone */ }
    fn emit_aggregated_warning_if_slot_free(&mut self) { /* counters, not a queue */ }
    fn pump_audio(&mut self) { /* decode, convert, push; wake-channel backpressure */ }
    fn check_end_of_track(&mut self) { /* played == pushed AND last span's end passed */ }
    fn shutdown(&mut self) { /* tear down without rebuilding */ }
}
```

The loop body executes in exactly that order:

1. Check the sticky `interrupt: AtomicU8`. `SHUTDOWN` dominates `STOP`.
2. `handshake.drain_spans(&mut timeline)` and publish a fresh `Progress` snapshot into the keep-latest `Mutex<Progress>`.
3. Drain `output.faults()`; classify with `OutputFault::classify`; `Recoverable` bumps the diagnostics counters, `Rebuild` runs capture-then-rebuild, `Fatal` enters `Failed`.
4. Flush `pending_events` into the event channel. **While `pending_events` is non-empty, do not select on the command channel** — this is the admission limit that bounds further event generation. Terminal events (`Failed`, `EndOfTrack`, `StateChanged{Stopped}`, shutdown acknowledgment) may use the reserved tail slots; ordinary events may not.
5. Emit an aggregated `Warning` **only if** a non-reserved slot is free. Diagnostics are never queued — counts accumulate in the link and the next emitted warning reports the larger total.
6. `select!` over: the command channel (only when admission is open), the wake channel, and a 10 ms timeout. Never a bare sleep.
7. If playing, decode → convert → push into the PCM ring, waiting on the wake-channel `select!` when the ring is full.

Every command dispatches through the transition table:

| Command | Position | Handshake |
|---|---|---|
| `Load` | establishes | freeze/capture (if live), discard, install |
| `SeekTo`/`SeekBy` success | establishes at refined `actual` | freeze, capture, discard, install |
| Seek failure | preserves the captured value | retain the old pipeline if valid, else reopen at that position with a fresh generation, else `Failed` |
| `Restart` | establishes at zero after validation | validate, then install |
| `Play`/`Pause` | preserves | `Run` / `Park`, same generation, new epoch |
| `Stop` | preserves | freeze, capture, teardown |
| Recreation | preserves | fresh `OutputLink`, re-anchored |

Non-obvious rules that the tests above pin:

- `Play` from `Ended` is a no-op plus `Warning`. `Play` from `Failed` is rejected.
- Seek while `Stopped` stores `requested_target` and emits `SeekTargetStored`, never `SeekCompleted`.
- `Restart` and stop-resume call `seek_refined(.., budget: None, ..)`; explicit seeks pass `Some(Duration::from_secs(5))`.
- `EndOfTrack` fires only when `timeline.played_frames(now) == pushed_total` **and** the final span's `t0 + frames/rate` has passed.
- On handshake timeout: attempt teardown, read `link.take_rescue_after_teardown()`, and rebuild. If the record is unrecoverable, keep the last validated position and mark it `Degraded` — never fabricate one.
- `session_rev` increments on load, stop and recreation; every event and every `Progress` carries the current value.

- [ ] **Step 5: Run the tests and confirm they pass**

Run: `cargo test --locked --test engine_contract`
Expected: PASS — all twelve tests.

- [ ] **Step 6: Verify and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked
git add src/playback tests/engine_contract.rs tests/support
git commit -m "Add playback engine with state machine, admission control, and recovery"
```

---

### Task 10: The CLI and key loop

**Files:**
- Create: `src/cli.rs`, `src/app.rs`
- Modify: `src/lib.rs`, `src/main.rs`
- Create: `tests/cli_playback.rs`
- Modify: `docs/architecture.md`

**Interfaces:**
- Consumes: `EngineHandle`, `PlaybackCommand`, `PlaybackEvent`, `Progress`, `Volume` (Task 9).
- Produces: the `continuo play <path>` binary surface.

- [ ] **Step 1: Write the failing tests**

```rust
// tests/cli_playback.rs
use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_continuo")).args(args).output().unwrap()
}

#[test]
fn no_arguments_prints_help_and_exits_nonzero() {
    let output = run(&[]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("play"), "help must mention the play subcommand: {text}");
}

#[test]
fn an_absent_file_exits_nonzero_with_a_concise_message() {
    let output = run(&["play", "/nonexistent/definitely-not-here.flac"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("continuo:"), "expected a prefixed message: {text}");
    assert!(text.contains("definitely-not-here.flac"), "expected the path: {text}");
}

#[test]
fn a_directory_is_rejected_as_not_a_regular_file() {
    let output = run(&["play", env!("CARGO_MANIFEST_DIR")]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("regular file"), "expected the regular-file rule: {text}");
}

#[test]
fn a_relative_path_is_accepted_and_canonicalized() {
    // Canonicalization happens at the worker's source-opening boundary, so a
    // relative path must not be rejected by argument parsing.
    let output = Command::new(env!("CARGO_BIN_EXE_continuo"))
        .args(["play", "tests/fixtures/sine.flac", "--probe-only"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("44100"), "expected the negotiated rate: {text}");
}
```

`--probe-only` opens the file, prints metadata, and exits without touching a device. It exists so the CLI path is testable in CI, where no audio device is available.

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --locked --test cli_playback`
Expected: FAIL — the binary still prints the M0 startup message and takes no arguments.

- [ ] **Step 3: Write the CLI**

```rust
// src/cli.rs
use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "continuo", about = "A keyboard-first terminal audio player")]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Play a local audio file.
    Play {
        /// Path to an MP3, FLAC, WAV, or M4A file.
        path: PathBuf,
        /// Open the file, print what was found, and exit without using a device.
        #[arg(long)]
        probe_only: bool,
    },
}
```

- [ ] **Step 4: Write the app loop**

```rust
// src/app.rs
pub fn run(cli: cli::Cli) -> Result<(), PlaybackError> {
    let cli::CliCommand::Play { path, probe_only } = cli.command;
    let canonical = path
        .canonicalize()
        .map_err(|source| PlaybackError::Open { path: path.clone(), source })?;
    let absolute = AbsolutePath::new(canonical)
        .map_err(|error| PlaybackError::UnsupportedInput {
            path: path.clone(),
            reason: error.to_string(),
        })?;

    if probe_only {
        let source = DecodedSource::open(&absolute)?;
        println!(
            "{} {} Hz {} ch {:?}",
            source.metadata().title.as_deref().unwrap_or("(untitled)"),
            source.sample_rate(),
            source.channels(),
            source.metadata().duration,
        );
        return Ok(());
    }

    let engine = EngineHandle::spawn(Box::new(CpalOutput::default()));
    let _raw = RawModeGuard::enable()?; // Drop restores the terminal on every path
    let mut mirror = Mirror::default();
    engine.commands().send(PlaybackCommand::Load {
        media: MediaId::LocalFile(absolute.clone()),
        source: SourceLocation::LocalPath(absolute.as_path().to_path_buf()),
        start_at: Duration::ZERO,
    }).ok();
    engine.commands().send(PlaybackCommand::Play).ok();

    loop {
        if crossterm::event::poll(Duration::from_millis(100))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                match to_command(key, &mirror) {
                    Some(PlaybackCommand::Shutdown) => break,
                    Some(command) => { engine.commands().send(command).ok(); }
                    None => {}
                }
            }
        }
        while let Ok(event) = engine.events().try_recv() {
            mirror.apply(event);
        }
        let progress = engine.progress();
        // A snapshot can overtake queued lifecycle events, so render only when
        // it belongs to the session the mirror is showing.
        if progress.session_rev == mirror.session_rev {
            mirror.position = progress.position;
            mirror.quality = progress.quality;
        }
        render(&mirror)?;
    }
    engine.commands().send(PlaybackCommand::Shutdown).ok();
    engine.join();
    Ok(())
}
```

`src/app.rs` runs on the main thread:

1. Canonicalize the path, build `AbsolutePath` and `MediaId::LocalFile`.
2. For `--probe-only`: open a `DecodedSource`, print `title`, `duration`, `sample_rate`, `channels`, and exit zero.
3. Otherwise spawn the engine with a `CpalOutput`, enable crossterm raw mode, and send `Load { start_at: Duration::ZERO }` followed by `Play`.
4. Loop on a 100 ms tick: poll `crossterm::event::poll` for keys, drain `engine.events()` into the mirror, read `engine.progress()`, and repaint the status line.
5. **Render progress only when `progress.session_rev == mirror.session_rev`**, so a snapshot cannot overtake a queued lifecycle event and show one track's position under another's title.

Key bindings:

| Key | Command |
|---|---|
| `space` | `TogglePause` |
| `←` / `→` | `SeekBy(-10)` / `SeekBy(10)` |
| `Home` | `Restart` |
| `-` / `+` | `SetVolume(volume.adjusted(-0.05 / 0.05))` |
| `s` / `p` | `Stop` / `Play` |
| `q`, `Ctrl-C`, EOF | `Shutdown` |

Raw mode must be disabled on every exit path, including the error path and the panic path — install the guard as a `Drop` type so an early return cannot leave the terminal raw.

Status line:

```
ep.flac [playing] 00:04:12 / 01:02:30  vol 80%
space pause · ←/→ seek 10s · Home restart · -/+ volume · s stop · p play · q quit
```

Unknown duration renders as `--:--:--`. `PositionQuality::Degraded` appends ` ~` after the position.

- [ ] **Step 5: Rewrite the binary**

```rust
// src/main.rs
use std::process::ExitCode;

use clap::Parser;
use continuo::{app, cli, telemetry};

fn main() -> ExitCode {
    if let Err(error) = telemetry::init() {
        eprintln!("continuo: {error}");
        return ExitCode::FAILURE;
    }
    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            eprint!("{error}");
            return ExitCode::FAILURE;
        }
    };
    match app::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("continuo: {error}");
            tracing::error!(error = ?error, "playback failed");
            ExitCode::FAILURE
        }
    }
}
```

Add `pub mod app;` and `pub mod cli;` to `src/lib.rs`.

- [ ] **Step 6: Run the tests and confirm they pass**

Run: `cargo test --locked --test cli_playback`
Expected: PASS — all four tests.

`tests/cli.rs` from M0 asserts the old "Continuo foundation initialized" startup message on a bare invocation. That invocation now prints help and exits nonzero, so update that test to assert the new behaviour rather than deleting it.

- [ ] **Step 7: Manual acceptance**

Simulated tests do not establish audible playback. Run each of these on a machine with audio and record the actual result:

```bash
cargo run --locked -- play tests/fixtures/sine.flac
cargo run --locked -- play <a real MP3, several minutes long>
```

- [ ] Audio is audible and correct at the natural rate.
- [ ] `space` pauses and resumes; position freezes while paused and does not jump on resume.
- [ ] `←` and `→` seek both directions; position matches what is heard.
- [ ] `s` then `p` resumes at the preserved position, not zero.
- [ ] `Home` restarts from zero from `Playing`, `Paused`, `Stopped` and `Ended`.
- [ ] `-` and `+` change volume audibly and without clicks.
- [ ] Playback reaches the end and reports `ended` after the audio finishes, not before.
- [ ] `q` exits cleanly and the terminal is left un-raw.
- [ ] Repeat with a file whose rate differs from the device's (verify with `pw-metadata -n settings | grep rate`).
- [ ] Unplug or disable the output device mid-playback; recovery reports `DeviceRecovered` and resumes at the preserved position.

- [ ] **Step 8: Update the architecture document**

Add M1's shipped contracts to `docs/architecture.md`: the media-span position mechanism, the acknowledged handshake, and the note that diagnostics coalesce while lifecycle events stay lossless.

- [ ] **Step 9: Verify and commit**

```bash
cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked
git add src tests docs/architecture.md
git commit -m "Add play CLI with raw-mode key control and status rendering"
```

---

## Open items carried from the spec

These are deliberately unresolved and must be closed during implementation, not silently skipped:

1. **The `faults` counter versus the fault channel.** Spec §4 lists `faults` as a callback-written diagnostic while §10 routes device errors through a bounded channel. Task 8 uses the channel and Task 3 omits the counter. Confirm during Task 9 that nothing needs both.
2. **Span-ring capacity (64) and the PCM ring target (250 ms)** are starting values. During Task 10's manual acceptance, log `spans_dropped` and xrun counts over a full track and adjust if either is non-zero under normal load.
3. **The reserve budget is a proof obligation.** Before Task 9 is reviewed, enumerate each command's maximum event count and show the reserved tail is large enough for the worst case — simultaneous EOF and output fault.
