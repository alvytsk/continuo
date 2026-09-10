# Continuo M3.1 — Estimated Seeking and Position Provenance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make seeking work on a long MP3 with no seek index served over HTTP — the shape of nearly every podcast — without wedging the player, and without an unconfirmed position ever overwriting a confirmed one.

**Architecture:** Seeks on such a source stop asking the demuxer to scan and instead compute a byte offset directly, landing approximately and saying so. Provenance travels beside `PositionQuality` as a second, independent axis, through `Progress` into the session policy, where an estimated location is persisted in its own field and never in the established one. A wall-clock deadline bounds the seek, and the plan's first task decides — by experiment, not argument — whether the unbounded `FormatReader::seek()` can be removed from this path rather than merely fenced.

**Tech Stack:** Rust 2024, `symphonia` (mp3/aac/isomp4/alac), `tokio` + `reqwest` for the HTTP source, `serde`/`serde_json` for persistence, `crossbeam-channel`, `rtrb`, `cpal`.

**Spec:** `docs/superpowers/specs/2026-09-10-continuo-estimated-seek-design.md` — read it alongside this plan. It amends `docs/superpowers/specs/2026-09-09-continuo-finite-http-design.md`, which remains in force for everything it does not touch. Every `§n` below points into the amendment unless it says M3.

## Context an implementer needs

The defect, verified against `symphonia-bundle-mp3-0.6.1/src/demuxer.rs`:

```rust
fn preseek_accurate(&mut self, required_ts: Timestamp, min_ts: Timestamp) -> Result<()> {
    if required_ts < self.next_packet_ts {
        let seeked_pos = self.reader.seek(SeekFrom::Start(self.first_packet_pos))?;
```

`next_packet_ts` is the *demuxer's* read position, ahead of audible playback by the PCM ring plus `MediaSourceStream`'s read-ahead. A seek forward in audible terms is backward in demuxer terms, so it rewinds to the first packet and rescans. The scan parses frame headers and skips bodies — cheap locally, expensive over HTTP — and all of it happens inside one uncancellable `FormatReader::seek()`, during which the worker dispatches no commands. That is why pause and resume appear dead.

The reproduction is already in the tree, `#[ignore]`d:
`tests/engine_remote.rs::a_short_forward_seek_on_a_no_index_mp3_rescans_the_whole_file_instead_of_landing_quickly`.
Run it with `cargo test --locked --test engine_remote -- --ignored a_short_forward_seek`. It must still fail at the start of this work and pass at the end.

## Review round 1 — eight issues resolved before execution

Every claim below was verified against the source, and two turned out worse than reported.

| # | Issue | Resolution |
|---|---|---|
| R1 | A source-level deadline cannot bound `ByteChannel::read`: its stall budget is *suspended* while frozen (by design), so a paused, stalled read blocks indefinitely. Passing "remaining deadline" as a stall budget inherits that suspension and bounds nothing. | Task 4 now includes `src/http/channel.rs`. An **absolute** `Instant` deadline is checked inside the wait loop **regardless of freeze**, distinct from the stall budget, with a paused-and-stalled seek regression. |
| R2 | Routing only `seek_to` leaves three other entry points scanning. Verified: `seek_refined` is called at `engine.rs:1910` (`load` — launch resume), `:2314` (`seek_to`), `:2484` (`reseek` — stop/play and device recovery) and `:2633` (`verify_seek_support` — stopped-seek validation). Under shape A the reader's timestamps may also be relative to a new origin, so a later absolute seek lands wrong. | Task 4 requires **one shared routing and timestamp policy** across all four sites, and must bound the refined remote fallback too — an *indexed* MP3 still takes the same `Accurate` scan today. |
| R3 | `src/persistence/store.rs:107` rejects **every** unequal version, older included — its own comment says "Not `> SCHEMA_VERSION`". Bumping the constant would mark every existing v1 file unsupported and silently disable persistence for every current user. The plan's "confirm the store needs no change beyond the constant" was wrong. | Task 5 requires explicit **acceptance of v1 and normalisation to v2**, tested through load → modify → write → reload. Deserialisation preserves the file's version while writing asserts the current one, so a partial migration would write estimates under schema 1. |
| R4 | Verified EOF does not establish absolute media time. `engine.rs:1702` computes the terminal position as `landed_anchor + frames_to_duration(pushed_total, rate)` — anchor plus decoded frames. HTTP verification establishes that the *body* completed, not that the anchor was right. `EndOfTrack` then clears protection and writes that number. | Task 6 must define how EOF under estimated provenance is represented and test estimated seek → EOF → persisted state. **Completion must not silently promote an estimated timestamp.** |
| R5 | The write rules omitted `SeekTargetStored`. `Session` writes that unvalidated target directly and substitutes it for the sampled position at shutdown (`position_for`). | Task 6 defines estimated seek → stop → new stored target → quit: the newest intent survives without overwriting the established checkpoint. Existing tests pin the old behaviour for established playback, so the compatibility decision must be explicit. |
| R6 | **The known-debt claim that `Coarse` refuses without Xing is false.** `symphonia-bundle-mp3-0.6.1/src/demuxer.rs:465-471`: with no Xing/Info tag and a seekable source, it calls `estimate_num_mpeg_frames` and sets `num_frames` from bitrate and byte length. So `num_frames` is usually `Some`, `Coarse` would likely not refuse — and our reported duration is itself an estimate. Also, the spike's proposed baseline (play to 60 s, seek to 70 s) does not guarantee the target precedes `next_packet_ts`; encoded read-ahead alone does not advance that timestamp. | Task 1 measures **Coarse alongside A and B**, records both `next_packet_ts` and the target to prove the rewind condition was actually exercised, and records how **index evidence is distinguished from estimated duration** — `num_frames` being present does not mean a seek index exists. |
| R7 | One sine fixture cannot establish a universal preroll count, and the proposed test only asserted a number between 1 and 4. Reservoir requirements depend on frame payload and back-references, and a pure sine is the easiest possible case. | Task 2 requires a **justified bound or a dependency-aware preroll**, validated by comparing decoded output against continuous decoding across varied offsets and bitrate/channel configurations. |
| R8 | `position: Duration` is mandatory and cannot express "an entry that carries only an estimate", which §4.2 requires. | `position` becomes `Option<Duration>`: `None` means nothing has ever established one for this media. A v1 file's present value deserialises as `Some`, so the migration is free. `StartDisposition::ResumedEstimated { established: Option<Duration> }` reports `None` when there is no established fallback. |

