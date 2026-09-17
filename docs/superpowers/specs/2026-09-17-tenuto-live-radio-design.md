# Tenuto M7: live HTTP radio

Status: draft for review, revision 2, 2026-09-17. The behavior below is the proposed contract, not an assertion that it already exists. Provider-resolved media (YouTube) is deliberately not in this document; see §13.

Branch: `feat/m7-live-radio`.

## 1. Product decision

Tenuto plays a direct live HTTP audio stream — an Icecast or Shoutcast v2 mount answering `audio/mpeg` or ADTS AAC with ICY response headers — from `tenuto play <url>` and from the queue.

This reverses a documented rule. `architecture.md` §1, the README and `reference.md` say a live stream is refused, and that a dropped connection never reconnects on its own. M7 replaces both statements with the rules in §3 and §7. Finite-media behavior changes only where §6 says so.

The invariant M7 extends:

> Media identity and logical playback state survive replacement of the transport used to play them.

For a station: connection #1 → disconnect → connection #2 is one station session, with one listening-time counter, one queue entry, and no completion.

## 2. Existing foundations, and what they do not give us

Reusable:

- `response::is_live` reads `icy-name`, `icy-metaint`, `icy-br` into `SourceEvidence.live`; `DecodedSource::capabilities` maps it to `Continuity::Indefinite`, checked before byte length. `prepare()` is the single place that refuses it.
- The engine keeps `media` (identity) apart from `descriptor` (`SourceLocation`); `ensure_source_open()` reopens a retired remote source with a fresh `prepare()` — fresh probe, fresh decoder.
- `open_transport` builds the converter from the current source and anchors the new generation at `self.position`. `Timeline` advances only from played media frames.
- The engine already recovers one transport on its own (`service_faults` → `rebuild` → `DeviceRecovered`).
- `EndOfTrack` is the only origin of `completed` and of `Advance::Next`.
- `tenuto play` drives `EngineHandle` directly, without `PlayerRuntime`. Behavior that must hold for both front ends belongs in the engine.

Verified gaps this spec must close (all read in the code on 2026-09-17):

| Where | Fact | Consequence |
| --- | --- | --- |
| `EngineHandle::submit_seek` | Retires the observed source generation before the worker checks `SeekSupport::Unsupported`; `SeekBy` reaches it through `KeyRouter::flush` with no capability gate | A rejected seek still kills the connection |
| `EngineHandle::submit_pause` | Raises the freeze level only; freezing suspends the stall timer | A stalled live read can stay blocked, so the worker never reaches the dispatched `Pause` |
| `EngineHandle::submit(Load)` | Queues and wakes; cancels nothing | A replacement `Load` waits behind a blocked read or open |
| `WaitService::service_as` | On thaw, releases the transport and clears `frozen_by_hook` | A thaw could announce `Playing` before a fresh live source is open |
| `Worker::reinstall` | Resets the existing converter; never rebuilds it; `prime_and_run` can fail or be cancelled and `reinstall` still returns `Ok` | Unsafe for a fresh decoder with different parameters; "returned" is not "playing" |
| `Worker::publish_progress` / `WaitService::publish_progress` | Timeline accounting runs only for `Playing | Paused`; the code comment warns about a transport left running outside those states | Audio drained during reconnect would not be counted |
| `Worker::ensure_source_open` | Installs the decoder and emits `CapabilitiesChanged` before returning | A caller-side continuity check is too late |
| `Worker::restore` | Refuses `position > 0 && seek == Unsupported` before anything else | Stop → Play on a station is refused today |
| `Worker::restart` | Reopened-source branch zeroes position and emits `RestartEstablished` with no capability check | Restart would "succeed" on a station |
| `Worker::rebuild` | Always reseeks `self.position` | Listening time would become a decoder seek target |
| `Worker::reseek` | Compares `source.position()` (decoder cursor) with its target | Meaningless for listening time; recovery must not call it |
| `response::accept` | Status is matched last | ICY classification must be placed inside the success arms, not before them |
| `Session::on_loaded` | Records outgoing media, then resets per-media fields | The checkpoint gate needs a stated order |
| `route_command(engine, playing, …)`, engine `TogglePause` | Direction is decided by `state == Playing` alone | Space during reconnect would send `Play` |

