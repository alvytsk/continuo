# Continuo — Estimated Seeking and Position Provenance (M3.1)

Date: 2026-09-10.
Status: draft for review; not approved for implementation.
Amends `2026-09-09-continuo-finite-http-design.md` §§4, 6, 8–10 and the M1
position contract published in `docs/architecture.md` §§1, 4.
Occasioned by a defect found in M3's manual acceptance.

## 1. The defect this amends

`continuo play <2h15m podcast>`, two presses of `→`, and playback wedges: the
status line freezes, position quality degrades, and pause/resume do nothing.
Only `s` recovers it. Reproduced deterministically; the reproduction is
`tests/engine_remote.rs::a_short_forward_seek_on_a_no_index_mp3_rescans_the_whole_file_instead_of_landing_quickly`,
currently `#[ignore]`d pending this work.

The mechanism, verified against `symphonia-bundle-mp3-0.6.1/src/demuxer.rs`:

```rust
fn preseek_accurate(&mut self, required_ts: Timestamp, min_ts: Timestamp) -> Result<()> {
    if required_ts < self.next_packet_ts {
        let seeked_pos = self.reader.seek(SeekFrom::Start(self.first_packet_pos))?;
        ...
```

The rewind is **conditional**, and the condition compares against
`next_packet_ts` — the *demuxer's* read position, which runs ahead of audible
playback by the PCM ring plus `MediaSourceStream`'s read-ahead. So a seek that
is forward in audible terms is backward in demuxer terms, and rewinds to the
first packet. Audible 48 s, demuxer past 58 s, user asks for 58 s: rewind, then
rescan forward from byte 37849. The second press finds playback un-advanced and
does it again — which is exactly the captured log, two range requests at the
same byte, 160 ms apart, then silence.

The scan parses frame headers and skips bodies; it does not decode. Cheap on a
local file, expensive over HTTP, because the bytes are still read sequentially
through the byte channel. None of this depends on a Xing/VBRI header — the
`Accurate` branch behaves this way regardless.

**The defect is not the rescan. It is that the rescan is unbounded and
uninterruptible.** It happens inside one `FormatReader::seek()` call. M3's
`SEEK_BUDGET` does not bound it: that budget limits decoded media frames
*after* `reader.seek()` returns, so it is not a timeout and never applies.
While the worker is parked there it dispatches no commands, which is why
pause and resume appear dead and why position captures degrade.

`SeekMode::Coarse` is not the escape hatch M3's known-debt entry implied. It
refuses outright when the track has no `num_frames`, trading a hang for
"seeking unavailable" on the same files.

## 2. What this amendment decides

Three things, in dependency order: what an estimated position *is*, what
persistence does with one, and how a seek is bounded.

## 3. Position provenance

M1's contract says position "advances from estimated playback of media
frames" and that seeks *establish* a new one. That wording assumed every
landing was decoder-confirmed. A byte-offset seek is not.

**This is a second axis, not a fourth `PositionQuality`.** `PositionQuality`
already has an `Estimated` state and it means something else: how precisely we
know how much has been *heard*, reconstructed from the output callback's
spans. It is the ordinary state during playback (`Timeline::quality` returns
it whenever the timing base has not jumped), and `Exact` is what a stopped or
captured position reports. Overloading it would make "we are unsure how far
this has played" and "we are unsure what media time this even is"
indistinguishable, when a byte-seek landing that is then playing is both at
once for unrelated reasons.

So provenance is carried *alongside* quality:

```rust
/// Whether the absolute media time is trustworthy, independent of how
/// precisely we know what has been played (`PositionQuality`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositionProvenance {
    /// The decoder established this media time.
    Established,
    /// Derived from a byte-offset estimate; the true media time may differ.
    Estimated,
}
```

| | `Established` | `Estimated` |
|---|---|---|
| `Exact` | stopped at a confirmed position | stopped after a byte-seek landing |
| `Estimated` | playing from a confirmed landing | playing from a byte-seek landing |
| `Degraded` | timing base jumped | both, for unrelated reasons |

Four consequences, and they are the substance of this section:

1. **Estimation is sticky.** Decoding forward from an estimated landing
   advances an *estimated* timeline. Playing on does not make the absolute
   position exact, and no amount of elapsed playback converts one into the
   other. Only independently establishing the absolute position does.
2. **Provenance and quality compose and are never merged.** A device fault
   under a byte-seek landing is `Degraded` *and* `Estimated` provenance. The
   status line may render one summary, but `Progress` carries both fields and
   the session policy reads provenance, never quality — a `Degraded` position
   whose media time was established is still established.
3. **A seek that lands by estimate reports `SeekCompleted` with an estimated
   actual.** It is a real landing — playback continues correctly from it —
   but it is not a measurement.
4. **No accuracy figure is promised.** The design deliberately does not claim
   "±n seconds": for arbitrary VBR the error is unbounded in principle, and a
   number we cannot honour is worse than an honest flag.

## 4. Persistence

The binding rule, and the baseline for everything below:

> An estimated position may drive display and resume. It must never replace an
> established checkpoint for the same media.

### 4.1 Two locations, not one

`PersistedCheckpoint` keeps its existing `position` field with its existing
meaning — an **established** position, decoder-confirmed. An estimated
location is stored *separately*, with explicit provenance, never written into
the established field:

```rust
pub struct PersistedCheckpoint {
    /// The established position — decoder-confirmed, unchanged in meaning
    /// from M2. `None` when nothing has ever established one for this media:
    /// an entry that exists only to carry `estimated`. Distinct from
    /// `Some(ZERO)`, which means established at the start; `decide_resume`
    /// must not conflate them, since M2's `ResumeDecision::AtStart` already
    /// means the second.
    pub position: Option<Duration>,
    pub completed: bool,
    pub touch_seq: u64,
    pub updated_at: OffsetDateTime,
    /// Where an estimated seek left the listener, when one did. Never a
    /// substitute for `position`: it records a location the engine believes
    /// but has not confirmed.
    pub estimated: Option<Duration>,
}
```