**What R6 may do to this plan.** If the spike finds `Coarse` both works and does not rescan, it is dramatically simpler than shapes A or B — symphonia performs the byte estimate itself, and Tasks 2 and 4 shrink to routing plus bounding. Task 1 must therefore evaluate `Coarse` **first**, and the plan below is written so that outcome collapses work rather than invalidating it.

## Global Constraints

Every task's requirements implicitly include this section.

- Rust edition **2024**, `rust-version = "1.98.1"`. Do not raise either.
- `[lints.rust] unsafe_code = "forbid"`. `[lints.clippy] unwrap_used = "deny"`, `expect_used = "deny"`. `clippy.toml`'s `allow-unwrap-in-tests` / `allow-expect-in-tests` exempt the **body of a `#[test]` function only** — a bare helper is not exempt even under `tests/`. Every `#[allow]` carries a comment saying why.
- Every task ends green on all three: `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked`.
- Baseline before this plan: **382 passed, 0 failed, 2 ignored** (`device_smoke`, which needs real hardware, and the parked reproduction).
- **No existing test may be weakened or deleted.** `tests/engine_contract.rs`, `tests/engine_shutdown.rs`, `tests/resume_contract.rs`, `tests/session_policy.rs`, `tests/decode_fixtures.rs` are M1/M2 contract evidence for the binding invariant — stopping or recreating the pipeline never implicitly resets position. Mechanical call-site updates are expected; a broken assertion is a finding to report, not a chore.
- No public network, no real device: `127.0.0.1` and `TestOutput` only. Persistence tests use `tempfile::TempDir`.
- **No `std::thread::sleep` as a synchronization primitive.** Poll a condition against a deadline with a failing assertion, or use `server.wait_until_stalled`. A cancellation test proves the wait it targets was entered first.
- Signed query strings and URL userinfo stay out of diagnostics; every URL in a log or an error passes through `redact_url`.
- The codebase's comment style explains **why** a rule exists, not what the code does. It is unusually dense with rationale; match that.

---

## File Structure

**Created**

| File | Responsibility |
|---|---|
| `src/playback/seek_estimate.rs` | `SeekEstimator` — media time to byte offset, audio-data offset, frame resync, reservoir preroll |
| `src/playback/provenance.rs` | `PositionProvenance` and its composition rules |
| `tests/seek_estimate.rs` | Estimator unit coverage against real fixtures |
| `tests/estimated_seek.rs` | End-to-end estimated seeking over the loopback server |
| `tests/provenance_policy.rs` | Session-policy coverage for §4's write rules, restart preference and clearing |
| `tests/fixtures/sine-long-noxing.mp3` | A no-index MP3 long enough that a rescan is measurable |

**Modified**

| File | Change |
|---|---|
| `src/playback/decode.rs` | `seek_estimated`; the `SeekMode` decision; preroll discard |
| `src/http/source.rs` | Wall-clock seek deadline distinct from the stall budget |
| `src/playback/engine.rs` | Seek path, provenance on `Progress`, recovery with a fresh deadline |
| `src/playback/wait.rs` | Carry provenance through `SessionFacts` and `publish_progress` |
| `src/playback/event.rs` | `SeekCompleted` provenance; `StartDisposition::ResumedEstimated` |
| `src/persistence/model.rs` | `PersistedCheckpoint.estimated`; `SCHEMA_VERSION` 2 |
| `src/persistence/store.rs` | v1-to-v2 acceptance |
| `src/session.rs` | §4's write rules, restart preference, clearing |
| `src/resume.rs` | Resume decision over two locations |
| `src/app.rs` | Status line renders provenance |
| `docs/architecture.md`, `docs/m1-known-debt.md`, `docs/m3-acceptance.md`, `tests/fixtures/README.md` | Ship it honestly |

**Dependency order.** 1 gates everything. 2 and 3 are independent of each other and need only 1. 4 needs 1–3. 5 → 6. 7 needs 4 and 6. 8 last.

---

## Task 1: Spike — settle how a byte seek reaches the demuxer

**This task writes no shipping code.** Its deliverable is a decision backed by a working experiment, because §5.2 is the difference between removing the unbounded call and merely fencing it, and the rest of the plan is shaped by the answer.

**Files:**
- Create: `tests/fixtures/sine-long-noxing.mp3`, and a throwaway experiment you delete before committing
- Modify: `tests/fixtures/README.md`

**The question.** Three shapes, and **evaluate `Coarse` first** — if it works, the other two are unnecessary.

- **C — `SeekMode::Coarse`.** The known-debt entry claims it refuses without a Xing header. **That is false**, verified at `symphonia-bundle-mp3-0.6.1/src/demuxer.rs:465-471`: with no Xing/Info tag and a seekable source, symphonia calls `estimate_num_mpeg_frames` and sets `num_frames` from bitrate and byte length. So `num_frames` is usually `Some` and `Coarse` would likely not refuse — it would do the byte estimate itself. If that holds and it does not rescan, this collapses most of Tasks 2 and 4.
- **A — re-probe at the offset.** Byte-seek the `MediaSource`, then build a *fresh* `FormatReader` from that position. Legitimate for MP3 from any frame boundary. Removes `FormatReader::seek()` from this path entirely, so §5.3's bound becomes real rather than partial. Costs a probe per seek.
- **B — byte-seek beneath a live reader.** Cheaper, but depends on demuxer internals the API does not promise, and leaves the stale-timestamp problem to solve.