Verified externally: `https://radio.cliamp.stream/lofi/stream` answers `Range: bytes=0-` with `200`, `audio/mpeg`, `Icy-Name`, `Icy-Br`, no `Content-Length`, and no `icy-metaint` when `Icy-MetaData` is not requested. Tests never depend on it.

## 3. Rules

1. **Recovery continues a current user request.** Automatic reconnect attempts exist only while an explicit Play of that station is current. Pause, Stop, a replacement Load, entry removal and Shutdown cancel them at once (§6). They never start, continue or resume after a process restart. The budget in §7 is a default value, not the principle.
2. **Listening time is never a seek target.** For indefinite media `Progress.position` means audio heard since `Load`. No path hands it to a decoder or a range request.
3. **A station never completes.** No `EndOfTrack`, no `completed`, no queue advance, no checkpoint.
4. **User seeks on live media are rejected and harmless.** `SeekTo`, `SeekBy` and `Restart` are refused with a notice and leave the source, the playback intent and the reconnect budget exactly as they were. A recovery is never reported as a successful seek or restart.
5. **Continuity is fixed for a session.** A reopen that comes back with a different continuity is a failure, detected before anything is adopted or published.
6. **Metadata framing never reaches the decoder.** Until M7.1 exists, a response that carries `icy-metaint` is refused.

## 4. HTTP layer

`response::Accepted` gains a third variant and remains the only body-mode model:

```
Accepted::Sequential { len } | Accepted::Ranged { range } | Accepted::Live
```

`accept()` keeps its current order — content-encoding, multipart, validator, then status — and classifies live **inside the success arms only**:

| Status | Extra condition | Outcome |
| --- | --- | --- |
| any non-2xx, 416 | — | today's typed failure (`Status`, `InvalidRange`), whatever ICY headers it carries |
| 200 or 206 | `content-type: application/vnd.apple.mpegurl` | `Err(UnsupportedLiveMedia)` — HLS stays refused, now before the probe |
| 200 or 206 | `icy-metaint` present | `Err(RemoteFailure::IcyFramingUnsupported)` (new; deleted by M7.1) |
| 200 at origin | `icy-name` or `icy-br` | `Accepted::Live`; `Content-Length` ignored |
| 206 | `icy-name` or `icy-br`, `requested_start == 0`, `Content-Range` parses with `first == 0` | `Accepted::Live`; length and total ignored |
| 206 | ICY headers, any other start | `Err(InvalidRange { WrongStart })` — Tenuto never requests one |
| 200 / 206 | no ICY headers | today's `Sequential` / `Ranged` rules, unchanged |

M7 never sends `Icy-MetaData: 1` and never issues a live request with a start above zero.

Consumers, one `match` arm each:

- `run_fetch`: `Live` means no advertised length, no in-place range resume, and **every** body end is a failure, a clean `Ok(None)` included: `classify_body_end` takes the mode and returns `Outcome::Failed(RemoteFailure::LiveEnded)` (new) instead of `Outcome::Eof`. The stall timer is unchanged.
- `HttpMediaSource::open`: `Live` forces `byte_len = None`, `byte_seekable = false`, `live = true`, and keeps `icy-name` as `station_name() -> Option<String>`.
- `prepare()`: stops refusing `Indefinite`; `Unresolved` is still refused. It gains an `expected: Option<Continuity>` in `PrepareContext`; when set and the prepared source disagrees, it returns `RemoteFailure::ResourceChanged` and the source is dropped (§3.5). When the decoder supplied no title, `prepare` sets `MediaMetadata.title` from `station_name()`, passed through the existing display escaping.

`RemoteFailure::is_retryable()`: true for `Transport`, `Timeout`, `LiveEnded`, and `Status` 5xx or 429; false for everything else, `Cancelled` included. A failure that says the location is unusable is never retried.

