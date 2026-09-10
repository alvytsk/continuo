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

| `src/playback/provenance.rs` | `PositionProvenance` and its composition rules |

| `tests/estimated_seek.rs` | End-to-end estimated seeking over the loopback server |
| `tests/provenance_policy.rs` | Session-policy coverage for §4's write rules, restart preference and clearing |
| `tests/fixtures/sine-long-noxing.mp3` | A no-index MP3 long enough that a rescan is measurable |

**Modified**

| File | Change |
|---|---|
| `src/playback/decode.rs` | The `SeekMode` decision (one line, `:290`). No estimator, no resync, no preroll — `seek_refined`'s existing loop covers the reservoir |
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

## Task 2: *Withdrawn by the Task 1 spike*

**Status: do not implement. Nothing in this task is needed, and building it
would duplicate machinery symphonia already has.**

This task specified a `SeekEstimator` — a `byte_for`/`time_for` pair turning
media time into a byte offset. The spike established that
`MpaReader::preseek_coarse` performs exactly this arithmetic internally
(`(required_ts / total_dur) × audio_byte_len`), then walks forward
frame-by-frame to a real frame boundary, which a hand-rolled estimator would
also have had to do. Reimplementing it would have bought a second copy of the
same formula with the same accuracy characteristics and an additional resync
adapter to maintain.

The whole of this task collapses into **one line** — `SeekMode::Accurate` →
`SeekMode::Coarse` at `src/playback/decode.rs:290` — which now lives in
Task 4.

**The numbering is deliberately not compacted.** Tasks 3 and 5-8 are
unchanged and are referred to by number in the spec, the ledger, and the
review round tables. Renumbering to close a gap would invalidate every one of
those references to save nothing. **This plan has seven live tasks: 1, 3, 4,
5, 6, 7, 8.**

**One thing this task got right, and it moves to Task 4.** The estimator's
doc comment said the byte offset "is an *estimate*, and every caller is
expected to treat its landing as such". The spike proved that far more
strongly than this task assumed — see §5.2's measurements, where a landing
can be 235 s from its target — so the requirement survives its task and is
carried into Task 4's provenance handling.

---

## Task 3: Position provenance

**Files:**
- Create: `src/playback/provenance.rs`
- Modify: `src/playback/mod.rs`, `src/playback/event.rs`, `src/playback/wait.rs`, `src/playback/engine.rs`, `src/app.rs`
- Test: `tests/provenance.rs` (new); mechanical updates wherever `Progress` is constructed

**Interfaces:** `PositionProvenance { Established, Estimated }`, exactly as §3 defines it, plus `Progress.provenance`, `SeekCompleted.provenance`, **and the same axis on `MediaMetadata::duration` (§5.5)**.

**Duration carries provenance too, and it is the more dangerous of the two.** The spike found that `estimate_num_mpeg_frames` samples only the first ~16 frames: on a VBR file that produced an estimated duration of 361 s for a true 600 s — 40 % short. Exact for CBR, which is why nothing has noticed. Two verified paths turn that into silent loss:

- `src/resume.rs:85` returns `StalePastEnd` when `position > duration`, and `start_at()` maps it to zero. A listener 70 % through such a file **resumes at the beginning**, their checkpoint discarded as stale.
- `clamp_target` clamps seeks to `metadata().duration`, so the tail is unreachable and a seek into it lands silently at the estimated ceiling.

So `MediaMetadata::duration` records whether it was derived or observed, and §5.5's rule applies: **an estimated duration may inform display and must never drive a destructive decision.** `decide_resume` treats an estimated duration exactly as it treats an absent one — `ResumeDecision::Unvalidated`, which M2 already implements and which retains the stored position. `StalePastEnd` requires an *established* duration. `clamp_target` does not clamp to an estimated one.

**Two boundaries on this, both binding.** First, provenance is scoped to **position and duration only** — those are the two quantities with demonstrated destructive consequences. Do not generalise it to other derived values; that is a framework ahead of its second use case.

Second, **removing the clamp does not make the tail seekable.** Symphonia's own `max_ts` check still refuses a target past its estimated ceiling (`OutOfRange`, identically under `Coarse` and `Accurate`). The change converts a silent mislanding into a visible refusal, nothing more, and the tail of an under-estimated VBR file stays unreachable — a retained limitation to record, not a problem to solve here. The case that must be tested is a **launch resume** to a position past that ceiling: it fails, and the checkpoint must survive untouched.

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

