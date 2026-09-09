# Continuo M2 — durable playback state (design)

Status: approved and implemented.
Date: 2026-09-08. Revised the same day after design review, and again on
2026-09-09 after a third review; §18 records what changed and what was declined,
and §19 records the amendments adopted while implementing it.
Supersedes nothing. Refines `docs/architecture.md` §6 with the decisions that
section deferred to M2.

## 1. Understanding summary

- **What.** A versioned, atomically written JSON snapshot at the platform state
  path, so pausing, stopping, seeking, or quitting and relaunching the same local
  file resumes close to where the user left off.
- **Why.** M1 proved logical position survives *transport* recreation inside one
  process. M2 extends the same invariant across *process* restarts, completing
  step 11 of the architecture doc's acceptance scenario for local media.
- **Who.** A single local user running one `continuo play <file>` at a time.
- **Key constraint.** The playback engine stays ignorant of persistence. It
  publishes truth (`Progress`, `PlaybackEvent`); the application layer decides
  what to store and when; a writer thread owns the disk. No filesystem concern
  enters `playback::engine::Worker`.
- **Reuse over invention.** `Load { start_at }` already performs a cancellable
  refined seek and is the only resume path. `PlaybackCheckpoint` is the currency
  between session and persistence. `MediaId` is the key — architecture §5 already
  states its string serde exists to serve as a JSON map key.
- **Non-goals.** HTTP media or Range, feeds/RSS, SQLite, TUI, multi-file queue,
  cross-process locking, M1 cleanup, any decoder refactor toward M3.

## 2. Baseline

Verified before design, on the M1 tree at `e2164a5`:

```
99 passed · 0 failed · 1 ignored (device_smoke, needs real hardware)
cargo fmt --check                                          clean
cargo clippy --all-targets --all-features -- -D warnings   clean
```

Requires `libasound2-dev` on Linux.

## 3. Facts established by reading the M1 source

These are load-bearing for the design and were verified, not assumed.

| Fact | Evidence |
|---|---|
| `Load { media, source, start_at }` already performs a cancellable *refined* seek to `start_at` and adopts the landing via `adopt_preserved`. No new decoder path is needed for resume. | `engine.rs` `fn load` |
| `publish_progress` is called unconditionally on **every** worker loop pass, in **all** states; only recomputation from the timeline is gated on `Playing \| Paused`. The loop ticks every 10 ms (`TICK`). | `engine.rs:367`, `engine.rs:571`, `engine.rs:37` |
| A seek taken while **stopped** stores `requested_target` and emits `SeekTargetStored`, but does **not** move `self.position`. The target is validated later, in `restore()`. | `engine.rs:1328`, `engine.rs` `fn restore` |
| `Play` from `Ended` refuses to restart implicitly and only warns, so no spurious `Playing` can follow an `EndOfTrack`. | `engine.rs:1193` |
| `restore()` — the `Play` path out of `Stopped`/`Paused` — adopts any stored target, emits `SeekCompleted`, then `announce_playing()`. | `engine.rs` `fn restore` |
| `Restart` dispatches to `restart()`, **not** `restore()`. `restart()` clears `requested_target`, seeks to zero and reaches `Playing` through `announce_playing()` alone: no `SeekCompleted` is ever emitted for it. And `set_state` is a no-op when the state is unchanged, so a `Restart` taken while already `Playing` emits **nothing at all**. | `engine.rs:1103`, `engine.rs` `fn restart`, `engine.rs:529`, `engine.rs:536` |
| `PlaybackCheckpoint` exists (`media`, `position`, `updated_at`) and is unused by runtime code — only a serde round-trip test. Ready to reuse as-is. | `playback/checkpoint.rs`, `tests/domain_values.rs` |
| `PlaybackEvent::Loaded` carries no position, and `app.rs:192` zeroes position on `Loaded`. | `playback/event.rs`, `app.rs:192` |
| Within one worker pass the order is: interrupts → `publish_progress` → `flush_events` → dispatch one command. `emit` only ever appends to `pending_events`; it never sends to the channel directly. So an event produced by a command becomes visible to the app on the *following* pass, after that pass has already published progress for the state the command established. | `engine.rs` `fn run`, `fn emit`, `fn flush_events` |
| `shutdown()` calls `capture_position()` and then returns without publishing; `run()` returns immediately after. The last `Progress` a reader can see is the one published on the previous pass, so the captured final position is currently unreachable. | `engine.rs` `fn shutdown`, `fn run` step 1 |
| `load()` calls `capture_and_teardown()` and then overwrites `self.position` with `start_at` before anything publishes, so the *outgoing* media's final position never reaches `Progress` at all. | `engine.rs` `fn load` |
| `do_stop` bumps `session_rev`; `pause` does not. Neither zeroes the position. | `engine.rs:1310`, `engine.rs` `fn pause` |
| `SetVolume` with no transport stores `self.volume` and emits `VolumeChanged`; a transport created later adopts it through `link.set_gain`. Volume can therefore be restored before `Load`. | `engine.rs:1104`, `engine.rs:749` |
| `EngineHandle::join` consumes the handle, so `progress()` cannot be called after it. Every call site is the statement `handle.join();`. | `engine.rs` `fn join`, `app.rs:133`, `tests/support/mod.rs:671,770` |
| `load()` ends with `set_state(PlaybackState::Paused)`, so **every** launch emits a `StateChanged{Paused}` that no user asked for, before the queued `Play` is dispatched one pass later. | `engine.rs:1181` |
| `rebuild()` bumps `session_rev` and emits `DeviceRecovered` on a successful recovery, and the position is continuous across it (`adopt_preserved` on the reseek). A revision bump therefore does **not** imply the media changed. | `engine.rs:701`, `engine.rs` `fn rebuild` |
| `emit` drops non-terminal events once `pending_events` reaches `PENDING_CAP`. `DeviceRecovered` is non-terminal, so a revision bump can be invisible to the app until some later event carries the new number. | `engine.rs:432`, `engine.rs:42` |
| The worker's shutdown path never flushes: `run()` step 1 calls `shutdown()` and returns, and `shutdown()` does not call `flush_events()`. Whatever stands in `pending_events` at that moment is discarded, not delivered. | `engine.rs:353`, `engine.rs` `fn shutdown`, `fn flush_events` |
| `app::run` breaks the loop on `Shutdown` **before** that iteration's event drain, so events already sitting in the channel when `q` is pressed are never observed. An event produced by a command becomes sendable one pass (10 ms) later — well inside one poll window. | `app.rs:80`, `app.rs:101`, `engine.rs:37` |
| The event channel holds `EVENT_CAPACITY` = 64; `pending_events` holds up to `PENDING_CAP` = 128. A shutdown flush through the channel alone therefore cannot be lossless, and it cannot block: the app is inside `join` and no longer draining. | `engine.rs:41`, `engine.rs:42`, `engine.rs:145` |
| `render(&mirror)?` propagates out of `app::run` from inside the loop, skipping the post-loop `interrupt_shutdown` / `join`. It is the only exit that does. | `app.rs:122`, `app.rs:131-133` |

## 4. Mismatches between the M2 brief and the repository (repository wins)