Known limits for `reference.md`: a Shoutcast v1 `ICY 200 OK` status line is rejected by hyper as a transport error (retryable, so it exhausts the budget on reconnect but fails the initial open at once); a stream with no ICY headers stays `Unresolved` and refused.

## 5. Engine: position, recovery and user commands

Above `http`, nothing sees a body mode. The engine acts on `capabilities.continuity == Indefinite` through one helper, `Worker::is_indefinite()`.

**Position.** `Progress.position` keeps its type. For indefinite media it is listening time since `Load`: it survives reconnect, Pause → Play and Stop → Play; silence contributes zero; it resets on the next `Load`. Provenance stays `Established`. Consumers tell the meanings apart by `capabilities.continuity`. `architecture.md` §1 gains this paragraph.

### 5.1 Three distinct requirements

1. **User seeks and Restart are rejected** before any source or decoder call, and before any interrupt is raised (§6.1).
   - `seek_to`, `SeekBy`: the existing `SeekSupport::Unsupported` rejection, now reached without the connection having been retired.
   - `restart()`: a new check at the very top — `SeekRejected { reason: "a live stream cannot restart" }` — before `ensure_source_open()`. The reopened-source branch must be unreachable for indefinite media; no `RestartEstablished`.
2. **Recovery preserves listening time without seeking.** `restore()` and `rebuild()` branch on `is_indefinite()` and never call `reseek` or `seek_bounded`:
   - `restore()`: the "this server cannot resume" guard applies to finite media only. The indefinite branch is the fresh-open sequence of §5.2. It never emits `SeekCompleted`. A `requested_target` found on indefinite media is dropped with a warning.
   - `rebuild()` (device fault): capture the position, tear the transport down, skip the reseek, `open_transport` at the captured anchor with the still-open decoder. Less than `RING_MILLIS` of audio is lost.
   - `reseek()` gets no indefinite no-op; it is simply not called.
   - `load()`: an indefinite source always starts at zero with no seek. A stale positive candidate yields `StartDisposition::Fresh`, never `ResumeUnavailable`, so no protection is raised for media that never checkpoints.
3. **Pause → Play opens a fresh source and discards buffered audio** (§5.2, §6.2).

### 5.2 The fresh-open sequence

One sequence serves reconnect attempts, Play from `Paused`, Play from `Stopped` and Play from `Failed` on an established indefinite session:

```
1. prepare(descriptor, expected = Indefinite)      cancellable; mismatch → ResourceChanged, nothing adopted
2. interrupt check                                  cancelled → abandon, nothing adopted
3. capture final played position of the old transport, if one exists; tear it down, discarding its ring
4. adopt the prepared source (capabilities unchanged by construction; no CapabilitiesChanged)
5. open_transport(playing = false) at the captured anchor   builds converter and output for THIS decoder
6. prime: pump until at least one frame is staged   read error, decode error or cancellation → not a success
7. interrupt check, then start running; StateChanged(Playing)
```

Success is step 7, not the return of any earlier call. `reinstall()` is not used: a fresh decoder may differ in sample rate or channel count, and only `open_transport` configures conversion for the source it is given. Reopening the device costs a gap that falls inside silence anyway.

`ensure_source_open()` is split so that steps 1–2 produce a *prepared, unadopted* source; finite-media callers adopt immediately, as today, but also pass `expected = Finite` so the rule in §3.5 is symmetric.

### 5.3 Initial load

`Load` opens and primes, then announces `Paused` and waits for `PlayLoaded`, as today — so one paused state does hold a live connection. Defined behavior:

- `PlayLoaded` for the load that just completed releases the primed transport as it is. The buffered audio is seconds old at most.
- A plain `Play` (or `TogglePause`) from that `Paused` state, on indefinite media, takes the fresh-open sequence instead of releasing the old ring.
- If nothing arrives, server backpressure may close the connection; that surfaces on the next Play, which is a fresh open anyway.

Every other `Paused`, and every `Stopped`, holds no connection.

## 6. Engine: interruption

### 6.1 What the handle may do before the worker looks

