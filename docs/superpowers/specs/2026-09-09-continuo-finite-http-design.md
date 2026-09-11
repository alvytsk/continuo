# Continuo — Finite HTTP Media (Milestone 3)

Date: 2026-09-09.
Status: draft for review; not approved for implementation.
Baseline inspected: `2bd0819` (M2 documentation and shipped durable state).
Refines `docs/architecture.md` §§2–5, 7–9 and extends the M1/M2 contracts.

## 1. Outcome and scope

`continuo play <http-or-https-url>` plays a finite remote recording through the
existing Symphonia/CPAL pipeline. A range-capable recording can be sought,
stopped and resumed, and reopened near its saved checkpoint in a later process.
Redirects and transport recreation preserve the original media identity.

The central invariant remains: stopping or recreating transport does not
implicitly reset logical playback position. Network bytes, decoder read-ahead,
and underrun silence never count as listened progress.

M3 includes HTTP source opening, finite-media classification, bounded buffering,
range validation, cancellation, CLI integration, and remote checkpoint policy.
Existing local formats and mono/stereo output remain the format/output scope.

Feeds, subscriptions, episode discovery, queues, TUI, downloads, disk caching,
live radio, HLS/DASH, authentication configuration, and automatic network retry
are outside M3. M4 owns the feed-driven Radio-T scenario.

### Draft scope choices

These are recommendations awaiting review, not previously approved decisions:

1. Servers without ranges may play sequentially. M3 does not implement
   `RestartAndDiscard`; seek and resume require verified support.
2. Live sources are recognized as unsupported. A source whose continuity remains
   unresolved after a bounded probe is also refused, without calling it live.
3. Network failures stop the current attempt with position retained. A new
   user-requested attempt may reconnect; no background retry loop is introduced.

## 2. Alternatives considered

| Approach | Benefit | Cost | Decision |
|---|---|---|---|
| Stream through a bounded memory buffer; ranges supply random access | Playback can start before download completes; fits existing ownership | Requires cancellation and response validation | Recommended |
| Download the whole object to a temporary file before playback | Reuses local decoder opening and seeking | Startup scales with recording size; adds storage lifecycle and limits | Outside M3 |
| Stream, then reopen and decode from zero when ranges are unavailable | More servers support seek/resume | Long seeks can transfer and decode hours of audio | Defer `RestartAndDiscard` |

Requiring ranges for all HTTP playback would simplify the source adapter but
would unnecessarily reject finite recordings that can be decoded sequentially.

## 3. Observed baseline and required changes

The repository already provides `SourceLocation::Http`, `MediaId::RemoteUrl`,
`Continuity`, `SeekSupport`, and the derived resume-capability matrix. Persistence
already keys checkpoints by `MediaId`; no new identity grammar is needed.

The remaining integration boundaries are concrete:

- `app::run` canonicalizes a local path and probes a decoder on the main thread,
  then the worker opens it again. HTTP must not duplicate that probe or block
  terminal input on network I/O.
- `DecodedSource::open` accepts an `AbsolutePath`, owns path-specific context,
  and reports local capabilities. Separate source opening from decoder probing.
- The worker rejects HTTP and retains a decoder across local stop. Remote stop
  must also cancel fetching, so reopening must use the stored source descriptor.
- Stop/shutdown already have an out-of-band interrupt. Seek currently uses the
  ordinary command queue and cannot wake a blocked source read.
- Session policy ignores capabilities in `Loaded`. It must distinguish an
  unavailable remote resume from a successfully established fresh position.

M1 known-debt entries are historical findings, not proof that each issue remains
in the current tree. Only changes required by these boundaries belong in M3.

## 4. Ownership and components

| Component | Responsibility | Boundary |
|---|---|---|
| Application, main thread | CLI, keys, rendering, session policy, persistence submissions | Owns engine handle and HTTP runtime lifetime |
| Tokio runtime and HTTP tasks | Requests, redirects, deadlines, streamed response bodies | Never own a decoder, resampler, or CPAL stream |
| HTTP source adapter | Synchronous byte reads/seeks over bounded shared storage | Implements the pinned Symphonia `MediaSource` contract |
| Decode worker | Source/decoder preparation, capability decisions, audio production, CPAL lifecycle | Waits synchronously and cancellably; never calls runtime `block_on` |
| CPAL callback | Existing PCM drain, silence and span publication | No network changes; no locks, allocations, waits or I/O |
| Persistence writer | Existing atomic snapshots and flush | Filesystem work stays on its own thread |