Add to `tests/resume_decision.rs`, additively — these two are the ones that protect a real listener from losing a real position:

```rust
#[test]
fn a_checkpoint_past_an_estimated_duration_is_retained_rather_than_declared_stale() {
    // The spike measured a 361 s estimate for a 600 s VBR file. Under the old
    // rule a listener 70 % in resumes at zero and their entry is discarded as
    // stale — silent loss, from a number nothing ever measured.
    let candidate = ResumeCandidate { position: Duration::from_secs(420), completed: false };
    assert_eq!(
        decide_resume(Some(candidate), estimated(Duration::from_secs(361))),
        ResumeDecision::Unvalidated(Duration::from_secs(420))
    );
}

#[test]
fn a_checkpoint_past_an_established_duration_is_still_stale() {
    // The M2 rule is unchanged where the duration was actually observed:
    // a position past a known end really is a file that changed underneath us.
    let candidate = ResumeCandidate { position: Duration::from_secs(420), completed: false };
    assert_eq!(
        decide_resume(Some(candidate), established(Duration::from_secs(361))),
        ResumeDecision::StalePastEnd
    );
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

## Task 4: The bounded Coarse seek

**Files:**
- Modify: `src/playback/decode.rs`, `src/http/channel.rs`, `src/http/source.rs`, `src/playback/engine.rs`
- Test: `tests/estimated_seek.rs` (new)

**Interfaces:**

```rust
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

**The core change is one line.** At `src/playback/decode.rs:290`, inside
`seek_refined`, `SeekMode::Accurate` becomes `SeekMode::Coarse`. That is the
entire fix for the wedge. Everything else in this task exists to bound and
label it honestly.

Three consequences the spike settled, which mean this stays a one-line change
rather than growing:

- **Reservoir priming is already handled.** `Coarse` is *not* reservoir-safe
  on its own — the spike measured real corruption in the first three frames
  after a landing (max sample error 0.119 CBR / 1.581 VBR, then bit-exact
  from frame 3, consistent with `MAX_REF_FRAMES = 4`). But `seek_refined`'s
  existing loop at `decode.rs:309` already decodes forward from `actual_ts`
  to the target and discards as it goes, and it does not know or care which
  preseek mode produced its starting frame. **Write no new reservoir code.**
- **Timestamp origin is a non-issue.** `Coarse` seeks the *original* reader,
  whose `next_packet_ts` has counted from the true start since it was opened,
  so `actual_ts` is already absolute. The base-offset treatment the withdrawn
  Task 2 would have needed does not arise.
- **The local path changes too, and that is intended.** There is one
  `SeekMode` call site, shared by local and remote sources. `Coarse` on a
  local file trades a rescan for byte arithmetic there as well. Local seeks
  are not the reported bug, so treat any local-fixture regression as a signal
  to stop and reconsider, not to special-case the mode by transport.

**The routing and deadline policy (R2), one decision applied in four places.**
`seek_refined` has four callers, and bounding only `seek_to` leaves three
unbounded:

| Site | Reached by |
|---|---|
| `engine.rs:1910` (`load`) | launch resume at a stored position |
| `engine.rs:2314` (`seek_to`) | the user's own seek |
| `engine.rs:2484` (`reseek`) | stop→play, and device recovery |
| `engine.rs:2633` (`verify_seek_support`) | stopped-seek capability validation |

Introduce **one** routing function they all call, so the decision cannot
drift, and give it the deadline. The deadline applies to every remote seek —
an indexed MP3 takes the same path and needs the same bound.

**What the deadline can and cannot promise (§5.3).** Checks in
`HttpMediaSource` bound source **I/O**. They do not bound demuxer work over
bytes already buffered above them — `MediaSourceStream` holds 64 KiB and
frame headers can be parsed out of it without touching the source. Write that
limitation in the code, at the deadline. `Coarse` shrinks the exposure by
roughly 320× (3,072 B consumed against 983,040 B, measured) but does not
remove it: the `FormatReader::seek()` call is still on this path.

**Recovery (§5.4).** A failed or expired seek restores the pre-seek position
with **one fresh bounded attempt carrying its own deadline** — never the
expired one, never a loop. An inherited expired deadline fails recovery
instantly, leaving the decoder mid-scan with no way back: bounded, but the
position becomes unrecoverable rather than merely late, which is worse than
the wedge being fixed. If the fresh attempt also fails, report the seek
failed, restore the position logically, and retire the source so the next
explicit action reopens cleanly.