`EngineHandle` gains two atomics the worker publishes whenever it sets `capabilities`: `seek_unsupported` and `indefinite`. They gate only what the *handle* does out of band; the worker still validates every command.

| Submission | Today | M7 |
| --- | --- | --- |
| `submit_seek` | retire observed generation, raise `SEEK` | When `seek_unsupported`: enqueue `SeekTo` only — no retirement, no `SEEK` bit. The worker answers `SeekRejected`; the stream is untouched. This also protects a finite source on a server without ranges |
| `submit_pause` | raise the freeze level | When `indefinite`: enqueue `Pause`, then retire the observed generation (the `e51bae3` observed-generation pattern). No freeze level is raised |
| `submit(Load)` | enqueue, wake | For every medium: after admission, retire the observed generation. The load was going to tear that source down regardless |
| `submit_play` | thaw | unchanged |
| Stop, Shutdown | interrupt word | unchanged |

Retirement wakes every wait of the current source operation — header wait, probe read, body read (stalled or not), priming read — with `Cancelled`. `Cancelled` is not a disconnect: for indefinite media `pump_audio`'s retired-read arm retires the source and returns, and the queued command decides what happens next. It never enters `Reconnecting` and never charges the budget.

Stale-flag window: a `Pause` submitted while a station's first open is still classifying takes the freeze route. That open is bounded by `limits.open`, which is wall-clock and not suspended by freezing; the worker-side rules below then apply.

### 6.2 Pause on indefinite media (worker side)

Both routes — the dispatched `Pause`, and the `frozen_by_hook` early return when the hook parked first — end in the same place:

1. capture the position; tear the transport down, discarding the ring;
2. retire the fetch and the decoder;
3. clear `frozen_by_hook` — no transport remains for a thaw to release, so `service_as`'s thaw arm cannot release or announce anything;
4. cancel any pending reconnect attempt and clear the outage (§7);
5. `StateChanged(Paused)`, unless the hook already announced it.

`Playing` is announced only by step 7 of §5.2. Finite-media pause is unchanged.

### 6.3 Toggle direction

Engine `TogglePause`: `Playing | Reconnecting` → pause; otherwise play. `Play` while `Reconnecting` is a no-op: it neither forces an attempt nor resets the budget.

## 7. Engine: disconnect and reconnect

```
                          ┌───────── attempt failed, retryable, budget left ─────────┐
                          ▼                                                           │
Playing ──disconnect──▶ Reconnecting ──(next_attempt_at)──▶ fresh-open sequence §5.2 ─┤
   ▲                      │                                        │ step 7           │
   └──────────────────────┼────────────────────────────────────────┘                  │
                          │ Pause ─▶ Paused   Stop ─▶ Stopped   Load/Shutdown ─▶ theirs
                          └── non-retryable, or budget exhausted at failure time ─▶ Failed
                                         (listening time kept; Play = one explicit fresh open)
```

`PlaybackState` gains `Reconnecting` (label `reconnecting`), non-terminal. `Loading` is not reused: it carries load-token semantics.

**Disconnect** (indefinite media, state `Playing`, not a cancellation): the source returning `Ok(None)`; a retryable `RemoteFailure` from a read, the 15 s stall timeout included; a fatal decode error. These arms in `pump_audio` enter reconnect instead of setting `source_eof` or failing. A non-retryable failure fails at once. `check_end_of_track` never runs for indefinite media.

**Entering `Reconnecting`:** retire the fetch and decoder; leave the output transport running so the ring plays out; `StateChanged(Reconnecting)`; record `outage_started` if unset; schedule `next_attempt_at`.

**Accounting while reconnecting.** `facts.playing` becomes `Playing | Paused | Reconnecting`, so buffered audio actually heard after the disconnect advances listening time and underrun silence does not. Step 3 of §5.2 captures the old generation's final played position before replacing it, and the new generation is anchored there. The first backoff step (1 s) exceeds `RING_MILLIS` (300 ms), so the ring has drained before any attempt.

**Attempts** run from the worker loop when `next_attempt_at` has passed; the loop never sleeps on the backoff, so commands stay serviced. An attempt that fails during priming (step 6) is a failed attempt in the same outage, not a nested disconnect: the disconnect handler is idempotent in `Reconnecting` and only reschedules.