1. **Snapshot shape.** §6 specifies one checkpoint *per media identity* — a map.
   The brief sketched a single `Option`. Map adopted (D2).
2. **Field name.** §6 says `schema_version`; the brief said `version`.
   `schema_version` adopted.
3. **fsync.** §6 mandates fsync(temp) → rename → fsync(parent); the brief said not
   to overengineer durability. §6 adopted; it is roughly ten lines.
4. **Rejected files.** §6 requires malformed and unsupported-version files be
   *preserved and reported rather than overwritten* — stricter than "do not
   panic". Adopted, and split by cause (D3).
5. **Completion.** §6 defers the policy to M2 *and* states "there is no near-end
   reset". The brief recommended completed → restart from zero. Resolved as D1.

## 5. Pre-identified debt this milestone triggers

`docs/m1-known-debt.md` records:

> After a discard timeout during a seek, `seek_to` still emits
> `SeekCompleted { actual }` even though `rebuild` has restored the pre-seek
> position, so the event and the position disagree. **Fix before anything treats
> `actual` as authoritative (a checkpoint writer, a UI seek bar).**

This design **sidesteps rather than fixes** it: `SeekCompleted.actual` is never
persisted. The event only marks state dirty; the position persisted is the next
canonical `Progress.position` with a matching `session_rev`. The debt entry
stands for a future UI seek bar.

## 6. Decisions

| # | Decision | Chosen | Rejected | Why |
|---|---|---|---|---|
| D1 | Completion semantics | `EndOfTrack` sets `completed = true` and **retains** the position. Reopening a completed entry starts at 0. | Zeroing position on completion; dropping the entry | Satisfies "do not resume at EOF" while honoring §6's "no near-end reset"; retains played history for M4 |
| D2 | State shape | Map keyed by `MediaId`, plus `current_media`, `volume`, `schema_version`. Capped at 512. | Single `Option`; uncapped map | §6 mandates per-identity checkpoints; §5 built `MediaId` serde for JSON map keys; a cap bounds file growth |
| D3 | Rejected state files | Classified from a **version envelope read before the model** (§13). Malformed → quarantine aside, keep writing. `schema_version != 1` → preserve in place, **disable writing**. A quarantine that cannot be performed → also disable writing. | Uniform quarantine; uniform disable; deserializing the model first | Garbage should not cost a session of persistence; a newer build's state must survive a downgrade; §6 says rejected files are preserved, and the only way to preserve one whose quarantine failed is to stop writing |
| D4 | Ordering | Persistence-owned monotonic `touch_seq`, assigned only on an accepted `record`. `updated_at` stays inspection-only. | Ordering by `updated_at`; ordering by max position | §6: wall clocks do not order updates, and a deliberate backward seek supersedes a larger position |
| D5 | Coalescing | 2 s is a **maximum age**. Deadline is anchored when the first snapshot enters an empty slot; replacement never extends it. Forced checkpoints bypass it. | A minimum write spacing (floor) | A floor delays the shutdown flush and buys nothing: forced writes are human-paced, ordinary ones already gated by the 5 s capture interval |
| D6 | Seek position source | `SeekCompleted` raises a pending force only (D13); the next `Progress.position` with a matching `session_rev` is persisted. | Persisting `SeekCompleted.actual` | Avoids the M1 debt above and matches "position comes from the engine's canonical position" |
| D7 | Stopped-seek target | `SeekTargetStored.target` **is** persisted — a narrow, named exception to D6, held against later overwrites by D17. | Persisting canonical position only; adding a `validated` flag | It is user intent, not a decoder claim; `Load { start_at }` revalidates it through the same path `restore()` would have used |
| D8 | Engine change | `PlaybackEvent::Loaded` gains `position: Duration`, the adopted landing. | Leaving the engine untouched | Without it the app cannot report where a resume landed, and would render 00:00:00 after a resume |
| D9 | Writer topology | Dedicated thread with a keep-latest slot (Approach A). | B: synchronous write on the app thread. C: policy inside the writer thread. | B is incompatible with §2's execution-context requirement and the responsiveness invariant. C makes completion and media-switch semantics testable only through threads and real time |
| D10 | Writer shutdown | Bounded: ACK channel plus `recv_timeout(2 s)`; on timeout report unconfirmed and **detach**. | Unconditional `join` after the timeout | An unconditional join defeats the bound it was meant to enforce |
| D11 | Failure policy | Log, retry at cadence; one `tracing::warn!` from the writer thread after 3 consecutive failures; never fatal, never self-disabling. No writer→app channel. | A writer-status channel plus a rendered warning line; failing the run; disabling persistence on error | Transient `ENOSPC`/`EIO` should recover on its own. Every other diagnostic in this app is a `tracing` record on stderr; a persistence warning is not the place to invent the notification surface the app does not yet have (§17). Only D3 disables writing |
| D12 | Permissions | Unix: dir `0700`, destination and temp `0600`, set explicitly. | Relying on umask | A permissive umask should not expose the user's listening history |
| D13 | Position for event-driven forces | `Session` retains `last_sample` — the `(session_rev, media, position)` of the most recent accepted `tick`. An event that forces a checkpoint but carries no position marks a **pending force** keyed by the revision the event itself carries; the `tick` of the *same* app iteration resolves it. A newer revision **re-keys** the force; only `Loaded` drops it. `Paused` raises a force only from `Playing`. | `observe` returning a snapshot it has no position for; transition events carrying a captured position; dropping the force on any newer revision | `observe(event)` runs before the iteration's single `Progress` sample, so it has no position to use. §3's pass ordering guarantees the sample that follows an event is *newer* than the transition the event reports. Dropping on any bump would lose a real pause to a device recovery (§3: `rebuild` bumps the revision with the position continuous across it); gating `Paused` on `Playing` excludes the establishment `Paused` that `load()` emits on every launch |
| D14 | Shutdown position | `engine.shutdown()` publishes once after `capture_position()`, and `EngineHandle::join` returns that final `Progress`. | Accepting the last pre-shutdown sample; an engine shutdown result/ack channel | Two one-line engine changes beat a new channel, keep the engine ignorant of persistence, and turn "final `Progress` sample" from a claim into a fact. `join` already establishes the happens-before |
| D15 | Volume | Persisted as a bare `f32`; `VolumeChanged` marks state dirty at **ordinary** urgency; restored by a `SetVolume` issued **before** `Load` (§11). | Dropping `volume` from M2; forced urgency | §6 lists volume among the file's contents, so dropping it contradicts the architecture. Holding `-`/`+` emits a burst of `VolumeChanged`; ordinary urgency lets D5's coalescing absorb it, and shutdown forces the last one out |
| D16 | Clock | The injected clock yields `ClockSample { monotonic: Instant, wall: OffsetDateTime }`. Deadlines and intervals use `monotonic`; `updated_at` uses `wall`. | A single wall clock; a single monotonic clock | A wall-clock step backwards must not stall the 5 s capture or the 2 s coalesce, and a monotonic instant cannot be written into an RFC 3339 field |
| D17 | Outstanding stopped-seek target | The target from `SeekTargetStored` is retained by the session and **supersedes `Progress.position`** for that media until the engine **resolves** it — `SeekCompleted`, `StateChanged{Playing}`, `Loaded` or `EndOfTrack`. | Persisting the target once and letting later triggers overwrite it; clearing on adoption alone (`SeekCompleted`, `Loaded`, `EndOfTrack`) | The engine deliberately does not move `self.position` for a stopped seek (§3), so any position-derived checkpoint that follows — in practice the shutdown force — writes the pre-seek position back over the target. Without this, D7 is defeated by the very sequence it exists for: stop, seek, quit. Resolution is not the same as adoption: `restart()` **discards** the target and starts at zero, announcing `Playing` with no `SeekCompleted` (§3). §12 already treats `Playing` for the current media as the observable establishment; D17 reuses that signal rather than inventing a second one |
| D18 | Flush path | The engine shutdown and final-checkpoint sequence is reached by **every** exit from `app::run`, including a render error. | Leaving `render(&mirror)?` to return early, as M1 does | A terminal write failure is not a reason to skip the final checkpoint, and §9's sequence is only a guarantee if nothing can bypass it |
| D19 | Shutdown event handoff | `Worker::run` returns its undelivered `pending_events`; `EngineHandle::join` joins the thread, then drains the event channel, and returns `ShutdownReport { progress, events }` — channel events first, worker backlog after. The app replays them through `Session::observe` **before** `shutdown_snapshot`. | Leaving the drain to the app loop; a best-effort `try_send` flush inside `fn shutdown`; a shutdown result channel | Two independent losses (§3): the worker discards `pending_events` when the shutdown interrupt fires, and `app::run` breaks before its own drain. Either can swallow the `SeekTargetStored` that D7 and D17 exist for, or the `EndOfTrack` that sets `completed`, and the final `Progress` carries none of those facts. A channel flush cannot be lossless (64 slots against a 128-deep backlog) and must not block, because the app is inside `join`. The thread's own join value is lossless, ordered and already the happens-before D14 relies on |
| D20 | Establishment gate on the shutdown force | The shutdown force records a checkpoint **for the current media** only once the session has observed, at the current revision, a `StateChanged{Playing}`, `SeekCompleted`, `SeekTargetStored` or `EndOfTrack`. `Loaded` clears the flag rather than setting it. `volume` and `current_media` are ungated. | An unconditional shutdown checkpoint; a guard written specifically for `completed` entries | §11 maps `completed == true`, `position == duration` and `position > duration` all to `start_at = 0` while retaining the position, and `load()` emits `Loaded` *before* opening the device — so a device-open failure ends at `Failed` with `Progress.position == 0`, and the shutdown force writes that zero over the retained position. §8's `Paused` gate blocks one overwrite of exactly that value; this blocks the other, and generalizes to any launch that quits before playback ever established |

