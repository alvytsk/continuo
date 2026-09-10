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