**Policy** (constants beside `Limits`, injectable for tests):

| Item | Default |
| --- | --- |
| Backoff | 1 s, 2 s, 4 s, 8 s, then 15 s |
| Budget | 5 min of wall time from `outage_started`, evaluated **when an attempt or a playing connection fails**. It does not cut an in-flight open short (that is bounded by `limits.open`, so the worst case is budget + 30 s), and reaching it during successful but not-yet-stable playback stops nothing |
| Outage ends — backoff and `outage_started` cleared | after 30 s of **sustained playback**: listening time advanced 30 s since step 7. Bytes downloaded or frames decoded do not count |
| Non-retryable failure | `Failed` at once |
| Pause, Stop, Load | clear the outage |

A server that accepts, bursts a few seconds and closes is therefore one continuing outage.

**Initial open.** A `Load` that fails is `Failed`, as today. No station session exists yet to continue.

Each reconnect replays whatever burst-on-connect audio the server sends. M7 accepts this.

## 8. Failure and end taxonomy

| Event | Finite (unchanged) | Indefinite |
| --- | --- | --- |
| Source `Ok(None)`, output drained | tail confirmed → `EndOfTrack`, `Ended`, completed | disconnect → `Reconnecting` |
| Retryable remote failure mid-stream | `Failed { cause }` | `Reconnecting` |
| Non-retryable remote failure | `Failed { cause }` | `Failed { cause }` |
| Fatal decode error | remote: `Failed`; local: drain and warn | failed attempt → `Reconnecting` |
| Reopen with different continuity | `Failed { ResourceChanged }` | `Failed { ResourceChanged }`; identity, listening time and the checkpoint prohibition retained |
| Budget exhausted | — | `Failed { cause: last failure }` |
| Read cancelled by Pause / Load / Stop | the command's own transition | the command's own transition; never a disconnect |
| Pause | park, connection held | source closed, ring discarded, `Paused` |
| Stop | `Stopped`, checkpoint | `Stopped`, no checkpoint |
| Shutdown | final snapshot with checkpoint | final snapshot, no checkpoint for this media |

## 9. Session, queue and persistence

- A station is `MediaId::RemoteUrl` with `QueueSource::RemoteUrl`. **No schema change**; `schema_version` stays 3.
- `Session` gains one per-media field, `checkpointable`. Transition order in `on_loaded`:

  ```
  record_outgoing under the OUTGOING media's gate
      → reset per-media fields, adopt incoming media
      → set the gate from this Loaded's capabilities.continuity
  ```

  Loading a station therefore still checkpoints the finite track on its way out; loading a finite track never writes one for the outgoing station.
- `CapabilitiesChanged` updates the gate only when `accepts_media_event` says it belongs to the adopted session. §3.5 means continuity never actually changes within one; the gate is not loosened by any event after adoption.
- With the gate shut, `checkpoint_from_progress` returns `false`, and `record_current`, `record_current_estimated`, `record_outgoing` and `capture_current` write no checkpoint entry. `current_media`, the queue and `active_entry` are still written.
- A pre-existing checkpoint under the same `remote:<url>` is left untouched.
- `on_state(Reconnecting)` takes the no-op arm.
- Restart of Tenuto with a station active: the entry is restored, unloaded. No socket opens until Enter or Space; `tests/m5_no_network.rs` gains a station case.

## 10. Application and front ends

- `PlaybackPhase` gains `Reconnecting`. `transport::decide` treats it as `Playing` for TogglePause and Stop, and answers seek inputs and Restart with `live stream: seeking is unavailable` whenever the mirror's continuity is `Indefinite` — so no seek is submitted and no `KeyRouter` burst opens.
- `route_command`'s boolean becomes "pause direction": `Playing | Reconnecting`. `PlayerRuntime::route` and `app.rs` both pass it that way.
- The view gains `live: bool` and the reconnect state. TUI: `LIVE` and listening time, no bar, no duration; `reconnecting…` while reconnecting. Before the first load in a process a station renders as any remote URL entry.
- `tenuto play <url>`: `status_line` shows `live` in place of a duration and `reconnecting` for the new state. `--probe-only` reports `continuity: indefinite` and exits zero.