**Provenance, and why it is unconditional (§5.2).** Every `Coarse` landing
reports `PositionProvenance::Estimated` — never conditionally, never
"established because this file looked like CBR". Nothing observable at seek
time distinguishes a file where the arithmetic is exact from one where it
lands 235 s away, and the spike's measurements are the evidence that this is
a real span rather than a rounding concern.

- [ ] **Step 1: Write the failing tests**

`tests/estimated_seek.rs`, all against `TestServer` and `TestOutput`:

1. `a_forward_seek_on_a_no_index_mp3_lands_without_rescanning_from_the_first_packet` — seek forward, assert the requested byte is near the arithmetic estimate and **not** `first_packet_pos`, and that the demuxer consumed far less than the whole prefix.
2. `repeated_seeks_stay_responsive` — five `submit_seek` calls in succession; each is serviced, none rescans, and the engine reaches a landing after each. This is the user's actual report.
3. `a_seek_against_a_stalled_source_fails_within_its_deadline` — `stall_body_after`, prove the wait was entered with `wait_until_stalled`, assert the seek fails inside the deadline and the pre-seek position is restored.
4. `pause_stop_and_quit_are_serviced_during_a_seek` — issue a seek against a slow source, prove it is in flight, then `submit_pause` and assert the state changes promptly rather than after the seek.
5. `an_estimated_landing_reports_estimated_provenance` — and playing on from it keeps reporting `Estimated`, per §3.1.
6. `recovery_after_a_failed_seek_gets_a_fresh_deadline` — the recovery attempt is not refused instantly by an inherited expired deadline.
7. `a_seek_that_stalls_while_paused_still_fails_within_its_deadline` — **R1's regression.** Pause first, then seek against a stalled source, and prove the read was entered before asserting. The stall budget is suspended while frozen, so only a deadline checked inside the wait loop regardless of freeze can end this; a version that bounds the seek when playing and hangs when paused passes every other test here.
8. `a_seek_on_an_indexed_remote_mp3_is_bounded_too` — the same deadline applies.
9. `every_seek_entry_point_is_routed` — launch resume, stop→play, device recovery and stopped-seek validation all go through the shared routing rather than calling `seek_refined` directly. Assert on observable behaviour (no rescan from `first_packet_pos`) rather than on structure.
10. `a_launch_resume_past_an_estimated_ceiling_preserves_the_checkpoint` — **the retained-limitation test.** Against `sine-long-vbr-noxing.mp3`, whose duration symphonia estimates at ~361 s against a true 600 s, resume at a stored position of 400 s (real audio exists there). Symphonia's own `max_ts` check refuses it with `SeekErrorKind::OutOfRange` — mode-independently, since the check at `demuxer.rs:267-271` precedes the mode dispatch at `:291-295` — so the seek *must* fail. Assert that it fails, and that **the stored checkpoint still reads 400 s afterwards**: a position we could not reach is not a position we may discard. This limitation is retained deliberately (§5.5); the test pins the behaviour so a later change cannot quietly turn a refusal into a reset.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --test estimated_seek 2>&1 | tail -20`

- [ ] **Step 3: Implement**

Swap the `SeekMode` at `decode.rs:290`. Introduce the shared routing function and move all four callers onto it. Add `set_operation_deadline` to `SourceInterrupt`, checked inside `read`'s wait loop regardless of freeze level, and `set_seek_deadline` on `HttpMediaSource` to reach it. Set and clear the deadline around the operation. Report every landing from this path as `Estimated`. Add no estimator, no resync adapter, and no reservoir bookkeeping.

- [ ] **Step 4: Verify, then un-ignore the reproduction**

Run: `cargo test --locked --test engine_remote -- --ignored a_short_forward_seek` — it must now **pass**. Remove the `#[ignore]` and its comment, leaving the corrected root-cause explanation as history.

Run: `cargo test --locked 2>&1 | tail -5` — expected 382 + this task's tests, 0 failed, **1 ignored** (`device_smoke` alone).
Run the new file three times consecutively; report each.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "fix(playback): seek Coarse rather than rescanning from the first packet

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

Bump `SCHEMA_VERSION` to 2, change `position` to `Option<Duration>`, and add `estimated` with `#[serde(skip_serializing_if = "Option::is_none", default)]`.