**A consequence to carry forward whichever shape wins:** if `num_frames` is estimated from bitrate, then the **duration we report is itself an estimate**, and a byte offset derived from it inherits that error. Record how index evidence is to be distinguished from estimated duration — `num_frames` being present does **not** mean a seek index exists, and `SeekSupport` must not treat it as one.

- [ ] **Step 1: Generate the fixture**

`tests/fixtures/sine-noxing.mp3` exists but is 5 seconds — too short for a rescan to be measurable. Generate a longer sibling the same way:

```bash
ffmpeg -f lavfi -i "sine=frequency=440:duration=600" -c:a libmp3lame -b:a 128k \
  -write_xing 0 tests/fixtures/sine-long-noxing.mp3
```

Verify it has no Xing/VBRI frame and no seek index, and record it in `tests/fixtures/README.md` in that file's existing style — name, purpose, exact command, and why `-write_xing 0` is load-bearing (a contributor regenerating with defaults destroys the property the fixture exists for). Follow `sine-noxing.mp3`'s entry.

- [ ] **Step 2: Measure the current behaviour**

Write a throwaway binary or `#[test]` that opens the fixture through `HttpMediaSource` against a `TestServer` and records: bytes the server sent, offsets requested, and how long `reader.seek()` blocked.

**The obvious baseline does not work.** "Play to 60 s, seek to 70 s" does not guarantee the target precedes `next_packet_ts` — encoded read-ahead alone does not advance that timestamp, and the rewind fires only on `required_ts < next_packet_ts`. So a run staged that way may never exercise the bug and would report a clean baseline for the wrong reason.