Keep the synchronous terminal loop on the main thread. Introduce an owned Tokio
runtime for networking instead of moving blocking terminal operations onto its
executor. An injected HTTP service handle lets the worker open remote sources;
local tests need no network runtime.

The source adapter supplies byte length, byte position, seekability, and a typed
terminal outcome. The decoder supplies media metadata and time-to-byte seeking.
The engine combines those capabilities; HTTP range support alone does not prove
that a particular container can seek in media time.

Use a concrete HTTP module with small source/response/cancellation units. Reuse
Symphonia's existing source interface instead of adding a speculative playback
backend abstraction. Select and lock compatible Tokio and asynchronous HTTP
client versions during implementation; require TLS verification and streaming
body access. No dependency version is prescribed by this draft.

## 5. CLI and opening sequence

Accept `continuo play <source> [--probe-only]`, where a source is a local path or
an HTTP(S) URL. Parse an explicit HTTP(S) scheme as a URL and report malformed
URLs as such. Other values retain existing path behavior; `./https:...` remains
an unambiguous local spelling. Reject URL userinfo rather than introducing an
implicit credential feature.

Create `MediaId::RemoteUrl` from the supplied URL's existing identity
normalization. Retain the separate parsed fetch URL. Follow redirects for
fetching without replacing identity with a redirect destination. Queries retain
their existing serialized spelling and order.

Read persistence and restore volume before issuing a load. Extend the load
contract to carry a resume intent: an explicit start position or the persisted
checkpoint candidate. The worker resolves a persisted candidate with the
existing completion/duration rules after its single source probe, then applies
the capability rules in §10. This keeps decoder ownership on the worker and
avoids loading at zero before discovering the resume position.

`Loaded.position` still means the position actually established by the decoder.
It must not report an unvalidated checkpoint as a successful landing. The
application displays Loading while preparation is in flight and remains able
to stop or quit. Restored volume applies to the first audible buffer.

Extend `Loaded` with a typed start disposition distinguishing fresh playback,
successful resume, completed replay, and unavailable-resume fallback. The fallback
disposition tells Session to protect its retained checkpoint before processing
any Playing/progress event. Capability change events cannot establish or clear
that protection by themselves.

`--probe-only` uses the same preparation logic on a worker without constructing
an audio device or entering raw mode. It prints metadata, continuity, seek and
resume capability, and exits. It neither reads nor writes playback state.

## 6. Finite media and capability evidence

Treat `RemoteFile` and `LiveStream` as semantic classifications derived from
the existing continuity model, not a second contradictory source enum.

| Evidence after HTTP and decoder probing | Continuity | M3 behavior |
|---|---|---|
| Fixed response length or a valid range total, and supported audio | Finite | Open; duration may remain unknown |
| No byte length, but decoder establishes a finite recording length | Finite | Sequential playback if otherwise supported |
| Explicit live/ICY stream semantics | Indefinite | Refuse as unsupported live media |
| Neither finite nor live evidence | Unresolved | Refuse with continuity-undetermined diagnostic |

A URL suffix, audio MIME type, a missing `Content-Length`, or an absent
`Accept-Ranges` header does not by itself decide continuity. A chunked response
can carry finite media. M3 deliberately does not wait for the entire body just
to discover whether it eventually ends.

Use a real range GET to probe byte access. `Accept-Ranges` is a hint, and HEAD is
not a required preliminary request. Start with `Range: bytes=0-` and reuse the
accepted response as the initial stream. A valid 206 establishes byte-range
support; an ordinary 200 selects sequential access. Malformed responses fail
opening rather than masquerading as unsupported seek.