## 7. Architecture

```
app::run loop (every ~100 ms)
  ├─ drain PlaybackEvents (lossless)  ──► Session::observe(event) ──► Action
  └─ sample engine.progress() ONCE    ──► Session::tick(&progress, clock) ──► Action
                                                    │
                                         Action::Submit { state, urgency }
                                                    ▼
                              WriterHandle::submit  (keep-latest slot, never blocks)
                                                    ▼
                                    writer thread ──► StateStore::write ──► state.json
```

`Progress` is **sampled, not forwarded**: the app already reads the keep-latest
snapshot once per poll iteration and hands that one sample to the session.
Intermediate positions are discarded by design. The session ignores any sample
whose `session_rev` differs from the revision learned from the event stream —
the same guard `Mirror` already applies.

**Session state.** Besides the authoritative `PersistedState`, `Session` keeps a
little working state, all of it derived from the event stream:

- `session_rev` — adopted from **every** observed event, exactly as
  `Mirror::apply` already does, and not only from the events the policy acts on.
  §3 records that `emit` drops non-terminal events when `pending_events` is full,
  so a `DeviceRecovered` can go missing; a session whose revision has fallen
  behind rejects every sample that follows, which would silently suspend
  checkpointing for the rest of the run. Adopting from every event bounds that
  exposure to "until the next event of any kind".
- `state` — the last `PlaybackState` observed. Both the 5 s ordinary trigger and
  §12's `completed` rule are gated on it, and so is the `Paused` force (§8).
- `current_media` — from `Loaded`; the key `record` and `checkpoint_for` use.
- `last_sample: Option<(session_rev, MediaId, Duration)>` — updated by every
  `tick` whose sample passes the revision guard. It is the position used for any
  checkpoint that must describe a moment the session can no longer sample: the
  outgoing entry on a media switch (§3: `load()` overwrites `self.position`
  before publishing, so the engine cannot supply it).
- `pending_force: Option<(session_rev, Trigger)>` — set by `observe` for the
  events that force a checkpoint without carrying a position (D13).
- `outstanding_target: Option<Duration>` — set by `SeekTargetStored`, cleared by
  `SeekCompleted`, `StateChanged{Playing}`, `Loaded` or `EndOfTrack`. While it is
  set it supersedes `Progress.position` for the current media (D17). The
  `Playing` clause is the one that covers `Restart`, which discards the target
  and announces `Playing` without a `SeekCompleted` (§3).
- `established: bool` — cleared by `Loaded`, set by the first
  `StateChanged{Playing}`, `SeekCompleted`, `SeekTargetStored` or `EndOfTrack`
  observed at the current revision. It gates the shutdown checkpoint for the
  current media (D20) and nothing else.

`pending_force` is keyed by the `session_rev` **the event itself carries** — stop
and device recovery both bump it — and is resolved by the first `tick` sample
carrying that revision, normally the one later in the same iteration. When a
subsequent event carries a newer revision the force is **re-keyed** to it, not
dropped: §3 records that `rebuild` bumps the revision on device recovery with the
position continuous across it, so the force is still answerable and dropping it
would lose a real pause to an unrelated fault — and lose it for good, because no
ordinary trigger fires while paused. `Loaded` is the one exception: it replaces
the media, so a force raised against the previous one is dropped, and the
`Loaded` handling has already recorded the outgoing entry from `last_sample`.
Liveness is the engine's: `publish_progress` runs on every 10 ms pass in all
states, so a matching sample is never more than one tick away.

**Ownership boundary.** `Session` owns the authoritative `PersistedState`.
`Action::Submit` carries a **clone**, which becomes the writer's property; the
session never shares a reference into live state. At ≤ 512 small entries the
clone is a trivial `BTreeMap` copy on the app thread, nowhere near the audio path.

## 8. Checkpoint policy

| Trigger | Urgency | Position source |
|---|---|---|
| 5 s elapsed while `Playing` | Ordinary | the tick's `Progress.position` |
| `StateChanged{Paused}` **from `Playing`** | Forced, pending | pending force; resolved by the tick sample carrying the same `session_rev` (D13) |
| `StateChanged{Stopped}` | Forced, pending | as above |
| `SeekCompleted` | Forced, pending | as above — never `SeekCompleted.actual` (D6) |
| `SeekTargetStored` | Forced | the stored target, carried by the event (D7) |
| `VolumeChanged` | Ordinary | none — updates `volume`, touches no checkpoint (D15) |
| `Loaded` for a **different** media | Forced | **one** snapshot: the outgoing entry is recorded from `last_sample` and `current_media` moves, in a single mutation submitted once |
| `EndOfTrack` | Forced | event `position`, plus `completed = true` |
| graceful shutdown, `established` | Forced | the final `Progress` from `EngineHandle::join` (D14) — unless `outstanding_target` supersedes it (D17) |
| graceful shutdown, not `established` | Forced | no checkpoint for the current media; `volume` and `current_media` are still written (D20) |