A v1 file's present `position` deserialises straight to `Some`, so the field
change costs the migration nothing.

### 4.2 The write rules

- With an established checkpoint present, estimated progress **never** writes
  `position`. It writes `estimated` only.
- With **no** established checkpoint for the media, an estimate may be
  persisted — but only in the `estimated` representation. It never
  bootstraps itself into `position`.
- Established-checkpoint writes resume only once the absolute position is
  independently established again (§4.4).
- **An estimated timeline *can* complete a track, but cannot promote its
  timestamp.** An earlier draft claimed completion was "established by
  construction". It is not: the engine computes the terminal position as the
  anchor plus decoded frames, so an estimated anchor yields an estimated
  terminal position. What a verified HTTP completion establishes is that the
  *body* finished — not that the anchor was right. So `completed` may be set
  from an estimated timeline, while the position written beside it goes to
  `estimated` and never to `position`. Completion and position are separable
  facts here, and conflating them is how an unconfirmed number would inherit a
  confirmed one's authority.

### 4.3 Which location a restart selects

Restart prefers **`estimated` when present**, falling back to `position`.
That is the listener's most recent expressed intent, and discarding it in
favour of an older established point would silently undo their last seek —
the failure this ruling exists to prevent. The established point is retained
as the fallback throughout, exactly as M3's range-less protection retains it.

The resume reports `StartDisposition::ResumedEstimated { established: Option<Duration> }`
so the application can say which location it used and what it kept —
`None` for an estimate-only entry, which must be rendered without implying a
fallback that does not exist.

### 4.4 When the preference clears

`estimated` is discarded — and established writes resume — on either of:

- a **successful establishing seek**: one whose landing the decoder confirmed;
- a **successfully established Restart** (M3's `RestartEstablished`).

**Verified completion is not a third exit**, though M3's range-less protection
uses it as one. The two cases differ: there, a completion is reached along a
timeline whose positions were decoder-established throughout, so nothing
unconfirmed survives it. Here the terminal position is the estimated anchor
plus decoded frames, so a completion reached from an estimated timeline is
itself estimated — it sets `completed`, and retains both estimated provenance
and checkpoint protection. A completion reached from an *established* timeline
clears as it always did, because there is nothing estimated to retain.

`estimated` is *not* cleared by a capability change, by ordinary playback
however long, by a failed seek, or by an estimated completion. The principle is
unchanged and is what the exits are derived from: only an act that
re-establishes the absolute position earns the right to overwrite an
established checkpoint.

### 4.5 Schema

`estimated` is a new field, so `schema_version` moves to **2**.

The alternative — an optional field that a v1 reader ignores — is rejected.
M2's store refuses to write when it reads a *newer* schema, preserving the
file; that behaviour is what makes a downgrade safe, and it only works if the
version actually changes. A v1 build silently dropping `estimated` on its next
write would discard the listener's most recent position with no diagnostic,
which is precisely the class of loss this amendment exists to prevent.

A v2 reader accepts a v1 file (absent `estimated` reads as `None`). A v1
build reading a v2 file preserves it and declines to write, which is M2's
existing, tested behaviour.

## 5. Seeking

### 5.1 The approach

For a byte-seekable source whose demuxer cannot seek in media time cheaply,
compute the byte offset directly and seek there, rather than asking the
demuxer to scan. The estimate must account for:

- the **audio-data offset** — the first frame's position, past any ID3v2 tag
  (37849 bytes in the captured case, which is why every rescan started there);
- **frame resynchronisation** — an arbitrary byte offset is mid-frame, so the
  next frame header must be found before decoding;
- **reservoir preroll** — MP3's bit reservoir means frames immediately after
  an arbitrary offset can reference bits that were never read. Those frames
  decode to garbage and must be discarded, not played.

### 5.2 The open implementation question

Doing our own byte seek leaves the `FormatReader`'s `next_packet_ts` stale,
and symphonia exposes no way to reset it. Two shapes are possible and the
plan must settle which, with evidence rather than assertion:

- **Re-probe the reader at the new byte offset.** Legitimate for MP3 from any
  frame boundary, and it removes the `FormatReader::seek()` call from this
  path entirely rather than bounding it — which is the stronger answer to
  §5.3. Costs a fresh probe per seek.
- **Byte-seek beneath a still-live reader** and reconcile its timestamp state.
  Cheaper, but depends on demuxer internals the API does not promise.

### 5.2 Resolution

Settled by spike (Task 1 of the implementation plan), against two fixtures
served through `HttpMediaSource`/`TestServer` on loopback:
`tests/fixtures/sine-long-noxing.mp3` (600 s, mono, 128 kbps CBR, no
Xing/VBRI) for the baseline/cost comparison, and
`tests/fixtures/sine-long-vbr-noxing.mp3` (600 s, mono, genuinely variable
bitrate — 300 s of white noise then 300 s of digital silence, no Xing/VBRI)
for the accuracy measurement. Both are generated per
`tests/fixtures/README.md`. The throwaway experiments are not committed; the
measurements below are what they produced.

This section went through two corrections, both from the same review, and
both are recorded because the corrections are as load-bearing as the final
numbers:

1. A CBR fixture cannot measure `Coarse`'s accuracy in general, only the
   case where its underlying byte-rate arithmetic happens to be exact — the
   VBR fixture above was added to fix this.
2. The first VBR measurement was **circular**: it compared `Coarse`'s own
   self-reported `actual_ts` against another self-reported `actual_ts`
   (a fresh reader's `Accurate` seek). `MpaReader::seek`'s Step 2 loop stops
   once its own — possibly mis-anchored — `next_packet_ts` counter reaches
   within one frame of `required_ts`, *by construction*, regardless of
   whether the underlying byte position is anywhere near the true target.
   Two numbers both manufactured to converge on the same target will agree
   with each other independent of whether they agree with reality; the
   near-zero error that measurement reported was the tell, not the answer.
   The real measurement — described below — parses every real MPEG frame
   header directly from the fixture's own bytes to build an
   independent byte-offset → true-cumulative-time map, locates which byte
   `Coarse` actually landed on without asking symphonia anything about its
   own position, and compares *that* against the target. The numbers below
   are from that corrected measurement; the circular ones are not repeated
   here because they were never evidence of anything.

**Decision: neither A nor B. Use `SeekMode::Coarse`, unmodified.** The known-
debt claim that Coarse refuses without a Xing header is false, verified both
by reading `symphonia-bundle-mp3-0.6.1/src/demuxer.rs:465-471` and by
exercising it: with no Xing/VBRI tag and a byte-seekable source,
`estimate_num_mpeg_frames` still populates `track.num_frames` from bitrate
arithmetic, `preseek_coarse`'s `is_seekable` and `max_ts` guards are
satisfied, and the seek lands. Coarse collapses most of the rest of §5 —
no re-probe, no hand-rolled resync adapter, no *new* reservoir bookkeeping
(the existing decode-and-discard refinement already handles it, see below)
— because it reuses machinery `MpaReader::seek` already has for both modes.
It does **not** collapse §5.1's accuracy premise, which the measurements
below correct rather than confirm.

**The baseline trap, defeated.** The demuxer's `next_packet_ts` was advanced
to 120.007 s by pulling packets (not by decoding audio — encoded read-ahead
alone does not move it, per the trap this section warns about), then a seek
to 60 s was issued: `target_ts=2,646,000 < next_packet_ts=5,292,288`, the
exact condition `preseek_accurate` checks. Only a run that demonstrates this
inequality has exercised the bug; this one does.

**Measurements** (same advance-then-seek scenario, all three seek attempts
targeting 60 s; "demuxer consumed" is `HttpMediaSource`'s own byte counter —
the precise, implementation-level cost — because "server wrote" turned out
to be a noisy proxy: on loopback, a Range request with no upper bound lets
the connection thread race ahead of what the demuxer actually reads, bounded
only by kernel socket buffering and this crate's 1 MiB channel capacity, not
by logical necessity. Both are reported; only "consumed" is load-bearing):

| Shape | New requests (offset) | Server wrote | Demuxer consumed | Wall clock | Self-reported landing |
|---|---|---|---|---|---|
| **Baseline** (`Accurate`, shipped today) | 1, at byte 44 (`first_packet_pos`) | 3.7–5.4 MB (noisy) | **983,040 B** | ~8–9 ms | ts=2,642,688 (59.925 s) |
| Ground truth (fresh reader, first-ever `Accurate` seek — no rewind, honest forward scan from true start) | — | 3.1–5.5 MB (noisy) | **975,872 B** | ~7–8 ms | ts=2,642,688 (59.925 s) |
| **C — `Coarse`** | 1, at byte 957,162 (the arithmetic estimate) | 0.3–1.4 MB (noisy) | **3,072 B** | <1 ms | ts=2,642,688 (59.925 s) |
| A — re-probe at offset (target 300 s; corrected methodology, see below) | 1, at the estimated byte exactly | 0.6–2.0 MB (noisy) | n/a (bypasses `HttpMediaSource`'s read path differently — see note) | <1 ms | pts=0, relative (see timestamp-origin) |
| B — byte-seek beneath a live reader | — | — | — | — | not implementable (see below) |

"Self-reported landing" is exactly what it says — what the seek call itself
returns. On this CBR fixture it happens to also be the *true* landing (see
below), which is what makes the cost comparison on this row trustworthy; it
is not, in general, safe to read a self-reported `actual_ts` as ground
truth, and the rest of this section exists because an earlier draft did
exactly that.

Coarse's request always starts at its arithmetic estimate, never at
`first_packet_pos`; on this **CBR** fixture its landing matches the
from-true-start ground truth exactly (identical `actual_ts`). That exactness
is a property of the fixture, not a general guarantee, and must not be read
as one — see the VBR requalification immediately below, which is the
measurement that actually bounds the claim. What does generalise: after
`preseek_coarse` jumps near the target, `MpaReader::seek`'s Step 2 loop
(shared by both modes) walks forward frame-by-frame toward the exact
target regardless of seek mode — Coarse only changes *where that walk
starts*, not whether it happens — and that walk is what makes Coarse's
*cost* independent of how far into the file the target is: bounded by the
estimate's own error (here, ~1.6 KB before the walk finds sync, well under
`MAX_MPEG_FRAME_SIZE`), while the baseline's cost is
`O(target − first_packet_pos)`. The cost comparison (3,072 B vs. 983,040 B)
does not depend on CBR and is decisive regardless of what the accuracy
measurement below shows.

**How the true landing was measured, non-circularly.** Every real MPEG
frame header was parsed directly from each fixture's own bytes — the same
per-frame arithmetic `estimate_num_mpeg_frames` and `preseek_coarse` use
internally, just applied to every frame instead of a 16-frame sample and
extrapolated — building a byte-offset → cumulative-sample-count map that
depends on nothing symphonia reports. Both fixtures parse cleanly to their
exact end byte with zero desyncs, which is what makes the map trustworthy.
Locating *which* real frame a `Coarse` seek actually landed on could not
use content matching: this VBR fixture has a long stretch of literal
digital silence where every frame is byte-identical to every other one in
that stretch (constant input encodes to constant output — no amount of
context disambiguates it), and the CBR fixture's pure sine tone turns out
to be exactly periodic at the byte level too (1,152 samples/frame and a
2,205-sample sine period share a period of 245 frames = 6.4 s). Instead,
`HttpMediaSource::consumed()` — the exact byte count it has read since
opening, already used for cost above — sampled immediately before and after
the seek gives the number of bytes the seek's own resync-and-walk consumed,
independent of content; combined with the seek's own Range request's start
byte, that locates the landing directly. (`MediaSourceStream`'s ring buffer
has a hard minimum of 64 KiB, but its *adaptive read block size* resets to
1 KiB on every seek and only doubles from there, so the overshoot this
introduces stays within a fraction of a second even at 64 KiB capacity.)

**The true landing error is large, and grows toward the estimated
ceiling — this is the finding that overturns round 2.**

| Target | True landing (time) | True error |
|---|---|---|
| 10 s | 10.397 s | 0.40 s |
| 30 s | 31.216 s | 1.22 s |
| 100 s | 104.020 s | 4.02 s |
| 200 s | 208.379 s | 8.38 s |
| 280 s | 291.370 s | 11.37 s |
| 295 s | 326.922 s | 31.92 s |
| 305 s | 368.222 s | 63.22 s |
| 320 s | 430.132 s | 110.13 s |
| 350 s | 554.005 s | 204.00 s |
| 360 s (near the estimated ceiling) | 595.278 s | 235.28 s |

At 360 s, asked to land a third of the way through the file, `Coarse`
actually lands **at 595 s — five seconds from the true end of a 600 s
recording** — while its own `SeekedTo::actual_ts` reports a plausible-
looking value near 360 s. This is not a rounding error; it is the seek
reporting a false position with no signal to the caller that anything is
wrong. The CBR fixture, measured the same non-circular way, stays within
~0.3 s throughout (consistent with measurement slack from the technique
above, not a real algorithmic error) — confirming the earlier round's
premise that CBR really is (near-)exact, while showing that VBR is nowhere
close to the "≤0.0261 s" this section previously and wrongly reported.

**Why: `preseek_coarse` divides by the wrong denominator.** Its byte
estimate is `(required_ts / total_dur) × audio_byte_len`, where
`audio_byte_len` is the fixture's real, exact byte length, but `total_dur`
is `num_frames`'s *estimated* duration (361.04 s here, against a true
600 s — see §5.5). As `required_ts` approaches that wrong ceiling, the
formula pushes the byte estimate toward 100% of the *real* (much longer)
byte length — i.e. toward the true end of the file — regardless of what
audible position the target actually names. `MpaReader::seek`'s Step 2 loop
does not fix this: it walks forward from the (mis-anchored) estimate using
its *own* `next_packet_ts` counter until that counter numerically reaches
`required_ts` — a counter seeded from the same wrong ratio — so the walk
converges the *self-report*, not the *landing*, which is exactly the
circularity the first VBR measurement fell into.

**Does this reopen the shape question? No — because A and B share the
identical vulnerability.** Shape A's own byte estimate (§5.1) is computed
from the same `total_dur`/`audio_byte_len` ratio; it would misland by
comparable amounts for the identical reason, with the added disadvantage of
no Step 2 walk to at least land on a real frame boundary methodically.
Shape B remains structurally unimplementable regardless (below). This is
not a per-shape defect at all — it is what an under-sampled duration
estimate does to *any* byte-offset arithmetic derived from it, and it
would need to be fixed at that layer (a better duration estimate, or a
bound on how far a byte offset may be trusted), not by picking a different
seek mode. That is future work this spike did not scope.

**Is the error at least bounded? Yes, structurally, but not usefully.**
`preseek_coarse`'s ratio is `required_ts / total_dur`, and `required_ts`
can never exceed `total_dur` (`MpaReader::seek`'s own bounds check, next
paragraph, guarantees this before the arithmetic ever runs) — so the byte
estimate can never fall outside `[0, audio_byte_len]`: the seek cannot
request a byte before the first packet or past the real end of file. But
"bounded by the file's own length" is not a useful accuracy guarantee for a
listener asking to land at a specific point — a third of the way through
landing five seconds from the end demonstrates that concretely. **This is
exactly what §3.4's "no accuracy figure is promised" was written to cover,
and this measurement is the evidence that the clause is load-bearing, not
a formality**: `Coarse` still wins on cost (3,072 B vs. 983,040 B, which
does not depend on any of this), but nothing in this design may describe
its landing as approximately correct.

**A second, sharper failure mode found alongside this: an underestimated
duration refuses the seek outright, for every mode.** `MpaReader::seek`
checks the target against `max_ts` — derived from `num_frames` — *before*
branching on seek mode. A target past the estimated 361.04 s ceiling
(400 s, verified real audio exists there) was rejected with
`SeekErrorKind::OutOfRange` for **both** `Coarse` and `Accurate`
identically; this is not a Coarse-specific defect, it is what a wrong
`num_frames` does to the shared bounds check. Ordinary forward playback is
unaffected — `next_packet_ts` advances from real per-frame durations and is
never clamped to `num_frames` — only an *explicit seek* past the (wrong)
estimate is refused. At least this failure mode is loud and honest, unlike
the mislanding above.

**When does the large error actually occur? Only under a conjunction, and
the reported bug does not meet it.** The mislanding above is not a property
of `Coarse`. It is a property of `Coarse`'s *denominator*, and that
denominator is `num_frames`, whose provenance this amendment already tracks.
Two independent conditions must both hold to produce it:

1. **No Xing/Info/VBRI tag**, so `num_frames` comes from
   `estimate_num_mpeg_frames`'s ~16-frame sample — which is what made
   `total_dur` 361 s against a true 600 s. This is the dominant term: a 40%
   wrong denominator drives the byte estimate toward 100% of the *real* byte
   length as the target approaches the *estimated* ceiling.
2. **Genuinely variable bitrate**, so that even a correct denominator would
   not make the uniform-bitrate arithmetic exact.

With a tag present, condition 1 fails and the denominator is the encoder's
own declared frame count. The residual error is then bounded by local
bitrate deviation from the file average — zero for CBR, and for
tagged VBR a fraction of the file rather than a third of it.

**Measured against the file that actually wedged.** `rt_podcast900.mp3`
(the Radio-T episode from the manual acceptance report) was checked
directly over its own CDN with byte-range requests:

| Property | Value | How |
|---|---|---|
| ID3v2.4 tag | 37,432 B, pushing the first frame past a naive 8 KB probe | header parse |
| First frame | MPEG-1 Layer III, 128 kbps, 44.1 kHz | frame header at 37,432 |
| `Info` tag | present at 37,453, `flags=0xf`, `frames=312238`, TOC present | tag parse |
| LAME tag | present at 37,573 | tag parse |
| Duration from `Info` | 8156.42 s (2:15:56) | `312238 × 1152 / 44100` |
| Duration if uniform 128 kbps | 8156.45 s (2:15:56) | `(130540588 − 37432) × 8 / 128000` |
| **Divergence** | **0.03 s over 2 h 16 m** | — |

`Info` (rather than `Xing`) is the identifier LAME writes for a constant-
bitrate file, and symphonia accepts both (`INFO_TAG_ID`, `demuxer.rs:736`).
So on this file `num_frames` is the encoder's exact count, not an estimate,
**and** the uniform-bitrate assumption is exact to within one frame across
the whole recording. `Coarse` lands essentially exactly here. The wedge this
amendment exists to fix is fixed, on the very file that exhibited it, with
no accuracy cost at all.

This does not soften the finding above — it locates it. The catastrophic
case is real and must be designed for; it is simply not the common case,
and it is not the reported one.

**What this means structurally: the finding validates the provenance axis
rather than undermining it.** §5.5 already rules that an estimated duration
must never drive a destructive decision. A `Coarse` seek's landing *is*
driven by the duration, so the rule reaches it directly, and the two cases
fall out of the axis already specified:

- **Duration established** (Xing/Info/VBRI): the denominator is the
  encoder's. Landing is exact for CBR and bounded by bitrate variance for
  VBR. Still reported `Estimated` — §3.4 promises no figure, and nothing at
  seek time proves which of the two it is.
- **Duration estimated** (no tag): the denominator may be wrong by tens of
  percent, and the landing error has no useful ceiling below the file's own
  length. The landing is not merely imprecise; it may name a different part
  of the recording entirely.

The design consequence is already written and needs no new mechanism: such a
landing must not overwrite an established checkpoint (§4), must not drive a
`StalePastEnd` verdict (§5.5), and must be shown as unreliable rather than
as a number the listener can act on (§3.4).

**Not adopted, recorded as future work.** A better duration estimate would
collapse the dominant error term cheaply — sampling frame headers at ten
points across the file costs ~40 KB against the 983 KB rescan this
amendment removes, and would replace a 16-frame extrapolation with something
defensible. Symphonia's discarded TOC would do better still if it were
reachable. Neither is in scope here: this amendment removes a wedge, and
widening it into duration-estimator work would be the same scope drift §5.5
was just narrowed to avoid.

**Does the decision survive all of this? Yes, on cost — not on accuracy.**
`Coarse` is not switched out for shape A or B, because both would carry the
identical duration-estimate vulnerability with none of Coarse's advantages
(shape A adds a bespoke adapter and manual timestamp/reservoir bookkeeping
on top; shape B is not implementable at all). The cost argument
(3,072 B vs. 983,040 B for the rescan this whole amendment exists to
remove) is completely unaffected by any of the above and remains decisive.
What must change is how confidently this document, and the tasks after it,
are allowed to talk about where a `Coarse` landing actually is: never as a
number a listener can trust to be close, only as an estimate whose error
has no practical ceiling below the file's own length.

**Which formats this reaches, and why it is MP3 alone.** `SeekMode` looks
like a global switch and is not. Across the whole symphonia tree, exactly one
demuxer reads the parameter:

| Demuxer | Signature |
|---|---|
| `symphonia-bundle-mp3-0.6.1/src/demuxer.rs:232` | `fn seek(&mut self, mode: SeekMode, ..)` |
| `symphonia-bundle-flac-0.6.1/src/demuxer.rs:249` | `_mode` |
| `symphonia-format-isomp4-0.6.1/src/demuxer.rs:671` | `_mode` |
| `symphonia-format-ogg-0.6.1/src/demuxer.rs:492` | `_mode` |
| `symphonia-format-mkv-0.6.1/src/demuxer.rs:595` | `_mode` |
| `symphonia-codec-aac-0.6.1/src/adts.rs:324` | `_mode` |

For every format except MP3, `Coarse` and `Accurate` execute byte-identical
code. The reason is structural rather than incidental: **FLAC frames carry
absolute sample numbers in their headers**, so FLAC's seek binary-searches the
byte range against real timestamps and lands on the frame that actually
contains the target. **MP3 frames carry no timestamps at all**, which is
exactly why `preseek_coarse` must divide a byte ratio and why its landing can
be wrong by the margins measured above.

**So provenance follows the demuxer that ran, not the seek mode requested:**

- **MP3 → `Estimated`, unconditionally.** Never conditioned on Xing/Info
  presence, on bitrate, or on anything sampled from the file.
- **Every other format → `Established`,** exactly as M1 and M2 reported it.
  This amendment changes nothing about their seeks, so it must not change
  what they claim about them.

Read the format from `FormatReader::format_info().format` against
`FORMAT_ID_MP3` — never from a file extension, a URL, or the transport.

**This is not a softening of the rule below; it is its scope.** Marking a FLAC
landing `Estimated` would not be conservative, it would be false, and §4 makes
falseness expensive: an estimated position may not replace an established
checkpoint, so a blanket `Estimated` would freeze the checkpoint of every
local file at its pre-seek value the moment the listener seeks. M1 and M2 do
not have that bug and this amendment must not introduce it.

**Consequence for provenance, sharpened by this finding.** Nothing
observable at seek time distinguishes a CBR file (where the estimate
happens to be exact) from a VBR one (where the error can span a large
fraction of the file) — the demuxer does not expose that fact, and this
codebase must not try to infer it from, say, a small sample of frame sizes.
**Every `Coarse` landing must be reported as `PositionProvenance::
Estimated`, unconditionally**, and "estimated" here must be read as "may be
in a substantially different part of the recording," not as "approximately
right." A later reader must resist the temptation to mark a landing
"established" because it happened to come from a CBR file; that judgement
needs evidence this codebase does not have at the moment of the seek.

**Coarse is not reservoir-safe "for free" — the earlier claim here was
wrong, for the same reason the accuracy claim was.** "No decode errors"
was the evidence for that claim, and MP3 decoders do not fail loudly on a
violated bit reservoir — they produce successful-but-wrong output, so a
lack of errors proves nothing. Decoding forward from a `Coarse` landing
with a fresh decoder and diffing against a reference decoded continuously
from the true start (found by the same non-circular localisation above,
choosing a target whose true landing falls in non-repeating content so the
comparison itself is meaningful) shows real, measurable corruption: on the
CBR fixture, max sample error 0.119 (frame 0), 0.119 (frame 1), 0.036
(frame 2), then exactly 0.000000 from frame 3 on; on the VBR fixture (true
landing in the noise region), 1.336, 1.581, 0.732, then exactly 0.000000
from frame 3 on. **`Coarse` needs the same 3-frame discard shape A needed**,
consistent with `MAX_REF_FRAMES = 4` in both cases. The reason this does
not cost Coarse anything new: `MpaReader::seek`'s reference-frame
backtracking repositions the *byte stream* to a reservoir-safe reference
frame before returning, but does not decode anything — priming the
reservoir still requires decoding forward from `actual_ts` to the target,
discarding as it goes. `DecodedSource::seek_refined` (`src/playback/
decode.rs`) already does exactly this, unconditionally on seek mode, for
`Accurate` today. Swapping its hard-coded `SeekMode::Accurate` for `Coarse`
inherits this refinement with no new reservoir-specific code — the
existing decode-and-discard-to-target loop does not know or care which
preseek mode produced the frame it started from.

**Shape A, corrected, and what it costs.** The first version of this
measurement was wrong in an instructive way: wrapping an already
byte-sought `HttpMediaSource` in a fresh `MediaSourceStream` and calling
`symphonia::default::get_probe().probe(...)` does not probe from that
offset. `MediaSourceStream::new` always initialises its internal position
bookkeeping (`abs_pos`) to 0, with no way to tell it the source starts
elsewhere; `Probe::probe()` additionally runs a trailing-metadata pre-scan
(seekable sources only) that reads `mss.pos()` as its restore point —
0, by the above — and seeks back there before the main format scan runs. The
net effect: the "fresh probe" silently re-read from the file's true byte 0,
which is why its first pass reported the whole-file `num_frames` estimate
and a request back to offset 0. **This means shape A cannot be `Probe::
probe()` plus a byte seek; it needs either a codec-specific constructor
called directly (bypassing the generic prober, since the container is
already known) or a `MediaSource` adapter that translates `byte_len()` to
the *remaining* length so the stream's zero-based bookkeeping stays
internally consistent.** The corrected experiment does the latter and
confirms: `symphonia::default::formats::MpaReader::try_new` resyncs
correctly from an arbitrary mid-frame byte (one request, no fallback to
byte 0, `num_frames` now correctly reflects the *remaining* length), one
request at the estimated byte, sub-millisecond and a few hundred KB to ~2 MB
of noisy server-side write (again a channel/OS-buffer artifact, not
something the reader needed).

Decoding from that landing cold — a brand-new decoder, no prior reservoir
context, exactly what a re-probe hands the pipeline — never returned a
`Result::Err` (MP3's decoder does not fail loudly on a missing reservoir; it
just produces wrong samples). Diffing those samples against a reference
decoded continuously from the true file start through the same frame
(verified by an independent byte-level frame count to be MPEG frame #11485,
pts 13,230,720) quantifies the damage: max sample error 0.119 (frame 0),
0.119 (frame 1), 0.031 (frame 2), then **0.000000 — bit-exact — from frame 3
on**. So shape A needs **3 frames discarded**, consistent with
`MAX_REF_FRAMES` — the same number `Coarse` needed above, for the same
reason: neither shape decodes anything during the seek itself, so both
land the caller on a byte-stream position that still requires decoding and
discarding forward before its output can be trusted. The difference is
where that discard has to be implemented: for `Coarse`, `seek_refined`'s
existing loop already does it; shape A would need the same discard written
again from scratch around its own bespoke reader.

**Timestamp origin (load-bearing for Tasks 2 and 4): zero-based, not
absolute.** A freshly built `MpaReader` sets `next_packet_ts = -delay` at
construction (`try_new`, no LAME tag ⇒ delay 0) regardless of where in the
byte stream it started — confirmed empirically: the fresh reader's first
packet reported `pts = 0` after resyncing at true byte 4,800,305 (300.02 s
into the file). **Whichever shape ships must treat every packet timestamp
from a reader built mid-stream as relative to that reader's own start, and
add the estimated base itself to recover an absolute media position.**
Coarse sidesteps this entirely: it seeks the *original* reader, whose
`next_packet_ts` has been counting from the true start since it was opened,
so `actual_ts` is already absolute.

**Shape B: not implementable, confirmed structurally rather than by trial.**
`FormatReader`'s only accessor back to the underlying stream is `into_inner
(self: Box<Self>) -> MediaSourceStream`, which consumes the reader — there
is no live handle from which to byte-seek "beneath" a reader that is still
in use, and nothing exposes `next_packet_ts` for external mutation even if
there were. Reaching it would mean downcasting the trait object to the
concrete, private `MpaReader`, which this crate's `unsafe_code = "forbid"`
and ordinary API discipline both rule out. What `into_inner` does offer
(rebuild a reader from the stream it returns) is exactly shape A, with no
cost advantage over calling it directly.

**Index evidence versus estimated duration — the rule `SeekSupport` must
follow.** `track.num_frames.is_some()` says nothing about whether a seek
index exists. For MP3 specifically, `num_frames` is populated by exactly one
of three code paths in `MpaReader::try_new` — a Xing/Info tag, a VBRI tag, or
`estimate_num_mpeg_frames`'s bitrate arithmetic — and symphonia's public
`Track` type carries no field distinguishing which one ran; the only trace
is a `log::info!` line, which is not a stable API to branch on. Distinguishing them would nonetheless buy something real, and an earlier
draft of this section denied it on a false premise. That draft claimed a
Xing/VBRI tag is "only a self-declared total frame count, not a byte-offset
table". **That is wrong.** The Xing/Info tag carries an optional 100-entry
byte-offset TOC (flags bit 2), and symphonia *parses* it —
`XingInfoTag { toc: Option<[u8; 100]>, is_cbr: bool, .. }` at
`demuxer.rs:749-757` — then discards it: the struct is `#[allow(dead_code)]`
and only `num_frames` (`:439`) and `lame` (`:434`) are ever read. `toc`,
`is_cbr`, `num_bytes` and `quality` are constructed at `:924` and never
consumed by anything.

So the correct statement is narrower and more useful: the TOC exists in the
format and in many real files, but symphonia's binding does not expose or
use it. That is a limitation of the binding, not of MP3, and it is the
reason `Coarse` performs the same uniform-bitrate arithmetic regardless of
which path populated `num_frames`. What the tag's *presence* does buy is
described in the section immediately below, and it is the difference between
an exact landing and a catastrophic one. None of the four containers M1 ships (WAV, FLAC,
MP3, AAC/ISO-BMFF) expose a true random-access seek index through
symphonia's public `FormatReader`/`Track` API that this codebase can
observe. **The rule, therefore: `SeekSupport`/`MediaCapabilities` must
continue to derive seek confidence only from demonstrated demuxer behaviour
(`DemuxerSeek::Proven`/`Unproven`, established by a trial seek actually
landing, per §6 of the M3 design), never from `num_frames` or the duration
it produces.** A byte offset computed from that duration (§5.1) is always an
estimate — this holds even when the count came from a Xing/LAME tag, since
that count is still just arithmetic input, not an index lookup — and must
report `PositionProvenance::Estimated` (§3) accordingly.

### 5.3 Bounding, and the limit of what a bound can promise

M3's `SEEK_BUDGET` is not a timeout and must stop being described as one. A
real deadline is a wall clock, and it must interrupt the *underlying
operation*.

**The honest statement of what a source-level deadline achieves:** checks in
`HttpMediaSource` bound source **I/O**. They do not bound demuxer work over
bytes already buffered above it — `MediaSourceStream` holds 64 KiB, and frame
headers can be parsed out of it without touching the source at all. So a
source deadline alone yields "deadline plus one buffer's worth of parsing",
not a hard interrupt.

**Corrected per §5.2's actual resolution:** the shape chosen (`Coarse`) does
still call `FormatReader::seek()` — it does not remove the call the way a
re-probe shape would have. What it changes is how much work that one call
does: §5.2's own measurements show `Coarse`'s resync-and-walk consuming a
few KB and a handful of frames, never the unbounded rescan-from-
`first_packet_pos` `Accurate` performs. So the call is not eliminated, but
the amount of I/O behind it is small enough that the ordinary per-read
`limits.stall` bound (already enforced inside `HttpMediaSource::read`)
covers it in practice — there is no equivalent here to Accurate's "many
cheap reads adding up to an unbounded total" failure mode, because Coarse's
own walk is short by construction, not because the call disappeared.

### 5.4 Recovery

A failed or expired seek must restore the pre-seek position, and that
restoration gets **one fresh bounded attempt with its own deadline** — not an
inherited expired one, and not a retry loop.

Inheriting the expired deadline would fail recovery immediately: bounded, but
it leaves the decoder at an arbitrary mid-scan point with no way back, making
the position unrecoverable rather than merely late. That is worse than the
wedge being fixed. If the fresh attempt also fails, the seek reports failure
with the pre-seek position restored logically, and the source is retired so
the next explicit action reopens cleanly.

## 5.5 Estimated *duration*, and why it is more dangerous than an estimated position

The spike surfaced this while measuring something else, and it is the sharpest
finding in this amendment.

`estimate_num_mpeg_frames` samples only the **first ~16 frames** to derive
`num_frames`. On the VBR fixture that produced an estimated duration of
361 s for a genuinely 600 s file — 40 % short. For CBR it is exact, which is
why nothing has noticed: most podcasts, including the one that occasioned this
work, are CBR.

An under-estimated duration is not merely a cosmetic display error. Two paths
turn it into silent loss, both verified:

1. **`src/resume.rs:85` — a legitimate checkpoint is judged stale.**
   `decide_resume` returns `StalePastEnd` when `position > duration`, and
   `start_at()` maps that to zero. A listener 70 % through a VBR recording
   whose duration is under-estimated by 40 % **resumes at the beginning**, and
   the entry that would have saved them is discarded as stale. This is exactly
   the class of loss §4 exists to prevent, arriving by a different door.
2. **`clamp_target` — the tail becomes unreachable.** Seeks are clamped to
   `metadata().duration`, so the last 40 % of such a file cannot be reached,
   and a seek into it lands silently at the estimated ceiling rather than
   failing. (A target past the ceiling is refused `OutOfRange` by both
   `Coarse` and `Accurate` alike — a duration problem, not a seek-mode one,
   and at least it fails loudly.)

**The rule: an estimated duration may inform display, and must never drive a
destructive decision.** Concretely:

- `MediaMetadata::duration` carries its provenance, the same axis §3
  introduces for position. A duration derived from `estimate_num_mpeg_frames`
  is `Estimated`; one from a real index, container header or seek table is
  `Established`.
- `decide_resume` treats an **estimated** duration as it treats an absent one.
  M2 already has the right answer for that case — `ResumeDecision::Unvalidated`
  — which retains the stored position rather than discarding it. `StalePastEnd`
  requires an established duration, because declaring a listener's checkpoint
  stale is destructive and an estimate is not evidence enough to do it.
- `clamp_target` does not clamp to an estimated duration. A seek beyond it is
  attempted and allowed to fail honestly, which is better than landing
  silently somewhere the listener did not ask for.

  **This does not make the tail seekable, and the amendment must not be read
  as claiming it does.** Symphonia applies its own bounds check and refuses a
  target past its estimated ceiling. Verified in
  `symphonia-bundle-mp3-0.6.1/src/demuxer.rs`:

  ```rust
  // :250-262 — the ceiling is derived from the same bad estimate
  let dur_ts = self.tracks[0].num_frames.map(Duration::from);
  let max_ts = dur_ts.and_then(|dur| min_ts.checked_add(dur))
                     .and_then(|dur| dur.checked_add(Duration::from(delay + padding)));

  // :267-271 — refused before any mode dispatch
  else if let Some(max_ts) = max_ts {
      if required_ts > max_ts { return seek_error(SeekErrorKind::OutOfRange); }
  }

  // :291-295 — Coarse vs Accurate is only chosen *after* the check above
  match mode {
      SeekMode::Coarse if is_seekable => self.preseek_coarse(...)?,
      SeekMode::Accurate => self.preseek_accurate(...)?,
      _ => (),
  };
  ```

  The refusal is mode-independent by construction — it precedes the branch —
  so no choice of `SeekMode` can recover the tail. And `max_ts` descends from
  `num_frames`, which for a no-Xing VBR file is exactly the ~16-frame
  `estimate_num_mpeg_frames` guess that caused the problem: the ceiling is
  wrong in the same direction and by the same amount as the duration. Removing the application-level clamp changes
  a silent mislanding into a visible refusal; it does not extend reach. The
  tail of an under-estimated VBR file stays unreachable, and that is a
  **retained limitation** of this change.

  The case that matters is a **launch resume** to a stored position past the
  estimated ceiling: it will fail. The checkpoint must survive that failure
  untouched, which is the same principle as §4 — a position we could not
  reach is not a position we may discard.
- The status line already renders an unknown duration as `--:--:--`. An
  estimated one is displayed, not hidden — but it must not be presented as
  though it were measured.

**Scope, deliberately narrow.** Provenance applies to **position and duration
only** in this change. Those are the two quantities with demonstrated
destructive consequences — a discarded checkpoint and an unreachable tail —
and both are fixed by the rules above. Generalising to "every derived
quantity" would be a framework built ahead of its second use case, and this
project has no other quantity today whose estimation destroys anything. If a
third appears, the pattern is here to copy.

## 6. Acceptance

- The parked reproduction passes, with `#[ignore]` removed.
- **Repeated seeks**: several rapid `→` presses land and stay responsive; no
  press is serviced by a rescan from the first packet.
- **Deadline expiry**: a seek against a stalled source fails within its
  deadline, restores the pre-seek position, and leaves the session usable.
- **Transport responsiveness**: pause, stop and quit are all serviced promptly
  *during* a seek, not after it.
- **Provenance**: an estimated landing reports `PositionProvenance::Estimated`,
  and playing on from it keeps reporting it — including while
  `PositionQuality` moves between `Estimated` and `Degraded`, which must not
  disturb provenance in either direction.
- **Persistence**: an estimated position never overwrites an established
  `position`; with none present it persists only as `estimated`; restart
  selects `estimated` when present and reports what it kept, reporting
  `established: None` for an estimate-only entry; **both** exits in §4.4 clear
  it and re-permit established writes, and an estimated completion clears
  neither.
- **Completion under estimate**: an estimated timeline reaching EOF sets
  `completed` while its terminal position goes to `estimated`, leaving any
  established `position` untouched and protection in force.
- **Upgrade**: a session started against an existing v1 file keeps persisting
  — the file becomes v2, every entry survives, and the session was never
  marked unwritable. Nothing in this amendment may cost an existing listener
  their stored positions.
- **Schema**: a v1 file loads under v2 with `estimated: None`; a v2 file read
  by a v1 build is preserved unwritten.
- **Estimated duration**: a checkpoint past an *estimated* duration resumes at
  the stored position rather than at zero; the same checkpoint past an
  *established* duration is still `StalePastEnd`; a seek past an estimated
  duration is attempted rather than silently clamped.

## 7. Review boundary

Settle §4.3's restart preference, §4.5's schema bump, and §5.2's choice of
shape before task breakdown. §5.2 in particular should be decided by a spike
against the real fixture, not by argument.