Publish `Native` only when both byte access and the selected demuxer's seek
behavior support the required operation. Until conclusive evidence exists,
publish `Unknown`, and verify on demand. A negative decoder result yields
`Unsupported`, independently of HTTP support. A capability change is an ordered
event carrying `session_rev`; session and mirror consume it like `Loaded`.

## 7. HTTP response contract

The following are M3 acceptance rules. HTTP range and conditional-request
semantics are defined by [RFC 9110 §§13.1.5 and 14](https://www.rfc-editor.org/rfc/rfc9110.html#section-14).

- Request identity content encoding and disable client decompression. Reject
  non-identity encoded bodies so decoder offsets refer to received media bytes.
- Follow at most five HTTP(S) redirects. Reject loops, unsupported schemes,
  invalid locations and HTTPS-to-HTTP downgrades. Reopening starts from the
  original fetch URL, allowing a fresh redirect target.
- For a 206, validate a single byte `Content-Range`, the requested start, an
  ordered inclusive interval and a consistent total when supplied. Validate
  body length against the advertised interval. M3 does not request multipart
  ranges and rejects a multipart response.
- A 200 is usable as sequential data only during an opening at byte zero. A
  later nonzero range receiving 200 never installs those bytes at that offset.
- A 416 is not track completion. Seeking to a known byte EOF may return EOF
  locally without requesting it; other 416 responses fail the operation.
- Preserve and compare response validators and length within an open session.
  Use a strong ETag with `If-Range` when available; never send a weak ETag as
  `If-Range`. A changed validator or conflicting total fails with ResourceChanged.
- Without a strong validator, range access is best effort: compare available
  length and validator metadata, but do not claim to detect same-length content
  replacement. No cross-request disk cache or durable byte-offset state is built.
- Do not reinterpret status failures, malformed ranges, timeouts, or truncated
  response bodies as clean source EOF.

A response that ends a smaller valid interval before the object ends requires
another validated request at the next byte; it is not media EOF. Late failures
invalidate the attempt even if some audio was already heard. Previously heard
position remains meaningful and may be checkpointed.

## 8. Buffering, deadlines and cancellation

Use a bounded encoded-byte buffer, initially 1 MiB, plus at most one bounded
64 KiB application transfer chunk. Configure decoder buffering explicitly and
document HTTP/TLS library buffering separately; the application buffer cap is
not a claim about total process memory. There is one active fetch per source
generation and no unbounded body accumulation or prefetch task fan-out.

Producer backpressure waits asynchronously. Reader waits use shared predicates
and a condition variable or equivalent synchronous wake mechanism. Read returns
zero only for clean EOF, never for an empty buffer or cancellation. Errors and
cancellation remain distinguishable even if Symphonia wraps the I/O error.

All waiting paths test terminal predicates under the same synchronization used
for notification, preventing lost wakes. Stop, seek and shutdown publish their
interrupt and notify both the reader and producer directly. A queued command
alone is insufficient. Superseded responses are tagged and rejected before
their bytes or outcomes can enter the active generation.

Provide engine-handle submission methods that own queue admission and waking.
A seek interrupt is published only when its target has been accepted. Queue
saturation returns a visible busy result instead of blocking the application;
accepted command order and lossless seek outcomes remain defined. Shutdown
dominates stop, and both cancel an outstanding seek without committing its target.
Every accepted seek receives a completed, stored, rejected or cancelled outcome;
cancelled outcomes participate in the existing lossless shutdown report. Revisit
the event-reserve arithmetic for these new possible outcomes.

Cancelling a read or seek retires the current decoder/source attempt. The caller
must reopen and re-establish position before decoding again; arbitrary decoder
state after cancellation is not reusable.

Initial limits are 10 seconds to connect, 15 seconds awaiting response headers,
15 seconds without received data while actively demanding it, and 30 seconds
for opening/probing. Probe input consumption is additionally capped at 8 MiB;
larger required scans fail with ProbeLimitExceeded. Ordinary playback has no
whole-response deadline. Time spent paused or waiting for local buffer space
does not count as a server stall. Limits are named constants and injectable
for tests; exposing configuration is deferred.

Tests must demonstrate source waits wake within one second after stop, seek or
shutdown without releasing the server's stall. This is a source-layer bound;
it does not remove M1's documented platform-device teardown limitation.

While waiting for source bytes, drain and publish playback spans sufficiently
often to keep position and checkpoints current. An underrun displays buffering
as a status detail of Playing; only actually played media advances position.
Networking cannot block that progress maintenance indefinitely.
The worker supplies a narrow wait-service hook to the synchronous adapter: it
may collect spans, publish progress and act on pause/control interrupts, but
must not re-enter decoder reads or seeks. Network tasks never execute this hook.

## 9. Playback transitions

| Action | Required behavior |
|---|---|
| Pause | Freeze output and retain decoder, PCM and buffered bytes; producer backpressure bounds additional fetching |
| Play after pause | Release retained pipeline; do not reset position |
| Stop | Capture played position, retire fetch, wake reads, discard transport and remote decoder; keep identity/source/position |
| Play after stop | Reopen and restore preserved position if supported; otherwise stay Stopped with an unavailable-resume diagnostic |
| Seek while active | Preserve current position; cancel obsolete reads; establish requested target through decoder seeking and ranges |
| Seek while stopped | Store intent only after capability admission; Unknown requires a cancellable probe before acceptance |
| Restart | Explicitly open from zero; clear completion/checkpoint protection only after successful establishment |
| Network failure | Preserve captured position, fail current attempt, retire requests; emit a typed error |
| Play after remote network failure | Permit one explicit reopen at preserved position; fail honestly if restoration is unavailable |

Pause must also work during a stalled read. Service its freeze request without
returning a destructive read error to the demuxer; keep the interrupted read
pending and continue to service stop/shutdown. Resume releases that suspension.
Tests must cover this independently of pausing while bytes happen to be ready.

Successful seek publishes the actual installed landing. If restoration after a
failed/cancelled seek cannot reopen, retain the old logical position and report
failure. Device recovery uses the same restoration rules and cannot silently
start an unseekable remote recording at zero.

Completion requires clean decoder end, no known unresolved source failure, and
output drain. For finite containers that finish decoding before consuming the
HTTP body, finish validating the remaining response under bounded streaming
memory and the normal active-read deadline before declaring completion. A
truncated body or timed-out tail cannot set completed status.

## 10. Resume and persistence

Keep schema version 1 and existing checkpoint values: media time, completion,
touch sequence and timestamp. Do not persist redirects, network buffers,
validators, capabilities or byte offsets. Relaunching the same original URL
re-probes capabilities and reuses its existing identity. A changed URL query is
a different identity under the existing contract.

For a positive incomplete checkpoint, attempt restoration only after capability
resolution. Unknown triggers a bounded attempt, not an assumption of support.
Protocol or network failure preserves the checkpoint and fails the load; it
does not silently retry at zero.

If support is conclusively unavailable, sequential playback may begin at zero
with an explicit warning that the previous resume point is retained. Mark this
load as protecting the existing entry: periodic, pause, stop, outgoing-media and
shutdown captures must not overwrite that entry just because playback fell back
to zero. Volume and current-media snapshot updates continue normally.

Protection ends only on a successfully established explicit Restart, a
successfully established user seek if support becomes available, or verified
completion of the recording. Until then even subsequently heard progress from
the fallback run does not replace the retained entry. This deliberately favors
recovering the earlier resume point over saving progress from an automatic
fallback; it is not a maximum-position merge rule.

With no existing positive checkpoint to protect, sequential playback records
heard progress normally even though it cannot currently be resumed. Completed
entries follow M2's replay policy; merely failing a new attempt still preserves
their position and completion.

Reject unsupported stopped seeks before `SeekTargetStored`, since M2 treats that
event as a durable user target. Capability changes alone never delete, clear or
replace checkpoints. All new events participate in revision reconciliation,
reserved terminal capacity and shutdown backlog handling.

## 11. Diagnostics and errors

Expose typed remote failures through the engine event boundary, with categories
for invalid source, HTTP status, redirect rejection, timeout, invalid range,
resource change, truncated body, probe limit, unresolved continuity, unsupported
live media and unavailable seek/resume. Preserve operation and source context;
do not add another path that collapses remote faults into `Failed(String)`.
Existing error-event plumbing may need a targeted migration to carry that type.

Log source opening, redirect count, capability evidence, range operations,
requested/actual seek, cancellation, reconnect, and source completion. Log no
per-frame or per-chunk events. Keep signed query strings and URL userinfo out of
normal diagnostics, status text and third-party error display. Fetching and
identity still retain required query data; this does not change the existing
plaintext identity representation in state.json.

The status line and probe output distinguish unknown duration, unresolved
capability, unsupported capability and current buffering. An HTTP transport
must never cause the UI to label a finite recording as live radio.

## 12. Acceptance evidence

Automated acceptance uses a controllable loopback server, deterministic barriers,
test output and temporary persistence directories. No public network or physical
audio device is needed. Each cancellation test first proves the target wait was
entered; sleeping and hoping for a race is insufficient.

| ID | Scenario and observable result |
|---|---|
| H1 | Range-capable MP3, FLAC and WAV open and play through test output before the full body arrives |
| H2 | Forward/backward seek yields the installed media position and the expected nonzero range request |
| H3 | Stop closes the fetch; Play opens a new request and resumes at preserved position |
| H4 | A second process/session using the same original URL resumes from the flushed checkpoint; redirected identity is unchanged |
| H5 | Server ignores ranges, with and without `Accept-Ranges`: sequential playback works, seek is rejected and resume fallback protects an existing entry |
| H6 | Redirects preserve query serialization and original identity; loops, excess hops, downgrade and non-HTTP schemes fail |
| H7 | Wrong range start, reversed interval, conflicting total, missing required range, multipart, unexpected 200 and 416 produce errors without committing target position |
| H8 | Short response, stalled body, disconnect and malformed audio cannot become successful completion |
| H9 | Stop/seek/shutdown wake header waits, empty-buffer reads and full-buffer producer waits; stale responses cannot repopulate the new generation |
| H10 | Pause during a stalled read freezes output; resume continues correctly; quit while paused still wakes all source waits |
| H11 | Strong-validator change fails; weak/absent validators follow the documented best-effort policy without an invalid If-Range |
| H12 | Finite unknown-duration media stays finite; finite chunked media with decoder evidence works; unresolved and explicit-live sources are distinct refusals |
| H13 | Buffer occupancy stays bounded under fast server/slow consumer; silence during network starvation does not advance position |
| H14 | Capability events obey revision guards; unavailable stopped seek emits no durable target; shutdown preserves ordered pending events and the correct checkpoint |
| H15 | Probe-only uses no terminal/device/persistence; URL failures are legible; normal diagnostics redact signed queries |
| H16 | Protected fallback entry survives periodic/pause/stop/switch/shutdown paths; explicit restart or verified completion supersedes it; a fresh sequential session records progress |
| H17 | A byte-seekable source with an unseekable demuxer is not falsely advertised as Native; opening that exceeds probe byte/time limits fails cancellably |
| H18 | Local playback/resume/completion and persisted schema round-trips remain compatible |

Include a small M4A fixture if the existing supported M4A demuxer needs tail
metadata: demonstrate range-backed opening, or record a specific supported-layout
limitation. Do not claim all container layouts work from extension alone.

Implementation verification runs `cargo fmt --check`,
`cargo clippy --locked --all-targets --all-features -- -D warnings`, and
`cargo test --locked`. This draft records no new test-pass claim.

Manual acceptance: play a finite episode URL on a real device, seek, stop and
resume, quit and relaunch the same URL, and check position continuity and
diagnostics. Feed discovery remains an M4 step. Update README and architecture
to describe the shipped milestones accurately when implementation completes.

## 13. Review boundary

This document is the M3 design proposal, not an implementation plan. Review
should settle the three scope choices in §1, the no-range checkpoint protection
in §10, and the continuity policy in §6 before task breakdown and code changes.
The numerical limits are proposed defaults whose adequacy is checked by the
acceptance tests; changes during implementation must be recorded explicitly.