**Why a pending force is not a delay.** §3 establishes the worker's pass order:
a command's event is flushed on the pass *after* the one that applied it, and
that pass publishes progress before it flushes events. So by the time the app
can drain `StateChanged{Paused}`, `StateChanged{Stopped}` or `SeekCompleted`,
the progress slot already holds a position taken *after* the transition. The
app's own order — drain events, then sample once — therefore resolves the
pending force within the same iteration, using a position that is fresher than
the transition rather than older than it. Nothing is ever deferred to a later
poll: a force that outlives its revision is re-keyed, not queued, and the only
force that never resolves is one a `Loaded` retires.

**Why `Paused` is gated on `Playing`.** §3 records that `load()` ends with
`set_state(Paused)`, so every launch emits a `StateChanged{Paused}` before the
queued `Play` is dispatched. Ungated, that would force a checkpoint from the
resume landing on every launch — and for an entry with `completed == true`, §11
resumes at 0, so the forced write would record `position: 0` and destroy the
position D1 deliberately retains, before the user has pressed anything. A pause
that interrupts no playback is not a checkpoint. `Stopped` needs no such gate:
`do_stop` returns early from `Idle`, `Stopped` and `Failed`, so the event only
exists when something was actually running.

For pause specifically the sampled position is deliberately a little ahead of
the instant the park was requested: `publish_progress` keeps recomputing while
`Paused` because frames already handed to the device go on playing for one
output latency. That is the position the listener actually heard, which is the
one worth resuming from.

**Why an outstanding stopped-seek target wins (D17).** A seek taken while
stopped stores a target and leaves `self.position` where it was (§3). The target
is persisted immediately by its own row, but the engine's canonical position
still reads the pre-seek value, so the next position-derived checkpoint writes it
back. While stopped that is exactly one checkpoint — the shutdown force — and it
lands on the sequence D7 was written for: play to 93 s, `s`, `←`, quit. The
session therefore keeps the target and persists it in place of
`Progress.position` until the engine **resolves** it, at which point
`SeekCompleted`, `StateChanged{Playing}`, `Loaded` or `EndOfTrack` clears it and
the canonical position is authoritative again.

Resolving is not the same as adopting, which is why `StateChanged{Playing}` is in
that set. `Restart` runs `restart()`, not `restore()`: it **discards**
`requested_target`, seeks to zero and announces `Playing`, emitting no
`SeekCompleted` (§3). Without the `Playing` clause, `stop → seek to Y → Restart →
play → quit` persists Y — and not only at quit, because D17 supersedes
`Progress.position` for as long as the target stands, so every ordinary
checkpoint of the restarted playback persists Y too. A `Restart` taken while
already `Playing` emits nothing at all (§3), which is sound here: a target can
only be stored from `Idle` or `Stopped`, and both exits from `Stopped` —
`restore()` and `restart()` — pass through `Playing`.

**Why the shutdown checkpoint is gated on establishment (D20).** `load()` pins
`self.position = start_at` *before* opening the decoder or the device, so a load
that fails still reports the position that was asked for, and re-persisting it
costs nothing. That holds only where `start_at` equals the position on disk. §11
maps three rows to `start_at = 0` while a position is retained: `completed ==
true`, `position == duration`, and `position > duration`. For those, a load that
reaches `Loaded` and then fails to open the device ends at `Failed` with
`Progress.position == 0`, while the session already knows the media and the
revision. An ungated shutdown force writes that zero over the retained position —
the same destruction §8's `Paused` gate exists to prevent, arriving by the other
door.

So the gate is not written for failed device opens in particular: the session
records no checkpoint for the current media at shutdown until it has observed, at
the current revision, either an establishment (`StateChanged{Playing}`) or an
explicit position change (`SeekCompleted`, `SeekTargetStored` or `EndOfTrack`).
`Loaded` does not establish — it is what *clears* the flag, for the same reason
§12 refuses to clear `completed` on it. This also keeps launch-then-immediate-quit
from spending a `touch_seq` and an `updated_at` on a checkpoint that records
nothing the file did not already hold. `volume` and `current_media` stay ungated:
neither is a position claim, and D15 already gives volume its own trigger.

**Why the media switch is one snapshot, not a flush then a move.** A keep-latest
slot cannot promise that an intermediate submission reaches disk — a second
submission simply replaces it — so "flush the previous entry before
`current_media` moves" is unenforceable without an ACK the writer does not owe
the app. It is also unnecessary: the snapshot is the whole `PersistedState`, so
recording the outgoing checkpoint and moving `current_media` in one mutation
makes the two atomic by construction. The M2 CLI loads exactly one file, so this
row is reached only at startup, where there is no outgoing entry.

Worst-case loss: **5 s capture + 2 s coalesce = 7 s**, both single-digit as §6
requires.

## 9. Writer thread

```
submit(state, Ordinary) into EMPTY slot     -> slot = { state, deadline: now + 2s }
submit(state, Ordinary) into OCCUPIED slot  -> replace state; DEADLINE UNCHANGED
submit(state, Forced)   any slot            -> replace state; deadline = now
```

There is deliberately **no minimum write spacing** (D5).

**Ordering.** One producer, one consumer, replace-on-submit: the newest snapshot
structurally wins. Made checkable rather than assumed — each submission carries an
in-memory `submit_seq`, and the writer drops any snapshot whose `submit_seq` is
≤ the last written. `submit_seq` is **not** persisted; §6 excludes session
counters from the file.

**Failure.** The writer holds the snapshot it is writing *outside* the slot, so
the producer can fill the emptied slot while the I/O is in flight. On failure the
writer therefore re-inserts its snapshot **only if the slot is still empty**,
with `deadline = monotonic_now + 2 s`; if a newer snapshot has arrived, that one
supersedes it and the failed snapshot is dropped. Keep-latest is never violated
by a retry, and the retry still happens — with better data. The consecutive
failure counter lives on the writer, not on the snapshot, so it survives the
replacement and still trips D11's warning at three.

**Shutdown.**

```
app  : engine.interrupt_shutdown(); commands.send(Shutdown)
app  : let report = engine.join()    // audio is fully torn down here; the worker
                                     // published its captured position before
                                     // returning (D14), and the report carries
                                     // every event still undelivered (D19)
app  : for event in report.events { session.observe(event) }
                                     // reconciliation, not submission: any
                                     // Action returned here is folded into the
                                     // single snapshot below
app  : session.shutdown_snapshot(&report.progress, clock.sample()) -> forced submit
                                     // uses outstanding_target over
                                     // progress.position when one stands (D17);
                                     // writes no checkpoint for the current
                                     // media when it never established (D20)
app  : drop(raw_mode_guard)          // restore the terminal before waiting
app  : writer.shutdown()             // sets Closing; further submits rejected
write: take pending -> final write -> send result on ack (bounded 1) -> exit
app  : ack.recv_timeout(2s)
         Ok(Ok(()))  -> "final checkpoint written"
         Ok(Err(e))  -> log; exit
         Err(_)      -> log "final checkpoint UNCONFIRMED"; DETACH; exit
```