## 11. Out of scope

ICY now-playing titles (§12), `.m3u`/`.pls`, a station library or a `--live` override for header-less streams, Shoutcast v1, HLS/DASH, Ogg/Opus stations beyond the enabled Symphonia features, time-shift buffering, provider-resolved media.

## 12. M7.1 follow-up: ICY metadata

Recorded so M7 leaves the right seams; implemented separately.

```
reqwest chunk ─▶ IcyDemux (http/icy.rs, pure state machine)
                   ├─▶ audio bytes ─▶ ByteChannel ─▶ Symphonia
                   └─▶ StreamTitle / StreamUrl ─▶ generation-keyed latest-value slot
                         ─▶ worker poll ─▶ PlaybackEvent::StreamMetadata { session_rev, title, url }
                         ─▶ Mirror ─▶ view
```

- `Icy-MetaData: 1` on an open-at-zero request only. Demultiplex iff the response carries a valid `icy-metaint`; `Accepted::Live` becomes `Live { metaint: Option<NonZeroU32> }` and `IcyFramingUnsupported` is deleted.
- `StreamMetadata` is ordinary, droppable, coalescible. Now-playing text is never persisted and is separate from the persisted station name.
- The title is cleared on every transport replacement — reconnect, Pause → Play, Stop → Play — though the logical session continues.
- Titles: UTF-8, falling back to Latin-1, then the existing control/bidi escaping.
- `ponytail:` ceiling — the title leads the audio by the buffered depth. Upgrade: tag each title with its byte offset and release it when the decoder has consumed it.

## 13. Deferred to the provider work (not M7)

Decisions reached in review; they need their own spec:

- Prerequisite fix, useful today: carry `Option<RemoteFailure>` through `SeekRejected`, the two seek-restoration failures in `seek_to`, and `restore()`'s reseek failure.
- Resolution uses a monotonic request token, cancelled by Stop, replacement, removal and Shutdown. Play/pause intent is honored through `Loaded`, not only when the resolver returns; Stop invalidates both the pending resolution and the replacement load's automatic start; a newer seek supersedes the old refresh destination. One refresh budget spans re-resolution and the replacement load. Checkpoint protection is preserved by that recovery operation specifically, not by every same-ID load.
- A seek that meets an expired transport retries the requested destination on the fresh transport. Fresh transport opens but the seek fails: one bounded restoration to the last played position, destination reported as rejected. Fresh transport fails: `Failed`, last played position retained.
- Typed `youtube:<id>` identity; transient `ResolvedMedia`; request headers from a narrow allowlist, dropped on cross-origin redirect; explicit rejection of active and upcoming live content alongside format and protocol validation, with `was_live` recordings accepted.
- Resolver implementation is decided by a time-boxed spike showing playback and seeking through Tenuto's real HTTP path, cancellation and resource bounds. If the supported playback path requires an attestation implementation beyond the spike's scope, the native experiment stops there — a project decision, not an impossibility claim. The application-facing contract is stable either way; the number of provider-local modules is not promised.

## 14. Validation

All against the local test server; no public network. "No request" always means no further request *for the cancelled operation*; a replacement `Load` issues its own.