**Record both numbers and prove the condition fired.** Instrument or infer `next_packet_ts` (decoded packets' timestamps are the observable proxy) alongside the seek target, and assert in your notes that `target < next_packet_ts` held for the run you measured. A baseline that cannot show the rewind condition was met measures nothing.

- [ ] **Step 3: Measure shape C first**

Set `SeekMode::Coarse` and repeat the baseline. Answer: does it refuse (`SeekErrorKind::Unseekable`) or seek? Does `num_frames` come back `Some` for this fixture, and from where — a real index or `estimate_num_mpeg_frames`? Does it rescan from the first packet, or land by arithmetic? How far off is the landing?

If `Coarse` seeks without rescanning, say so plainly and record the accuracy you measured. Steps 3a and 4 then become confirmatory rather than load-bearing, and you should still run them briefly so the decision records what the alternatives cost.

- [ ] **Step 3a: Try shape A**

Byte-seek the source to an estimated offset, resync to the next frame header, build a fresh `FormatReader` from there, and decode. Answer, with measurements:

- Does symphonia probe successfully from a mid-file frame boundary?
- What does the fresh reader report as the packet timestamp — zero-based from the new offset, or absolute? This determines whether the engine must add the estimated base itself.
- What does a probe cost against the loopback server, in bytes and in wall-clock?
- Do the first decoded frames sound wrong (the bit reservoir)? How many must be discarded?

- [ ] **Step 4: Try shape B, only far enough to compare**

Byte-seek beneath the live reader and observe what `next_packet_ts` does. You are looking for whether the reader can be made coherent without touching internals it does not expose. Do not invest in making it work if the first probe says it cannot.

- [ ] **Step 5: Decide and record**

Write the decision, the measurements behind it, and the rejected shape's failure mode to `docs/superpowers/specs/2026-09-10-continuo-estimated-seek-design.md` as a new `### 5.2 Resolution` subsection. State the timestamp-base answer explicitly — Task 2 and Task 4 both depend on it.

**If neither shape works,** say so and stop. That is a real outcome and it changes the plan: the fallback is §5.3's partial bound plus honest refusal, and that is the human's decision, not yours.

- [ ] **Step 6: Clean up and commit**

Delete the experiment. Commit only the fixture, its README entry, and the spec resolution.

```bash
git add tests/fixtures/sine-long-noxing.mp3 tests/fixtures/README.md docs/superpowers/specs/2026-09-10-continuo-estimated-seek-design.md
git commit -m "spike: settle how an estimated byte seek reaches the demuxer"
```

---

## Task 2: The seek estimator

**Files:**
- Create: `src/playback/seek_estimate.rs`, `tests/seek_estimate.rs`
- Modify: `src/playback/mod.rs`

**Interfaces:**
- Consumes: Task 1's timestamp-base answer.
- Produces:

```rust
/// Turns a media time into a byte offset for a source whose demuxer has no
/// usable seek index.
///
/// Deliberately not a general "seek table": this is an *estimate*, and every
/// caller is expected to treat its landing as such (§3).
pub struct SeekEstimator {
    /// Where the first audio frame begins — past any ID3v2 tag. A seek that
    /// ignores this lands inside metadata and resyncs to the wrong place.
    audio_data_offset: u64,
    /// Total audio bytes, excluding the header offset and any trailing tag.
    audio_data_len: u64,
    /// The recording's duration, from whatever established it.
    duration: Duration,
}

impl SeekEstimator {
    pub fn new(audio_data_offset: u64, audio_data_len: u64, duration: Duration) -> Self;
    /// The byte to seek to for `target`, clamped into the audio data.
    pub fn byte_for(&self, target: Duration) -> u64;
    /// The media time a byte offset corresponds to — the inverse, needed to
    /// report where a landing actually is.
    pub fn time_for(&self, byte: u64) -> Duration;
}

/// How many frames after an arbitrary byte offset must be decoded and thrown
/// away before the output is trustworthy.
///
/// **Not a measurement from one fixture.** MP3's bit reservoir lets a frame
/// reference bits from up to 511 bytes of earlier frames, so the requirement
/// depends on frame payload and back-reference depth, not on the file. This is
/// a *bound* justified from the format, and Task 2's tests validate it by
/// comparing decoded output against continuous decoding across varied offsets,
/// bitrates and channel modes — a pure sine at one bitrate is the easiest
/// possible case and proves nothing about the worst one.
pub const RESERVOIR_PREROLL_FRAMES: usize = /* justify from main_data_begin's range */;

/// Find the next frame header at or after `from`, so decoding starts on a
/// boundary rather than mid-frame.
pub fn resync(bytes: &[u8], from: usize) -> Option<usize>;
```

- [ ] **Step 1: Write the failing tests**

`tests/seek_estimate.rs`, against the real fixtures so the numbers are not invented:

```rust
use std::time::Duration;

use continuo::playback::seek_estimate::{RESERVOIR_PREROLL_FRAMES, SeekEstimator, resync};

#[allow(clippy::unwrap_used)] // A fixture committed to this repository.
fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read(path).unwrap()
}

#[test]
fn a_seek_to_zero_lands_on_the_first_audio_byte_not_byte_zero() {
    // The captured defect started every rescan at byte 37849 — the first
    // frame, past a large ID3v2 tag. An estimator that ignores the offset
    // seeks into metadata and resyncs somewhere arbitrary.
    let estimator = SeekEstimator::new(37_849, 130_502_739, Duration::from_secs(8156));
    assert_eq!(estimator.byte_for(Duration::ZERO), 37_849);
}

#[test]
fn the_estimate_is_linear_in_the_audio_data_and_inverts() {
    let estimator = SeekEstimator::new(1_000, 16_000_000, Duration::from_secs(1_000));
    // Half way through the recording is half way through the audio data,
    // measured from the first frame rather than from the file's start.
    assert_eq!(estimator.byte_for(Duration::from_secs(500)), 1_000 + 8_000_000);
    // And the inverse round-trips, which is what lets a landing report where
    // it actually is rather than where it was asked for.
    assert_eq!(
        estimator.time_for(1_000 + 8_000_000),
        Duration::from_secs(500)
    );
}

#[test]
fn a_target_past_the_end_clamps_into_the_audio_data() {
    // A clamp, not a failure: seeking past the end is an ordinary thing for a
    // listener to ask for, and 416 is never how this project answers it.
    let estimator = SeekEstimator::new(1_000, 16_000_000, Duration::from_secs(1_000));
    let last = estimator.byte_for(Duration::from_secs(9_999));
    assert!(last < 1_000 + 16_000_000, "clamped past the audio data: {last}");
    assert!(last >= 1_000);
}

#[test]
fn a_zero_length_recording_cannot_divide_by_zero() {
    let estimator = SeekEstimator::new(1_000, 0, Duration::ZERO);
    assert_eq!(estimator.byte_for(Duration::from_secs(5)), 1_000);
    assert_eq!(estimator.time_for(1_000), Duration::ZERO);
}

#[test]
fn resync_finds_the_next_frame_header_from_an_arbitrary_offset() {
    // An estimated byte lands mid-frame essentially always. Decoding from
    // there produces garbage until the next header, which is what resync
    // exists to skip.
    let bytes = fixture("sine-long-noxing.mp3");
    let start = bytes.len() / 2;
    let found = match resync(&bytes, start) {
        Some(found) => found,
        None => panic!("no frame header found after {start} in a 10-minute MP3"),
    };
    assert!(found >= start);
    // A frame header begins with eleven set sync bits.
    assert_eq!(bytes[found], 0xFF);
    assert_eq!(bytes[found + 1] & 0xE0, 0xE0);
    // And it is genuinely nearby: a frame at 128 kbps is ~417 bytes, so a
    // resync that walked kilobytes is finding false syncs.
    assert!(found - start < 2_048, "resync walked {} bytes", found - start);
}

#[test]
fn resync_reports_absence_rather_than_guessing() {
    assert_eq!(resync(&[0x00; 64], 0), None);
    assert_eq!(resync(&[], 0), None);
}

#[test]
fn the_preroll_discards_every_frame_the_reservoir_can_poison() {
    // The real assertion, not a range check on a constant. Decode the fixture
    // continuously to get the truth, then decode from a byte offset with the
    // preroll applied, and require the two to agree from the first frame the
    // preroll admits. A number between one and four proves nothing; matching
    // samples do.
    //
    // Vary the offset across the file and repeat for each bitrate and channel
    // mode fixture: the reservoir's depth depends on frame payload and
    // back-reference distance, so a pure sine at one bitrate is the easiest
    // possible case and the one least likely to expose an insufficient bound.
    for fixture_name in ["sine-long-noxing.mp3", /* add the varied fixtures */] {
        let bytes = fixture(fixture_name);
        let truth = decode_continuously(&bytes);
        for fraction in [0.1_f64, 0.37, 0.5, 0.84] {
            let raw = (bytes.len() as f64 * fraction) as usize;
            let start = match resync(&bytes, raw) {
                Some(start) => start,
                None => panic!("no frame header after {raw} in {fixture_name}"),
            };
            let (admitted_at, samples) = decode_from_offset_with_preroll(&bytes, start);
            assert_samples_match(&truth, admitted_at, &samples, fixture_name, fraction);
        }
    }
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --test seek_estimate 2>&1 | tail -20`
Expected: FAIL — `unresolved import continuo::playback::seek_estimate`.

- [ ] **Step 3: Implement**

Add `pub mod seek_estimate;` to `src/playback/mod.rs` and write the module. `byte_for` is `audio_data_offset + (target / duration) * audio_data_len`, saturating and clamped, with `duration == 0` yielding the offset. `time_for` inverts it. `resync` scans for `0xFF` followed by three set bits, and — because false syncs are common in audio data — validates the candidate's version, layer and bitrate fields before accepting it; say in a comment why a bare sync-word match is not enough.

**Justify `RESERVOIR_PREROLL_FRAMES` from the format, not from one measurement.** MP3's `main_data_begin` back-reference has a bounded range (511 bytes), which bounds how many prior frames a frame can draw on; derive the constant from that and say so in the doc comment. If the comparison test shows the derived bound is insufficient for any fixture, that is a finding — either the derivation is wrong or the preroll must become dependency-aware (reading `main_data_begin` and discarding until the reservoir is satisfied). Report which, rather than raising the constant until tests pass.

You will need extra fixtures at varied bitrates and channel modes. Generate them the same way as `sine-long-noxing.mp3`, and record each in `tests/fixtures/README.md` with its command and its purpose. Prefer material with more spectral content than a pure sine — a sine compresses to almost nothing and barely uses the reservoir at all.

- [ ] **Step 4: Verify**

Run: `cargo test --test seek_estimate` — expected PASS, 7 tests.
Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add src/playback/seek_estimate.rs src/playback/mod.rs tests/seek_estimate.rs
git commit -m "feat(playback): add the byte-offset seek estimator"
```

---

## Task 3: Position provenance

**Files:**
- Create: `src/playback/provenance.rs`
- Modify: `src/playback/mod.rs`, `src/playback/event.rs`, `src/playback/wait.rs`, `src/playback/engine.rs`, `src/app.rs`
- Test: `tests/provenance.rs` (new); mechanical updates wherever `Progress` is constructed

**Interfaces:** `PositionProvenance { Established, Estimated }`, exactly as §3 defines it, plus `Progress.provenance` and `SeekCompleted.provenance`.

**The rule that matters, and the one a reviewer should check hardest:** provenance is a **second axis**, never merged into `PositionQuality`. `PositionQuality::Estimated` already exists and means something else entirely — how precisely we know what has been *heard*, reconstructed from callback spans, which is the ordinary state during playback. A `Degraded` position whose media time was decoder-established is still `Established`. The session policy reads provenance and never quality.

**Stickiness (§3.1).** Decoding forward from an estimated landing keeps reporting `Estimated`. Provenance changes only when something establishes the absolute position: a confirmed seek landing, an established restart, a fresh load. It is not a function of elapsed playback, and no timer clears it.

- [ ] **Step 1: Write the failing tests**

`tests/provenance.rs`:

```rust
use std::time::Duration;

use continuo::playback::provenance::PositionProvenance;
use continuo::playback::timeline::PositionQuality;

#[test]
fn provenance_and_quality_are_independent_axes() {
    // The whole point of a second field. Every combination is meaningful, so
    // none of them may be collapsed into the other enum.
    for quality in [
        PositionQuality::Exact,
        PositionQuality::Estimated,
        PositionQuality::Degraded,
    ] {
        for provenance in [
            PositionProvenance::Established,
            PositionProvenance::Estimated,
        ] {
            // Constructing the pair is the assertion: if a later change folds
            // provenance into quality this stops compiling.
            let _ = (quality, provenance);
        }
    }
}

#[test]
fn a_degraded_position_can_still_be_established() {
    // A device fault says nothing about whether the media time was confirmed.
    // Reading provenance off quality would call this estimated and refuse to
    // checkpoint a position the decoder actually established.
    let quality = PositionQuality::Degraded;
    let provenance = PositionProvenance::Established;
    assert_eq!(provenance, PositionProvenance::Established);
    assert_eq!(quality, PositionQuality::Degraded);
}

#[test]
fn established_is_the_default_so_every_existing_path_keeps_its_meaning() {
    // M1 and M2 wrote positions the decoder confirmed. Anything that does not
    // opt into an estimate must keep reporting what it always reported.
    assert_eq!(PositionProvenance::default(), PositionProvenance::Established);
}
```

Add to `tests/engine_contract.rs`, additively:

```rust
#[test]
fn an_ordinary_local_seek_reports_an_established_landing() {
    // The M1 path is unchanged: a local file's refined seek lands where it
    // says, and nothing about this milestone may make it claim otherwise.
    let mut engine = TestEngine::start(TRACK);
    engine.load(fixture(TRACK));
    engine.play_for(Duration::from_millis(200));
    engine.send(PlaybackCommand::SeekTo(Duration::from_secs(2)));
    let completed = engine.await_seek_completed(Duration::from_secs(10));
    assert_eq!(completed.provenance, PositionProvenance::Established);
    engine.finish();
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --test provenance --test engine_contract 2>&1 | tail -20`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Implement**

Write `src/playback/provenance.rs` with `PositionProvenance`, `#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]` and `#[default] Established`. Add `provenance` to `Progress`, to `SessionFacts`, and to `SeekCompleted`. Thread it through `WaitService::publish_progress` beside `quality` — read from facts, never derived. Update **every** `Progress` literal in the tree: `src/playback/engine.rs`, `src/playback/wait.rs`, `tests/wait_service.rs`, `tests/session_policy.rs`. The compiler finds them; the point of saying so is that naming a subset is how the M3 plan got this wrong.

`src/app.rs`'s status line appends ` ~est` when provenance is `Estimated`, beside the existing `~` for degraded quality — two marks, because they are two facts.

- [ ] **Step 4: Verify**

Run: `cargo test --locked 2>&1 | tail -5` — the whole suite, since this touches every `Progress`.
Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(playback): carry position provenance beside quality

A byte-offset landing is not a measurement. Provenance is a second axis
rather than a fourth PositionQuality, because quality already means how
precisely we know what has been heard - a Degraded position whose media
time the decoder established is still established."
```

---

## Task 4: The bounded estimated seek

**Files:**
- Modify: `src/playback/decode.rs`, `src/http/channel.rs`, `src/http/source.rs`, `src/playback/engine.rs`
- Test: `tests/estimated_seek.rs` (new)

**Interfaces:**

```rust
// src/playback/decode.rs
impl DecodedSource {
    /// Seek by byte estimate, for a source whose demuxer cannot seek in media
    /// time without rescanning. Lands approximately; the caller reports the
    /// landing as `PositionProvenance::Estimated`.
    pub fn seek_estimated(
        &mut self,
        target: Duration,
        estimator: &SeekEstimator,
        deadline: Instant,
    ) -> Result<SeekOutcome, PlaybackError>;

    /// Whether this source needs the estimated path — a byte-seekable
    /// transport under a demuxer with no usable index.
    pub fn needs_estimated_seek(&self) -> bool;
}

// src/http/channel.rs — the deadline must live HERE, not only above it.
impl SourceInterrupt {
    /// An absolute deadline for one operation, checked inside `read`'s wait
    /// loop **regardless of the freeze level**.
    ///
    /// Distinct from the stall budget on purpose. The stall budget is
    /// suspended while frozen — deliberately, so a pause is never reported as
    /// a server stall — which means a paused, stalled read waits forever. A
    /// deadline expressed as "remaining time, passed as a stall budget"
    /// inherits that suspension and bounds nothing at all.
    pub fn set_operation_deadline(&self, deadline: Option<Instant>);
}

// src/http/source.rs
impl HttpMediaSource {
    /// Sets the deadline through to the interrupt, for one seek.
    pub fn set_seek_deadline(&self, deadline: Option<Instant>);
}
```

**The routing and timestamp policy (R2), which is one decision applied in four places.** `seek_refined` has four callers, and routing only `seek_to` leaves three scanning:

| Site | Reached by |
|---|---|
| `engine.rs:1910` (`load`) | launch resume at a stored position |
| `engine.rs:2314` (`seek_to`) | the user's own seek |
| `engine.rs:2484` (`reseek`) | stop→play, and device recovery |
| `engine.rs:2633` (`verify_seek_support`) | stopped-seek capability validation |

Introduce **one** routing function they all call, so the decision cannot drift, and give it the deadline. Two consequences to handle explicitly:

- **Timestamp origin.** If Task 1 chose shape A, a re-probed reader's timestamps may be relative to its new origin rather than absolute. Every one of these four sites then needs the same base-offset treatment, or a later absolute seek lands wrong. Task 1 recorded the answer; apply it in one place.
- **The refined fallback is not automatically safe.** An *indexed* MP3 still takes the same `Accurate` path today. Bound it too — the deadline applies to every remote seek, not only estimated ones.

**What the deadline can and cannot promise (§5.3).** Checks in `HttpMediaSource` bound source **I/O**. They do not bound demuxer work over bytes already buffered above them — `MediaSourceStream` holds 64 KiB and frame headers can be parsed out of it without touching the source. Write that limitation in the code, at the deadline. If Task 1 chose shape A, the `FormatReader::seek()` call is gone from this path and the bound is real; say which case you are in.

**Recovery (§5.4).** A failed or expired seek restores the pre-seek position with **one fresh bounded attempt carrying its own deadline** — never the expired one, never a loop. An inherited expired deadline fails recovery instantly, leaving the decoder mid-scan with no way back: bounded, but the position becomes unrecoverable rather than merely late, which is worse than the wedge being fixed. If the fresh attempt also fails, report the seek failed, restore the position logically, and retire the source so the next explicit action reopens cleanly.

- [ ] **Step 1: Write the failing tests**

`tests/estimated_seek.rs`, all against `TestServer` and `TestOutput`:

1. `a_forward_seek_on_a_no_index_mp3_lands_without_rescanning_from_the_first_packet` — seek forward, assert the requested byte is near the estimate and **not** the audio-data offset, and that the server sent far less than the whole prefix.
2. `repeated_seeks_stay_responsive` — five `submit_seek` calls in succession; each is serviced, none rescans, and the engine reaches a landing after each. This is the user's actual report.
3. `a_seek_against_a_stalled_source_fails_within_its_deadline` — `stall_body_after`, prove the wait was entered with `wait_until_stalled`, assert the seek fails inside the deadline and the pre-seek position is restored.
4. `pause_stop_and_quit_are_serviced_during_a_seek` — issue a seek against a slow source, prove it is in flight, then `submit_pause` and assert the state changes promptly rather than after the seek.
5. `an_estimated_landing_reports_estimated_provenance` — and playing on from it keeps reporting `Estimated`, per §3.1.
6. `recovery_after_a_failed_seek_gets_a_fresh_deadline` — the recovery attempt is not refused instantly by an inherited expired deadline.
7. `a_seek_that_stalls_while_paused_still_fails_within_its_deadline` — **R1's regression, and the one the source-level design would have missed.** Pause first, then seek against a stalled source, and prove the read was entered before asserting. The stall budget is suspended while frozen, so only a deadline checked inside the wait loop regardless of freeze can end this; a version that bounds the seek when playing and hangs when paused passes every other test here.
8. `a_seek_on_an_indexed_remote_mp3_is_bounded_too` — the refined fallback path takes the same deadline.
9. `every_seek_entry_point_is_routed` — launch resume, stop→play, device recovery and stopped-seek validation all go through the shared routing rather than calling `seek_refined` directly. Assert on observable behaviour (no rescan from the audio-data offset) rather than on structure.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --test estimated_seek 2>&1 | tail -20`

- [ ] **Step 3: Implement**

Build the estimator from the source's evidence at open (`byte_len`, the audio-data offset the spike identified, and the decoder's duration). Route `seek_to` through `seek_estimated` when `needs_estimated_seek()`, and through the existing refined seek otherwise — the local path must not change. Apply the preroll discard. Set and clear the seek deadline around the operation. Report the landing's provenance from which path ran.

- [ ] **Step 4: Verify, then un-ignore the reproduction**

Run: `cargo test --locked --test engine_remote -- --ignored a_short_forward_seek` — it must now **pass**. Remove the `#[ignore]` and its comment, leaving the corrected root-cause explanation as history.

Run: `cargo test --locked 2>&1 | tail -5` — expected 382 + this task's tests, 0 failed, **1 ignored** (`device_smoke` alone).
Run the new file three times consecutively; report each.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "fix(playback): seek by byte estimate rather than rescanning

Closes the wedge found in M3's manual acceptance."
```

---

## Task 5: Persistence — the estimated location and schema 2

**Files:**
- Modify: `src/persistence/model.rs`, `src/persistence/store.rs`
- Test: `tests/persistence_model.rs`, `tests/persistence_store.rs` (both additive)

**Interfaces:** `PersistedCheckpoint.position: Option<Duration>` (R8), `PersistedCheckpoint.estimated: Option<Duration>`, `SCHEMA_VERSION = 2`.

**Why a version bump and not an optional field (§4.5).** M2's store refuses to write when it reads a *newer* schema, preserving the file — that is what makes a downgrade safe, and it only engages if the version actually changes. A v1 build silently dropping `estimated` on its next write would discard the listener's most recent position with no diagnostic, which is the exact class of loss this work exists to prevent.

**The bump needs a real migration, and the plan's first draft was wrong to say otherwise.** `src/persistence/store.rs:107` rejects **every** unequal version, older included — its own comment says "Not `> SCHEMA_VERSION`: a file from a build that renumbered downward is just as unreadable". So bumping the constant alone marks every existing v1 file `UnsupportedVersion`, and **silently disables persistence for every current user**: they keep their file, and never write to it again.

Required instead:
- **Accept v1 and normalise it to v2 on load.** A v1 entry becomes a v2 entry with `estimated: None` and its `position` as `Some`.
- **Reject genuinely unknown versions** — anything above `SCHEMA_VERSION`, and anything below the oldest migratable one — with the existing `UnsupportedVersion` behaviour intact.
- **Test the whole cycle**, not just the read: load a v1 file → modify → write → reload, and assert the file on disk is v2 and the data survived. Deserialisation preserves the file's version while writing asserts the current one, so a partial migration writes estimates *under schema 1* — a file that claims v1 and contains v2 data, which the next v1 build will read and quietly discard.

**On `position: Option<Duration>` (R8).** §4.2 requires an entry that carries only an estimate, and a mandatory `Duration` cannot express "nothing has ever established one". `Option` can, and the migration is free: a v1 file's present value deserialises straight to `Some`. `None` means exactly what it says, and `decide_resume` must handle it rather than defaulting to zero.

- [ ] **Step 1: Write the failing tests**

Additive, in the existing files' style:

- `a_v1_file_is_accepted_and_normalised_to_v2` — a hand-written v1 JSON loads, `estimated` reads `None`, `position` reads `Some`, every other field survives, and `writable` is **true**. A v1 file that loads unwritable is the regression this task exists to prevent.
- `the_upgrade_cycle_writes_v2_and_survives_a_reload` — load v1 → modify → write → reload; the file on disk claims v2, the data round-trips, and no entry was lost. This is the test that catches a partial migration writing v2 data under a v1 header.
- `a_v2_file_round_trips_its_estimated_location`.
- `an_entry_with_no_established_position_round_trips` — `position: None` with an `estimated` present, which is §4.2's estimate-only entry.
- `a_genuinely_unknown_version_is_still_preserved_unwritten` — both a version above `SCHEMA_VERSION` and one below the oldest migratable, against the existing `LoadReason::UnsupportedVersion`.
- `an_estimated_location_serialises_absent_rather_than_null_when_unset` — so a v2 file with no estimate stays comparable to what M2 wrote.

- [ ] **Step 2–5: Run failing, implement, verify, commit**

Bump `SCHEMA_VERSION`, add the field with `#[serde(skip_serializing_if = "Option::is_none", default)]`, and confirm the store's version handling needs no change beyond the constant — if it does, that is a finding worth reporting.

```bash
git commit -m "feat(persistence): record an estimated location beside the established one"
```

---

## Task 6: Session policy — write rules, restart preference, clearing

**Files:**
- Modify: `src/session.rs`, `src/resume.rs`, `src/playback/event.rs`
- Test: `tests/provenance_policy.rs` (new); `tests/session_policy.rs`, `tests/resume_contract.rs` additive

**The rules, from §4.2–4.4, each of which needs a test that fails without it:**

- Estimated progress never writes `position` when an established checkpoint exists — it writes `estimated` only.
- With no established checkpoint, an estimate persists **only** as `estimated`; it never bootstraps into `position`.
- **`completed` under estimated provenance (R4).** The plan's first draft called completion "established by construction". That is false: `engine.rs:1702` computes the terminal position as `landed_anchor + frames_to_duration(pushed_total, rate)` — the anchor plus decoded frames. If the anchor came from an estimate, so does the terminal position. HTTP verification establishes that the **body** completed, not that the anchor was right, and `EndOfTrack` currently makes `Session` clear protection and write that number.

  So decide and implement, explicitly: an `EndOfTrack` reached from an estimated timeline **must not silently promote its timestamp**. Reaching the end of the body is real evidence the recording finished, so `completed` may legitimately be set — but the *position* written alongside it is still estimated and must go to `estimated`, not `position`. Say in a comment why completion and position are separable here. Test: estimated seek → EOF → inspect the persisted state, asserting `position` was not overwritten by the estimated terminal value.
- Restart prefers `estimated` when present, falling back to `position`, and reports `StartDisposition::ResumedEstimated { established }` so the application can say what it used and what it kept.
- `estimated` clears — and established writes resume — on exactly three things: an establishing seek, an established `Restart`, verified completion. **Not** on a capability change, not on elapsed playback however long, not on a failed seek.

- **`SeekTargetStored` needs its own rule (R5).** The write rules omitted it, and `Session` currently writes that unvalidated target straight through `record_current` *and* substitutes it for the sampled position at shutdown via `position_for`. Define the sequence: estimated seek → stop → a new stored target → quit. The newest intent must survive — it is the listener's most recent expressed wish — **without** overwriting the established checkpoint.

  The compatibility question must be answered explicitly rather than discovered: existing tests in `tests/session_policy.rs` pin the current behaviour for *established* playback, where a stored target writes `position` directly. Decide whether a stored target inherits the provenance of the position it was computed from (the natural reading — a target 10 s past an estimated position is itself estimated), state that decision in a comment, and make sure the established path is bit-for-bit unchanged. If any existing assertion has to move, stop and report it.

- **When there is no established fallback (R8).** `StartDisposition::ResumedEstimated { established: Option<Duration> }` reports `None` for an estimate-only entry, and the application must render that case without implying a fallback exists. `decide_resume` takes both locations and must not treat an absent `position` as zero — "never established" and "established at the start" are different facts, and M2's `ResumeDecision::AtStart` already means the second one.

**Where the gates go.** M3 put checkpoint protection in `record_current` and `record_outgoing` because those are the only two paths that reach the stored state, and one gate would have missed the other. The same reasoning applies here — find both, and say in a comment why one is not enough.

- [ ] **Steps: failing tests, implement, verify, commit**

Model the tests on `tests/session_policy.rs`'s existing shape, driven synchronously with `FakeClock`. Each of the three clearing exits gets its own test, and each must be shown to fail without its clause — say in the report which ablation you ran.

```bash
git commit -m "feat(session): an estimated location never overwrites an established one"
```

---

## Task 7: Acceptance

**Files:** `tests/estimated_seek.rs`, `tests/provenance_policy.rs`, `docs/m3-acceptance.md`

Discharge §6's seven bullets, each pointing at a named test. Several are already written by Tasks 4 and 6 — do not duplicate a test to tick a box; check honestly and write only what is missing. Add the new rows to `docs/m3-acceptance.md` under an M3.1 heading, keeping its existing format, with partial or undischarged rows marked as such.

The cross-process half needs a real `StateStore` in a `tempfile::TempDir`: a session that seeks by estimate, quits, and relaunches must select the estimated location, report what it kept, and leave the established checkpoint intact on disk.

Four rows come from review round 1 and are easy to forget because §6 predates them:

- **Upgrade** (R3): a session started against an existing **v1** file keeps persisting — the file becomes v2, the data survives, and `writable` was never false. This is the one that protects every current user.
- **Paused seek deadline** (R1): a seek that stalls while paused still fails within its deadline.
- **Completion under estimate** (R4): an estimated timeline reaching EOF marks the recording complete without overwriting the established `position`.
- **Estimate-only entry** (R8): a media with no established checkpoint persists an estimate, resumes from it, and reports `established: None`.

```bash
git commit -m "test: the M3.1 acceptance evidence"
```

---

## Task 8: Documentation

**Files:** `docs/architecture.md`, `docs/m1-known-debt.md`, `docs/m3-acceptance.md`, `README.md`

- **`docs/architecture.md`** — §§1 and 4 carry M1's position contract, which this work amends. Say that a position now carries provenance, that an estimated one drives display and resume but never overwrites an established checkpoint, and what the seek deadline does and does not bound.
- **`docs/m1-known-debt.md`** — correct the M3 entry that says `SeekMode::Accurate` is merely a testing limitation; it was a live defect and this is its fix. Add M3.1's own carried debt, including anything Task 1's spike rejected and why.
- **`README.md`** — seeking now works on podcasts; say plainly that a landing on a file with no seek index is approximate.

Documentation only; no source or test file changes. Every claim checked against the code as it stands — the M3 plan was repeatedly wrong about dependency behaviour, and a document repeating a stale claim is worse than one that omits it.

```bash
git commit -m "docs: describe estimated seeking and position provenance"
```

---

## Self-review

**Spec coverage.** §1 defect → Task 1's fixture and Task 4's fix. §3 provenance → Task 3, with §3.1 stickiness pinned in Tasks 3 and 4. §4.1–4.2 write rules → Tasks 5 and 6. §4.3 restart preference → Task 6. §4.4 clearing → Task 6, one test per exit. §4.5 schema → Task 5. §5.1 estimator → Task 2. §5.2 → Task 1, by experiment. §5.3 bounding and its limits → Task 4. §5.4 recovery → Task 4. §6 acceptance → Task 7. §7's review boundary is settled: §4.3 and §4.5 are decided in this plan, §5.2 is Task 1's deliverable.

**Placeholder scan.** Tasks 5, 6 and 7 give test *names and obligations* rather than full bodies, because each mirrors an existing file's established shape and the naming carries the requirement. Tasks 2 and 3 give complete test code. Task 1 deliberately specifies an experiment rather than an implementation — that is its nature, and it is the one task allowed to end in "neither shape works".

**Type consistency.** `SeekEstimator`, `resync`, `RESERVOIR_PREROLL_FRAMES` (Task 2) → Task 4. `PositionProvenance` (Task 3) → Tasks 4, 6, 8. `seek_estimated`, `needs_estimated_seek`, `set_seek_deadline` (Task 4) → Task 7. `PersistedCheckpoint.estimated`, `SCHEMA_VERSION` (Task 5) → Task 6. `StartDisposition::ResumedEstimated` (Task 6) → Tasks 7, 8. Each is defined before first use.

**Corrections made while writing and reviewing this plan.** Four claims in earlier drafts were false about code the plan does not own, and each would have propagated:

- `PositionQuality` "gains a third state" — it already has `Estimated`, meaning how precisely we know what has been *heard*. Provenance is now a separate axis, which is both correct and a better design.
- "`Coarse` refuses without a Xing header" — inherited from M3's known-debt entry. `symphonia-bundle-mp3-0.6.1/src/demuxer.rs:465-471` estimates `num_frames` from bitrate and byte length instead, so `Coarse` may well work. That entry needs correcting in Task 8, and the spike now evaluates `Coarse` first.
- "The store needs no change beyond the constant" — `store.rs:107` rejects every unequal version including older ones, so the bump alone would have disabled persistence for every existing user.
- "Completion is established by construction" — the terminal position is anchor plus decoded frames, so an estimated anchor yields an estimated completion.

The pattern is the same each time: a claim about someone else's code, asserted rather than checked. Verify before building on it.

**A note on Task 1's leverage.** If the spike finds `Coarse` works, Tasks 2 and 4 shrink to routing and bounding, and `SeekEstimator` may not be needed at all. The plan is deliberately ordered so that outcome deletes work rather than invalidating it — do not start Task 2 before Task 1 reports.