**Every exit runs this sequence** (D18). In M1 the loop has three exits — a
`break` with `Ok`, a `break` with `Err`, and `render(&mirror)?`, which returns
from `app::run` outright (§3). Only the first two reach the code above. The
render error must therefore be folded into the loop's outcome and broken on, like
the `Failed` path already is, so that no exit can skip the final checkpoint. This
is the one place where the app change is a restructure rather than wiring.

**The event handoff (D19).** The final `Progress` carries a position and a
revision and nothing else — not the seek target a stopped seek stored, not the
`completed` an `EndOfTrack` sets, not the last `VolumeChanged`. Those facts exist
only as events, and §3 records two independent ways they are lost at quit: the
worker discards `pending_events` when the shutdown interrupt fires, and
`app::run` breaks before its own drain, leaving whatever the channel already
holds unread. Both are reachable in one keystroke pair — `←` then `q` inside a
single poll window — and both lose exactly the `SeekTargetStored` that D7 and D17
were written for.

Flushing through the channel at shutdown cannot fix it: the channel holds 64
against a backlog of up to 128 (§3), and the flush must not block, because the
app is inside `join` and has stopped draining. So the backlog travels out through
the thread's own join value instead. `Worker::run` returns its undelivered
`pending_events`; `EngineHandle::join` joins the worker — after which nothing can
send again — drains the channel, and returns both, channel events first and
backlog after, which is the order they were emitted in. The app replays them
through `Session::observe` before building the final snapshot, so the snapshot is
built from a session that has seen everything the run produced. This is only
reconciliation: `observe` may return `Action::Submit`, and those are discarded,
because the single forced submit that follows supersedes every one of them.

CPAL teardown never waits on any of this — it has already completed inside
`join` by the time the final snapshot is submitted. If `report.progress.session_rev`
does not match the revision the session learned from the event stream — including
from the replayed backlog, which can carry the very event that advances it — the
session falls back to `last_sample`; it never persists a position from a session
it was not tracking.

## 10. State model

```json
{ "schema_version": 1,
  "current_media": "local:/music/a.flac",
  "volume": 1.0,
  "checkpoints": {
    "local:/music/a.flac": { "position": { "secs": 93, "nanos": 0 },
                             "completed": false,
                             "touch_seq": 41,
                             "updated_at": "2026-09-08T10:00:00Z" } } }
```

- `touch_seq` is assigned **only** by `PersistedState::record` — an accepted
  update. Loading, reading, or restoring never touches it.
- `next_seq` is **derived, not stored**: `1 + max(touch_seq)` computed once at
  load and cached in a `#[serde(skip)]` field, so a hand-edited or truncated file
  cannot produce a regressing or duplicate sequence.
- **Cap = 512 counting the current entry.** On insert at cap, evict the lowest
  `touch_seq` among **non-current** entries; `current_media`'s entry is never
  evictable. Guarded for the degenerate all-current case, which cap > 1 makes
  unreachable.
- `Duration` keeps its default `{ secs, nanos }` serde, matching the existing
  `PlaybackCheckpoint` round-trip test.
- `volume` is a bare `f32`, written from `Volume::as_gain` and read back through
  `Volume::new`, which already clamps to `[0, 1]` and maps non-finite input to
  `0.0` — so a hand-edited `2.0` or `-1.0` cannot deafen or silence the user, and
  no separate validation rule is needed. A missing field defaults to
  `Volume::FULL`, the value M1 starts at. `Volume` itself gains no serde derive:
  the persisted representation stays a number owned by the persistence model.

## 11. Resume validation

Applied in `app::run` against the duration from the probe it **already performs**
before spawning the engine — no extra file open.

| Persisted state | `start_at` | Log |
|---|---|---|
| no file / no entry for this media | `0` | debug |
| `completed == true` | `0` | info — resume declined, media completed |
| incomplete, `position == 0` | `0` | — |
| incomplete, `0 < position < duration` | `position` | info — resume position selected |
| incomplete, `position == duration` | `0` | debug — degenerate start, see below |
| incomplete, `position > duration` | `0` | warn — stale state, position past end |
| duration unknown | `position` | info — retained, unvalidated |

No near-end heuristic anywhere. `position == duration` is preserved in *storage*
as distinct from `completed`, but is not a usable *start*: seeking exactly to EOF
is degenerate. Completion is never inferred from `position >= duration`.

If the restore seek fails, the engine emits `Failed` with the requested position
intact and the application surfaces it. A failed resume is never reported as a
successful one.

**Initial command sequence.** Restored volume is applied first:

```
SetVolume(restored)                      // no transport yet: stores the gain and
                                         // emits VolumeChanged, so the mirror
                                         // shows it from the first frame drawn
Load { media, source, start_at }         // the transport it builds adopts the
                                         // stored gain via link.set_gain
Play
```

Ordering `SetVolume` before `Load` is what keeps the restored level from being
audible only after the first buffer: §3 records that the engine accepts the
command with no transport and that a transport created later adopts the value.
Nothing about this sequence is conditional — an absent or default volume issues
the same command with `Volume::FULL`.

## 12. Clearing `completed`

> `completed` is cleared when a new playback establishment succeeds after a
> completed state — successful restart, seek, or media replacement. Persistence
> restoration alone does not clear it.

Observable as `SeekCompleted` for the current media, **or** `StateChanged{Playing}`
for the current media. `Loaded` alone does not clear it. Safe because `Play` from
`Ended` refuses to restart implicitly (§3), so no spurious `Playing` follows an
`EndOfTrack`.

The `Playing` half is load-bearing rather than a second opinion: the restart this
rule exists to catch reaches `Playing` through `restart()`, which emits no
`SeekCompleted` at all (§3). D17 and D20 lean on the same signal for the same
reason, and the three should move together if it ever changes.

## 13. Store

- **Path.** `directories::ProjectDirs::state_dir()` (Linux, honors
  `XDG_STATE_HOME`), falling back to `data_local_dir()` on macOS and Windows
  where `state_dir()` is `None`. `StateStore` takes an explicit path; only `app`
  resolves the platform one, so tests never touch `$HOME`.
- **Permissions.** Unix: directory `0700`, destination and temp `0600`, set
  explicitly via `OpenOptionsExt::mode` and `set_permissions`. The temp file
  carries the private mode *before* any content is written. Non-Unix: platform
  defaults, documented as a known gap.