**The store's version handling must change, and that is the substance of this task.** `store.rs:107` currently rejects every unequal version, older included. Replace that equality check with: accept `SCHEMA_VERSION`, accept and **normalise** v1, reject everything else through the existing `UnsupportedVersion` path. Normalising means a v1 entry loads as a v2 entry — `position` as `Some`, `estimated` as `None` — and the *next write emits v2*. A migration that reads v1 but writes without updating the envelope produces a file claiming v1 that holds v2 data, which the next v1 build reads and quietly discards.

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
- `estimated` clears — and established writes resume — on exactly two things: an **establishing seek**, and an **established `Restart`**. Both re-establish the absolute position by decoder confirmation, which is the only thing that earns the right to overwrite an established checkpoint.

  **Verified completion is deliberately not a third exit**, and this supersedes the earlier draft that listed it. Per the rule immediately above, an `EndOfTrack` reached from an estimated timeline sets `completed` but its terminal position is still `anchor + decoded frames` over an estimated anchor. Completion establishes that the *body* finished, not that the anchor was right — so an estimated completion **retains estimated provenance and retains checkpoint protection**. A completion reached from an *established* timeline clears as it always did, because there is nothing estimated to retain.

  Not cleared by: a capability change, elapsed playback however long, a failed seek, or an estimated completion.

- **`SeekTargetStored` needs its own rule (R5).** The write rules omitted it, and `Session` currently writes that unvalidated target straight through `record_current` *and* substitutes it for the sampled position at shutdown via `position_for`. Define the sequence: estimated seek → stop → a new stored target → quit. The newest intent must survive — it is the listener's most recent expressed wish — **without** overwriting the established checkpoint.

  **The decision is made, and this is the requirement, not a question to reopen: a stored target inherits the provenance of the position it was computed from.** A target ten seconds past an estimated position is itself estimated, and is written to `estimated`; a target computed from an established position is established, and writes `position` exactly as it does today. Implement that, and say why in a comment — a stored seek target is arithmetic on a position, and arithmetic cannot make an unconfirmed number confirmed.

  The established path must be **bit-for-bit unchanged**: `tests/session_policy.rs` pins it, including `position_for`'s substitution of a stored target for the sampled position at shutdown. If any existing assertion has to move, stop and report it rather than editing it — that would mean this rule changed established behaviour, which it must not.

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

**Type consistency.** `PositionProvenance` (Task 3) → Tasks 4, 6, 8. `set_operation_deadline`, `set_seek_deadline` (Task 4) → Task 7. Task 2's `SeekEstimator`, `resync` and `RESERVOIR_PREROLL_FRAMES` are withdrawn and referenced by nothing. `PersistedCheckpoint.estimated`, `SCHEMA_VERSION` (Task 5) → Task 6. `StartDisposition::ResumedEstimated` (Task 6) → Tasks 7, 8. Each is defined before first use.

**Corrections made while writing and reviewing this plan.** Four claims in earlier drafts were false about code the plan does not own, and each would have propagated:

- `PositionQuality` "gains a third state" — it already has `Estimated`, meaning how precisely we know what has been *heard*. Provenance is now a separate axis, which is both correct and a better design.
- "`Coarse` refuses without a Xing header" — inherited from M3's known-debt entry. `symphonia-bundle-mp3-0.6.1/src/demuxer.rs:465-471` estimates `num_frames` from bitrate and byte length instead, so `Coarse` may well work. That entry needs correcting in Task 8, and the spike now evaluates `Coarse` first.
- "The store needs no change beyond the constant" — `store.rs:107` rejects every unequal version including older ones, so the bump alone would have disabled persistence for every existing user.
- "Completion is established by construction" — the terminal position is anchor plus decoded frames, so an estimated anchor yields an estimated completion.

The pattern is the same each time: a claim about someone else's code, asserted rather than checked. Verify before building on it.

**Task 1's leverage, realised.** The spike found `Coarse` works, so Task 2 is withdrawn entirely and Task 4 is one line plus routing and bounding. The plan was ordered so that outcome would delete work rather than invalidate it, and it did: seven live tasks remain (1, 3, 4, 5, 6, 7, 8), numbering left uncompacted so existing references stay valid.

**What the spike cost, and what it bought.** It ran three rounds, and the first two produced wrong answers that survived until challenged — a CBR-only accuracy claim, then a circular VBR one that compared two self-reported timestamps both constructed to converge on the same target. The third round measured against an independent byte-offset → cumulative-frame-time map and found `Coarse`'s VBR error reaching 235 s on a 600 s file. That number did not change the decision, because the cost argument (3,072 B against 983,040 B) never depended on it and the alternatives share the same vulnerability — but it did change what the design is allowed to claim, which is the point of gating on a spike.