| ID | Assertion |
| --- | --- |
| L1 | An ICY-header `200` with no length loads as `Indefinite` / `Unsupported` and plays |
| L2 | The same headers plus `Content-Length`, or a `206` starting at zero, still classify as live |
| L3 | `icy-metaint` → `IcyFramingUnsupported`; HLS content type → `UnsupportedLiveMedia` |
| L4 | ICY headers on `404`, `429`, `503` yield `Status` failures; `is_retryable` is false, true, true. On the initial open all three are `Failed` with one request |
| L5 | Finite file, finite chunked body and unresolved source behave as before (existing H-suite unchanged) |
| L6 | Server closes mid-stream: `Reconnecting`, then `Playing`; no `EndOfTrack`, `Ended`, `completed` or `Advance` |
| L7 | Listening time may advance after the disconnect while buffered audio plays, by at most the ring's content; it is then constant until step 7; the new generation is anchored at the captured value |
| L8 | Cancellation, for each of Pause, Stop, replacement Load and Shutdown, in each of five situations — waiting out backoff, waiting for headers, a stalled body read, a delivering body read, decoder priming: the command's own transition is observed within the handshake deadline, no `Playing` is announced, and no further request for the cancelled operation is issued |
| L9 | `503` on every reconnect → `Failed` when the injected budget expires, with `cause` the last `Status`; a following `Play` makes exactly one attempt. `404` on the first reconnect → `Failed` at once, one request |
| L10 | A server that accepts and closes after 2 s, repeatedly, exhausts the budget; 30 s of played audio (injected clock) clears the outage; an in-flight open is not cut short by the budget |
| L11 | Initial open failure is `Failed` with no retry |
| L12 | Pause: the server observes the close — also when the body read is stalled; the ring is discarded; Play issues a new request at byte zero; no pre-pause audio is played; `Playing` is announced only after priming. Thaw after a hook-parked pause announces nothing |
| L13 | **No seek**: across Stop → Play, Pause → Play, reconnect and device-fault `rebuild` on a live source, a counting `MediaSource` records zero `seek` calls and the server records no `Range` start above zero |
| L14 | Rejected commands are harmless: `submit_seek`, a `SeekBy` burst through `KeyRouter`, and `Restart`, through the public submission paths, each yield `SeekRejected`; audio continues, the server sees no new request, the state and the outage fields are unchanged, no `RestartEstablished`. The `submit_seek` half is repeated for a finite source on a server without ranges |
| L15 | Reconnect to a decoder with a different sample rate, and with a different channel count, plays correctly. A source that probes but fails while priming is a failed attempt: no `Playing`, next attempt scheduled |
| L16 | Checkpoints: none for a station on tick, pause, stop, track change or shutdown. Finite → live writes the outgoing finite checkpoint; live → finite writes none for the station; same-URL finite → live leaves the old checkpoint byte-identical |
| L17 | Continuity mismatch on each reopen path — reconnect, Pause → Play, Stop → Play, `Failed` → Play — fails with `ResourceChanged`; no `CapabilitiesChanged` with `Finite` is emitted, no checkpoint is written, listening time is retained. The symmetric finite → live reopen fails the same way |
| L18 | Space during `Reconnecting` pauses, through `PlayerRuntime` and through the direct-engine `TogglePause`; an explicit `Play` during `Reconnecting` changes nothing and does not reset the budget |
| L19 | A plain `Play` from the post-load `Paused` on a station takes a fresh open; `PlayLoaded` releases the primed transport |
| L20 | Process restart with a station active: entry restored, zero requests until Play |

Manual, recorded in `docs/m7-acceptance.md`: play the verified endpoint in Ghostty from the release build; pull the network for 20 s and for 6 min; pause for 2 min and resume; hammer the arrow keys while playing; quit and relaunch.

## 15. Documentation

`architecture.md` §1 (scope sentence, position contract, the network-activity rule reworded per §3.1), §7.1 (new state; out-of-band submission rules of §6.1), §8 (the `Indefinite` row now plays), §12; README and `reference.md` lines on live refusal and reconnection; `CHANGELOG`; `m1-known-debt.md` gains the Shoutcast v1 and header-less-stream limits.

## 16. Modules

Expected to change: `http/{response,service,source,error}`, `playback/{prepare,engine,state,wait}`, `session`, `application/{runtime,transport,view,seek}`, `tui/render`, `app.rs`, docs.

Expected to stay as they are, without promise: `media/*`, `queue`, `persistence/*`, `feed/*`, `subscription/*`, `library`, `http/{channel,document}`, `playback/{timeline,callback,handshake,resample,output,spectrum,link}`, `artwork`, `lifecycle`. Cancellation and hook reconciliation may reach `http/channel` and `playback/handshake`; the plan says so where it does.