- **Load order.** Read the bytes → deserialize a **minimal envelope** carrying
  `schema_version` alone, all other fields ignored → only then deserialize the
  full model. If the envelope fails to parse, or the field is absent, the file is
  *malformed*. If `schema_version != 1` — not merely `> 1` — the file is
  *unsupported*: preserved in place, writing disabled. A version-1 file that then
  fails full deserialization is malformed. Deserializing the model first would
  misclassify a valid future file whose shape has changed as garbage and move it
  aside, which is exactly what §6 forbids; the envelope is the only thing whose
  shape every future version is obliged to keep.
- **Names.** Filesystem-safe, no colons: temp `state.json.tmp-<pid>-<seq>`,
  quarantine `state.json.rejected-20260908T143211Z`. On `AlreadyExists`, append
  `-2`, `-3`, … up to 100, then stop rather than clobber an existing rejected
  file.
- **A quarantine that cannot be performed disables writing** for the session,
  exactly as the unsupported-version case does — whether the rename failed or all
  100 candidate names were taken. Skipping the quarantine and continuing to write
  would overwrite on the next checkpoint the very file §6 requires be preserved,
  which is worse than losing a session of persistence. The session runs normally
  with empty in-memory state; only the disk write is suppressed, and the reason
  is logged once.
- **Atomic write.** temp in the destination directory → write → fsync(temp) →
  rename over target → fsync(parent) where supported. A parent fsync failure is
  logged, not fatal.

## 14. Components

**Added**

| File | Responsibility |
|---|---|
| `src/persistence/model.rs` | `PersistedState`, `PersistedCheckpoint`, `SCHEMA_VERSION`, `record`, eviction, `checkpoint_for`, derived `next_seq` |
| `src/persistence/store.rs` | `StateStore` — `load() -> LoadOutcome`, `write()`; atomic replace, permissions, quarantine |
| `src/persistence/writer.rs` | `WriterHandle::{submit, shutdown}`, keep-latest slot, coalescing thread, ACK |
| `src/persistence/mod.rs` | Re-exports, `PersistenceError` |
| `src/session.rs` | `Session::{observe, tick, shutdown_snapshot}` — pure policy; tracked `session_rev`/`state`/`current_media`, `last_sample`, `pending_force`, `outstanding_target`, `established`, injected clock |
| `src/clock.rs` | `Clock` trait yielding `ClockSample { monotonic, wall }`; real and fake implementations (D16) |

`PlaybackCheckpoint` is the currency: `Session` builds one, `record(checkpoint,
completed)` stores it, `checkpoint_for(&media)` returns one. The map is a storage
layout, not a second concept, and `PlaybackCheckpoint` is itself unmodified so
`tests/domain_values.rs` keeps compiling.

**Changed.** `src/lib.rs` (+3 modules) · `src/app.rs` (wiring, plus the one
restructure D18 requires: `render` no longer returns from `run` with `?`) ·
`src/playback/event.rs` (`Loaded { position }`) · `tests/support/mod.rs`
(**additive** `TestEngine::start_at`; existing `start` untouched) · `Cargo.toml` ·
`README.md` · `docs/architecture.md` §6.

`src/playback/engine.rs` takes exactly four changes, none of which know that
persistence exists:

1. one line populating `Loaded { position }` (D8);
2. one line — `self.publish_progress()` at the end of `fn shutdown`, after
   `capture_position` and `teardown`. With the transport already gone the
   recompute branch is skipped, so it publishes precisely the captured position
   (D14);
3. `fn run` returns its undelivered `pending_events` instead of `()` — four
   `return` sites, each yielding the backlog it was about to discard (D19);
4. `fn join(mut self)` returns `ShutdownReport { progress: Progress, events:
   Vec<PlaybackEvent> }` instead of `()`: join the worker, take the backlog from
   the join value, then drain the event channel — a drain that is only safe
   because the thread has already gone, which is the same happens-before D14
   rests on (D19). Every existing call site is the statement `handle.join();`,
   which compiles unchanged against a returning function, so this stays additive
   in practice.

**Explicitly unchanged.** The worker's decode/transport logic, `timeline`,
`callback`, `handshake`, `link`, `decode`, `resample`, `output/*`,
`checkpoint.rs`, and all 20 engine contract tests.

## 15. Error model

```rust
pub enum PersistenceError {
    Io { path: PathBuf, op: &'static str, source: io::Error },
    Serialize { path: PathBuf, source: serde_json::Error },
    Deserialize { path: PathBuf, source: serde_json::Error },
    UnsupportedVersion { path: PathBuf, found: u32, supported: u32 },
    NoStateDirectory,
}
```

No stringly-typed variants — the trap `m1-known-debt.md` records for
`PlaybackError::Failed`. `serde_json` moves from dev-dependency to runtime
dependency; `directories` is new; `tempfile` is a new dev-dependency.

## 16. Test strategy

Roughly 85 new tests. Policy tests call `Session::observe`/`tick` synchronously
with a fake clock — no threads, no tempdirs, no sleeps. Store tests use
`tempfile`. Resume contract tests pair `TestEngine` with a `StateStore` in a
tempdir to stage session 1 → persist → session 2.

- **Model.** round-trip; `schema_version` present; `touch_seq` assigned on
  `record` only; `next_seq` derived at load beats max; eviction picks the lowest
  non-current `touch_seq`; current never evicted; cap counts the current entry.
- **Store.** initial write; overwrite; read back the latest; missing file;
  malformed → quarantined, original preserved under the new name, collision
  suffixing; version 2 → incompatible, file untouched, writing disabled;
  **version 2 carrying fields this build cannot deserialize → still classified by
  version, not as malformed**; **an unquarantinable malformed file disables
  writing and is still present afterwards**; temp removed; Unix modes; no
  truncated target after replacement.
- **Policy.** 5 s becomes an ordinary submit and 4.9 s does not; rapid ticks
  coalesce to the newest; pause, stop and `SeekCompleted` each raise a pending
  force that the same iteration's `tick` resolves, with the tick's position and
  not the event's; **a `Paused` observed from any state but `Playing` raises
  nothing, so a launch on a `completed` entry writes no checkpoint and its
  retained position survives**; **a pending force is re-keyed across a
  `DeviceRecovered` bump and still resolves**, while a `Loaded` bump drops it;
  **a `session_rev` is adopted from an event the policy otherwise ignores, so a
  missed `DeviceRecovered` costs at most one event's worth of samples**; `EndOfTrack` and `SeekTargetStored` submit
  from the event itself; a media switch produces **one** snapshot holding both
  the outgoing entry (from `last_sample`) and the new `current_media`; a
  `Progress` with a stale `session_rev` is ignored; `VolumeChanged` submits at
  ordinary urgency without touching a checkpoint; shutdown forces; a wall clock
  that jumps backwards does not disturb the 5 s interval; **a
  `StateChanged{Playing}` clears `outstanding_target`, so the checkpoint after it
  carries `Progress.position` and not the stale target** (D17); **the shutdown
  snapshot writes no checkpoint for a media the session never saw establish, and
  still writes `volume` and `current_media`** (D20); **`Loaded` clears
  `established`, so a second load that fails cannot inherit the first's** (D20).
- **Writer.** keep-latest replaces without extending the deadline; forced
  bypasses; an older `submit_seq` is dropped; a write error is retried; **a
  submission that lands while a failing write is in flight is not replaced by the
  failed snapshot**; three failures raise one warning; the shutdown ACK path; the
  shutdown timeout detaches.
