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
to requalify the accuracy claim once a reviewer correctly pointed out that a
CBR fixture cannot measure `Coarse`'s accuracy in general — only the case
where its underlying arithmetic is exact. Both are generated per
`tests/fixtures/README.md`. The throwaway experiments are not committed; the
measurements below are what they produced.

**Decision: neither A nor B. Use `SeekMode::Coarse`, unmodified.** The known-
debt claim that Coarse refuses without a Xing header is false, verified both
by reading `symphonia-bundle-mp3-0.6.1/src/demuxer.rs:465-471` and by
exercising it: with no Xing/VBRI tag and a byte-seekable source,
`estimate_num_mpeg_frames` still populates `track.num_frames` from bitrate
arithmetic, `preseek_coarse`'s `is_seekable` and `max_ts` guards are
satisfied, and the seek lands. Coarse collapses the rest of §5 — no re-probe,
no hand-rolled resync, no reservoir bookkeeping — because it reuses
machinery `MpaReader::seek` already has for both modes.

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

| Shape | New requests (offset) | Server wrote | Demuxer consumed | Wall clock | Landing |
|---|---|---|---|---|---|
| **Baseline** (`Accurate`, shipped today) | 1, at byte 44 (`first_packet_pos`) | 3.7–5.4 MB (noisy) | **983,040 B** | ~8–9 ms | ts=2,642,688 (59.925 s) |
| Ground truth (fresh reader, first-ever `Accurate` seek — no rewind, honest forward scan from true start) | — | 3.1–5.5 MB (noisy) | **975,872 B** | ~7–8 ms | ts=2,642,688 (59.925 s) |
| **C — `Coarse`** | 1, at byte 957,162 (the arithmetic estimate) | 0.3–1.4 MB (noisy) | **3,072 B** | <1 ms | ts=2,642,688 (59.925 s) |
| A — re-probe at offset (target 300 s; corrected methodology, see below) | 1, at the estimated byte exactly | 0.6–2.0 MB (noisy) | n/a (bypasses `HttpMediaSource`'s read path differently — see note) | <1 ms | pts=0, relative (see timestamp-origin) |
| B — byte-seek beneath a live reader | — | — | — | — | not implementable (see below) |

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

**Requalification: `Coarse`'s accuracy on genuinely variable content.**
`sine-long-vbr-noxing.mp3` pairs 300 s of white noise (~417 B/frame) with
300 s of digital silence (~104 B/frame), a ~4x local bitrate ratio, chosen
because a pure sine tone compresses to an almost perfectly constant frame
size even under VBR encoding (verified: a sine-only VBR attempt produced
22,967 of 22,970 frames at one identical byte size — it would have measured
nothing). Because the sampled region (`estimate_num_mpeg_frames` reads only
the first ~16 frames) is denser than the file's second half, the estimated
duration comes back as **361.04 s — short of the true 600 s.**

| Target | Ground truth | `Coarse` landing | Error |
|---|---|---|---|
| 10 s | 9.9265 s | 9.9265 s | 0 |
| 30 s | 29.9363 s | 29.9363 s | 0 |
| 100 s | 99.9445 s | 99.9445 s | 0 |
| 200 s | 199.9412 s | 199.9412 s | 0 |
| 280 s | 279.9282 s | 279.9282 s | 0 |
| 295 s (5 s before the noise→silence cut) | 294.9224 s | 294.8963 s | −1 frame (−0.0261 s) |
| 305 s (5 s after the cut) | 304.9012 s | 304.9012 s | 0 |
| 320 s | 319.9216 s | 319.9216 s | 0 |
| 350 s | 349.9102 s | 349.9102 s | 0 |
| 360 s (near the estimated-duration ceiling) | 359.9151 s | 359.9151 s | 0 |

Landing error stayed within a **single frame (≤0.0261 s)** everywhere
tested, including straddling the abrupt 4x bitrate step. This is larger than
the CBR fixture's zero error, but far smaller than "so far off as to be
unusable." The reason is `MAX_MPEG_FRAME_SIZE` (2,881 B) — the fixed
backward margin `preseek_coarse` subtracts from its estimate before seeking
is generous relative to this fixture's frame sizes (max 835 B observed), and
the forward walk that follows self-corrects any remaining undershoot exactly
(at the cost of a longer local scan, never an unbounded one back to
`first_packet_pos`). An overshoot beyond that margin is the one case Step 2
cannot fully correct (it only backtracks up to `MAX_REF_FRAMES = 4`), and it
did not occur here even across a genuinely sharp rate change; it plausibly
could for a much longer file or a much larger local rate disparity than this
spike constructed, and no accuracy figure is promised for that case (§3.4
already declines to promise one, for exactly this reason).

**A sharper failure mode than inaccuracy: an underestimated duration refuses
the seek outright, for every mode.** `MpaReader::seek` checks the target
against `max_ts` — derived from `num_frames` — *before* branching on seek
mode. A target past the estimated 361.04 s ceiling (400 s, verified real
audio) was rejected with `SeekErrorKind::OutOfRange` for **both** `Coarse`
and `Accurate` identically; this is not a Coarse-specific defect, it is what
a wrong `num_frames` does to the shared bounds check. Ordinary forward
playback is unaffected — `next_packet_ts` advances from real per-frame
durations and is never clamped to `num_frames` — only an *explicit seek*
past the (wrong) estimate is refused. This is a real limitation to carry
into later tasks, but it fails the way §5.4 already wants failures to fail:
loud and refused, never a silent wrong landing.

**Does the decision survive this? Yes.** Within the estimate's reachable
range, `Coarse` never landed more than one frame off, even under a sharp,
deliberately adversarial rate change — nowhere near "unusable." The one
real defect this VBR run surfaces (an underestimated duration refusing valid
late-file seeks) applies identically to `Accurate` and to duration
reporting in general; it is not a reason to prefer a different shape, and
choosing shape A or B would not avoid it either, since both would still
need `num_frames`/duration for the byte-offset arithmetic in the first
place. `Coarse` stands.

**Consequence for provenance, regardless of CBR or VBR.** Nothing observable
at seek time distinguishes a CBR file (where the estimate happens to be
exact) from a VBR one (where it is merely bounded) — the demuxer does not
expose that fact, and this codebase must not try to infer it from, say, a
small sample of frame sizes. **Every `Coarse` landing must be reported as
`PositionProvenance::Estimated`, unconditionally.** A later reader must
resist the temptation to mark a landing "established" because it happened
to come from a CBR file; that judgement needs evidence this codebase does
not have at the moment of the seek.

**Coarse is already reservoir-safe.** `MpaReader::seek`'s Step 2 loop is also
where the reference-frame backtracking lives (the `main_data_begin` /
`n_ref_frames` logic, up to `MAX_REF_FRAMES = 4`), and it runs identically
after either preseek mode. Decoding five packets from the Coarse landing
produced no decode errors, confirming this landing needs no discard — a
property shape A does not get for free (below).

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
`MAX_REF_FRAMES`. Coarse needs this reasoning not at all, because it never
leaves `MpaReader::seek`'s own backtracking.

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
is a `log::info!` line, which is not a stable API to branch on. Nor would
distinguishing Xing/VBRI from the estimate actually buy anything: a
Xing/VBRI tag is itself only a self-declared *total frame count*, not a
byte-offset table — this spike's own evidence is that `Coarse` performs the
identical byte arithmetic plus local frame-walk regardless of which path
populated `num_frames`. None of the four containers M1 ships (WAV, FLAC,
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

This is the argument for §5.2's first shape: not calling `FormatReader::seek()`
on this path removes the unbounded call rather than fencing it, and is the
only option that makes the bound a real one.

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

## 7. Review boundary

Settle §4.3's restart preference, §4.5's schema bump, and §5.2's choice of
shape before task breakdown. §5.2 in particular should be decided by a spike
against the real fixture, not by argument.