- **Resume contract.** play → X → stop → persist → session 2 resumes X; the pause
  variant; the seek variant; **play → X → stop → stopped-seek to Y → quit →
  session 2 resumes Y, not X** (D17, the sequence the shutdown force used to
  overwrite); **play → X → stop → stopped-seek to Y → `Restart` → play → quit →
  session 2 resumes the restarted position, not Y** (D17, the sequence
  `restart()` used to leave the target standing through); **a stopped-seek to Y
  whose `SeekTargetStored` is still undelivered when the shutdown interrupt
  fires still persists Y** (D19, driven by holding the event backlog and quitting
  in the same iteration); **a `completed` entry reopened while the device
  refuses to open keeps its retained position across the quit**, with the
  `position > duration` row asserted the same way (D20); **a render failure still
  writes the final checkpoint** (D18); `EndOfTrack` → completed → session 2 starts at 0 with
  the position retained; a stopped-seek target persists; `position > duration` →
  0 with a warning; `completed` cleared on `Playing` after a restart but not on
  `Loaded`; volume survives a restart and is in force before the first frame.
- **Engine (new, additive).** `join` returns the position captured by
  `shutdown()`, and that position is at least the last one published before the
  shutdown interrupt — the fact D14 rests on. `join` also returns every event the
  run produced and the app never drained: one test stops draining, issues two
  volume changes and a stop, interrupts, and asserts the report carries all three
  in emission order — which holds whichever side of the handoff each one was
  sitting on when the interrupt landed, and that indifference is the guarantee
  (D19).

## 17. Risks and deferred debt

- **Cross-process concurrency.** Two Continuo processes: last writer wins, no
  locking. Documented limitation, not solved in M2.
- **Non-Unix permissions.** Windows and macOS fall back to platform defaults.
- **`m1-known-debt.md` seek entry stands.** Sidestepped by D6, not fixed; a future
  UI seek bar still has to deal with it.
- **`tests/support/mod.rs` is shared.** The `start_at` addition must be purely
  additive; all 20 engine contract tests stay byte-identical.
- **`PlaybackEvent::Loaded` gains a field**, so every construction site and any
  exhaustive match must be updated. `Mirror::apply` already uses `..`.
- **No notification surface.** D11 downgrades the persistence warning to a
  `tracing` record because the app has nowhere to put a transient message: the
  status loop owns two rendered lines and stderr logging already smears across
  them while the terminal is in raw mode. That is a pre-existing M1 gap, not one
  M2 introduces, and it is the right milestone-sized answer — but a repeatedly
  failing write is exactly the kind of thing a user should see. A message area
  belongs with the TUI work, and D11 should be revisited then.
- **Cross-iteration pending force.** D13 drops a pending force only for a
  `Loaded` bump — reachable only when a new `Load` is observed in the same drain
  as a pause, stop or seek of the previous session, which the M2 CLI never does.
  If M4's queue makes it reachable, the dropped force must be re-examined rather
  than inherited.
- **`outstanding_target` has no engine-side counterpart.** D17 mirrors, in the
  session, a `requested_target` the engine already holds, and the mirror has
  already drifted once: `restart()` drops the target while emitting none of
  `SeekCompleted`, `Loaded` or `EndOfTrack`, which is why `StateChanged{Playing}`
  had to join the clearing set. That fix closes the M1 paths, but it closes them
  by enumeration — any future path that discards `requested_target` without
  reaching one of those four events reopens the drift silently, because nothing
  in the engine asserts the two agree. The duplication should collapse into the
  engine reporting its own outstanding target as soon as a later milestone needs
  it for a seek bar.
- **An unvalidated position cannot be rescued once the media behind it
  changes.** When the probe reports no duration — realistic for a VBR MP3 with
  no Xing header — §11's last row retains the stored position as
  `Unvalidated`, and the `position > duration` row that would have caught a
  stale one needs a duration it does not have. If the file at that path is then
  replaced by a shorter recording, the resume asks for a position past its end.
  A reader that errors instead of clamping makes the worker fail *before* it
  ever reports `Loaded`, so the session never learns which media it was playing:
  the shutdown snapshot leaves the entry untouched, the position stays in the
  file, and every subsequent launch fails identically. Nothing in the failure
  says the file is recoverable, so the user's only route out is deleting the
  state file, which the README now documents. The candidate fix is retrying the
  initial `Load` once at zero when a load carrying a restored position fails
  before `Loaded`. It is deliberately not in M2: reachability depends on decoder
  behaviour nobody has demonstrated, and the retry would add an untestable path
  to the least testable code in the milestone.
- **Deferred to M3.** `SourceLocation::Http` remains unsupported by the engine;
  nothing in this design is shaped around remote media beyond `MediaId` already
  covering it.

## 18. Review resolutions

A design review after approval raised eight findings. Seven are resolved above;
one is declined on the evidence.

| Finding | Resolution |
|---|---|
| Shutdown cannot obtain the final position it claims | **Accepted**, cause confirmed at `engine.rs` `fn shutdown`. Resolved by D14 — publish once after the capture, return it from `join` — rather than by the suggested engine shutdown-result channel, which is more machinery for the same fact |
| Pause and stop can use pre-transition progress | **Declined as stated, accepted as a structural gap.** The engine's pass order (§3) makes the sample that follows a transition event strictly *newer* than the transition, so the app's drain-then-sample order cannot read a stale position here. What was genuinely missing is that `observe(event)` has no `Progress` to build a snapshot from; D13 gives pause, stop and seek one explicit pending-force mechanism resolved by the same iteration's tick. Transition events do **not** need to carry positions |
| Rejected-file preservation is incomplete | **Accepted**, both halves. §13 now reads a version envelope before the model and defines `schema_version != 1` as unsupported; a quarantine that cannot be performed disables writing |
| Volume has no persistence behavior | **Accepted**, and resolved by specifying it rather than by dropping it: §6 of the architecture lists volume among the file's contents. D15, §10 and §11 |
| The writer warning has no transport | **Accepted as a real gap; resolved by weakening the promise.** D11 is now a `tracing::warn!` from the writer thread, matching every other diagnostic in the app; §17 records the missing notification surface as debt |
| Failed-write reinsertion can violate keep-latest | **Accepted.** §9 reinserts only into an empty slot; a newer snapshot supersedes the failed one, with a test |
| Media-switch wording conflicts with asynchronous writing | **Accepted**, and the underlying problem is worse than reported: `load()` overwrites `self.position` with `start_at` before publishing, so the outgoing media's final position is unreachable from `Progress` at all. §8 records it from the session's retained `last_sample`, in one atomic snapshot |
| The injected clock needs two notions of time | **Accepted.** D16, `ClockSample { monotonic, wall }` |

A second review of the revision found five more, all accepted; the first two are
faults the revision itself introduced by combining D13's mechanism with paths it
had not traced.

| Finding | Resolution |
|---|---|
| The shutdown force overwrites a stopped-seek target, defeating D7 | D17: the target supersedes `Progress.position` until the engine adopts it |
| The establishment `Paused` forces a checkpoint that wipes a `completed` entry's retained position, defeating D1 | D13: `Paused` raises a force only from `Playing` |
| D13 dropped a pending force on *any* newer revision, but `rebuild` bumps it on device recovery with the position continuous | D13: a newer revision re-keys the force; only `Loaded` drops it |
| `render(&mirror)?` exits `app::run` without reaching §9's sequence | D18: every exit runs the flush path |
| §7 understated the session's state, and a dropped `DeviceRecovered` could suspend checkpointing for the run | §7: the tracked fields are listed in full, and `session_rev` is adopted from every event |

A third review found three more, all accepted. Two rest on a mistake in §3
itself: the row claiming `Restart` reaches `Playing` through a completed seek
described `restore()`, the `Play` path, and `Restart` does not go through it. A
table that says it was verified rather than assumed is worth more than the
decisions built on it, so the row is corrected and split in two.

| Finding | Resolution |
|---|---|
| A restart leaves the persisted stopped-seek target standing, because `restart()` clears the target and announces `Playing` without a `SeekCompleted` | **Accepted**, cause confirmed at `engine.rs` `fn restart`. §12 had already named `StateChanged{Playing}` as the observable establishment for exactly this reason; D17 now uses the same signal rather than defining a second one. The §3 row that misdescribed `Restart` is corrected. The reported severity is if anything understated: D17 supersedes `Progress.position` while the target stands, so the stale target reaches every checkpoint after the restart, not only the shutdown one |
| Shutdown builds the final snapshot from state that queued and worker-pending events never reached | **Accepted**, and it is two independent losses rather than one: the worker discards `pending_events` at the shutdown interrupt, and `app::run` breaks before its own drain. D19 returns the backlog through the thread's join value and drains the channel inside `join`, which is lossless and ordered; the suggested channel flush is neither, and would have to block against a full channel while the app sits in `join` |
| A failed startup still overwrites a completed entry's retained position | **Accepted**, and widened. `load()` pins `start_at` before opening, so an *incomplete* entry survives a device-open failure unharmed; the loss is confined to the three §11 rows that map to `start_at = 0` while retaining a position — `completed`, `position == duration`, `position > duration`. D20 gates the shutdown checkpoint on establishment rather than special-casing `completed`, which also stops launch-then-immediate-quit from spending a `touch_seq` on a checkpoint that records nothing new |

## 19. Amendments adopted during implementation

The shipped code departs from the decisions above in six places, and resolves one
contradiction the spec itself contained. Every change was reviewed and agreed as
it arose. They are recorded here rather than edited into D1–D20 because the
sequence that forced each one is the part worth keeping: a decision rewritten in
place reads as though it had always said that, and the next reader inherits the
rule without the evidence for it.

### D20 gates every position that derives from `Progress`

D20 gates the shutdown force on establishment. The shipped policy routes *every*
position that comes from `Progress` through one helper and gates all of them: the
resolved pending force, the ordinary 5 s capture and the shutdown snapshot. The
outgoing entry recorded on a media switch is gated on the same flag. Three
sequences forced the widening, each of them writing a zero over a position D1
deliberately retains:

- **The resolved force.** Launch on a completed entry: §11 starts it at 0,
  `load()` emits `Loaded` and then the establishment `Paused`, and a stop in the
  same iteration resolves its force against a sample of 0 — written over the
  240 s the completed entry had retained.
- **The ordinary capture.** A switch onto a completed entry: the policy's
  `playback` can still read `Playing` from the outgoing media, because the
  `StateChanged{Loading}` that precedes `Loaded` is an ordinary event the engine
  drops at `PENDING_CAP` — exactly the backlog pressure a switch happens under.
  The 5 s rule then captures the incoming media's unvalidated 0 over its retained
  240 s.
- **The outgoing entry.** `Loaded{a, 0}` → `Failed` → `Loaded{b, 0}`: the switch
  records `a` from `last_sample`, which holds the 0 `load()` pinned before the
  device failed, over a retained 300 s.

The two sources D6 names as exceptions — `SeekTargetStored.target` (D7) and
`EndOfTrack.position` — are deliberately **not** gated. Neither is a number the
engine has still to validate: one is the listener's own intent, the other a
position the engine reached and reported in the event that carries it. So exactly
six paths can write a stored position, and each is either behind the gate or
carries its own position.

### D20's flag is latched rather than read at the current revision

D20 asks for an establishment observed *at the current revision*. The shipped
flag is latched: set by the first establishing event, cleared only by `Loaded`.
Read literally, the gate would close again whenever a `DeviceRecovered` bumped
the revision — and §3 records that `rebuild` keeps the position continuous across
that bump, so the gate would discard a position D20 never asks anyone to discard.
It would discard it for good in the case that matters, since a paused session
fires no ordinary trigger to replace it.

### §12's clearing set gains `SeekTargetStored`

A contradiction resolved rather than a deviation. D7 says a stopped seek's target
is persisted; §11 says a completed entry resumes at 0. On a completed entry the
two decisions cancel: the target is written beside `completed == true`, and the
resume D7 exists to steer then throws it away. A stopped seek is the same
listener intent one step earlier than the seek §12 already clears completion for,
so `SeekTargetStored` joins `SeekCompleted` and `StateChanged{Playing}` in the
clearing set.

### §7: `pending_force` carries no trigger

§7 describes `pending_force: Option<(session_rev, Trigger)>`. The shipped field is
`Option<u64>`, the revision alone. Nothing ever read the trigger: every forced
checkpoint resolves identically, from the tick sample carrying that revision, so
the enum was write-only state and was dropped.

### §8's media-switch row gains two qualifications

The row records the outgoing entry from `last_sample`. Two conditions qualify it.
A completed outgoing entry is not re-recorded at all, because the retained sample
can only be behind the position `EndOfTrack` already recorded (D1). And an
outstanding stopped-seek target supersedes the sampled position (D17): resolving
the target first would write the pre-seek sample back over the target.

### §14: `checkpoint_for` was not shipped, and `session_rev()` was added

§14 lists `PersistedState::checkpoint_for` and calls `PlaybackCheckpoint` the
currency between session and persistence. That is true of what the policy
**records** — `record(&PlaybackCheckpoint, completed)` is the only write path —
and false of what the resume **reads**. `PlaybackCheckpoint` carries no
`completed` flag, so it structurally cannot answer §11's first question; the
resume path runs `entry_for(&media)` → `decide_resume(Option<&PersistedCheckpoint>,
Option<Duration>)` instead. `checkpoint_for` would have had no caller and was not
written.

`PlaybackEvent::session_rev()` is an addition §14 does not list. §7 requires the
revision to be adopted from every observed event, which wants one accessor
covering every variant rather than a match repeated at each call site.

### D14: the shutdown publish carries confidence as well as position

D14 settles *which* position the shutdown publishes. The shipped `fn shutdown`
also carries *how well that position is known*: a `capture_position` that did not
confirm marks the state degraded before publishing, so the final `Progress`
reports `Degraded` instead of inheriting `Exact` from the `Idle` state the
teardown has just set. The position is the one D14 promises; only its honesty
about confidence is new.
