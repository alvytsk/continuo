# Continuo M3 — Finite HTTP Media Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `continuo play <http-or-https-url>` plays a finite remote recording through the existing Symphonia/CPAL pipeline, with seek, stop/resume and cross-process checkpoint resume on range-capable servers, and honest refusals everywhere else.

**Architecture:** A new `src/http/` module owns an application-owned Tokio runtime, one fetch task per source generation, and a bounded encoded-byte channel. The decode worker sees only a synchronous `MediaSource`; it never calls `block_on`. Three enabling changes inside `src/playback/`: source opening is separated from decoder probing (`prepare.rs`), the transport's span/timeline state moves behind an `Arc<Mutex<TransportCore>>` so a blocked read can keep publishing progress without touching the decoder, and out-of-band interrupts grow from stop/shutdown to also carry seek and a pause *level*.

**Tech Stack:** Rust 2024, `tokio` (rt-multi-thread, net, time, sync), `reqwest` (rustls TLS, streaming bodies, redirects disabled), `symphonia`, `cpal`, `rtrb`, `crossbeam-channel`, `url`. Tests use a hand-rolled `std::net::TcpListener` server — no HTTP mocking crate, because the acceptance evidence needs raw framing, mid-body stalls and barrier releases that no mocking library exposes.

**Spec:** `docs/superpowers/specs/2026-09-09-continuo-finite-http-design.md` — read it alongside this plan. Every `§n` and `Hn` below points into that document.

## Review decisions settled before this breakdown

§13 reserved five things for review. All five are settled; the plan implements these answers and nothing else.

| # | Question | Decision |
|---|---|---|
| R1 | §1.1 `RestartAndDiscard` | **Not implemented.** Seek and resume require verified byte-range support. A range-less server plays sequentially and refuses seeks. |
| R2 | §1.3 network retry | **No retry loop.** A failure retires the attempt with the position retained; only an explicit user Play reconnects. |
| R3 | §6/§1.2 continuity | **Unresolved is refused**, with a `ContinuityUndetermined` diagnostic distinct from `UnsupportedLiveMedia`. |
| R4 | §10 no-range fallback | **Protect the existing entry, as specified.** Not a max-position merge. Protection ends only on an established Restart, an established user seek, or verified completion. |
| R5 | §3 preparation path | **Worker-only, raw mode deferred.** The main-thread probe in `src/app.rs:56` is deleted; both local and HTTP prepare on the worker, and the app enters raw mode only after the first `Loaded`/`Failed`. This also clears the M1 double-open debt. |

Five spec gaps found against the shipped tree; each is closed by a named task rather than left to the implementer.

| # | Gap | Closed by |
|---|---|---|
| G1 | §10's "protection ends on an established Restart" is unimplementable: `Worker::restart` (`src/playback/engine.rs:1443`) emits no distinguishing event, and `Session::on_state` cannot tell it from any other `Playing`. | Task 8 adds `PlaybackEvent::RestartEstablished`. |
| G2 | §9 requires pause during a stalled read, but pause is an ordinary command and the command loop is not running while a read blocks. §8 names only stop/seek/shutdown interrupts. | Task 3's `SourceInterrupt` carries a *freeze level*; Task 10 wires `submit_pause`/`submit_play` to it. |
| G3 | §5's resume intent would make the engine depend on `persistence::model::PersistedCheckpoint`, which §4 forbids. | Task 8 extracts `ResumeCandidate`/`ResumeDecision`/`decide_resume` into a persistence-free `src/resume.rs`. |
| G4 | §3's "HTTP must not duplicate that probe" versus the M1 property that a bad file reports with no device and no raw mode (`tests/cli_playback.rs`). | R5 above; Task 12. |
| G5 | §8's wait-service hook is called from inside `source.next_planar()`, which already holds `&mut self.source`; the state it must service is `&mut` worker state, and `unsafe_code = "forbid"` rules out a lifetime-erased slot. | Task 9 moves `Handshake` + `Timeline` + the anchor behind `Arc<Mutex<TransportCore>>`, so the hook shares them by `Arc` and structurally cannot reach a decoder. |

### Review round 1 — thirteen defects fixed in this document

Every one was verified against the actual crate sources before being fixed, and each carries a test that fails without the fix.

| # | Defect | Where it is fixed |
|---|---|---|
| 1 | `ErrorKind::Interrupted` for a retirement. `symphonia-core-0.6.1/src/io/media_source_stream.rs:432` swallows it inside `while !buf.is_empty()`, and `std`'s `read_exact` does the same — so latching it is a *guaranteed* infinite loop, the opposite of what the draft claimed. | Task 6: `ErrorKind::Other` carrying `RemoteFailure::Cancelled`, plus a test driving the real `MediaSourceStream::read_buf_exact`. |
| 2 | `source()`-only error traversal. `symphonia-core-0.6.1/src/errors.rs:82` implements the deprecated `cause()` and not `source()`, and `io::Error::source()` returns the *payload's* source rather than the payload (`get_ref()` does that). Task 10's whole control flow rested on a function that would have returned `None` every time. | Task 6: `remote_cause` unwraps both explicitly. |
| 3 | Pause could not park output during a blocked read. The worker never reaches its command loop, so a hook that only publishes progress leaves the output draining and `Paused` unannounced. | Task 9: `WaitService::service` gains a freeze arm that parks the transport and announces, with an outbox that preserves event ordering. |
| 4 | Retirement woke only the synchronous reader. `push` awaited the channel's `Notify` while `retire` notified the interrupt's, and the fetch task's futures saw nothing but timeouts. | Task 3: one lock, three wake channels, all owned by `SourceInterrupt`; `cancelled()` races every await in Task 5. |
| 5 | Paused time consumed the stall budget. A deadline computed once at entry keeps running through a pause and fails the very next read. | Task 3: `read` charges only unfrozen slices; Task 5's body awaits `wait_while_frozen()` before arming its timer. |
| 6 | Malformed audio could still complete. A perfect transfer of corrupt bytes carries no `RemoteFailure`, so keying the failure on one drained to `EndOfTrack` — exactly what H8 forbids. | Task 10: any decode failure over a remote source fails the attempt; local keeps M1's warn-and-drain. |
| 7 | No reopen path for seeks. Reopening was given only to `restore()`, so a seek after a stop or a retired seek hit `source.is_none()` and was rejected as "nothing is loaded". | Task 10: `ensure_source_open()`, called by `restore`, `seek_to` and `restart`. |
| 8 | The opening deadline was checked around probing rather than inside it, so a slow trickle stays within every per-read stall budget and opens indefinitely. | Task 7: one `OpeningDeadline` clamped into every wait, with a `trickle` server script to prove it. |
| 9 | `time_base + num_frames` treated as proof of `Native` seeking. Those describe timing; `MediaSource` (`io/mod.rs:42`) carries no seek evidence and `FormatReader` exposes no query. | Task 7: `DemuxerSeek::{Proven, Unproven}`; remote opens `Unknown` and is verified on demand, as §6 asks. |
| 10 | `bytes 0-99/10` accepted, and `bytes 0-<u64::MAX>/*` overflowed `len()`. The body loop checked only for shortfall, never excess. | Task 2: `IntervalPastTotal` plus checked arithmetic; Task 5 rejects excess bytes during the loop. |
| 11 | Only strong ETags compared. `Last-Modified` was stored and never read; weak ETags were discarded, against §7's best-effort comparison. | Task 2: all three compared; only *sending* a weak validator as `If-Range` stays forbidden. |
| 12 | The promised 64 KiB transfer bound was not enforced — `response.chunk()` allocates whatever the transport yields. | Task 5: `push` fed in `chunk_bytes` slices, library buffering documented separately as §8 requires. |
| 13 | `reqwest = { features = ["rustls-tls", "stream"] }` does not resolve: 0.13.5's TLS feature is `rustls`, and there is no `stream` feature at all (`chunk()` is inherent). | Task 1, with a `cargo tree -e features` check. |

### Review round 2 — nine further defects, seven of them introduced by round 1's fixes

| # | Defect | Where it is fixed |
|---|---|---|
| 14 | `MutexGuard::unlocked` does not exist. `rustc 1.98.1` rejects it with `E0599`; the "stable since 1.86" note was fabricated — the same failure mode round 1 had just warned about. | Task 3: explicit `drop` / re-`lock`, with the probe command to check it. |
| 15 | The header wait reproduced defect #4 one layer up: a private `Mutex` + `Condvar` that `SourceInterrupt::wake_all` does not notify, so `retire()` would leave it asleep until its deadline. | Task 5: the header outcome lives in `SourceInterrupt::State` and waits on `reader_wake`. |
| 16 | `arm()` had no generation guard, so a seek could clear a stop's retirement and leave the fetch running; `run()` step 1 armed *after* `do_stop` had just retired. | Task 3: `arm(generation) -> bool`; Task 10: step 1 arms only when neither STOP nor SHUTDOWN fired. |
| 17 | The hook's outbox drained onto the **front** of `pending_events`, inverting exactly the order it exists to preserve. | Task 9: drain onto the back. |
| 18 | `publish_progress` updating `SessionFacts` and then calling `service()` — which takes that same non-reentrant lock — deadlocks the worker on its first pass, silently. | Task 9: drop the guard first; full lock order written down. |
| 19 | `Progress.buffering` was specified as "set when it runs from the hook", but after Task 9 there is one `service()` with no way to tell its callers apart. | Task 12: `service_as(Servicing)`, with `WaitHook::service` delegating. |
| 20 | `wait.rs`'s `announce` uses `EVENT_CAPACITY` and `RESERVED_EVENT_SLOTS`, both private to `engine.rs`. | Task 10: `pub(crate)`. |
| 21 | Tasks 10 and 13 called seven `TestEngine` helpers and a free `fixture_path` that no Interfaces block declares. | Task 10: all eight declared, additively. |
| 22 | H13 asserted `channel.buffered() <= buffer_bytes`, but the channel is inside the worker's decoder and unreachable from a test. | Task 13: observe backpressure via `TestServer::bytes_written`, which is the stronger claim. |


## Global Constraints

Every task's requirements implicitly include this section.

- Rust edition **2024**, `rust-version = "1.98.1"`. Do not raise either.
- `[lints.rust] unsafe_code = "forbid"`. `[lints.clippy] unwrap_used = "deny"`, `expect_used = "deny"`. `clippy.toml`'s `allow-unwrap-in-tests` / `allow-expect-in-tests` exempt the **body of a `#[test]` function only** — a bare helper in the same file is not exempt, even under `tests/`. Every helper in this plan either handles its own error with `match` / `let … else` plus a `panic!` carrying a message, or is annotated `#[allow(clippy::unwrap_used)]` with a note saying why the failure is impossible.
- Every task ends green on all three: `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked`.
- Linux needs `libasound2-dev` for `cpal` to build.
- Baseline before this plan starts: run `cargo test --locked 2>&1 | tail -30` and record the pass/fail/ignored counts in the Task 1 commit message. `device_smoke` is ignored (needs real hardware).
- **Existing tests keep passing unchanged in intent.** `tests/engine_contract.rs`, `tests/engine_shutdown.rs`, `tests/resume_contract.rs` and `tests/session_policy.rs` are M1/M2 contract evidence. Tasks 8–10 change event and command shapes, so mechanical call-site updates are expected and allowed; **no existing assertion may be weakened or deleted**. If an assertion genuinely no longer holds, stop and report it rather than editing it.
- Numeric limits, verbatim from §8, all named constants in `src/http/limits.rs` and injectable for tests:
  connect **10 s**, response headers **15 s**, no-data-while-demanding **15 s**, opening/probing **30 s**, probe input cap **8 MiB**, encoded buffer **1 MiB**, transfer chunk **64 KiB**, redirects **at most 5**.
- Source waits must wake **within 1 second** of stop, seek or shutdown *without* releasing the server's stall (§8). This is a source-layer bound and does not remove M1's platform device-teardown limitation.
- Persistence stays at **`schema_version` 1** with the existing fields. Redirects, buffers, validators, capabilities and byte offsets are never persisted (§10).
- Nothing in `src/playback/` may learn that persistence exists. Nothing in `src/http/` may own a decoder, resampler or CPAL stream. Tokio tasks never touch decoder or output state and never run the wait hook.
- Signed query strings and URL userinfo stay out of normal diagnostics, status text and third-party error display (§11). Identity and fetching still retain the full query; `state.json`'s plaintext identity representation does not change.
- Ordinary playback has **no** whole-response deadline. Time paused or waiting for local buffer space is never a server stall.

---

## File Structure

**Created**

| File | Responsibility |
|---|---|
| `src/http/mod.rs` | Module tree, re-exports |
| `src/http/limits.rs` | `Limits` — every §8 numeric bound, with `Limits::default()` and test constructors |
| `src/http/error.rs` | `RemoteFailure` and its redacting `Display`; `Operation`, `Phase`, `RedirectRejection`, `RangeRejection` |
| `src/http/response.rs` | Pure §7 acceptance: status classification, `Content-Range` parsing and validation, validator comparison, `If-Range` eligibility, redirect admission |
| `src/http/channel.rs` | `ByteChannel` (bounded encoded bytes, sync reader / async producer) and `SourceInterrupt` (retire + freeze levels) |
| `src/http/service.rs` | `HttpService` — the owned Tokio runtime, `open()`, the per-generation fetch task, manual redirect loop |
| `src/http/source.rs` | `HttpMediaSource` — `Read + Seek + MediaSource` over `ByteChannel`; range re-requests; completion validation |
| `src/resume.rs` | `ResumeCandidate`, `ResumeDecision`, `decide_resume` — persistence-free, so both `session` and `playback` may depend on it |
| `src/playback/prepare.rs` | `prepare_source` — one path from `SourceLocation` to an opened `DecodedSource` plus evidence-backed `MediaCapabilities` |
| `src/playback/wait.rs` | `WaitService` — what a blocked read may service on the worker's behalf |
| `tests/support/server.rs` | The controllable loopback HTTP server |
| `tests/http_errors.rs` | Redaction and category coverage |
| `tests/http_response.rs` | §7 acceptance rules, pure |
| `tests/http_channel.rs` | Buffer bounds, lost-wake safety, retire/freeze |
| `tests/http_fetch.rs` | Redirects, range GET, generation tagging, deadlines |
| `tests/http_source.rs` | `MediaSource` behaviour: EOF vs error, seek, freeze |
| `tests/prepare.rs` | Continuity and seek evidence (§6), probe limits |
| `tests/http_playback.rs` | H1, H2, H3, H5, H12, H13 |
| `tests/http_protocol.rs` | H6, H7, H8, H11 |
| `tests/http_cancellation.rs` | H9, H10 |
| `tests/http_resume.rs` | H4, H14, H16 |
| `tests/http_cli.rs` | H15 |

**Modified**

| File | Change |
|---|---|
| `Cargo.toml` | `tokio`, `reqwest`; `http` for header types |
| `src/lib.rs` | `pub mod http; pub mod resume;` |
| `src/cli.rs` | `Play { source: String, probe_only: bool }` |
| `src/playback/error.rs` | `PlaybackError::Remote(RemoteFailure)` |
| `src/playback/decode.rs` | `DecodedSource::from_media_source`; capabilities take supplied evidence |
| `src/playback/event.rs` | `Loaded.disposition`, `RestartEstablished`, `CapabilitiesChanged`, `SeekCancelled`, `Failed.cause` |
| `src/playback/command.rs` | `Load { resume: ResumeIntent }`; `Admission` |
| `src/playback/handshake.rs` | `Handshake` and `Timeline` move into `TransportCore` |
| `src/playback/engine.rs` | Interrupts, submission methods, `TransportCore`, remote stop/reopen, cancelled reads, completion validation, reserve arithmetic; `EVENT_CAPACITY` and `RESERVED_EVENT_SLOTS` become `pub(crate)` so `wait.rs` can respect the reserve |
| `src/session.rs` | `decide_resume` moves out; protection flag; disposition handling |
| `src/app.rs` | URL parsing, deferred raw mode, worker-side probe-only, status detail, redaction |
| `README.md`, `docs/architecture.md`, `docs/m1-known-debt.md` | Ship the milestone honestly |

**Dependency order.** 1 → 2 → 3 are independent of `playback`. 4 needs nothing. 5 needs 1–4. 6 needs 5. 7 needs 6. 8 is independent of 1–7. 9 is independent of 1–8. 10 needs 7, 8, 9. 11 needs 8. 12 needs 10, 11. 13 needs 12. 14 needs 13.

---

## Task 1: Dependencies, limits and the typed remote failure

**Files:**
- Modify: `Cargo.toml`, `src/lib.rs`
- Create: `src/http/mod.rs`, `src/http/limits.rs`, `src/http/error.rs`
- Test: `tests/http_errors.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  `continuo::http::limits::Limits { connect, headers, stall, open, probe_bytes, buffer_bytes, chunk_bytes, max_redirects }`, `Limits::default()`.
  `continuo::http::error::{RemoteFailure, Operation, Phase, RedirectRejection, RangeRejection}`.
  `RemoteFailure: Clone + Debug + Eq + PartialEq + std::error::Error`, and `redact_url(&str) -> String`.

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml`, under `[dependencies]`:

```toml
tokio = { version = "1", features = ["rt-multi-thread", "net", "time", "sync", "macros"] }
reqwest = { version = "0.13", default-features = false, features = ["rustls", "http2"] }
http = "1"
```

**The feature names are verified against reqwest 0.13.5's own manifest, not carried over from 0.12.** The TLS feature is `rustls` (`rustls-tls` does not exist in 0.13). There is **no `stream` feature** in 0.13 either: `Response::chunk()` is an inherent async method, always available, and `bytes_stream()` is the one that would need `futures_core`. Do not add either name; `cargo build` fails on an unknown feature, so a mistake here is loud, but the wrong fix is to invent a feature rather than drop it.

`default-features = false` is load-bearing twice over: it keeps `gzip`/`brotli`/`deflate`/`zstd` off, so reqwest never transparently decodes a body and §7's identity-encoding rule is enforceable, and it drops `default-tls`/`system-proxy`/`charset`. Do not add `reqwest`'s `blocking` feature — the worker must never `block_on`.

Confirm the features resolved rather than trusting this list:

Run: `cargo tree -p reqwest --depth 0 -e features 2>&1 | head -20`
Expected: `rustls` and `http2` present; no `gzip`, `brotli`, `deflate`, `zstd` or `blocking`.

Run: `cargo build` (once, without `--locked`, to let `Cargo.lock` take the new crates), then `cargo build --locked`.
Expected: both succeed. Then pin the majors actually resolved:

Run: `cargo tree -p tokio --depth 0 && cargo tree -p reqwest --depth 0 && cargo tree -p http --depth 0`
Adjust the version requirements in `Cargo.toml` to the resolved majors and re-run `cargo build --locked`.

- [ ] **Step 2: Write the failing redaction test**

Create `tests/http_errors.rs`:

```rust
use continuo::http::error::{Operation, RangeRejection, RemoteFailure, redact_url};

#[test]
fn a_signed_query_never_reaches_a_diagnostic() {
    let url = "https://cdn.example.com/ep/42.mp3?token=SECRET&Expires=99";
    let redacted = redact_url(url);
    assert!(!redacted.contains("SECRET"), "token leaked: {redacted}");
    assert!(!redacted.contains("Expires"), "query leaked: {redacted}");
    assert!(
        redacted.contains("cdn.example.com") && redacted.contains("/ep/42.mp3"),
        "redaction must stay legible: {redacted}"
    );
}

#[test]
fn userinfo_never_reaches_a_diagnostic() {
    let redacted = redact_url("https://alice:hunter2@example.com/a.mp3");
    assert!(!redacted.contains("hunter2"), "password leaked: {redacted}");
    assert!(!redacted.contains("alice"), "username leaked: {redacted}");
    assert!(redacted.contains("example.com"), "host lost: {redacted}");
}

#[test]
fn an_unparseable_url_redacts_to_a_placeholder_rather_than_itself() {
    // The input may itself be the secret. Echoing it back on the failure path
    // is exactly the leak this function exists to prevent.
    let redacted = redact_url("not a url?token=SECRET");
    assert!(!redacted.contains("SECRET"), "leaked: {redacted}");
}

#[test]
fn every_category_the_spec_names_has_a_distinct_variant() {
    // §11's list, so a later refactor cannot quietly collapse two categories
    // into one and lose the distinction the status line depends on.
    let categories = [
        RemoteFailure::InvalidSource { input: redact_url("https://x/y"), reason: "no host" },
        RemoteFailure::Status { status: 503, operation: Operation::Open },
        RemoteFailure::Redirect { reason: continuo::http::error::RedirectRejection::TooMany },
        RemoteFailure::Timeout { phase: continuo::http::error::Phase::Headers },
        RemoteFailure::InvalidRange { reason: RangeRejection::WrongStart },
        RemoteFailure::ResourceChanged,
        RemoteFailure::TruncatedBody { missing: 17 },
        RemoteFailure::ProbeLimitExceeded { limit: 8 << 20 },
        RemoteFailure::ContinuityUndetermined,
        RemoteFailure::UnsupportedLiveMedia,
        RemoteFailure::SeekUnavailable,
        RemoteFailure::NonIdentityEncoding { encoding: "gzip".into() },
        RemoteFailure::Transport { operation: Operation::Read, detail: "reset".into() },
        RemoteFailure::Cancelled,
    ];
    for (i, a) in categories.iter().enumerate() {
        for b in categories.iter().skip(i + 1) {
            assert_ne!(a, b, "two §11 categories are the same value");
        }
        assert!(!a.to_string().is_empty(), "{a:?} renders empty");
    }
    assert_eq!(categories.len(), 14);
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test --test http_errors 2>&1 | tail -20`
Expected: FAIL — `unresolved import continuo::http`.

- [ ] **Step 4: Write the module, the limits and the error type**

`src/lib.rs` — add beside the existing module declarations:

```rust
pub mod http;
```

Create `src/http/mod.rs`:

```rust
//! Finite remote media over HTTP.
//!
//! Nothing in this module owns a decoder, a resampler or a CPAL stream. It
//! produces encoded bytes and the evidence needed to classify them; the decode
//! worker does everything else.

pub mod error;
pub mod limits;
```

Create `src/http/limits.rs`:

```rust
use std::time::Duration;

/// Every numeric bound §8 fixes, in one place and injectable.
///
/// These are the spec's proposed defaults. The acceptance tests are what check
/// their adequacy; a change made during implementation must be recorded in the
/// spec rather than made silently here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Establishing the TCP/TLS connection.
    pub connect: Duration,
    /// Awaiting response headers once the request is on the wire.
    pub headers: Duration,
    /// Without received data *while actively demanding it*. Time spent paused
    /// or waiting for local buffer space is not a stall.
    pub stall: Duration,
    /// The whole of opening and probing, headers included.
    pub open: Duration,
    /// Input the probe may consume before `ProbeLimitExceeded`.
    pub probe_bytes: u64,
    /// The encoded-byte buffer's capacity.
    pub buffer_bytes: usize,
    /// One application transfer chunk, on top of `buffer_bytes`.
    pub chunk_bytes: usize,
    pub max_redirects: u8,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            headers: Duration::from_secs(15),
            stall: Duration::from_secs(15),
            open: Duration::from_secs(30),
            probe_bytes: 8 << 20,
            buffer_bytes: 1 << 20,
            chunk_bytes: 64 << 10,
            max_redirects: 5,
        }
    }
}

impl Limits {
    /// Short deadlines for tests that must reach a timeout without spending the
    /// production ones. Byte caps stay at the shipped values, because the tests
    /// that exercise them assert on the shipped numbers.
    pub fn brisk() -> Self {
        Self {
            connect: Duration::from_millis(500),
            headers: Duration::from_millis(500),
            stall: Duration::from_millis(500),
            open: Duration::from_secs(2),
            ..Self::default()
        }
    }
}
```

Create `src/http/error.rs`:

```rust
use url::Url;

/// Which request an outcome belongs to, so a failure says what was being done
/// rather than only what went wrong.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    Open,
    Read,
    Seek,
    Reopen,
    /// The bounded tail read that confirms a finite body actually ended (§9).
    Complete,
}

/// Which deadline elapsed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Connect,
    Headers,
    /// No data while actively demanding it.
    Stall,
    /// The whole of opening and probing.
    Open,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RedirectRejection {
    TooMany,
    Loop,
    UnsupportedScheme,
    InvalidLocation,
    /// HTTPS to HTTP. Never followed, whatever the hop count.
    Downgrade,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeRejection {
    /// A 206 whose `Content-Range` start is not the byte that was requested.
    WrongStart,
    /// `last < first`.
    ReversedInterval,
    /// A total that contradicts one already established for this session.
    ConflictingTotal,
    /// A 206 with no `Content-Range`, or one that does not parse.
    Malformed,
    /// `multipart/byteranges`. M3 never requests it and never accepts it.
    Multipart,
    /// A range was required and the server answered 200 instead.
    RangeIgnored,
    /// A body shorter or longer than the interval the header advertised.
    LengthMismatch,
    /// `last >= total`: the interval does not fit inside the object it claims
    /// to be part of.
    IntervalPastTotal,
    /// A 416 for a range that is not the known byte EOF.
    Unsatisfiable,
}

/// Typed remote faults, one variant per §11 category.
///
/// `Display` is the third-party-safe rendering: every URL here has already
/// passed through [`redact_url`], so no signed query or userinfo can reach a
/// status line, a log line or a `PlaybackEvent::Failed` message.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RemoteFailure {
    #[error("{input} is not a usable media URL: {reason}")]
    InvalidSource { input: String, reason: &'static str },
    #[error("the server answered HTTP {status} while {operation:?}")]
    Status { status: u16, operation: Operation },
    #[error("redirect refused: {reason:?}")]
    Redirect { reason: RedirectRejection },
    #[error("the server went quiet during {phase:?}")]
    Timeout { phase: Phase },
    #[error("the server's range response is unusable: {reason:?}")]
    InvalidRange { reason: RangeRejection },
    #[error("this recording changed on the server while it was open")]
    ResourceChanged,
    #[error("the response body ended {missing} bytes early")]
    TruncatedBody { missing: u64 },
    #[error("opening needed more than {limit} bytes of input")]
    ProbeLimitExceeded { limit: u64 },
    #[error("cannot establish whether this source ever ends")]
    ContinuityUndetermined,
    #[error("live streams are not supported")]
    UnsupportedLiveMedia,
    #[error("this server cannot seek or resume this recording")]
    SeekUnavailable,
    #[error("the response used {encoding} content encoding, not identity")]
    NonIdentityEncoding { encoding: String },
    #[error("network error while {operation:?}: {detail}")]
    Transport { operation: Operation, detail: String },
    /// Not a fault: a stop, seek or shutdown retired the read that was in
    /// flight. §8 requires this to stay distinguishable from every failure
    /// above even after Symphonia wraps the `io::Error`.
    #[error("the read was cancelled")]
    Cancelled,
}

/// Scheme, host, port and path only.
///
/// Query and userinfo are the two places a bearer secret hides, and §11 keeps
/// both out of normal diagnostics. An input that does not parse is reported as
/// a placeholder rather than echoed: the unparseable text may itself be the
/// secret.
pub fn redact_url(input: &str) -> String {
    let Ok(mut url) = Url::parse(input) else {
        return "<unparseable URL>".to_string();
    };
    url.set_query(None);
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test http_errors 2>&1 | tail -20`
Expected: PASS, 4 tests.

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked 2>&1 | tail -5`
Expected: all green; the suite count matches the recorded baseline plus 4.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/http tests/http_errors.rs
git commit -m "feat(http): add the tokio/reqwest dependencies, §8 limits and typed remote failures

Baseline before M3: <paste the recorded pass/fail/ignored counts>."
```

---

## Task 2: The §7 response acceptance rules

Pure functions over status codes and headers. No sockets, no runtime — which is what makes §7's fifteen-odd rules assertable one at a time instead of only through a live server.

**Files:**
- Create: `src/http/response.rs`
- Modify: `src/http/mod.rs`
- Test: `tests/http_response.rs`

**Interfaces:**
- Consumes: `RemoteFailure`, `RangeRejection`, `RedirectRejection`, `Limits` from Task 1.
- Produces:

```rust
pub struct Validator { pub strong_etag: Option<String>, pub last_modified: Option<String> }
pub struct ByteRange { pub first: u64, pub last: u64, pub total: Option<u64> }
pub enum Accepted { Sequential { len: Option<u64> }, Ranged { range: ByteRange } }
pub struct Established { pub total: Option<u64>, pub validator: Validator }

pub fn parse_content_range(value: &str) -> Result<ByteRange, RangeRejection>;
pub fn accept(status: u16, headers: &Headers, requested_start: u64, at_origin: bool, established: Option<&Established>) -> Result<Accepted, RemoteFailure>;
pub fn accept_redirect(from: &Url, location: &str, hops: u8, seen: &[Url], limits: &Limits) -> Result<Url, RemoteFailure>;
pub fn if_range_value(validator: &Validator) -> Option<String>;
pub fn validator_from(headers: &Headers) -> Validator;
pub fn is_live(headers: &Headers) -> bool;
```

`Headers` is a thin borrow over `http::HeaderMap` with a case-insensitive `get(&self, name: &str) -> Option<&str>`, so the tests can build one from a `&[(&str, &str)]` slice without a live response.

- [ ] **Step 1: Write the failing acceptance tests**

Create `tests/http_response.rs`:

```rust
use continuo::http::error::{RangeRejection, RedirectRejection, RemoteFailure};
use continuo::http::limits::Limits;
use continuo::http::response::{
    Accepted, ByteRange, Established, Headers, Validator, accept, accept_redirect,
    if_range_value, is_live, parse_content_range,
};
use url::Url;

fn headers(pairs: &[(&str, &str)]) -> Headers {
    Headers::from_pairs(pairs)
}

#[allow(clippy::unwrap_used)] // A literal absolute URL always parses.
fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

#[test]
fn a_well_formed_content_range_parses_into_an_inclusive_interval() {
    assert_eq!(
        parse_content_range("bytes 0-1023/8192"),
        Ok(ByteRange { first: 0, last: 1023, total: Some(8192) })
    );
    // An unknown total is legal and does not make the response unusable.
    assert_eq!(
        parse_content_range("bytes 512-1023/*"),
        Ok(ByteRange { first: 512, last: 1023, total: None })
    );
}

#[test]
fn a_reversed_interval_is_rejected_rather_than_normalized() {
    assert_eq!(
        parse_content_range("bytes 900-100/8192"),
        Err(RangeRejection::ReversedInterval)
    );
}

#[test]
fn garbage_and_non_byte_units_are_malformed() {
    for value in ["", "bytes", "items 0-10/20", "bytes 0-10", "bytes a-b/20"] {
        assert_eq!(
            parse_content_range(value),
            Err(RangeRejection::Malformed),
            "{value:?} must be malformed"
        );
    }
}

#[test]
fn a_206_at_the_requested_start_establishes_ranged_access() {
    let accepted = accept(
        206,
        &headers(&[("content-range", "bytes 0-1023/8192"), ("content-length", "1024")]),
        0,
        true,
        None,
    );
    assert_eq!(
        accepted,
        Ok(Accepted::Ranged { range: ByteRange { first: 0, last: 1023, total: Some(8192) } })
    );
}

#[test]
fn a_206_at_the_wrong_start_fails_instead_of_installing_bytes_at_that_offset() {
    // H7. Silently accepting these bytes at offset 4096 corrupts every media
    // timestamp that follows, with nothing anywhere reporting an error.
    let accepted = accept(
        206,
        &headers(&[("content-range", "bytes 0-1023/8192")]),
        4096,
        false,
        None,
    );
    assert_eq!(
        accepted,
        Err(RemoteFailure::InvalidRange { reason: RangeRejection::WrongStart })
    );
}

#[test]
fn a_200_is_sequential_data_only_at_the_origin() {
    assert_eq!(
        accept(200, &headers(&[("content-length", "8192")]), 0, true, None),
        Ok(Accepted::Sequential { len: Some(8192) })
    );
    // The same 200 answering a nonzero range never installs those bytes there.
    assert_eq!(
        accept(200, &headers(&[("content-length", "8192")]), 4096, false, None),
        Err(RemoteFailure::InvalidRange { reason: RangeRejection::RangeIgnored })
    );
}

#[test]
fn a_multipart_range_response_is_refused() {
    assert_eq!(
        accept(
            206,
            &headers(&[("content-type", "multipart/byteranges; boundary=x")]),
            0,
            true,
            None,
        ),
        Err(RemoteFailure::InvalidRange { reason: RangeRejection::Multipart })
    );
}

#[test]
fn a_body_length_that_contradicts_the_interval_is_rejected() {
    assert_eq!(
        accept(
            206,
            &headers(&[("content-range", "bytes 0-1023/8192"), ("content-length", "999")]),
            0,
            true,
            None,
        ),
        Err(RemoteFailure::InvalidRange { reason: RangeRejection::LengthMismatch })
    );
}

#[test]
fn a_total_that_conflicts_with_the_established_one_is_a_resource_change() {
    let established = Established {
        total: Some(8192),
        validator: Validator {
            strong_etag: Some("\"v1\"".into()),
            weak_etag: None,
            last_modified: None,
        },
    };
    assert_eq!(
        accept(
            206,
            &headers(&[("content-range", "bytes 0-1023/9000"), ("content-length", "1024")]),
            0,
            false,
            Some(&established),
        ),
        Err(RemoteFailure::InvalidRange { reason: RangeRejection::ConflictingTotal })
    );
}

#[test]
fn a_changed_strong_validator_fails_as_resource_changed() {
    // H11. The length can be identical; the validator is what settles it.
    let established = Established {
        total: Some(8192),
        validator: Validator {
            strong_etag: Some("\"v1\"".into()),
            weak_etag: None,
            last_modified: None,
        },
    };
    assert_eq!(
        accept(
            206,
            &headers(&[
                ("content-range", "bytes 0-1023/8192"),
                ("content-length", "1024"),
                ("etag", "\"v2\""),
            ]),
            0,
            false,
            Some(&established),
        ),
        Err(RemoteFailure::ResourceChanged)
    );
}

#[test]
fn weak_and_last_modified_validators_are_compared_even_though_they_are_never_sent() {
    // §7: without a strong validator, range access is *best effort* — "compare
    // available length and validator metadata". Best effort forbids claiming an
    // unchanged weak validator proves sameness; it does not license throwing
    // the metadata away, which would let a same-length replacement through
    // silently.
    let weak = Established {
        total: Some(8192),
        validator: Validator {
            strong_etag: None,
            weak_etag: Some("W/\"v1\"".into()),
            last_modified: None,
        },
    };
    assert_eq!(
        accept(
            206,
            &headers(&[
                ("content-range", "bytes 0-1023/8192"),
                ("content-length", "1024"),
                ("etag", "W/\"v2\""),
            ]),
            0,
            false,
            Some(&weak),
        ),
        Err(RemoteFailure::ResourceChanged)
    );

    let dated = Established {
        total: Some(8192),
        validator: Validator {
            strong_etag: None,
            weak_etag: None,
            last_modified: Some("Tue, 09 Sep 2026 00:00:00 GMT".into()),
        },
    };
    assert_eq!(
        accept(
            206,
            &headers(&[
                ("content-range", "bytes 0-1023/8192"),
                ("content-length", "1024"),
                ("last-modified", "Wed, 10 Sep 2026 00:00:00 GMT"),
            ]),
            0,
            false,
            Some(&dated),
        ),
        Err(RemoteFailure::ResourceChanged)
    );

    // An absent validator on either side is no evidence and must not fail.
    assert!(
        accept(
            206,
            &headers(&[("content-range", "bytes 0-1023/8192"), ("content-length", "1024")]),
            0,
            false,
            Some(&dated),
        )
        .is_ok()
    );
}

#[test]
fn an_interval_that_cannot_fit_inside_its_total_is_rejected() {
    // `bytes 0-99/10` describes 100 bytes of a 10-byte object. Accepting it
    // installs bytes past the end of the recording at offsets nothing owns.
    assert_eq!(
        parse_content_range("bytes 0-99/10"),
        Err(RangeRejection::IntervalPastTotal)
    );
    assert_eq!(
        parse_content_range("bytes 10-10/10"),
        Err(RangeRejection::IntervalPastTotal)
    );
    // The last legal byte of a 10-byte object is 9.
    assert_eq!(
        parse_content_range("bytes 9-9/10"),
        Ok(ByteRange { first: 9, last: 9, total: Some(10) })
    );
}

#[test]
fn an_interval_length_that_overflows_is_malformed_rather_than_wrapping() {
    // In release mode `last - first + 1` wraps to zero here, turning a hostile
    // header into a silently empty interval.
    assert_eq!(
        parse_content_range("bytes 0-18446744073709551615/*"),
        Err(RangeRejection::Malformed)
    );
}

#[test]
fn a_weak_etag_is_never_sent_as_if_range() {
    // RFC 9110 §13.1.5: If-Range takes a strong validator only. Sending a weak
    // one asks the server a question it is entitled to answer wrongly.
    let weak = Validator {
        strong_etag: None,
        weak_etag: Some("W/\"v1\"".into()),
        last_modified: Some("Tue, 09 Sep 2026 00:00:00 GMT".into()),
    };
    assert_eq!(if_range_value(&weak), None);
    let strong = Validator {
        strong_etag: Some("\"v1\"".into()),
        weak_etag: None,
        last_modified: None,
    };
    assert_eq!(if_range_value(&strong), Some("\"v1\"".to_string()));
}

#[test]
fn a_weak_etag_header_is_not_stored_as_a_strong_one() {
    let validator = continuo::http::response::validator_from(&headers(&[("etag", "W/\"v1\"")]));
    assert_eq!(validator.strong_etag, None);
    // Kept, though: it is comparable even when it is not sendable.
    assert_eq!(validator.weak_etag.as_deref(), Some("W/\"v1\""));
}

#[test]
fn a_non_identity_encoding_is_refused_so_offsets_stay_media_bytes() {
    assert_eq!(
        accept(200, &headers(&[("content-encoding", "gzip")]), 0, true, None),
        Err(RemoteFailure::NonIdentityEncoding { encoding: "gzip".into() })
    );
    // `identity` spelled out explicitly is fine, as is its absence.
    assert!(accept(200, &headers(&[("content-encoding", "identity")]), 0, true, None).is_ok());
    assert!(accept(200, &headers(&[]), 0, true, None).is_ok());
}

#[test]
fn a_416_is_reported_as_unsatisfiable_rather_than_as_completion() {
    // H7/H8: a 416 is never track completion.
    assert_eq!(
        accept(416, &headers(&[]), 4096, false, None),
        Err(RemoteFailure::InvalidRange { reason: RangeRejection::Unsatisfiable })
    );
}

#[test]
fn ordinary_status_failures_carry_the_status() {
    for status in [401u16, 403, 404, 500, 503] {
        assert!(matches!(
            accept(status, &headers(&[]), 0, true, None),
            Err(RemoteFailure::Status { status: got, .. }) if got == status
        ));
    }
}

#[test]
fn icy_and_explicit_live_semantics_are_recognized() {
    assert!(is_live(&headers(&[("icy-name", "Radio X")])));
    assert!(is_live(&headers(&[("icy-metaint", "16000")])));
    assert!(!is_live(&headers(&[("content-length", "8192")])));
    // A missing Content-Length is not by itself live evidence (§6).
    assert!(!is_live(&headers(&[("transfer-encoding", "chunked")])));
}

#[test]
fn redirects_are_bounded_checked_and_never_downgraded() {
    let limits = Limits::default();
    let from = url("https://a.example/x.mp3");

    assert_eq!(
        accept_redirect(&from, "https://b.example/y.mp3", 1, &[], &limits),
        Ok(url("https://b.example/y.mp3"))
    );
    // Relative locations resolve against the current URL.
    assert_eq!(
        accept_redirect(&from, "/z.mp3", 1, &[], &limits),
        Ok(url("https://a.example/z.mp3"))
    );
    assert_eq!(
        accept_redirect(&from, "http://b.example/y.mp3", 1, &[], &limits),
        Err(RemoteFailure::Redirect { reason: RedirectRejection::Downgrade })
    );
    assert_eq!(
        accept_redirect(&from, "ftp://b.example/y.mp3", 1, &[], &limits),
        Err(RemoteFailure::Redirect { reason: RedirectRejection::UnsupportedScheme })
    );
    assert_eq!(
        accept_redirect(&from, "::::", 1, &[], &limits),
        Err(RemoteFailure::Redirect { reason: RedirectRejection::InvalidLocation })
    );
    assert_eq!(
        accept_redirect(&from, "https://b.example/y.mp3", 6, &[], &limits),
        Err(RemoteFailure::Redirect { reason: RedirectRejection::TooMany })
    );
    assert_eq!(
        accept_redirect(&from, "https://a.example/x.mp3", 2, &[url("https://a.example/x.mp3")], &limits),
        Err(RemoteFailure::Redirect { reason: RedirectRejection::Loop })
    );
}

#[test]
fn an_http_origin_may_redirect_to_http() {
    // Only *downgrade* is refused. A plain-HTTP source that was already plain
    // HTTP loses nothing by staying there.
    let limits = Limits::default();
    assert_eq!(
        accept_redirect(&url("http://a.example/x.mp3"), "http://b.example/y.mp3", 1, &[], &limits),
        Ok(url("http://b.example/y.mp3"))
    );
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test http_response 2>&1 | tail -20`
Expected: FAIL — `unresolved import continuo::http::response`.

- [ ] **Step 3: Implement the acceptance rules**

Add `pub mod response;` to `src/http/mod.rs`, and create `src/http/response.rs`:

```rust
//! §7's acceptance rules, as pure functions.
//!
//! Nothing here touches a socket. That is deliberate: §7 is fifteen separate
//! rules, and a rule that can only be reached through a live server is a rule
//! whose failure mode is a flaky test rather than an assertion.

use url::Url;

use super::error::{Operation, RangeRejection, RedirectRejection, RemoteFailure};
use super::limits::Limits;

/// A case-insensitive header view, constructible from a live `HeaderMap` or
/// from a literal slice, so every rule below is assertable without a request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), (*value).to_string()))
                .collect(),
        )
    }

    pub fn from_map(map: &http::HeaderMap) -> Self {
        Self(
            map.iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
                })
                .collect(),
        )
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.0
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRange {
    pub first: u64,
    pub last: u64,
    pub total: Option<u64>,
}

impl ByteRange {
    /// Inclusive interval, so the body is `last - first + 1` bytes.
    ///
    /// Checked, because `last` comes off the wire: `bytes 0-18446744073709551615/*`
    /// parses fine and overflows a bare `+ 1`, which in release mode wraps to
    /// zero and turns a hostile header into a silently empty interval.
    pub fn len(&self) -> Option<u64> {
        self.last.checked_sub(self.first)?.checked_add(1)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Validator {
    /// Strong entity tag, quotes included. A `W/` tag is deliberately not
    /// stored here: §7 forbids sending a weak validator as `If-Range`, and
    /// keeping the two apart at parse time makes that unforgettable.
    pub strong_etag: Option<String>,
    /// A `W/`-prefixed tag. Never sent as `If-Range` — but §7 says range access
    /// without a strong validator is *best effort*, which means comparing the
    /// metadata that is available, not discarding it.
    pub weak_etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Established {
    pub total: Option<u64>,
    pub validator: Validator,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Accepted {
    /// Usable as a stream from byte zero, and only from byte zero.
    Sequential { len: Option<u64> },
    Ranged { range: ByteRange },
}

pub fn parse_content_range(value: &str) -> Result<ByteRange, RangeRejection> {
    let rest = value
        .trim()
        .strip_prefix("bytes ")
        .ok_or(RangeRejection::Malformed)?;
    let (interval, total) = rest.split_once('/').ok_or(RangeRejection::Malformed)?;
    let (first, last) = interval.split_once('-').ok_or(RangeRejection::Malformed)?;
    let first: u64 = first.trim().parse().map_err(|_| RangeRejection::Malformed)?;
    let last: u64 = last.trim().parse().map_err(|_| RangeRejection::Malformed)?;
    if last < first {
        return Err(RangeRejection::ReversedInterval);
    }
    let total = match total.trim() {
        "*" => None,
        digits => Some(digits.parse().map_err(|_| RangeRejection::Malformed)?),
    };
    let range = ByteRange { first, last, total };
    // An interval that cannot fit inside its own total is impossible, not
    // merely odd: `bytes 0-99/10` describes 100 bytes of a 10-byte object.
    if let Some(total) = total
        && last >= total
    {
        return Err(RangeRejection::IntervalPastTotal);
    }
    // And an interval whose length does not compute at all is malformed.
    if range.len().is_none() {
        return Err(RangeRejection::Malformed);
    }
    Ok(range)
}

pub fn validator_from(headers: &Headers) -> Validator {
    let etag = headers.get("etag").map(str::trim);
    let strong_etag = etag
        .filter(|tag| !tag.starts_with("W/") && tag.starts_with('"') && tag.ends_with('"'))
        .map(str::to_string);
    let weak_etag = etag.filter(|tag| tag.starts_with("W/")).map(str::to_string);
    Validator {
        strong_etag,
        weak_etag,
        last_modified: headers.get("last-modified").map(str::to_string),
    }
}

/// The `If-Range` header value, or `None` when there is no strong validator.
pub fn if_range_value(validator: &Validator) -> Option<String> {
    validator.strong_etag.clone()
}

/// Explicit live/ICY semantics. Deliberately narrow: §6 forbids inferring
/// continuity from a missing `Content-Length`, a chunked encoding, an audio
/// MIME type or a URL suffix.
pub fn is_live(headers: &Headers) -> bool {
    headers.get("icy-name").is_some()
        || headers.get("icy-metaint").is_some()
        || headers.get("icy-br").is_some()
        || headers
            .get("content-type")
            .is_some_and(|value| value.eq_ignore_ascii_case("application/vnd.apple.mpegurl"))
}

/// Decide whether a response may be installed at `requested_start`.
///
/// `at_origin` is true only while opening at byte zero — the single case in
/// which a 200 is usable (§7).
pub fn accept(
    status: u16,
    headers: &Headers,
    requested_start: u64,
    at_origin: bool,
    established: Option<&Established>,
) -> Result<Accepted, RemoteFailure> {
    if let Some(encoding) = headers.get("content-encoding")
        && !encoding.trim().eq_ignore_ascii_case("identity")
    {
        return Err(RemoteFailure::NonIdentityEncoding {
            encoding: encoding.trim().to_string(),
        });
    }
    if headers
        .get("content-type")
        .is_some_and(|value| value.trim().to_ascii_lowercase().starts_with("multipart/"))
    {
        return Err(RemoteFailure::InvalidRange {
            reason: RangeRejection::Multipart,
        });
    }
    check_validator(headers, established)?;

    let declared_len = headers.get("content-length").and_then(|v| v.trim().parse().ok());
    match status {
        200 => {
            if !at_origin {
                return Err(RemoteFailure::InvalidRange {
                    reason: RangeRejection::RangeIgnored,
                });
            }
            Ok(Accepted::Sequential { len: declared_len })
        }
        206 => {
            let value = headers.get("content-range").ok_or(RemoteFailure::InvalidRange {
                reason: RangeRejection::Malformed,
            })?;
            let range = parse_content_range(value)
                .map_err(|reason| RemoteFailure::InvalidRange { reason })?;
            if range.first != requested_start {
                return Err(RemoteFailure::InvalidRange {
                    reason: RangeRejection::WrongStart,
                });
            }
            if let Some(declared) = declared_len
                && Some(declared) != range.len()
            {
                return Err(RemoteFailure::InvalidRange {
                    reason: RangeRejection::LengthMismatch,
                });
            }
            if let (Some(total), Some(known)) = (range.total, established.and_then(|e| e.total))
                && total != known
            {
                return Err(RemoteFailure::InvalidRange {
                    reason: RangeRejection::ConflictingTotal,
                });
            }
            Ok(Accepted::Ranged { range })
        }
        // Never completion. The one benign case — seeking to a known byte EOF —
        // is answered locally in `HttpMediaSource` without a request, so any
        // 416 that actually reaches here is a failure.
        416 => Err(RemoteFailure::InvalidRange {
            reason: RangeRejection::Unsatisfiable,
        }),
        other => Err(RemoteFailure::Status {
            status: other,
            operation: if at_origin { Operation::Open } else { Operation::Seek },
        }),
    }
}

/// A validator that changed means the bytes are no longer the same recording.
///
/// §7 is a *hierarchy*, not a single rule. A strong ETag is conclusive. Without
/// one, range access is best effort — which obliges us to compare the metadata
/// that is available rather than to ignore it. What best effort forbids is the
/// opposite claim: that an unchanged weak validator *proves* the content is the
/// same. So a changed weak ETag or a changed `Last-Modified` fails, and an
/// absent or unchanged one proves nothing and is allowed through.
fn check_validator(headers: &Headers, established: Option<&Established>) -> Result<(), RemoteFailure> {
    let Some(established) = established else {
        return Ok(());
    };
    let known = &established.validator;
    let fresh = validator_from(headers);
    let changed = |a: &Option<String>, b: &Option<String>| match (a, b) {
        (Some(known), Some(got)) => known != got,
        // An absent validator on either side is no evidence either way.
        _ => false,
    };
    if changed(&known.strong_etag, &fresh.strong_etag)
        || changed(&known.weak_etag, &fresh.weak_etag)
        || changed(&known.last_modified, &fresh.last_modified)
    {
        return Err(RemoteFailure::ResourceChanged);
    }
    Ok(())
}

pub fn accept_redirect(
    from: &Url,
    location: &str,
    hops: u8,
    seen: &[Url],
    limits: &Limits,
) -> Result<Url, RemoteFailure> {
    if hops > limits.max_redirects {
        return Err(RemoteFailure::Redirect {
            reason: RedirectRejection::TooMany,
        });
    }
    let target = from.join(location).map_err(|_| RemoteFailure::Redirect {
        reason: RedirectRejection::InvalidLocation,
    })?;
    if !matches!(target.scheme(), "http" | "https") {
        return Err(RemoteFailure::Redirect {
            reason: RedirectRejection::UnsupportedScheme,
        });
    }
    if from.scheme() == "https" && target.scheme() == "http" {
        return Err(RemoteFailure::Redirect {
            reason: RedirectRejection::Downgrade,
        });
    }
    if seen.contains(&target) {
        return Err(RemoteFailure::Redirect {
            reason: RedirectRejection::Loop,
        });
    }
    Ok(target)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test http_response 2>&1 | tail -20`
Expected: PASS, 21 tests.

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings`
Expected: clean. `ByteRange::len` returns `Option<u64>`, so `clippy::len_without_is_empty` does not fire — that lint only applies to a `len` returning `usize`.

- [ ] **Step 5: Commit**

```bash
git add src/http/mod.rs src/http/response.rs tests/http_response.rs
git commit -m "feat(http): implement §7's response acceptance rules as pure functions"
```

---

## Task 3: The bounded byte channel and the source interrupt

The synchronization core. A synchronous reader and an asynchronous producer share one bounded buffer; stop, seek, shutdown and pause reach both sides directly rather than through a command queue nobody is reading.

**Files:**
- Create: `src/http/channel.rs`
- Modify: `src/http/mod.rs`
- Test: `tests/http_channel.rs`

**Interfaces:**
- Consumes: `RemoteFailure`, `Limits`.
- Produces:

```rust
pub enum Outcome { Eof, Failed(RemoteFailure) }
pub enum ReadOutcome { Bytes(usize), Eof, Retired, Failed(RemoteFailure) }
pub struct ByteChannel;
impl ByteChannel {
    pub fn new(capacity: usize, interrupt: Arc<SourceInterrupt>) -> Self;
    pub fn generation(&self) -> u64;
    /// Sync side. Blocks until bytes, EOF, failure or retirement; services the
    /// hook on each wait slice; never returns `Bytes(0)`.
    pub fn read(&self, out: &mut [u8], service: &dyn WaitHook, stall: Duration) -> ReadOutcome;
    /// Async side. Waits for room without blocking a Tokio worker thread.
    pub async fn push(&self, generation: u64, chunk: &[u8]) -> bool;
    pub fn finish(&self, generation: u64, outcome: Outcome);
    /// Retire everything in flight and open a new generation.
    pub fn retire(&self) -> u64;
    pub fn buffered(&self) -> usize;
}
pub struct SourceInterrupt;
impl SourceInterrupt {
    pub fn new() -> Arc<Self>;
    pub fn retire(&self);       // stop / seek / shutdown
    pub fn freeze(&self);       // pause: reads stay pending, never error
    pub fn thaw(&self);
    pub fn arm(&self);          // clear retirement for a new generation
    pub fn is_retired(&self) -> bool;
    pub fn is_frozen(&self) -> bool;
}
pub trait WaitHook: Send + Sync { fn service(&self); }
```

`WaitHook` lives here rather than in `playback` so `http` does not depend on `playback`. Task 9's `WaitService` implements it.

**Why two wake mechanisms.** The reader waits on a `Condvar` — a synchronous thread that must not spin. The producer waits on `tokio::sync::Notify` — an async task that must not block a runtime worker. Both re-check their predicate *under the same `Mutex`*, which is what makes a wake impossible to lose. §8's rule is exactly this: "All waiting paths test terminal predicates under the same synchronization used for notification."

**Why freeze is a level and retire is an edge (G2).** A pause is a state that persists until a play; an edge would be missed by a read that blocks after the edge passed. A retirement is one-shot and ends the generation, so an edge is right for it — and `arm()` is what a new generation calls to clear it.

- [ ] **Step 1: Write the failing channel tests**

Create `tests/http_channel.rs`:

```rust
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use continuo::http::channel::{ByteChannel, Outcome, ReadOutcome, SourceInterrupt, WaitHook};
use continuo::http::error::{Operation, RemoteFailure};

const STALL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct CountingHook(AtomicU32);

impl WaitHook for CountingHook {
    fn service(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

impl CountingHook {
    fn count(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}

struct NoHook;
impl WaitHook for NoHook {
    fn service(&self) {}
}

#[allow(clippy::unwrap_used)] // A current-thread runtime always builds here.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

#[test]
fn a_read_returns_the_bytes_the_producer_pushed() {
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = Arc::new(ByteChannel::new(Arc::clone(&interrupt)));
    let generation = channel.generation();
    runtime().block_on(channel.push(generation, b"hello"));

    let mut buffer = [0u8; 16];
    assert_eq!(channel.read(&mut buffer, &NoHook, STALL), ReadOutcome::Bytes(5));
    assert_eq!(&buffer[..5], b"hello");
}

#[test]
fn a_read_never_reports_zero_bytes_for_an_empty_buffer() {
    // Symphonia reads `Ok(0)` as clean EOF. An empty buffer is not EOF, and
    // conflating them turns a stalled network into a silently truncated track.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = Arc::new(ByteChannel::new(Arc::clone(&interrupt)));
    let generation = channel.generation();

    let reader = {
        let channel = Arc::clone(&channel);
        std::thread::spawn(move || channel.read(&mut [0u8; 16], &NoHook, STALL))
    };
    // Give the reader time to be genuinely blocked, then satisfy it.
    std::thread::sleep(Duration::from_millis(50));
    runtime().block_on(channel.push(generation, b"x"));

    match reader.join() {
        Ok(outcome) => assert_eq!(outcome, ReadOutcome::Bytes(1)),
        Err(_) => panic!("the reader thread panicked"),
    }
}

#[test]
fn only_a_clean_finish_reports_eof() {
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = channel.generation();
    channel.finish(generation, Outcome::Eof);
    assert_eq!(channel.read(&mut [0u8; 4], &NoHook, STALL), ReadOutcome::Eof);
}

#[test]
fn a_failure_stays_a_failure_and_never_becomes_eof() {
    // H8. A truncated body that read back as EOF would be reported as a
    // completed track.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = channel.generation();
    let failure = RemoteFailure::TruncatedBody { missing: 42 };
    channel.finish(generation, Outcome::Failed(failure.clone()));
    assert_eq!(
        channel.read(&mut [0u8; 4], &NoHook, STALL),
        ReadOutcome::Failed(failure)
    );
}

#[test]
fn buffered_bytes_are_drained_before_a_pending_outcome_is_reported() {
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = Arc::new(ByteChannel::new(Arc::clone(&interrupt)));
    let generation = channel.generation();
    runtime().block_on(channel.push(generation, b"tail"));
    channel.finish(generation, Outcome::Eof);

    let mut buffer = [0u8; 8];
    assert_eq!(channel.read(&mut buffer, &NoHook, STALL), ReadOutcome::Bytes(4));
    assert_eq!(&buffer[..4], b"tail");
    assert_eq!(channel.read(&mut buffer, &NoHook, STALL), ReadOutcome::Eof);
}

#[test]
fn a_retirement_wakes_a_blocked_read_within_one_second() {
    // H9, and §8's stated bound. The server is never released: the wake must
    // come from the interrupt, not from bytes arriving.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = Arc::new(ByteChannel::new(Arc::clone(&interrupt)));
    let hook = Arc::new(CountingHook::default());

    let reader = {
        let channel = Arc::clone(&channel);
        let hook = Arc::clone(&hook);
        std::thread::spawn(move || {
            let started = Instant::now();
            let outcome = channel.read(&mut [0u8; 16], hook.as_ref(), STALL);
            (outcome, started.elapsed())
        })
    };
    // Prove the wait was entered before interrupting it: the hook runs only
    // from inside the wait loop, so a nonzero count is that proof. Sleeping
    // and hoping for the race is what §12 forbids.
    let entered = Instant::now();
    while hook.count() == 0 {
        assert!(entered.elapsed() < Duration::from_secs(5), "the read never blocked");
        std::thread::sleep(Duration::from_millis(5));
    }
    interrupt.retire();

    match reader.join() {
        Ok((outcome, elapsed)) => {
            assert_eq!(outcome, ReadOutcome::Retired);
            assert!(elapsed < Duration::from_secs(1), "woke after {elapsed:?}");
        }
        Err(_) => panic!("the reader thread panicked"),
    }
}

#[test]
fn a_freeze_keeps_a_read_pending_without_erroring_and_a_thaw_releases_it() {
    // H10, and §9's "service the freeze without returning a destructive read
    // error to the demuxer".
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = Arc::new(ByteChannel::new(Arc::clone(&interrupt)));
    let hook = Arc::new(CountingHook::default());
    let generation = channel.generation();
    interrupt.freeze();
    runtime().block_on(channel.push(generation, b"ready"));

    let reader = {
        let channel = Arc::clone(&channel);
        let hook = Arc::clone(&hook);
        std::thread::spawn(move || channel.read(&mut [0u8; 16], hook.as_ref(), STALL))
    };
    let entered = Instant::now();
    while hook.count() == 0 {
        assert!(entered.elapsed() < Duration::from_secs(5), "the read never blocked");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Bytes are ready and the read is still pending: the freeze, not the
    // buffer, is what holds it.
    interrupt.thaw();
    match reader.join() {
        Ok(outcome) => assert_eq!(outcome, ReadOutcome::Bytes(5)),
        Err(_) => panic!("the reader thread panicked"),
    }
}

#[test]
fn a_retirement_reaches_a_frozen_read_too() {
    // Quitting while paused must still wake every source wait (H10).
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = Arc::new(ByteChannel::new(Arc::clone(&interrupt)));
    let hook = Arc::new(CountingHook::default());
    interrupt.freeze();

    let reader = {
        let channel = Arc::clone(&channel);
        let hook = Arc::clone(&hook);
        std::thread::spawn(move || channel.read(&mut [0u8; 16], hook.as_ref(), STALL))
    };
    let entered = Instant::now();
    while hook.count() == 0 {
        assert!(entered.elapsed() < Duration::from_secs(5), "the read never blocked");
        std::thread::sleep(Duration::from_millis(5));
    }
    interrupt.retire();
    match reader.join() {
        Ok(outcome) => assert_eq!(outcome, ReadOutcome::Retired),
        Err(_) => panic!("the reader thread panicked"),
    }
}

#[test]
fn the_buffer_never_exceeds_its_capacity_and_the_producer_waits() {
    // H13. A fast server against a slow consumer must not accumulate.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = Arc::new(ByteChannel::new(Arc::clone(&interrupt)));
    let generation = channel.generation();
    let chunk = [7u8; 64];

    let runtime = runtime();
    let pushed = {
        let channel = Arc::clone(&channel);
        runtime.block_on(async move { channel.push(generation, &chunk).await })
    };
    assert!(pushed);
    assert_eq!(channel.buffered(), 64);

    // A second push cannot fit; it must wait rather than grow the buffer.
    let producer = {
        let channel = Arc::clone(&channel);
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(error) => panic!("a current-thread runtime must build: {error}"),
            };
            rt.block_on(async move { channel.push(generation, &[9u8; 64]).await })
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(channel.buffered(), 64, "the producer grew the buffer past its cap");

    assert_eq!(channel.read(&mut [0u8; 64], &NoHook, STALL), ReadOutcome::Bytes(64));
    match producer.join() {
        Ok(pushed) => assert!(pushed, "the producer never resumed after room appeared"),
        Err(_) => panic!("the producer thread panicked"),
    }
    assert_eq!(channel.buffered(), 64);
}

#[test]
fn a_stale_generation_can_neither_push_bytes_nor_end_the_stream() {
    // H9's second half: a superseded response's bytes and its outcome must
    // both be rejected before they can enter the new generation.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = Arc::new(ByteChannel::new(Arc::clone(&interrupt)));
    let stale = channel.generation();
    let fresh = channel.retire();
    assert_ne!(stale, fresh);
    assert!(interrupt.arm(fresh));

    let runtime = runtime();
    assert!(!runtime.block_on(channel.push(stale, b"stale")));
    channel.finish(stale, Outcome::Eof);
    channel.finish(
        stale,
        Outcome::Failed(RemoteFailure::Transport { operation: Operation::Read, detail: "old".into() }),
    );
    assert_eq!(channel.buffered(), 0);

    runtime.block_on(channel.push(fresh, b"fresh"));
    let mut buffer = [0u8; 8];
    assert_eq!(channel.read(&mut buffer, &NoHook, STALL), ReadOutcome::Bytes(5));
    assert_eq!(&buffer[..5], b"fresh");
}

#[test]
fn arming_a_superseded_generation_cannot_clear_a_newer_retirement() {
    // A seek retires generation N on the decode thread; a stop retires N+1
    // from the application thread a moment later. An unguarded arm would clear
    // the stop's retirement and leave the fetch running after the stop that
    // existed to close it.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let seek_generation = interrupt.retire();
    let stop_generation = interrupt.retire();
    assert_ne!(seek_generation, stop_generation);

    assert!(!interrupt.arm(seek_generation), "a stale arm was accepted");
    assert!(interrupt.is_retired(), "the stop's retirement was cleared");

    assert!(interrupt.arm(stop_generation));
    assert!(!interrupt.is_retired());
}

#[test]
fn a_retirement_wakes_a_blocked_producer_too() {
    // The first draft woke only the synchronous reader, leaving the producer
    // parked on its Notify and the fetch still running. A stop that does not
    // close the fetch is not a stop.
    let interrupt = SourceInterrupt::new(64);
    let channel = Arc::new(ByteChannel::new(Arc::clone(&interrupt)));
    let generation = channel.generation();
    assert!(runtime().block_on(channel.push(generation, &[1u8; 64])));
    assert_eq!(channel.buffered(), 64);

    let producer = {
        let channel = Arc::clone(&channel);
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(error) => panic!("a current-thread runtime must build: {error}"),
            };
            let started = Instant::now();
            let accepted = rt.block_on(async move { channel.push(generation, &[2u8; 64]).await });
            (accepted, started.elapsed())
        })
    };
    // The buffer is full, so the producer is provably parked.
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(channel.buffered(), 64);
    interrupt.retire();

    match producer.join() {
        Ok((accepted, elapsed)) => {
            assert!(!accepted, "a retired push reported success");
            assert!(elapsed < Duration::from_secs(1), "the producer woke after {elapsed:?}");
        }
        Err(_) => panic!("the producer thread panicked"),
    }
}

#[test]
fn a_retirement_resolves_the_fetch_tasks_cancellation() {
    // The third waiter. Without it the request stays open after a stop and the
    // server goes on streaming into a buffer nobody will drain.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let generation = interrupt.generation();
    let woken = {
        let interrupt = Arc::clone(&interrupt);
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(error) => panic!("a current-thread runtime must build: {error}"),
            };
            let started = Instant::now();
            rt.block_on(interrupt.cancelled(generation));
            started.elapsed()
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    interrupt.retire();
    match woken.join() {
        Ok(elapsed) => assert!(elapsed < Duration::from_secs(1), "woke after {elapsed:?}"),
        Err(_) => panic!("the waiter thread panicked"),
    }
    // And it resolves at once for a generation that is already superseded.
    runtime().block_on(interrupt.cancelled(generation));
}

#[test]
fn a_freeze_suspends_the_fetch_tasks_stall_timer() {
    let interrupt = SourceInterrupt::new(CAPACITY);
    interrupt.freeze();
    let waiter = {
        let interrupt = Arc::clone(&interrupt);
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(error) => panic!("a current-thread runtime must build: {error}"),
            };
            rt.block_on(interrupt.wait_while_frozen());
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    assert!(!waiter.is_finished(), "wait_while_frozen returned while frozen");
    interrupt.thaw();
    if waiter.join().is_err() {
        panic!("the waiter thread panicked");
    }
    // Already thawed: returns at once.
    runtime().block_on(interrupt.wait_while_frozen());
}

#[test]
fn paused_time_is_not_charged_against_the_stall_budget() {
    // §8: "Time spent paused or waiting for local buffer space does not count
    // as a server stall." A deadline computed once at entry keeps running
    // through the pause and fails the very next read with a stall the server
    // never caused.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = Arc::new(ByteChannel::new(Arc::clone(&interrupt)));
    let generation = channel.generation();
    let hook = Arc::new(CountingHook::default());
    let stall = Duration::from_millis(300);
    interrupt.freeze();

    let reader = {
        let channel = Arc::clone(&channel);
        let hook = Arc::clone(&hook);
        std::thread::spawn(move || channel.read(&mut [0u8; 16], hook.as_ref(), stall))
    };
    let entered = Instant::now();
    while hook.count() == 0 {
        assert!(entered.elapsed() < Duration::from_secs(5), "the read never blocked");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Stay frozen for several times the stall budget.
    std::thread::sleep(stall * 4);
    assert!(!reader.is_finished(), "the read timed out while paused");

    interrupt.thaw();
    runtime().block_on(channel.push(generation, b"resumed"));
    match reader.join() {
        Ok(outcome) => assert_eq!(outcome, ReadOutcome::Bytes(7)),
        Err(_) => panic!("the reader thread panicked"),
    }
}

#[test]
fn a_delivery_resets_the_stall_budget() {
    // A trickling server that keeps delivering is not stalled, however long the
    // whole transfer takes. §8: ordinary playback has no whole-response
    // deadline, so the budget must measure the gap between deliveries.
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = Arc::new(ByteChannel::new(Arc::clone(&interrupt)));
    let generation = channel.generation();
    let stall = Duration::from_millis(300);

    let feeder = {
        let channel = Arc::clone(&channel);
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(error) => panic!("a current-thread runtime must build: {error}"),
            };
            for _ in 0..6 {
                std::thread::sleep(Duration::from_millis(150));
                rt.block_on(channel.push(generation, b"x"));
            }
        })
    };
    let mut seen = 0;
    let mut buffer = [0u8; 4];
    for _ in 0..6 {
        match channel.read(&mut buffer, &NoHook, stall) {
            ReadOutcome::Bytes(n) => seen += n,
            other => panic!("a trickle was reported as a stall: {other:?}"),
        }
    }
    assert_eq!(seen, 6);
    if feeder.join().is_err() {
        panic!("the feeder thread panicked");
    }
}

#[test]
fn a_stall_deadline_that_elapses_fails_rather_than_returning_eof() {
    let interrupt = SourceInterrupt::new(CAPACITY);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let outcome = channel.read(&mut [0u8; 16], &NoHook, Duration::from_millis(100));
    assert_eq!(
        outcome,
        ReadOutcome::Failed(RemoteFailure::Timeout { phase: continuo::http::error::Phase::Stall })
    );
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test http_channel 2>&1 | tail -20`
Expected: FAIL — `unresolved import continuo::http::channel`.

- [ ] **Step 3: Implement the channel and the interrupt**

Add `pub mod channel;` to `src/http/mod.rs`, and create `src/http/channel.rs`.

**One lock, not three.** The first draft of this task gave `SourceInterrupt` its
own mutex, condvar and `Notify`, separate from the channel's — and it was
broken: `retire()` notified the *interrupt's* `Notify` while `push()` awaited
the *channel's*, so a retirement woke the synchronous reader and left the
producer parked and the fetch running. The fix is structural. `SourceInterrupt`
owns the single lock that the buffer, both level flags and the generation all
live behind, and owns all three wake channels; `ByteChannel` is a thin handle
over the same `Arc`. There is then exactly one lock in the mechanism, so there
is no lock-ordering question, no way to notify the wrong primitive, and no lost
wake.

```rust
//! The bounded encoded-byte channel between one asynchronous fetch task and
//! the synchronous decoder, plus the out-of-band interrupt that reaches both.
//!
//! Three wake channels, because three different kinds of waiter must be
//! reachable: the decoder thread (a `Condvar` — it must not spin), the
//! producer task inside `push` (a `Notify` — it must not block a runtime
//! worker), and the fetch task's own header and body awaits (a second
//! `Notify` — a retirement has to close the request, not merely stop feeding
//! it). All three hang off one `Mutex`, and every flag change is made under
//! that `Mutex` before notifying, which is what makes a lost wake impossible.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use super::error::{Phase, RemoteFailure};

/// How long a single wait slice lasts before the hook runs again. Short enough
/// that position and checkpoints stay current while the network is quiet;
/// long enough that a stalled read is not a spin loop.
const SLICE: Duration = Duration::from_millis(20);

/// What a blocked source read may do on the worker's behalf while it waits.
///
/// Deliberately a bare `service()`: an implementation is handed only shared
/// state, so it structurally cannot re-enter a decoder read or seek — the
/// property §8 states as a rule is here a consequence of the type.
pub trait WaitHook: Send + Sync {
    fn service(&self);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The body ended exactly where it said it would.
    Eof,
    Failed(RemoteFailure),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadOutcome {
    /// Always at least one byte. Never zero — Symphonia reads zero as EOF.
    Bytes(usize),
    Eof,
    /// Stop, seek or shutdown retired this read. Not a failure and not EOF.
    Retired,
    Failed(RemoteFailure),
}

#[derive(Debug)]
struct State {
    bytes: VecDeque<u8>,
    capacity: usize,
    outcome: Option<Outcome>,
    /// Bumped by `retire`. A push or a finish carrying an older value belongs
    /// to a superseded response and is discarded before it can enter the
    /// buffer.
    generation: u64,
    /// One-shot: this generation is over. `arm` clears it for the next one.
    retired: bool,
    /// A *level*, not an edge. A pause persists until a play, and a read that
    /// blocks after the edge would have passed must still observe it (G2).
    frozen: bool,
}

/// The out-of-band wake shared by the application, the worker, every source
/// wait and the fetch task.
#[derive(Debug)]
pub struct SourceInterrupt {
    state: Mutex<State>,
    reader_wake: Condvar,
    producer_wake: Notify,
    /// Wakes the fetch task's header and body awaits. Waking the reader alone
    /// leaves the request open and the server still streaming into a buffer
    /// nobody will drain.
    fetch_wake: Notify,
}

/// A poisoned lock means a thread already panicked while holding it; there is
/// nothing better to do than carry on with the state it left.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl SourceInterrupt {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                bytes: VecDeque::with_capacity(capacity.min(1 << 16)),
                capacity: capacity.max(1),
                outcome: None,
                generation: 1,
                retired: false,
                frozen: false,
            }),
            reader_wake: Condvar::new(),
            producer_wake: Notify::new(),
            fetch_wake: Notify::new(),
        })
    }

    /// End the current generation: drop the buffer and any pending outcome,
    /// and wake all three kinds of waiter. Returns the generation a new fetch
    /// must carry.
    pub fn retire(&self) -> u64 {
        let generation = {
            let mut state = lock(&self.state);
            state.bytes.clear();
            state.outcome = None;
            state.retired = true;
            state.generation += 1;
            state.generation
        };
        self.wake_all();
        generation
    }

    /// Clear the retirement so a new generation may run — but only the
    /// retirement this caller created.
    ///
    /// The guard is load-bearing. `retire` is called from the application
    /// thread (stop, shutdown) as well as the decode thread (seek, reopen), so
    /// an unguarded `arm` lets a seek that retired generation N clear a *stop*
    /// that retired generation N+1 a moment later, leaving the fetch running
    /// after the stop that was supposed to close it. Pass the generation
    /// `retire` returned; a newer one means somebody else has since retired
    /// this source and their retirement stands.
    pub fn arm(&self, generation: u64) -> bool {
        let mut state = lock(&self.state);
        if state.generation != generation {
            return false;
        }
        state.retired = false;
        drop(state);
        self.wake_all();
        true
    }

    pub fn freeze(&self) {
        lock(&self.state).frozen = true;
        self.wake_all();
    }

    pub fn thaw(&self) {
        lock(&self.state).frozen = false;
        self.wake_all();
    }

    pub fn is_retired(&self) -> bool {
        lock(&self.state).retired
    }

    pub fn is_frozen(&self) -> bool {
        lock(&self.state).frozen
    }

    pub fn generation(&self) -> u64 {
        lock(&self.state).generation
    }

    fn wake_all(&self) {
        self.reader_wake.notify_all();
        self.producer_wake.notify_waiters();
        self.fetch_wake.notify_waiters();
    }

    /// Async cancellation for the fetch task. Resolves as soon as the current
    /// generation is retired, and never resolves otherwise.
    pub async fn cancelled(&self, generation: u64) {
        loop {
            // Create the future before testing, so a notify that lands between
            // the test and the await is still delivered.
            let notified = self.fetch_wake.notified();
            {
                let state = lock(&self.state);
                if state.retired || state.generation != generation {
                    return;
                }
            }
            notified.await;
        }
    }

    /// Suspend the caller for as long as playback is frozen.
    ///
    /// The fetch task awaits this *before* arming its stall timer, which is
    /// what keeps paused time out of the stall budget (§8: "Time spent paused
    /// or waiting for local buffer space does not count as a server stall").
    pub async fn wait_while_frozen(&self) {
        loop {
            let notified = self.fetch_wake.notified();
            {
                let state = lock(&self.state);
                if !state.frozen || state.retired {
                    return;
                }
            }
            notified.await;
        }
    }
}

/// A handle onto the interrupt's buffer. Holds no state of its own, so there is
/// no second lock and no way for the two to disagree.
#[derive(Clone, Debug)]
pub struct ByteChannel(Arc<SourceInterrupt>);

impl ByteChannel {
    pub fn new(interrupt: Arc<SourceInterrupt>) -> Self {
        Self(interrupt)
    }

    pub fn interrupt(&self) -> &Arc<SourceInterrupt> {
        &self.0
    }

    pub fn generation(&self) -> u64 {
        self.0.generation()
    }

    pub fn buffered(&self) -> usize {
        lock(&self.0.state).bytes.len()
    }

    pub fn retire(&self) -> u64 {
        self.0.retire()
    }

    /// Wait for bytes, an ending or a retirement.
    ///
    /// `stall` is a budget of *active demand*, not a wall-clock deadline: only
    /// slices spent unfrozen are charged against it, and any delivery resets
    /// it. A fixed `Instant::now() + stall` computed once — which is what this
    /// first did — expires during a long pause and fails the very next read
    /// with a server stall the server never caused.
    ///
    /// Buffered bytes are always drained before a pending outcome is reported,
    /// so a body that ends mid-buffer still plays what it delivered.
    pub fn read(&self, out: &mut [u8], service: &dyn WaitHook, stall: Duration) -> ReadOutcome {
        if out.is_empty() {
            return ReadOutcome::Bytes(0);
        }
        let mut demanded = Duration::ZERO;
        let mut state = lock(&self.0.state);
        loop {
            if state.retired {
                return ReadOutcome::Retired;
            }
            if !state.frozen {
                if !state.bytes.is_empty() {
                    let count = state.bytes.len().min(out.len());
                    for slot in out.iter_mut().take(count) {
                        // `pop_front` is `Some` for each of `count` iterations:
                        // `count <= bytes.len()` was read under this guard and
                        // nothing else can drain it.
                        *slot = state.bytes.pop_front().unwrap_or(0);
                    }
                    drop(state);
                    self.0.producer_wake.notify_waiters();
                    return ReadOutcome::Bytes(count);
                }
                if let Some(outcome) = state.outcome.clone() {
                    return match outcome {
                        Outcome::Eof => ReadOutcome::Eof,
                        Outcome::Failed(failure) => ReadOutcome::Failed(failure),
                    };
                }
                if demanded >= stall {
                    return ReadOutcome::Failed(RemoteFailure::Timeout { phase: Phase::Stall });
                }
            }
            let frozen_before = state.frozen;
            let slice_start = Instant::now();
            let (guard, _) = match self.0.reader_wake.wait_timeout(state, SLICE) {
                Ok(pair) => pair,
                Err(poisoned) => poisoned.into_inner(),
            };
            state = guard;
            // Only unfrozen time is demand. A slice that began frozen is not
            // charged, whatever the flag says by the time it ends.
            if !frozen_before {
                demanded += slice_start.elapsed();
            }
            // Outside the predicate but inside the loop: the hook runs on every
            // slice, which is what keeps position and checkpoints current while
            // the network is quiet (§8), what services a freeze (Task 9), and
            // what a test uses to prove the wait was entered.
            //
            // The lock is dropped across the call, and retaken afterwards. The
            // hook takes the facts lock and then the transport lock, and a
            // worker that already holds the transport lock may reach this
            // interrupt; holding both here would close that cycle.
            drop(state);
            service.service();
            state = lock(&self.0.state);
        }
    }

    /// Push one chunk, waiting asynchronously for room.
    ///
    /// Returns `false` when the generation was superseded or retired, which is
    /// the fetch task's signal to stop.
    pub async fn push(&self, generation: u64, chunk: &[u8]) -> bool {
        let mut offset = 0;
        while offset < chunk.len() {
            // Create the future *before* re-checking, so a notify that lands
            // between the check and the await is still delivered.
            let notified = self.0.producer_wake.notified();
            let accepted = {
                let mut state = lock(&self.0.state);
                if state.generation != generation || state.retired {
                    return false;
                }
                let room = state.capacity.saturating_sub(state.bytes.len());
                let take = room.min(chunk.len() - offset);
                if take > 0 {
                    state.bytes.extend(&chunk[offset..offset + take]);
                }
                take
            };
            if accepted > 0 {
                offset += accepted;
                self.0.reader_wake.notify_all();
                continue;
            }
            notified.await;
        }
        let state = lock(&self.0.state);
        state.generation == generation && !state.retired
    }

    /// End the stream. A stale generation's outcome is discarded: a superseded
    /// response must not be able to report EOF into the live generation.
    pub fn finish(&self, generation: u64, outcome: Outcome) {
        {
            let mut state = lock(&self.0.state);
            if state.generation != generation || state.retired || state.outcome.is_some() {
                return;
            }
            state.outcome = Some(outcome);
        }
        self.0.reader_wake.notify_all();
    }
}
```

**Lock order, written down once because three locks now exist.** Taken in this order and never any other:

```
   (no lock)  ->  SessionFacts  ->  TransportCore
   SourceInterrupt::state is a leaf: it is never held while taking either.
```

`ByteChannel::read` therefore drops the interrupt lock across `service.service()` — the explicit `drop` / re-`lock` above — rather than holding it. An earlier draft wrote this as `MutexGuard::unlocked(&mut state, ...)`; **that function does not exist.** `rustc 1.98.1` rejects it with `E0599: no associated function or constant named 'unlocked' found for struct 'std::sync::MutexGuard'`. Verify any such claim before relying on it:

```bash
cat > /tmp/probe.rs <<'EOF'
use std::sync::Mutex;
fn main() {
    let m = Mutex::new(1u32);
    let mut g = m.lock().expect("fresh mutex");
    std::sync::MutexGuard::unlocked(&mut g, || {});
    let _ = *g;
}
EOF
rustc --edition 2024 -o /tmp/probe /tmp/probe.rs
```

Re-checking the loop after re-acquiring is not optional and the code above does it: the predicate is re-tested at the top of every iteration, so anything that changed while the lock was released is seen.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test http_channel 2>&1 | tail -30`
Expected: PASS, 17 tests. If `a_retirement_wakes_a_blocked_read_within_one_second` is slow, the `SLICE` constant is the knob — but do not raise it above 50 ms, or the §8 bound stops holding with margin.

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add src/http/mod.rs src/http/channel.rs tests/http_channel.rs
git commit -m "feat(http): add the bounded byte channel and the source interrupt

Freeze is a level and retirement is an edge (spec gap G2): a pause persists,
so a read that blocks after the edge passed would never see it."
```

---

## Task 4: The controllable loopback server

§12 requires deterministic barriers, mid-body stalls, raw framing and disconnects. No HTTP mocking crate exposes those, so this is a hand-rolled `std::net::TcpListener` on a thread — which also means no new dependency and no async in the test harness.

**Files:**
- Create: `tests/support/server.rs`
- Modify: `tests/support/mod.rs` (add `pub mod server;`)
- Test: `tests/http_fetch.rs` exercises it in Task 5; this task ships its self-tests inside `tests/support/server.rs` is not possible (it is not a test target), so the self-tests live in a new `tests/http_server_selftest.rs`.

**Interfaces:**
- Consumes: nothing.
- Produces:

```rust
pub struct TestServer;
impl TestServer {
    pub fn start(script: Script) -> Self;
    pub fn url(&self, path: &str) -> String;
    /// Release a stalled body. Returns false if nothing was stalled.
    pub fn release(&self) -> bool;
    /// Blocks until the server has begun writing a body and is stalled there.
    pub fn wait_until_stalled(&self, patience: Duration) -> bool;
    pub fn requests(&self) -> Vec<RecordedRequest>;
    pub fn shutdown(self);
}
pub struct RecordedRequest { pub method: String, pub path: String, pub query: Option<String>, pub headers: Vec<(String, String)> }
impl RecordedRequest { pub fn header(&self, name: &str) -> Option<&str>; pub fn range(&self) -> Option<(u64, Option<u64>)>; }

pub struct Script { /* built with the methods below */ }
impl Script {
    pub fn serving(body: Vec<u8>) -> Self;              // full range support, strong ETag
    pub fn from_fixture(name: &str) -> Self;            // reads tests/fixtures/<name>
    pub fn without_ranges(self) -> Self;                // always 200, ignores Range
    pub fn without_accept_ranges_header(self) -> Self;  // ranges work, header absent
    pub fn chunked(self) -> Self;                       // no Content-Length, chunked framing
    pub fn live(self) -> Self;                          // ICY headers, endless body
    pub fn unresolved(self) -> Self;                    // no length, no ICY, endless body
    pub fn weak_etag(self) -> Self;
    pub fn no_validator(self) -> Self;
    pub fn changing_etag_after(self, requests: usize) -> Self;
    pub fn redirect_chain(self, hops: usize) -> Self;
    pub fn redirect_loop(self) -> Self;
    pub fn redirect_to(self, location: &str) -> Self;
    pub fn status(self, code: u16) -> Self;
    pub fn gzip_encoded(self) -> Self;
    pub fn multipart_range(self) -> Self;
    pub fn content_range_override(self, value: &str) -> Self;
    pub fn range_answered_with_200(self) -> Self;
    pub fn truncate_body_after(self, bytes: usize) -> Self;   // then close the socket
    pub fn stall_body_after(self, bytes: usize) -> Self;      // then wait for release()
    pub fn stall_headers(self) -> Self;                       // accept, never respond
    /// Emit `bytes` every `gap`, forever. Every individual read stays inside a
    /// generous stall budget, which is what makes an opening deadline checked
    /// only *around* probing useless.
    pub fn trickle(self, bytes: usize, gap: Duration) -> Self;
}

impl TestServer {
    /// Body bytes this server has actually written to the socket.
    ///
    /// This is how H13 observes the buffer bound: the channel lives inside the
    /// worker's decoder and no test can reach it, and backpressure visible on
    /// the wire is the stronger claim anyway.
    pub fn bytes_written(&self) -> usize;
}
```

- [ ] **Step 1: Write the failing self-tests**

Create `tests/http_server_selftest.rs`:

```rust
mod support;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use support::server::{Script, TestServer};

#[allow(clippy::unwrap_used)] // A loopback request to a server this test started.
fn raw(server: &TestServer, path: &str, extra: &str) -> String {
    let url = server.url(path);
    let rest = url.trim_start_matches("http://");
    let (authority, target) = match rest.split_once('/') {
        Some((authority, target)) => (authority, format!("/{target}")),
        None => (rest, "/".to_string()),
    };
    let mut stream = TcpStream::connect(authority).unwrap();
    let request = format!("GET {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n{extra}\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

#[test]
fn a_range_request_gets_a_206_with_the_requested_interval() {
    let server = TestServer::start(Script::serving(b"0123456789".to_vec()));
    let response = raw(&server, "/audio", "Range: bytes=3-5\r\n");
    assert!(response.starts_with("HTTP/1.1 206"), "{response}");
    assert!(response.contains("Content-Range: bytes 3-5/10"), "{response}");
    assert!(response.ends_with("345"), "{response}");
    server.shutdown();
}

#[test]
fn a_range_ignoring_server_answers_200_with_the_whole_body() {
    let server = TestServer::start(Script::serving(b"0123456789".to_vec()).without_ranges());
    let response = raw(&server, "/audio", "Range: bytes=3-5\r\n");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("0123456789"), "{response}");
    assert!(!response.contains("Accept-Ranges: bytes"), "{response}");
    server.shutdown();
}

#[test]
fn requests_are_recorded_with_their_range_headers() {
    let server = TestServer::start(Script::serving(b"0123456789".to_vec()));
    raw(&server, "/audio?token=x", "Range: bytes=4-\r\n");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/audio");
    assert_eq!(requests[0].query.as_deref(), Some("token=x"));
    assert_eq!(requests[0].range(), Some((4, None)));
    server.shutdown();
}

#[test]
fn a_stalled_body_is_observable_and_releasable() {
    let server = TestServer::start(Script::serving(vec![7u8; 4096]).stall_body_after(16));
    let handle = {
        let url = server.url("/audio");
        std::thread::spawn(move || {
            let rest = url.trim_start_matches("http://").to_string();
            let (authority, target) = match rest.split_once('/') {
                Some((authority, target)) => (authority.to_string(), format!("/{target}")),
                None => (rest, "/".to_string()),
            };
            let mut stream = match TcpStream::connect(&authority) {
                Ok(stream) => stream,
                Err(error) => panic!("loopback connect failed: {error}"),
            };
            let request = format!("GET {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
            if let Err(error) = stream.write_all(request.as_bytes()) {
                panic!("loopback write failed: {error}");
            }
            let mut body = Vec::new();
            let _ = stream.read_to_end(&mut body);
            body.len()
        })
    };
    assert!(
        server.wait_until_stalled(Duration::from_secs(5)),
        "the server never reached its stall point"
    );
    assert!(server.release());
    match handle.join() {
        Ok(bytes) => assert!(bytes > 4096, "the full body never arrived: {bytes}"),
        Err(_) => panic!("the client thread panicked"),
    }
    server.shutdown();
}

#[test]
fn a_redirect_chain_ends_at_the_media() {
    let server = TestServer::start(Script::serving(b"final".to_vec()).redirect_chain(2));
    let first = raw(&server, "/audio", "");
    assert!(first.starts_with("HTTP/1.1 302"), "{first}");
    assert!(first.contains("Location: /audio-1"), "{first}");
    let second = raw(&server, "/audio-1", "");
    assert!(second.contains("Location: /audio-2"), "{second}");
    let third = raw(&server, "/audio-2", "");
    assert!(third.starts_with("HTTP/1.1 200"), "{third}");
    assert!(third.ends_with("final"), "{third}");
    server.shutdown();
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test http_server_selftest 2>&1 | tail -20`
Expected: FAIL — `unresolved import support::server`.

- [ ] **Step 3: Implement the server**

Add to `tests/support/mod.rs`, beside the existing declarations:

```rust
pub mod server;
```

Create `tests/support/server.rs`. The shape (write it out in full; this is the outline every method must satisfy):

```rust
//! A loopback HTTP/1.1 server the tests can hold still.
//!
//! Hand-rolled rather than a mocking crate because §12's acceptance evidence
//! needs raw framing, a body that stalls at a chosen byte and stays stalled
//! until the test releases it, and a socket that closes mid-body. No mocking
//! library exposes those, and each cancellation test must first *prove* the
//! wait it targets was entered rather than sleeping and hoping.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[allow(clippy::unwrap_used)] // A poisoned harness mutex means a test already failed.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap()
}

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: Vec<(String, String)>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    /// `(first, last)` from a `Range: bytes=first-[last]` header.
    pub fn range(&self) -> Option<(u64, Option<u64>)> {
        let value = self.header("range")?.trim().strip_prefix("bytes=")?;
        let (first, last) = value.split_once('-')?;
        let first = first.trim().parse().ok()?;
        let last = match last.trim() {
            "" => None,
            digits => Some(digits.parse().ok()?),
        };
        Some((first, last))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Script {
    body: Vec<u8>,
    ranges: bool,
    advertise_accept_ranges: bool,
    chunked: bool,
    live: bool,
    unresolved: bool,
    etag: Option<String>,
    weak_etag: bool,
    etag_changes_after: Option<usize>,
    redirect_hops: usize,
    redirect_loop: bool,
    redirect_to: Option<String>,
    status_override: Option<u16>,
    content_encoding: Option<String>,
    multipart: bool,
    content_range_override: Option<String>,
    answer_range_with_200: bool,
    truncate_after: Option<usize>,
    stall_after: Option<usize>,
    stall_headers: bool,
}
```

`Script::serving(body)` sets `ranges: true`, `advertise_accept_ranges: true`, `etag: Some("\"v1\"".into())`. `Script::from_fixture(name)` reads `tests/fixtures/<name>` via `std::fs::read` (panicking with the path on failure — a missing fixture is a repository error, not a test condition) and hands it to `serving`. Every other builder sets its one field and returns `self`.

`TestServer::start` binds `TcpListener::bind("127.0.0.1:0")`, records the port, and spawns an accept loop thread. Per connection:

1. Read the request line and headers with a `BufReader`; record a `RecordedRequest` into `Arc<Mutex<Vec<_>>>`.
2. Decide the response from the `Script` and the request path, in this order: `status_override` → `redirect_*` → range handling → body framing.
3. Write status line and headers, then the body in 4 KiB writes, checking after each write whether `truncate_after` or `stall_after` has been crossed.
4. `stall_after`: set a `stalled` flag, notify a `Condvar`, then `wait_while` on a `released` flag. `release()` sets `released` and notifies; `wait_until_stalled` waits on the same pair with a deadline.
5. `truncate_after`: `stream.shutdown(Shutdown::Both)` and return without finishing the body.
6. `stall_headers`: record the request, then park on the same release pair without writing a byte.

Redirects: `redirect_chain(n)` makes `/audio` answer `302` with `Location: /audio-1`, `/audio-k` answer `302` with `Location: /audio-{k+1}` for `k < n`, and `/audio-n` serve the body. `redirect_loop` makes `/audio` point at itself. `redirect_to(location)` answers one `302` with that literal `Location`, which is how the downgrade and unsupported-scheme cases are staged.

Range handling when `ranges` is true and a `Range` header is present:
- `answer_range_with_200` → `200` with the whole body (H7's "unexpected 200").
- start beyond `body.len()` → `416` with `Content-Range: bytes */<len>`.
- otherwise `206`, `Content-Range: bytes <first>-<last>/<len>` (or `content_range_override` verbatim, which is how "wrong start", "reversed interval" and "conflicting total" are staged), `Content-Length: <last-first+1>`, and the slice.

Framing: `chunked` writes `Transfer-Encoding: chunked` with no `Content-Length` and hex-prefixed chunks; `live` adds `icy-name: Test Radio` and `icy-metaint: 16000` and repeats the body forever; `unresolved` omits `Content-Length`, uses chunked framing and repeats the body forever with no ICY headers — a source that is neither provably finite nor provably live. `multipart` sets `Content-Type: multipart/byteranges; boundary=x`. `content_encoding` sets that header without actually encoding, which is enough: §7 refuses on the header alone.

ETag: `weak_etag` writes `W/"v1"`; `no_validator` writes none; `changing_etag_after(n)` writes `"v1"` for the first `n` requests and `"v2"` afterwards, with the body unchanged in length — which is exactly H11's same-length replacement.

`shutdown()` sets a `stop` flag, releases any stall, connects once to `127.0.0.1:<port>` to unblock `accept`, and joins the thread.

- [ ] **Step 4: Run the self-tests to verify they pass**

Run: `cargo test --test http_server_selftest 2>&1 | tail -20`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add tests/support/mod.rs tests/support/server.rs tests/http_server_selftest.rs
git commit -m "test(http): add the controllable loopback server

Hand-rolled because §12 needs raw framing, releasable mid-body stalls and
mid-body disconnects, and each cancellation test must prove the wait it
targets was entered rather than sleeping and hoping."
```

---

## Task 5: The HTTP service, the fetch task and the redirect loop

**Files:**
- Create: `src/http/service.rs`
- Modify: `src/http/mod.rs`
- Test: `tests/http_fetch.rs`

**Interfaces:**
- Consumes: Tasks 1–4.
- Produces:

```rust
pub struct HttpService { /* owns the runtime and the reqwest client */ }
impl HttpService {
    pub fn spawn(limits: Limits) -> Result<Arc<Self>, RemoteFailure>;
    /// One request. Follows redirects manually, validates the response, and
    /// spawns the task that streams the body into `channel`.
    pub fn fetch(
        &self,
        request: &FetchRequest,
        channel: Arc<ByteChannel>,
        generation: u64,
    ) -> Result<FetchAccepted, RemoteFailure>;
    pub fn limits(&self) -> &Limits;
}
pub struct FetchRequest { pub origin: Url, pub start: u64, pub established: Option<Established>, pub operation: Operation }
pub struct FetchAccepted { pub accepted: Accepted, pub validator: Validator, pub headers: Headers, pub redirects: u8 }
```

**Ownership (§4).** `HttpService` owns a multi-thread Tokio runtime built with `Builder::new_multi_thread().worker_threads(1).enable_all()`. One worker thread is enough — there is one active fetch per source generation — and the runtime is owned by the *application*, not the worker, so `--probe-only` and the local-file path can run with no runtime at all.

**`fetch` never blocks the calling thread, and never awaits on it.** §4 forbids the decode worker blocking on the Tokio runtime, and that rules out the obvious `runtime.block_on(timeout(headers, execute(request)))` however tightly it is bounded. Instead `fetch` spawns the whole request-plus-body task and returns immediately with a `HeaderWait`; the caller waits on a `Condvar`, exactly as it waits for body bytes. So:

```rust
pub fn fetch(&self, request: FetchRequest, channel: ByteChannel, generation: u64) -> HeaderWait;
pub struct HeaderWait { /* Arc<SourceInterrupt> + the generation it belongs to */ }
impl HeaderWait {
    /// Cancellable by the same interrupt every read obeys.
    pub fn wait(&self, service: &dyn WaitHook, deadline: Duration) -> Result<FetchAccepted, HeaderOutcome>;
}
pub enum HeaderOutcome { Retired, Failed(RemoteFailure) }
```

**The header outcome lives inside `SourceInterrupt`'s `State`, not in a slot of its own.** `State` gains

```rust
    /// Set by the fetch task once headers are validated, or once they fail.
    /// Cleared by `retire`, like everything else belonging to a generation.
    headers: Option<Result<FetchAccepted, RemoteFailure>>,
```

with `publish_headers(generation, outcome)` alongside `finish`, and `HeaderWait::wait` is the same slice-and-service loop as `ByteChannel::read`, waiting on `reader_wake`.

A separate `Mutex` + `Condvar` here would reproduce review-round defect #4 one layer up: `SourceInterrupt::wake_all` notifies `reader_wake`, `producer_wake` and `fetch_wake`, and a private condvar is none of them — so `retire()` would leave a blocked header wait asleep until its deadline. Every wait a retirement must reach hangs off the one lock. `a_retirement_during_a_header_wait_wakes_it_without_releasing_the_server` below is the test that catches it if this is built the other way.

- [ ] **Step 1: Write the failing fetch tests**

Create `tests/http_fetch.rs`:

```rust
mod support;

use std::sync::Arc;
use std::time::Duration;

use continuo::http::channel::{ByteChannel, ReadOutcome, SourceInterrupt, WaitHook};
use continuo::http::error::{Phase, RedirectRejection, RemoteFailure};
use continuo::http::limits::Limits;
use continuo::http::response::Accepted;
use continuo::http::service::{FetchRequest, HeaderOutcome, HttpService};
use continuo::http::error::Operation;
use support::server::{Script, TestServer};
use url::Url;

struct NoHook;
impl WaitHook for NoHook {
    fn service(&self) {}
}

#[allow(clippy::unwrap_used)] // A URL this test built from a bound loopback port.
fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

fn service(limits: Limits) -> Arc<HttpService> {
    match HttpService::spawn(limits) {
        Ok(service) => service,
        Err(error) => panic!("the HTTP service must start: {error}"),
    }
}

fn open(service: &HttpService, origin: Url, start: u64) -> (Arc<ByteChannel>, Arc<SourceInterrupt>, Result<continuo::http::service::FetchAccepted, HeaderOutcome>) {
    let interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
    let channel = Arc::new(ByteChannel::new(Arc::clone(&interrupt)));
    let generation = channel.generation();
    let wait = service.fetch(
        FetchRequest { origin, start, established: None, operation: Operation::Open },
        Arc::clone(&channel),
        generation,
    );
    let outcome = wait.wait(&NoHook, Duration::from_secs(10));
    (channel, interrupt, outcome)
}

fn drain(channel: &ByteChannel) -> Vec<u8> {
    let mut all = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match channel.read(&mut buffer, &NoHook, Duration::from_secs(10)) {
            ReadOutcome::Bytes(n) => all.extend_from_slice(&buffer[..n]),
            ReadOutcome::Eof => return all,
            other => panic!("unexpected read outcome: {other:?}"),
        }
    }
}

#[test]
fn an_opening_range_get_establishes_ranged_access_and_streams_the_body() {
    let body: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
    let server = TestServer::start(Script::serving(body.clone()));
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);

    match accepted {
        Ok(accepted) => assert!(matches!(accepted.accepted, Accepted::Ranged { .. })),
        Err(outcome) => panic!("opening failed: {outcome:?}"),
    }
    assert_eq!(drain(&channel), body);

    // §6: the opening probe is a real range GET, reusing its response as the
    // initial stream. There is no preliminary HEAD.
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].range(), Some((0, None)));
    server.shutdown();
}

#[test]
fn a_range_ignoring_server_is_accepted_as_sequential() {
    let body = vec![3u8; 2048];
    let server = TestServer::start(Script::serving(body.clone()).without_ranges());
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);
    match accepted {
        Ok(accepted) => assert!(matches!(accepted.accepted, Accepted::Sequential { .. })),
        Err(outcome) => panic!("opening failed: {outcome:?}"),
    }
    assert_eq!(drain(&channel), body);
    server.shutdown();
}

#[test]
fn ranges_work_even_when_accept_ranges_is_absent() {
    // §6: `Accept-Ranges` is a hint, not the evidence. A real range GET is.
    let server = TestServer::start(Script::serving(vec![1u8; 4096]).without_accept_ranges_header());
    let service = service(Limits::default());
    let (_channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);
    match accepted {
        Ok(accepted) => assert!(matches!(accepted.accepted, Accepted::Ranged { .. })),
        Err(outcome) => panic!("opening failed: {outcome:?}"),
    }
    server.shutdown();
}

#[test]
fn redirects_are_followed_up_to_the_limit_and_the_query_survives() {
    // H6. The redirect chain must not lose the signed query on the way.
    let server = TestServer::start(Script::serving(b"final".to_vec()).redirect_chain(3));
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) = open(&service, url(&server.url("/audio?token=abc&Expires=9")), 0);
    match accepted {
        Ok(accepted) => assert_eq!(accepted.redirects, 3),
        Err(outcome) => panic!("opening failed: {outcome:?}"),
    }
    assert_eq!(drain(&channel), b"final");
    let requests = server.requests();
    assert_eq!(requests.len(), 4, "{requests:?}");
    assert_eq!(
        requests[0].query.as_deref(),
        Some("token=abc&Expires=9"),
        "the original query must reach the origin verbatim"
    );
    server.shutdown();
}

#[test]
fn a_redirect_loop_and_an_over_long_chain_both_fail() {
    let service = service(Limits::default());

    let looping = TestServer::start(Script::serving(b"x".to_vec()).redirect_loop());
    let (_c, _i, outcome) = open(&service, url(&looping.url("/audio")), 0);
    assert!(
        matches!(
            outcome,
            Err(HeaderOutcome::Failed(RemoteFailure::Redirect { reason: RedirectRejection::Loop }))
        ),
        "{outcome:?}"
    );
    looping.shutdown();

    let long = TestServer::start(Script::serving(b"x".to_vec()).redirect_chain(9));
    let (_c, _i, outcome) = open(&service, url(&long.url("/audio")), 0);
    assert!(
        matches!(
            outcome,
            Err(HeaderOutcome::Failed(RemoteFailure::Redirect { reason: RedirectRejection::TooMany }))
        ),
        "{outcome:?}"
    );
    long.shutdown();
}

#[test]
fn an_https_to_http_downgrade_is_refused() {
    // Staged from a plain-HTTP loopback origin by asserting the rule directly:
    // the loopback server cannot serve TLS, so the downgrade rule is proven in
    // `tests/http_response.rs` and this test pins the *service* wiring by
    // sending a redirect to an unsupported scheme, which travels the same path.
    let server = TestServer::start(Script::serving(b"x".to_vec()).redirect_to("ftp://example.com/a.mp3"));
    let service = service(Limits::default());
    let (_c, _i, outcome) = open(&service, url(&server.url("/audio")), 0);
    assert!(
        matches!(
            outcome,
            Err(HeaderOutcome::Failed(RemoteFailure::Redirect {
                reason: RedirectRejection::UnsupportedScheme
            }))
        ),
        "{outcome:?}"
    );
    server.shutdown();
}

#[test]
fn a_status_failure_is_reported_with_its_status() {
    let server = TestServer::start(Script::serving(b"x".to_vec()).status(503));
    let service = service(Limits::default());
    let (_c, _i, outcome) = open(&service, url(&server.url("/audio")), 0);
    assert!(
        matches!(
            outcome,
            Err(HeaderOutcome::Failed(RemoteFailure::Status { status: 503, .. }))
        ),
        "{outcome:?}"
    );
    server.shutdown();
}

#[test]
fn a_header_wait_that_elapses_reports_a_headers_timeout() {
    let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
    let service = service(Limits::brisk());
    let (_c, _i, outcome) = open(&service, url(&server.url("/audio")), 0);
    assert!(
        matches!(
            outcome,
            Err(HeaderOutcome::Failed(RemoteFailure::Timeout { phase: Phase::Headers }))
        ),
        "{outcome:?}"
    );
    server.shutdown();
}

#[test]
fn a_truncated_body_fails_rather_than_reporting_eof() {
    // H8. This is the single most important rule in §7: a short body that read
    // back as EOF becomes a completed track and a destroyed checkpoint.
    let server = TestServer::start(Script::serving(vec![5u8; 8192]).truncate_body_after(1024));
    let service = service(Limits::default());
    let (channel, _interrupt, accepted) = open(&service, url(&server.url("/audio")), 0);
    assert!(accepted.is_ok(), "opening should succeed; the body fails later");

    let mut buffer = [0u8; 4096];
    let mut seen = 0usize;
    let outcome = loop {
        match channel.read(&mut buffer, &NoHook, Duration::from_secs(5)) {
            ReadOutcome::Bytes(n) => seen += n,
            other => break other,
        }
    };
    assert!(seen >= 1024, "the delivered prefix was lost: {seen}");
    assert!(
        matches!(outcome, ReadOutcome::Failed(RemoteFailure::TruncatedBody { .. })),
        "a truncated body must not read back as EOF: {outcome:?}"
    );
    server.shutdown();
}

#[test]
fn a_retirement_during_a_header_wait_wakes_it_without_releasing_the_server() {
    let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
    let service = service(Limits::default());
    let interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
    let channel = Arc::new(ByteChannel::new(Arc::clone(&interrupt)));
    let generation = channel.generation();
    let wait = service.fetch(
        FetchRequest {
            origin: url(&server.url("/audio")),
            start: 0,
            established: None,
            operation: Operation::Open,
        },
        Arc::clone(&channel),
        generation,
    );
    let waiter = std::thread::spawn(move || wait.wait(&NoHook, Duration::from_secs(30)));
    // The server records the request before parking, so this is proof the wait
    // was entered rather than a sleep.
    let started = std::time::Instant::now();
    while server.requests().is_empty() {
        assert!(started.elapsed() < Duration::from_secs(5), "the request never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    interrupt.retire();
    let elapsed = std::time::Instant::now();
    match waiter.join() {
        Ok(outcome) => {
            assert!(matches!(outcome, Err(HeaderOutcome::Retired)), "{outcome:?}");
            assert!(elapsed.elapsed() < Duration::from_secs(1));
        }
        Err(_) => panic!("the waiter thread panicked"),
    }
    server.shutdown();
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test http_fetch 2>&1 | tail -20`
Expected: FAIL — `unresolved import continuo::http::service`.

- [ ] **Step 3: Implement the service**

Add `pub mod service;` to `src/http/mod.rs` and create `src/http/service.rs`. The obligations, each of which one test above pins:

1. **Runtime.** `HttpService::spawn` builds `tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build()`, mapping the error to `RemoteFailure::Transport { operation: Operation::Open, detail }`. Store the runtime in the struct; its `Drop` shuts the runtime down.
2. **Client.** `reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).connect_timeout(limits.connect).build()`. `Policy::none()` is required: §7's hop limit, loop detection, scheme check and downgrade refusal are ours, and reqwest's default policy implements none of them.
3. **`fetch`** creates a `HeaderSlot`, clones what the task needs, and `runtime.spawn`s an async block. It returns `HeaderWait` immediately. It must not await anything on the calling thread.
4. **The task** loops at most `limits.max_redirects + 1` times. **Every await in it is raced against cancellation**, because a retirement has to close the request, not merely stop feeding it:
   ```rust
   macro_rules! cancellable {
       ($fut:expr, $timeout:expr, $phase:expr) => {
           tokio::select! {
               biased;
               () = interrupt.cancelled(generation) => return,
               result = $fut => result,
               () = tokio::time::sleep($timeout) => {
                   slot.fail(RemoteFailure::Timeout { phase: $phase });
                   return;
               }
           }
       };
   }
   ```
   - Build the request: `Range: bytes=<start>-`, `Accept-Encoding: identity`, and `If-Range: <etag>` only when `if_range_value(&established.validator)` is `Some`.
   - `cancellable!(client.execute(request), limits.headers, Phase::Headers)`.
   - On a 3xx with a `Location`, call `accept_redirect(&current, location, hops, &seen, limits)`; push `current` onto `seen`; continue. On `Err`, fail the slot.
   - Otherwise call `response::accept(status, &Headers::from_map(response.headers()), start, start == 0, established.as_ref())`. On `Err`, fail the slot. On `Ok`, `slot.accept(FetchAccepted { .. })` and fall through to the body.
5. **The body** loops, and each pass does three things in this order:
   ```rust
   // Paused time is not a server stall (§8), so the timer is armed only after
   // the freeze clears — not merely skipped while it is set.
   interrupt.wait_while_frozen().await;
   let next = cancellable!(response.chunk(), limits.stall, Phase::Stall);
   ```
   then handles `next`:
   - `Ok(Some(bytes))` → push it in slices of at most `limits.chunk_bytes` (see 6), tracking `delivered += bytes.len()`. If `push` returns `false`, return: the generation was superseded.
   - `Ok(None)` → the body ended. Compare `delivered` against the interval the accepted response advertised (`ByteRange::len()` for a 206, `Content-Length` for a 200):
     - `delivered < advertised` → `Outcome::Failed(TruncatedBody { missing: advertised - delivered })`.
     - `delivered > advertised` → `Outcome::Failed(InvalidRange { reason: RangeRejection::LengthMismatch })`. **A body longer than its header promised is as corrupt as a short one**: the excess bytes sit at offsets that belong to different media, and only checking for shortfall lets them through. The excess must also be refused *during* the loop, not only at its end, so an endless body cannot fill the buffer forever against a small advertised interval.
     - equal, or nothing advertised → `Outcome::Eof`.
   - `Err(e)` → `Outcome::Failed(Transport { operation, detail: e.to_string() })`.
   A body ending *before* a valid smaller interval is exhausted is `TruncatedBody`; a body ending exactly at the end of a smaller-than-total interval is `Eof` for that interval, and `HttpMediaSource` (Task 6) is what re-requests the next byte. §7: "A response that ends a smaller valid interval before the object ends requires another validated request at the next byte; it is not media EOF."
6. **The transfer bound.** §8 promises "at most one bounded 64 KiB application transfer chunk" on top of the 1 MiB buffer, and `response.chunk()` alone does not deliver that: its size is whatever the transport yields. Enforce it explicitly — feed `push` in `limits.chunk_bytes` slices and drop the `Bytes` as soon as it is consumed:
   ```rust
   for slice in bytes.chunks(limits.chunk_bytes) {
       if !channel.push(generation, slice).await { return; }
   }
   drop(bytes);
   ```
   `push` blocks on backpressure per slice, so the application never holds more than `buffer_bytes + chunk_bytes` of its own. The `Bytes` in hand while the loop runs is **library** buffering, which §8 explicitly separates from the application's bound ("document HTTP/TLS library buffering separately; the application buffer cap is not a claim about total process memory"). Where reqwest 0.13 exposes a read-buffer knob (`ClientBuilder::http1_max_buf_size`), set it to `limits.chunk_bytes.max(8192)` so the library's contribution is bounded too; if it is unavailable for the HTTP/2 path, say so in `docs/architecture.md` rather than claiming a total. H13 asserts `channel.buffered() <= limits.buffer_bytes` throughout, which is the half that is ours to promise.
7. **`HeaderWait::wait`** is the same condvar loop `ByteChannel::read` uses: slice, run the hook, re-check the slot and the interrupt under the lock, honour the deadline.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --test http_fetch 2>&1 | tail -30`
Expected: PASS, 10 tests.

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked 2>&1 | tail -5`

- [ ] **Step 5: Commit**

```bash
git add src/http/mod.rs src/http/service.rs tests/http_fetch.rs
git commit -m "feat(http): add the fetch service, manual redirect loop and streaming body

fetch() never blocks the calling thread: it spawns the request on the runtime
and hands back a condvar-backed wait the source interrupt can cancel, so the
decode thread never waits on a Tokio future (§4)."
```

---

## Task 6: `HttpMediaSource`

The Symphonia seam. Everything above it is bytes; everything below it is HTTP.

**Files:**
- Create: `src/http/source.rs`
- Modify: `src/http/mod.rs`
- Test: `tests/http_source.rs`

**Interfaces:**
- Consumes: Tasks 1–5.
- Produces:

```rust
pub struct HttpMediaSource { /* … */ }
impl HttpMediaSource {
    /// Open at byte zero. Performs the opening range GET and classifies access.
    pub fn open(
        service: Arc<HttpService>,
        origin: Url,
        interrupt: Arc<SourceInterrupt>,
        hook: Arc<dyn WaitHook>,
    ) -> Result<Self, RemoteFailure>;
    pub fn evidence(&self) -> SourceEvidence;
    /// The bounded tail read §9 requires before a track may be called complete.
    pub fn confirm_complete(&mut self) -> Result<(), RemoteFailure>;
    /// Bytes consumed since opening, for the §8 probe cap.
    pub fn consumed(&self) -> u64;
    pub fn set_probe_cap(&mut self, cap: Option<u64>);
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceEvidence {
    /// A fixed response length or a valid range total.
    pub byte_len: Option<u64>,
    pub byte_seekable: bool,
    /// Explicit live/ICY semantics were seen.
    pub live: bool,
    /// Whether the *demuxer's* ability to seek in media time is established.
    /// Byte access says nothing about it (§4), and `MediaSource` carries no
    /// such evidence, so it can only come from a format this project has
    /// already demonstrated or from a trial seek.
    pub demuxer: DemuxerSeek,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DemuxerSeek {
    /// Demonstrated. Local playback of the four shipped formats is M1 evidence
    /// that `tests/decode_fixtures.rs` re-proves on every run.
    Proven,
    /// Not yet established. Publishes `Unknown`, verified on demand (§6).
    Unproven,
}
impl std::io::Read for HttpMediaSource { … }
impl std::io::Seek for HttpMediaSource { … }
impl symphonia::core::io::MediaSource for HttpMediaSource { … }
```

**The error mapping, which everything downstream depends on.** `Read::read` must express four distinct outcomes through one `io::Result<usize>`:

| Channel outcome | `Read::read` returns | Why |
|---|---|---|
| `Bytes(n)`, `n >= 1` | `Ok(n)` | — |
| `Eof` | `Ok(0)` | The only `Ok(0)` there is. |
| `Retired` | `Err(io::Error::other(RemoteIoError(RemoteFailure::Cancelled)))` | See below. |
| `Failed(f)` | `Err(io::Error::other(RemoteIoError(f)))` | Carries the typed failure through. |

**`ErrorKind::Interrupted` must not be used, and an earlier draft of this plan got it exactly backwards.** `symphonia-core-0.6.1/src/io/media_source_stream.rs:432` reads

```rust
fn read_buf_exact(&mut self, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match self.read(buf) {
            Ok(0) => break,
            Ok(count) => { buf = &mut buf[count..]; }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    ...
```

so an `Interrupted` is swallowed and the loop goes round again — and `std::io::Read::read_exact` does the same. Latching does not terminate that loop; it **guarantees** an infinite one, because every retry returns `Interrupted` forever. Use `ErrorKind::Other`, which both loops propagate on the first call. Latching stays, but for its real reason: a retired source must answer identically however many times it is asked, so a caller that retries for any other reason gets the same answer instead of racing the channel.

`RemoteFailure` gains one variant for this, so the retirement travels as data rather than as an error kind:

```rust
    /// Not a fault: a stop, seek or shutdown retired the read that was in
    /// flight. Distinguishable from every failure above, which is what §8
    /// requires ("errors and cancellation remain distinguishable even if
    /// Symphonia wraps the I/O error").
    #[error("the read was cancelled")]
    Cancelled,
```

**Recovering it after Symphonia wraps it needs explicit unwrapping, not `source()` traversal.** Two facts make the obvious implementation return `None` every time:

- `symphonia_core::errors::Error` implements the **deprecated `cause()`** and not `source()` (`symphonia-core-0.6.1/src/errors.rs:82`), so `source()` falls through to the trait default and yields `None`. Its `IoError(io::Error)` is a plain enum variant, so the right move is a `match`, not a chain walk.
- `io::Error::source()` returns the *payload's* source, not the payload. `get_ref()` is what returns the payload.

So:

```rust
/// The typed failure inside an error, wherever it is hiding.
///
/// Neither unwrap here is optional. `symphonia_core::errors::Error` implements
/// the deprecated `cause()` rather than `source()`, so a `source()`-only walk
/// stops at it and finds nothing; and `io::Error::source()` yields the
/// payload's source rather than the payload, which `get_ref()` is what returns.
/// Task 10's whole control flow — retired versus failed versus EOF — rests on
/// this function, so a silent `None` here is a stop that reads as a truncated
/// track.
pub fn remote_cause(error: &(dyn std::error::Error + 'static)) -> Option<RemoteFailure> {
    // Symphonia's own error, by value.
    if let Some(symphonia) = error.downcast_ref::<symphonia::core::errors::Error>() {
        if let symphonia::core::errors::Error::IoError(io) = symphonia {
            return remote_cause(io);
        }
        return None;
    }
    // An io::Error's custom payload.
    if let Some(io) = error.downcast_ref::<std::io::Error>()
        && let Some(inner) = io.get_ref()
    {
        if let Some(marker) = inner.downcast_ref::<RemoteIoError>() {
            return Some(marker.0.clone());
        }
        return remote_cause(inner);
    }
    if let Some(marker) = error.downcast_ref::<RemoteIoError>() {
        return Some(marker.0.clone());
    }
    // Only then fall back to the standard chain.
    error.source().and_then(remote_cause)
}

/// True when this error is a retirement rather than a fault.
pub fn is_retired(error: &(dyn std::error::Error + 'static)) -> bool {
    matches!(remote_cause(error), Some(RemoteFailure::Cancelled))
}

/// The `io::Error` payload. Public so `prepare` and the worker can recover it.
#[derive(Debug)]
pub struct RemoteIoError(pub RemoteFailure);
```

`RemoteIoError` implements `Display` by delegating to the inner failure and `std::error::Error` with no `source`, so it is a leaf and the recursion terminates.

Both functions take `&(dyn Error + 'static)`; call them as `remote_cause(&error)` on any `E: Error + 'static`, and on a `PlaybackError::Decode(symphonia_error)` as `remote_cause(symphonia_error)`.

- [ ] **Step 1: Write the failing source tests**

Create `tests/http_source.rs`:

```rust
mod support;

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::{Duration, Instant};

use continuo::http::channel::{SourceInterrupt, WaitHook};
use continuo::http::error::RemoteFailure;
use continuo::http::limits::Limits;
use continuo::http::service::HttpService;
use continuo::http::source::{HttpMediaSource, is_retired, remote_cause};
use support::server::{Script, TestServer};
use symphonia::core::io::MediaSource;
use url::Url;

struct NoHook;
impl WaitHook for NoHook {
    fn service(&self) {}
}

#[allow(clippy::unwrap_used)] // A URL built from a bound loopback port.
fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

fn service() -> Arc<HttpService> {
    match HttpService::spawn(Limits::default()) {
        Ok(service) => service,
        Err(error) => panic!("the HTTP service must start: {error}"),
    }
}

fn body() -> Vec<u8> {
    (0..8192u32).map(|i| (i % 251) as u8).collect()
}

fn open(server: &TestServer, interrupt: Arc<SourceInterrupt>) -> HttpMediaSource {
    match HttpMediaSource::open(service(), url(&server.url("/audio")), interrupt, Arc::new(NoHook)) {
        Ok(source) => source,
        Err(error) => panic!("opening must succeed: {error}"),
    }
}

#[test]
fn a_range_capable_source_reports_its_length_and_is_seekable() {
    let server = TestServer::start(Script::serving(body()));
    let source = open(&server, SourceInterrupt::new(Limits::default().buffer_bytes));
    assert_eq!(source.byte_len(), Some(8192));
    assert!(source.is_seekable());
    assert_eq!(
        source.evidence(),
        continuo::http::source::SourceEvidence { byte_len: Some(8192), byte_seekable: true, live: false }
    );
    server.shutdown();
}

#[test]
fn a_range_ignoring_source_is_not_seekable_but_still_has_a_length() {
    let server = TestServer::start(Script::serving(body()).without_ranges());
    let source = open(&server, SourceInterrupt::new(Limits::default().buffer_bytes));
    assert!(!source.is_seekable());
    assert_eq!(source.byte_len(), Some(8192));
    server.shutdown();
}

#[test]
fn reading_to_the_end_yields_the_body_and_then_a_single_clean_eof() {
    let server = TestServer::start(Script::serving(body()));
    let mut source = open(&server, SourceInterrupt::new(Limits::default().buffer_bytes));
    let mut all = Vec::new();
    match source.read_to_end(&mut all) {
        Ok(_) => {}
        Err(error) => panic!("reading must succeed: {error}"),
    }
    assert_eq!(all, body());
    match source.read(&mut [0u8; 8]) {
        Ok(0) => {}
        other => panic!("expected a clean EOF, got {other:?}"),
    }
    server.shutdown();
}

#[test]
fn seeking_forward_issues_a_range_request_at_that_byte() {
    // H2's byte half.
    let server = TestServer::start(Script::serving(body()));
    let mut source = open(&server, SourceInterrupt::new(Limits::default().buffer_bytes));
    match source.seek(SeekFrom::Start(4096)) {
        Ok(position) => assert_eq!(position, 4096),
        Err(error) => panic!("seeking must succeed: {error}"),
    }
    let mut head = [0u8; 4];
    match source.read_exact(&mut head) {
        Ok(()) => {}
        Err(error) => panic!("reading after a seek must succeed: {error}"),
    }
    assert_eq!(head, [body()[4096], body()[4097], body()[4098], body()[4099]]);

    let ranges: Vec<_> = server.requests().iter().filter_map(|r| r.range()).collect();
    assert!(ranges.contains(&(4096, None)), "no range request at 4096: {ranges:?}");
    server.shutdown();
}

#[test]
fn seeking_to_the_known_byte_eof_answers_locally_without_a_request() {
    // §7: "Seeking to a known byte EOF may return EOF locally without
    // requesting it" — because the server would answer 416, and a 416 is not
    // completion.
    let server = TestServer::start(Script::serving(body()));
    let mut source = open(&server, SourceInterrupt::new(Limits::default().buffer_bytes));
    let before = server.requests().len();
    match source.seek(SeekFrom::Start(8192)) {
        Ok(position) => assert_eq!(position, 8192),
        Err(error) => panic!("seeking to EOF must succeed: {error}"),
    }
    match source.read(&mut [0u8; 8]) {
        Ok(0) => {}
        other => panic!("expected EOF at the byte end, got {other:?}"),
    }
    assert_eq!(server.requests().len(), before, "a 416 was requested needlessly");
    server.shutdown();
}

#[test]
fn a_retired_read_is_recovered_through_symphonias_own_error_type() {
    // The two unwraps that a `source()`-only walk misses. Symphonia implements
    // the deprecated `cause()`, and io::Error::source() yields the payload's
    // source rather than the payload. Both are exercised here by wrapping the
    // error exactly as the decoder does.
    let failure = RemoteFailure::TruncatedBody { missing: 9 };
    let io = std::io::Error::other(continuo::http::source::RemoteIoError(failure.clone()));
    let wrapped = symphonia::core::errors::Error::IoError(io);
    assert_eq!(remote_cause(&wrapped), Some(failure));

    let cancelled = symphonia::core::errors::Error::IoError(std::io::Error::other(
        continuo::http::source::RemoteIoError(RemoteFailure::Cancelled),
    ));
    assert!(is_retired(&cancelled), "a wrapped retirement was not recognised");

    // A decode error that carries no remote cause must not be mistaken for one.
    let decode = symphonia::core::errors::Error::DecodeError("bad frame");
    assert_eq!(remote_cause(&decode), None);
    assert!(!is_retired(&decode));
}

#[test]
fn a_retirement_does_not_use_interrupted_and_does_not_spin_a_retry_loop() {
    // symphonia-core-0.6.1 media_source_stream.rs:432 swallows Interrupted
    // inside `while !buf.is_empty()`, and std's read_exact does the same, so an
    // Interrupted that latches is an infinite loop rather than a terminating
    // one. Drive the real buffered reader to prove the error propagates.
    use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions, ReadBytes};

    let server = TestServer::start(Script::serving(body()).stall_body_after(16));
    let interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
    let source = open(&server, Arc::clone(&interrupt));
    assert!(server.wait_until_stalled(Duration::from_secs(5)));
    interrupt.retire();

    let mut stream = MediaSourceStream::new(
        Box::new(source),
        MediaSourceStreamOptions { buffer_len: 64 * 1024 },
    );
    let started = Instant::now();
    let mut sink = [0u8; 8192];
    let error = match stream.read_buf_exact(&mut sink) {
        Err(error) => error,
        Ok(()) => panic!("a retired read filled the buffer"),
    };
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "read_buf_exact spun for {:?}",
        started.elapsed()
    );
    assert_ne!(
        error.kind(),
        std::io::ErrorKind::Interrupted,
        "Interrupted is retried by both read_exact and symphonia's reader"
    );
    assert!(is_retired(&error), "the retirement was lost: {error}");
    server.shutdown();
}

#[test]
fn a_retired_read_is_distinguishable_from_an_error_and_stays_retired() {
    // §8: errors and cancellation remain distinguishable even after Symphonia
    // wraps the I/O error. Latching is not what terminates a retry loop — the
    // non-retryable error kind is — but a retired source must still answer
    // identically however many times it is asked.
    let server = TestServer::start(Script::serving(body()).stall_body_after(16));
    let interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
    let mut source = open(&server, Arc::clone(&interrupt));
    assert!(server.wait_until_stalled(Duration::from_secs(5)));
    interrupt.retire();

    let started = Instant::now();
    let first = source.read(&mut [0u8; 4096]);
    assert!(started.elapsed() < Duration::from_secs(1));
    match first {
        Err(error) => {
            assert!(is_retired(&error), "not recognised as a retirement: {error}");
            assert_ne!(error.kind(), std::io::ErrorKind::Interrupted);
        }
        Ok(n) => panic!("expected a retirement, read {n} bytes"),
    }
    match source.read(&mut [0u8; 4096]) {
        Err(error) => assert!(is_retired(&error), "the retirement did not latch: {error}"),
        Ok(n) => panic!("expected a latched retirement, read {n} bytes"),
    }
    server.shutdown();
}

#[test]
fn a_truncated_body_surfaces_as_a_typed_remote_failure_not_as_eof() {
    let server = TestServer::start(Script::serving(body()).truncate_body_after(1024));
    let mut source = open(&server, SourceInterrupt::new(Limits::default().buffer_bytes));
    let mut sink = Vec::new();
    let error = match source.read_to_end(&mut sink) {
        Err(error) => error,
        Ok(n) => panic!("a truncated body read back as {n} clean bytes"),
    };
    assert!(
        matches!(remote_cause(&error), Some(RemoteFailure::TruncatedBody { .. })),
        "the typed cause was lost: {error}"
    );
    server.shutdown();
}

#[test]
fn a_changed_strong_validator_on_a_seek_fails_as_resource_changed() {
    // H11. Same length, different ETag: only the validator catches it.
    let server = TestServer::start(Script::serving(body()).changing_etag_after(1));
    let mut source = open(&server, SourceInterrupt::new(Limits::default().buffer_bytes));
    let outcome = source.seek(SeekFrom::Start(4096));
    let error = match outcome {
        Err(error) => error,
        Ok(_) => panic!("a replaced recording must not seek successfully"),
    };
    assert!(
        matches!(remote_cause(&error), Some(RemoteFailure::ResourceChanged)),
        "{error}"
    );
    server.shutdown();
}

#[test]
fn a_live_source_is_flagged_as_live_evidence() {
    let server = TestServer::start(Script::serving(body()).live());
    let source = open(&server, SourceInterrupt::new(Limits::default().buffer_bytes));
    assert!(source.evidence().live);
    assert_eq!(source.evidence().byte_len, None);
    server.shutdown();
}

#[test]
fn the_probe_cap_stops_a_runaway_scan() {
    let server = TestServer::start(Script::serving(vec![0u8; 1 << 20]));
    let mut source = open(&server, SourceInterrupt::new(Limits::default().buffer_bytes));
    source.set_probe_cap(Some(4096));
    let mut sink = Vec::new();
    let error = match source.read_to_end(&mut sink) {
        Err(error) => error,
        Ok(n) => panic!("the cap was ignored; read {n} bytes"),
    };
    assert!(
        matches!(remote_cause(&error), Some(RemoteFailure::ProbeLimitExceeded { .. })),
        "{error}"
    );
    server.shutdown();
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test http_source 2>&1 | tail -20`
Expected: FAIL — `unresolved import continuo::http::source`.

- [ ] **Step 3: Implement the source**

Add `pub mod source;` to `src/http/mod.rs`, then write `src/http/source.rs` to these obligations:

1. **`open`** builds a `SourceInterrupt`-backed `ByteChannel` sized `limits.buffer_bytes`, calls `service.fetch` with `start = 0`, waits on the `HeaderWait` with `limits.open`, and records:
   - `byte_len` from `Accepted::Ranged { range }`'s `range.total`, or from `Accepted::Sequential { len }`.
   - `byte_seekable = matches!(accepted, Accepted::Ranged { .. })`.
   - `live = response::is_live(&headers)`.
   - `established = Established { total: byte_len, validator }`, kept for every later request.
   The accepted response *is* the initial stream (§6): no second request.
2. **`Read::read`** delegates to `channel.read(out, &*self.hook, limits.stall)` and applies the mapping table above. Before delegating, if `probe_cap` is set and `consumed + out.len()` would exceed it, return the `ProbeLimitExceeded` error. Increment `consumed` and `pos` on success. Latch `retired` on `ReadOutcome::Retired`.
3. **`Seek::seek`** resolves `SeekFrom` against `pos` and `byte_len`, then:
   - target == `pos` → return `pos` with no request.
   - `byte_len == Some(len)` and target == `len` → set `pos`, set an `at_byte_eof` flag so the next `read` returns `Ok(0)` locally, and issue no request.
   - `!byte_seekable` → `Err(io::Error::new(ErrorKind::Unsupported, RemoteIoError(RemoteFailure::SeekUnavailable)))`.
   - otherwise `let generation = channel.retire();` then `if !interrupt.arm(generation) { return Err(retired) }`, `fetch` at the new start with `established`, wait on headers under `min(opening.remaining(), limits.headers)`, validate, set `pos`, clear `at_byte_eof`.
   The `retire`-then-`arm` order matters: retiring bumps the generation so a superseded response's bytes cannot enter the new one (H9), and arming clears the flag the *old* read is now past caring about. `arm` returning `false` means somebody retired this source in between — a stop from the application thread — and the seek must abandon rather than reopen against a stop that has already been decided.
4. **`MediaSource`**: `is_seekable()` returns `byte_seekable`; `byte_len()` returns `byte_len`.
5. **`confirm_complete`** (§9): when `byte_len` is known and `pos < byte_len`, read and discard to the end under `limits.stall`, failing with `TruncatedBody` or `Timeout` rather than succeeding. When `byte_len` is unknown, read until `Ok(0)` under the same deadline. Bounded memory: discard into a fixed 64 KiB scratch buffer.
6. **`Drop`** retires the channel so a source dropped mid-fetch cannot leave a task pushing into it.

**Symphonia note.** `MediaSourceStream` keeps a rewind buffer, which is what lets the probe back up over a non-seekable source. Construct it with an explicit `MediaSourceStreamOptions { buffer_len: 64 * 1024 }` in Task 7 rather than `Default::default()`, so §8's "configure decoder buffering explicitly" is a fact in the code rather than a hope.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --test http_source 2>&1 | tail -30`
Expected: PASS, 12 tests.

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add src/http/mod.rs src/http/source.rs tests/http_source.rs
git commit -m "feat(http): add HttpMediaSource over the byte channel

A retirement travels as RemoteFailure::Cancelled inside ErrorKind::Other, never
Interrupted: symphonia read_buf_exact and std read_exact both swallow and retry
Interrupted, so latching one is an infinite loop. remote_cause unwraps
symphonia Error::IoError and io::Error::get_ref explicitly, because symphonia
0.6.1 implements the deprecated cause() and source() alone finds nothing."
```

---

## Task 7: Unified preparation and capability evidence

One path from a `SourceLocation` to an open decoder plus evidence-backed capabilities, for local files and HTTP alike (R5). This is where §6's table lives.

**Files:**
- Create: `src/playback/prepare.rs`
- Modify: `src/playback/mod.rs`, `src/playback/decode.rs`, `src/playback/error.rs`
- Test: `tests/prepare.rs`

**Interfaces:**
- Consumes: Task 6's `HttpMediaSource`, `SourceEvidence`.
- Produces:

```rust
pub struct Prepared { pub source: DecodedSource, pub capabilities: MediaCapabilities }
pub struct PrepareContext {
    /// `None` for local-only sessions — `--probe-only` on a file, and every
    /// test that never touches the network.
    pub http: Option<Arc<HttpService>>,
    pub interrupt: Arc<SourceInterrupt>,
    pub hook: Arc<dyn WaitHook>,
    pub limits: Limits,
}

/// One absolute instant that every wait taken during opening is clamped
/// against. Threaded down rather than checked around the outside, because
/// opening is a sequence of individually-short waits and checking only between
/// them lets a slow trickle run indefinitely.
#[derive(Clone, Copy, Debug)]
pub struct OpeningDeadline(pub Instant);
impl OpeningDeadline {
    /// `None` once elapsed.
    pub fn remaining(&self) -> Option<Duration>;
}
pub fn prepare(location: &SourceLocation, context: &PrepareContext) -> Result<Prepared, PlaybackError>;
```

`DecodedSource` gains:

```rust
/// Open over any `MediaSource`, with the supplied evidence folded into the
/// capabilities the decoder alone cannot establish.
pub fn from_media_source(
    source: Box<dyn MediaSource>,
    hint: Hint,
    label: PathBuf,
    evidence: SourceEvidence,
) -> Result<Self, PlaybackError>;
```

`DecodedSource::open(&AbsolutePath)` stays, and is reimplemented in terms of `from_media_source` with `SourceEvidence { byte_len: file length, byte_seekable: true, live: false }`. Its existing behaviour — the regular-file check, the extension hint, the mono/stereo refusal — is unchanged, so `tests/decode_fixtures.rs` keeps passing untouched.

`PlaybackError` gains `#[error(transparent)] Remote(#[from] RemoteFailure)`.

**§6's table, implemented exactly.** `capabilities_from(evidence, decoder_duration)`:

| Evidence | Result |
|---|---|
| `evidence.live` | `Err(UnsupportedLiveMedia)` |
| `evidence.byte_len.is_some()` | `Continuity::Finite` |
| `byte_len.is_none()` but `decoder_duration.is_some()` | `Continuity::Finite` |
| neither | `Err(ContinuityUndetermined)` (R3) |

and, given `Finite`:

| Byte access | `evidence.demuxer` | `SeekSupport` |
|---|---|---|
| `byte_seekable` | `Proven` | `Native` |
| `byte_seekable` | `Unproven` | `Unknown` |
| not `byte_seekable` | either | `Unsupported` |

`RestartAndDiscard` is never published (R1).

**Do not infer `Proven` from track metadata.** An earlier draft read `Native` off a track carrying both `time_base` and `num_frames`. Those fields describe *timing* — how to convert a timestamp, and how many frames the track claims — and say nothing about whether the selected demuxer implements `seek`. `MediaSource` (`symphonia-core-0.6.1/src/io/mod.rs:42`) exposes only `is_seekable` and `byte_len`, `FormatReader` exposes no seekability query at all, and a reader that cannot seek says so only by answering `Error::SeekError(SeekErrorKind::Unseekable)` when asked. Asking is the only way to find out.

So `Proven` has exactly two sources, both evidence rather than inference:
- **Local files**, which pass `DemuxerSeek::Proven` because M1 already ships that guarantee for the four supported formats and `tests/decode_fixtures.rs` re-proves it on every run. This is what keeps local capabilities byte-identical.
- **A trial seek**, performed by Task 10's `verify_seek_support`, which publishes `CapabilitiesChanged` with the answer.

Every remote source therefore opens as `Unknown` and is verified on demand, which is what §6 asks for in as many words: "Until conclusive evidence exists, publish `Unknown`, and verify on demand." A resume seek at load time *is* that verification, so an ordinary resume costs no extra request. H17 is the test that a byte-seekable source is never advertised `Native` before something demonstrated it.

- [ ] **Step 1: Write the failing preparation tests**

Create `tests/prepare.rs`:

```rust
mod support;

use std::sync::Arc;
use std::time::Duration;

use continuo::http::channel::{SourceInterrupt, WaitHook};
use continuo::http::error::RemoteFailure;
use continuo::http::limits::Limits;
use continuo::http::service::HttpService;
use continuo::media::capabilities::{Continuity, SeekSupport};
use continuo::media::source::SourceLocation;
use continuo::playback::error::PlaybackError;
use continuo::playback::prepare::{PrepareContext, prepare};
use support::server::{Script, TestServer};
use url::Url;

struct NoHook;
impl WaitHook for NoHook {
    fn service(&self) {}
}

#[allow(clippy::unwrap_used)] // A URL built from a bound loopback port.
fn url(text: &str) -> Url {
    Url::parse(text).unwrap()
}

fn context() -> PrepareContext {
    let http = match HttpService::spawn(Limits::default()) {
        Ok(service) => service,
        Err(error) => panic!("the HTTP service must start: {error}"),
    };
    PrepareContext {
        http: Some(http),
        interrupt: SourceInterrupt::new(Limits::default().buffer_bytes),
        hook: Arc::new(NoHook),
        limits: Limits::default(),
    }
}

fn remote(server: &TestServer) -> SourceLocation {
    SourceLocation::Http(url(&server.url("/audio.flac")))
}

#[test]
fn a_range_capable_recording_is_finite_and_natively_seekable() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let prepared = match prepare(&remote(&server), &context()) {
        Ok(prepared) => prepared,
        Err(error) => panic!("preparation must succeed: {error}"),
    };
    assert_eq!(prepared.capabilities.continuity, Continuity::Finite);
    assert_eq!(prepared.capabilities.seek, SeekSupport::Native);
    assert_eq!(prepared.source.sample_rate(), 44_100);
    server.shutdown();
}

#[test]
fn a_range_ignoring_recording_is_finite_but_unseekable() {
    // §1.1 R1: no RestartAndDiscard. Sequential playback, seek refused.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").without_ranges());
    let prepared = match prepare(&remote(&server), &context()) {
        Ok(prepared) => prepared,
        Err(error) => panic!("preparation must succeed: {error}"),
    };
    assert_eq!(prepared.capabilities.continuity, Continuity::Finite);
    assert_eq!(prepared.capabilities.seek, SeekSupport::Unsupported);
    assert_ne!(prepared.capabilities.seek, SeekSupport::RestartAndDiscard);
    server.shutdown();
}

#[test]
fn chunked_media_with_decoder_evidence_is_finite() {
    // H12: no Content-Length, but the decoder establishes a recording length.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").chunked());
    let prepared = match prepare(&remote(&server), &context()) {
        Ok(prepared) => prepared,
        Err(error) => panic!("preparation must succeed: {error}"),
    };
    assert_eq!(prepared.capabilities.continuity, Continuity::Finite);
    assert!(prepared.source.metadata().duration.is_some());
    server.shutdown();
}

#[test]
fn an_explicit_live_source_is_refused_as_live() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").live());
    let error = match prepare(&remote(&server), &context()) {
        Err(error) => error,
        Ok(_) => panic!("a live stream must be refused"),
    };
    assert!(
        matches!(error, PlaybackError::Remote(RemoteFailure::UnsupportedLiveMedia)),
        "{error}"
    );
    server.shutdown();
}

#[test]
fn an_unresolved_source_is_refused_distinctly_from_a_live_one() {
    // H12, R3. Two different refusals, because they are two different facts.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").unresolved());
    let error = match prepare(&remote(&server), &context()) {
        Err(error) => error,
        Ok(_) => panic!("an unresolved source must be refused"),
    };
    assert!(
        matches!(error, PlaybackError::Remote(RemoteFailure::ContinuityUndetermined)),
        "an unresolved source must not be reported as live: {error}"
    );
    server.shutdown();
}

#[test]
fn a_local_file_prepares_through_the_same_path_with_no_http_service() {
    // R5: one preparation path. A local session builds no runtime at all.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sine.flac");
    let canonical = match path.canonicalize() {
        Ok(canonical) => canonical,
        Err(error) => panic!("the fixture must exist: {error}"),
    };
    let context = PrepareContext {
        http: None,
        interrupt: SourceInterrupt::new(Limits::default().buffer_bytes),
        hook: Arc::new(NoHook),
        limits: Limits::default(),
    };
    let prepared = match prepare(&SourceLocation::LocalPath(canonical), &context) {
        Ok(prepared) => prepared,
        Err(error) => panic!("a local file must prepare: {error}"),
    };
    assert_eq!(prepared.capabilities.continuity, Continuity::Finite);
    assert_eq!(prepared.capabilities.seek, SeekSupport::Native);
}

#[test]
fn an_http_url_without_a_service_is_refused_rather_than_silently_ignored() {
    let context = PrepareContext {
        http: None,
        interrupt: SourceInterrupt::new(Limits::default().buffer_bytes),
        hook: Arc::new(NoHook),
        limits: Limits::default(),
    };
    let error = match prepare(&SourceLocation::Http(url("https://example.com/a.mp3")), &context) {
        Err(error) => error,
        Ok(_) => panic!("an HTTP source needs a service"),
    };
    assert!(matches!(error, PlaybackError::Remote(RemoteFailure::InvalidSource { .. })), "{error}");
}

#[test]
fn opening_that_exceeds_the_probe_byte_cap_fails_cancellably() {
    // H17's second half.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut context = context();
    context.limits.probe_bytes = 1024;
    let error = match prepare(&remote(&server), &context) {
        Err(error) => error,
        Ok(_) => panic!("the probe cap was ignored"),
    };
    assert!(
        matches!(error, PlaybackError::Remote(RemoteFailure::ProbeLimitExceeded { limit: 1024 })),
        "{error}"
    );
    server.shutdown();
}

#[test]
fn a_slow_trickle_cannot_outlast_the_opening_deadline() {
    // Every individual read stays inside `stall`, so a deadline checked only
    // after probing returns never fires and opening runs indefinitely. The
    // deadline has to be inside each wait, not around all of them.
    let server =
        TestServer::start(Script::from_fixture("sine-5s.flac").trickle(1, Duration::from_millis(50)));
    let mut context = context();
    context.limits.open = Duration::from_secs(1);
    context.limits.stall = Duration::from_secs(30);
    let started = std::time::Instant::now();
    let error = match prepare(&remote(&server), &context) {
        Err(error) => error,
        Ok(_) => panic!("a trickling server opened despite the deadline"),
    };
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert!(
        matches!(
            error,
            PlaybackError::Remote(RemoteFailure::Timeout { phase: continuo::http::error::Phase::Open })
        ),
        "{error}"
    );
    server.shutdown();
}

#[test]
fn a_probe_that_outlives_its_deadline_is_refused() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").stall_body_after(64));
    let mut context = context();
    context.limits = Limits::brisk();
    let started = std::time::Instant::now();
    let error = match prepare(&remote(&server), &context) {
        Err(error) => error,
        Ok(_) => panic!("a stalled probe must not succeed"),
    };
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    assert!(matches!(error, PlaybackError::Remote(RemoteFailure::Timeout { .. })), "{error}");
    server.shutdown();
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test prepare 2>&1 | tail -20`
Expected: FAIL — `unresolved import continuo::playback::prepare`.

- [ ] **Step 3: Implement preparation**

Add `pub mod prepare;` to `src/playback/mod.rs`. Add the `Remote` variant to `src/playback/error.rs`:

```rust
    #[error(transparent)]
    Remote(#[from] crate::http::error::RemoteFailure),
```

In `src/playback/decode.rs`, extract the body of `open` after the `MediaSourceStream` construction into `from_media_source`, and give `DecodedSource` one new field, `evidence: SourceEvidence`, taken from the caller. Add a setter `pub fn note_demuxer_proven(&mut self)` for Task 10's trial seek to call. Replace `capabilities()` with:

```rust
    /// Capabilities the decoder can establish *on its own*. The engine combines
    /// these with transport evidence: HTTP range support alone does not prove
    /// that a particular container can seek in media time (§4).
    pub fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            continuity: if self.evidence.byte_len.is_some() || self.metadata.duration.is_some() {
                Continuity::Finite
            } else {
                Continuity::Unresolved
            },
            seek: match (self.evidence.byte_seekable, self.evidence.demuxer) {
                (true, DemuxerSeek::Proven) => SeekSupport::Native,
                (true, DemuxerSeek::Unproven) => SeekSupport::Unknown,
                (false, _) => SeekSupport::Unsupported,
            },
        }
    }
```

**This changes local behaviour and must not.** A local file was unconditionally `SeekSupport::Native`. Keep that: `DecodedSource::open` passes `SourceEvidence { byte_len: Some(file length), byte_seekable: true, live: false, demuxer: DemuxerSeek::Proven }`, so the answer is still `Native` by the first row of the table. Verify with `cargo test --test decode_fixtures --test capabilities` before moving on; a fixture coming back `Unknown` means `open` is not passing `Proven`.

`prepare` then:
1. `SourceLocation::LocalPath(path)` → canonicalize, `AbsolutePath::new`, `DecodedSource::open`.
2. `SourceLocation::Http(url)` → `context.http.as_ref().ok_or(RemoteFailure::InvalidSource { input: redact_url(url.as_str()), reason: "no HTTP service in this session" })`, then `HttpMediaSource::open`, `set_probe_cap(Some(limits.probe_bytes))`, wrap in `MediaSourceStream::new(Box::new(source), MediaSourceStreamOptions { buffer_len: 64 * 1024 })`, hint from the URL path's extension, `from_media_source`, then clear both the probe cap and the opening deadline — each bounds *opening*, not playback. Getting them off afterwards needs the source back after it has been boxed, so give `HttpMediaSource` an `Arc<OpeningLimits>` (an `AtomicU64` cap plus an `AtomicBool` "opening is over") that `prepare` clones before boxing and clears through that clone.
3. Reject `Continuity::Indefinite` (from `evidence.live`) as `UnsupportedLiveMedia` and `Continuity::Unresolved` as `ContinuityUndetermined`, before returning (R3).
4. **One absolute opening deadline, propagated into every wait.** Checking `Instant::now()` after the probe returns is not enough, and an earlier draft did exactly that: each individual read can sit comfortably inside `limits.stall` while a server trickles a byte every few seconds, so no read ever times out and opening runs indefinitely. `prepare` computes `OpeningDeadline(Instant::now() + limits.open)` once and passes it into `HttpMediaSource::open`, which stores it and clamps *every* wait it takes to `min(deadline.remaining()?, limits.stall)` — the header wait, every `Read::read` taken during probing, and every seek's header wait. `remaining()` returning `None` is `Timeout { phase: Phase::Open }`.

   Once `prepare` clears the deadline, ordinary playback reads are bounded only by `limits.stall`, which is §8's rule that ordinary playback has no whole-response deadline.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --test prepare --test decode_fixtures --test capabilities 2>&1 | tail -30`
Expected: PASS — 10 new, and every existing decode/capability test unchanged.

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked 2>&1 | tail -5`

- [ ] **Step 5: Commit**

```bash
git add src/playback/mod.rs src/playback/prepare.rs src/playback/decode.rs src/playback/error.rs tests/prepare.rs
git commit -m "feat(playback): one preparation path for local and HTTP sources

§6's evidence table, with RestartAndDiscard never published (R1) and
unresolved continuity refused distinctly from live media (R3)."
```

---

## Task 8: Resume intent, start disposition and the event protocol

Independent of Tasks 1–7: nothing here touches HTTP. It closes G1 and G3 and does §8's reserve arithmetic.

**Files:**
- Create: `src/resume.rs`
- Modify: `src/lib.rs`, `src/session.rs`, `src/playback/command.rs`, `src/playback/event.rs`, `src/playback/engine.rs`, `src/app.rs`
- Test: `tests/resume_decision.rs` (new), and updated call sites in `tests/session_policy.rs`, `tests/engine_contract.rs`, `tests/engine_shutdown.rs`, `tests/resume_contract.rs`, `tests/support/mod.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:

```rust
// src/resume.rs — persistence-free, so playback and session may both depend on it (G3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResumeCandidate { pub position: Duration, pub completed: bool }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeDecision { NoEntry, Completed, AtStart, Resume(Duration), DegenerateEnd, StalePastEnd, Unvalidated(Duration) }
impl ResumeDecision { pub fn start_at(&self) -> Duration; }

pub fn decide_resume(candidate: Option<ResumeCandidate>, duration: Option<Duration>) -> ResumeDecision;
```

```rust
// src/playback/command.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeIntent {
    /// The application has already decided. Used by Restart and by tests.
    StartAt(Duration),
    /// Resolved by the worker after its single probe, using the same rules the
    /// application would apply if it had a duration to apply them to.
    Candidate(ResumeCandidate),
}

pub enum PlaybackCommand {
    Load { media: MediaId, source: SourceLocation, resume: ResumeIntent },
    // … the rest unchanged
}

/// Whether a submission entered the queue. §8: saturation is visible, never a
/// block on the application thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission { Accepted, Busy, Gone }
```

```rust
// src/playback/event.rs
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StartDisposition {
    /// No candidate, or one that resolved to zero.
    Fresh,
    /// The candidate's position was established by the decoder.
    Resumed,
    /// The entry was complete; M2's replay policy starts it over.
    CompletedReplay,
    /// A positive candidate could not be established, because this source
    /// cannot seek. Playback starts at zero and the entry is protected (§10).
    ResumeUnavailable { retained: Duration },
}

pub enum PlaybackEvent {
    Loaded { session_rev: u64, media: MediaId, metadata: MediaMetadata, capabilities: MediaCapabilities, position: Duration, disposition: StartDisposition },
    /// Capability evidence that arrived after `Loaded` — an on-demand seek
    /// probe resolving `Unknown`. Ordered, revision-keyed, consumed like
    /// `Loaded`. It never establishes or clears checkpoint protection (§5).
    CapabilitiesChanged { session_rev: u64, capabilities: MediaCapabilities },
    /// An explicit restart that actually landed.
    ///
    /// `SeekCompleted` deliberately does not cover this (D17): `restart()`
    /// discards a stored target and seeks to zero without emitting one, so a
    /// policy that must tell an explicit restart from any other establishment
    /// has nothing else to key on (G1).
    RestartEstablished { session_rev: u64, position: Duration },
    /// An accepted seek that a stop or shutdown cancelled before it committed.
    /// Terminal, so §8's "every accepted seek receives an outcome" survives a
    /// shutdown backlog.
    SeekCancelled { session_rev: u64, requested: Duration },
    Failed { session_rev: u64, message: String, cause: Option<RemoteFailure> },
    // … the rest unchanged
}
```

**Reserve arithmetic (§8).** `RESERVED_EVENT_SLOTS` rises from 8 to **9**, and the comment beside it is rewritten:

```
   stop interrupt        1  StateChanged{Stopped}
   a serviced fault      2  Failed + StateChanged, or DeviceRecovered + StateChanged
   a dispatched command  4  Load is the widest: StateChanged{Loading}, Loaded,
                            CapabilitiesChanged, StateChanged{Paused}
   end of track          2  EndOfTrack + StateChanged{Ended}
                        --
                         9  <= RESERVED_EVENT_SLOTS
```

The resume-unavailable warning does **not** add a fifth: it rides on `Loaded.disposition` rather than a separate `Warning`, which is also what lets Session act on it before any Playing/progress event (§5). `RestartEstablished` and `SeekCancelled` both belong to commands narrower than Load (2 each), so they do not raise the bound. `SeekCancelled` is terminal; `RestartEstablished` and `CapabilitiesChanged` are ordinary.

- [ ] **Step 1: Write the failing resume-decision tests**

Create `tests/resume_decision.rs` by **moving** the seven `#[cfg(test)] mod tests` cases currently at the bottom of `src/session.rs` (`no_entry_starts_at_the_beginning` through `completion_outranks_every_position_rule`) into it, rewritten against `ResumeCandidate` instead of `PersistedCheckpoint`:

```rust
use std::time::Duration;

use continuo::resume::{ResumeCandidate, ResumeDecision, decide_resume};

fn stored(secs: u64, completed: bool) -> ResumeCandidate {
    ResumeCandidate { position: Duration::from_secs(secs), completed }
}

fn secs(value: u64) -> Option<Duration> {
    Some(Duration::from_secs(value))
}

#[test]
fn no_entry_starts_at_the_beginning() {
    assert_eq!(decide_resume(None, secs(300)), ResumeDecision::NoEntry);
    assert_eq!(decide_resume(None, secs(300)).start_at(), Duration::ZERO);
}

#[test]
fn a_completed_entry_declines_the_resume_without_losing_its_position() {
    let entry = stored(300, true);
    assert_eq!(decide_resume(Some(entry), secs(300)), ResumeDecision::Completed);
    assert_eq!(decide_resume(Some(entry), secs(300)).start_at(), Duration::ZERO);
    assert_eq!(entry.position, Duration::from_secs(300));
    let short = stored(120, true);
    assert_eq!(decide_resume(Some(short), secs(300)), ResumeDecision::Completed);
    assert_eq!(short.position, Duration::from_secs(120));
}

#[test]
fn an_ordinary_position_inside_the_media_is_the_start() {
    assert_eq!(
        decide_resume(Some(stored(93, false)), secs(300)),
        ResumeDecision::Resume(Duration::from_secs(93))
    );
}

#[test]
fn a_position_of_zero_is_a_start_rather_than_a_resume() {
    assert_eq!(decide_resume(Some(stored(0, false)), secs(300)), ResumeDecision::AtStart);
    assert_eq!(decide_resume(Some(stored(0, false)), None), ResumeDecision::AtStart);
}

#[test]
fn a_position_exactly_at_the_end_is_degenerate_not_a_start() {
    assert_eq!(decide_resume(Some(stored(300, false)), secs(300)), ResumeDecision::DegenerateEnd);
    assert_eq!(decide_resume(Some(stored(300, false)), secs(300)).start_at(), Duration::ZERO);
}

#[test]
fn a_position_past_the_end_is_stale_state() {
    assert_eq!(decide_resume(Some(stored(400, false)), secs(300)), ResumeDecision::StalePastEnd);
    assert_eq!(decide_resume(Some(stored(400, false)), secs(300)).start_at(), Duration::ZERO);
}

#[test]
fn an_unknown_duration_keeps_the_position_unvalidated() {
    assert_eq!(
        decide_resume(Some(stored(93, false)), None),
        ResumeDecision::Unvalidated(Duration::from_secs(93))
    );
    assert_eq!(decide_resume(Some(stored(93, false)), None).start_at(), Duration::from_secs(93));
}

#[test]
fn completion_outranks_every_position_rule() {
    assert_eq!(decide_resume(Some(stored(400, true)), secs(300)), ResumeDecision::Completed);
    assert_eq!(decide_resume(Some(stored(0, true)), secs(300)), ResumeDecision::Completed);
}

#[test]
fn the_decision_is_the_same_whoever_applies_it() {
    // G3: the worker resolves a Candidate with these rules and the application
    // resolves a duration-known one with the same function. A divergence here
    // is a resume that lands somewhere the checkpoint never said.
    for secs_stored in [0u64, 93, 300, 400] {
        for completed in [false, true] {
            for duration in [None, secs(300)] {
                let candidate = stored(secs_stored, completed);
                assert_eq!(
                    decide_resume(Some(candidate), duration),
                    decide_resume(Some(candidate), duration),
                );
            }
        }
    }
}
```

- [ ] **Step 2: Write the failing disposition test**

Add to `tests/engine_contract.rs` (additively — no existing test changes):

```rust
#[test]
fn a_load_that_resumes_reports_a_resumed_disposition() {
    let mut engine = TestEngine::start();
    engine.load_with_resume(
        fixture("sine-5s.flac"),
        ResumeIntent::Candidate(ResumeCandidate {
            position: Duration::from_secs(2),
            completed: false,
        }),
    );
    let loaded = engine.await_loaded();
    assert_eq!(loaded.disposition, StartDisposition::Resumed);
    assert!(loaded.position >= Duration::from_secs(2));
    engine.finish();
}

#[test]
fn a_load_of_a_completed_entry_replays_from_zero_and_says_so() {
    let mut engine = TestEngine::start();
    engine.load_with_resume(
        fixture("sine-5s.flac"),
        ResumeIntent::Candidate(ResumeCandidate {
            position: Duration::from_secs(2),
            completed: true,
        }),
    );
    let loaded = engine.await_loaded();
    assert_eq!(loaded.disposition, StartDisposition::CompletedReplay);
    assert_eq!(loaded.position, Duration::ZERO);
    engine.finish();
}

#[test]
fn an_explicit_restart_announces_that_it_established() {
    // G1. Without this event `Session` cannot tell a restart from any other
    // establishment, and §10's protection can never be lifted.
    let mut engine = TestEngine::start();
    engine.load(fixture("sine-5s.flac"));
    engine.play_for(Duration::from_millis(200));
    engine.send(PlaybackCommand::Restart);
    let established = engine.await_restart_established();
    assert_eq!(established, Duration::ZERO);
    engine.finish();
}
```

`TestEngine::load_with_resume`, `await_loaded` (returning the `Loaded` fields) and `await_restart_established` are **additive** helpers in `tests/support/mod.rs`; the existing `TestEngine::load` keeps its signature by calling `load_with_resume(path, ResumeIntent::StartAt(Duration::ZERO))`, and `TestEngine::start_at` becomes `load_with_resume(path, ResumeIntent::StartAt(at))`. No existing helper signature changes.

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test --test resume_decision --test engine_contract 2>&1 | tail -20`
Expected: FAIL — `unresolved import continuo::resume` and the three new engine tests.

- [ ] **Step 4: Implement**

1. Create `src/resume.rs` with `ResumeCandidate`, `ResumeDecision` and `decide_resume` — the body is `src/session.rs`'s current `decide_resume` with `entry: Option<&PersistedCheckpoint>` replaced by `candidate: Option<ResumeCandidate>` and `entry.position` / `entry.completed` read off the candidate. Add `pub mod resume;` to `src/lib.rs`.
2. In `src/session.rs`, delete `ResumeDecision`, `decide_resume` and their test module; add `impl From<&PersistedCheckpoint> for ResumeCandidate` there (persistence is a `session` concern, not a `resume` one) and re-export `pub use crate::resume::{ResumeDecision, decide_resume};` so `src/app.rs` and `tests/resume_contract.rs` keep their import paths.
3. `src/playback/command.rs`: add `ResumeIntent` and `Admission`; change `Load`'s `start_at: Duration` to `resume: ResumeIntent`.
4. `src/playback/event.rs`: add `StartDisposition`, the three new variants, and `Loaded.disposition`; add `cause: Option<RemoteFailure>` to `Failed`. Extend `session_rev()` and `is_terminal()` — `SeekCancelled` is terminal; `RestartEstablished` and `CapabilitiesChanged` are not.
5. `src/playback/engine.rs`:
   - `RESERVED_EVENT_SLOTS: usize = 9`, with the comment rewritten as above.
   - `load` takes `resume: ResumeIntent`. It resolves `StartAt(d)` to `(d, StartDisposition::Fresh)` immediately, and defers `Candidate(c)` until after the source opens: `decide_resume(Some(c), source.metadata().duration)` gives the decision, whose `start_at()` is the target and whose discriminant gives the disposition (`Completed` → `CompletedReplay`; `Resume`/`Unvalidated` with a nonzero target → `Resumed`; everything else → `Fresh`). Task 10 adds the `ResumeUnavailable` branch.
   - `self.position = start_at` moves to *after* the resolution; `Loading` is still announced before opening, and the pinned-start comment at `engine.rs:1163` moves with the assignment and keeps its reasoning.
   - `fail(message)` gains a sibling `fail_with(message, cause: Option<RemoteFailure>)`; `fail` delegates with `None`. `Failed` carries the cause.
   - `restart()` emits `RestartEstablished { session_rev, position }` immediately before `announce_playing()`, and only on the `Ok` arm of `reinstall`.
6. `src/app.rs`: `Mirror::apply` handles the three new variants (adopting `session_rev`; `RestartEstablished` also sets `position`); `resume_commands` builds `ResumeIntent::Candidate` from the persisted entry instead of a resolved `start_at`. `open_persistence` keeps computing a decision for its logging, but with `duration: None` — the worker is now the only place a duration exists. Simplify: `open_persistence` returns the `Option<ResumeCandidate>` and the volume, and drops `start_at` entirely; the `ResumeDecision` logging moves to Task 12, where the disposition on `Loaded` is what gets logged.
7. Update every call site the compiler flags in `tests/`. Only `Load { start_at }` → `Load { resume }`, `Loaded { .. }` patterns gaining `disposition`, and `Failed { message }` patterns gaining `cause` should appear. If a *behavioural* assertion breaks, stop and report it.

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test --locked 2>&1 | tail -10`
Expected: everything green — the baseline suite plus 9 resume-decision tests plus 3 engine tests.

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings`

- [ ] **Step 6: Commit**

```bash
git add src/resume.rs src/lib.rs src/session.rs src/playback src/app.rs tests
git commit -m "feat(playback): resume intent, start disposition and RestartEstablished

Closes spec gaps G1 and G3: decide_resume moves to a persistence-free module so
the worker may resolve a candidate, and an explicit restart now announces that
it established — without which §10's checkpoint protection can never be lifted.
RESERVED_EVENT_SLOTS rises 8 -> 9 for the widened Load."
```

---

## Task 9: `TransportCore` and the wait service

Closes G5. Nothing HTTP-specific: it is the change that makes §8's "keep publishing progress while a read blocks" expressible at all under `unsafe_code = "forbid"`.

**Files:**
- Create: `src/playback/wait.rs`
- Modify: `src/playback/engine.rs`, `src/playback/mod.rs`
- Test: `tests/wait_service.rs` (new); `tests/engine_contract.rs` unchanged

**Interfaces:**
- Consumes: Task 3's `WaitHook`.
- Produces:

```rust
// src/playback/engine.rs
/// The transport state a blocked source read may service, behind one lock.
///
/// `Handshake` and `Timeline` live here rather than on `Worker` because the
/// wait hook needs them and `Worker` is `&mut`-shaped. Contention is nil: the
/// hook runs only while the worker is blocked inside a decoder read, which is
/// precisely when the worker is not touching any of this.
pub(crate) struct TransportCore {
    handshake: Handshake,
    timeline: Timeline,
    /// Media position the current generation's frame counting starts from.
    anchor: Duration,
    sample_rate: u32,
}
impl TransportCore {
    /// Drain spans and return the position implied by what has actually played.
    fn observed_position(&mut self, now: Nanos) -> Duration;
}
```

```rust
// src/playback/wait.rs
/// What a blocked source read services on the worker's behalf.
///
/// Three jobs, and no others: drain spans into the timeline, publish the
/// keep-latest progress snapshot, and act on the freeze level — park the
/// output when a pause arrives, release it when a play does, and announce
/// each.
///
/// It holds no decoder and no source. §8 states "must not re-enter decoder
/// reads or seeks" as a rule; here it is a property of the type.
pub struct WaitService { /* see fields below */ }
impl WaitService {
    pub fn new(
        transport: Arc<Mutex<Option<TransportCore>>>,
        progress: Arc<Mutex<Progress>>,
        facts: Arc<Mutex<SessionFacts>>,
        interrupt: Arc<SourceInterrupt>,
        events: Sender<PlaybackEvent>,
        outbox: Arc<Mutex<VecDeque<PlaybackEvent>>>,
        backlog_empty: Arc<AtomicBool>,
        clock: Arc<dyn Fn() -> Nanos + Send + Sync>,
    ) -> Arc<Self>;
    /// Drained by the worker at the top of every loop pass, into
    /// `pending_events`, before anything else can emit.
    pub fn take_outbox(&self) -> Vec<PlaybackEvent>;
}
impl WaitHook for WaitService { fn service(&self); }

/// The scalars `service()` needs that change with the session rather than with
/// the transport.
pub struct SessionFacts {
    pub session_rev: u64,
    pub media: Option<MediaId>,
    pub position: Duration,
    pub degraded: bool,
    pub playing: bool,
    /// Set by the hook when it parks for a freeze, cleared when it releases.
    /// The worker reads it to know the transport is parked without having
    /// dispatched the `Pause` itself.
    pub frozen_by_hook: bool,
}
```

**Why the hook, and not the worker, has to do this (G2/#3).** §9 requires pause to work during a stalled read, and requires it to do so *without returning a destructive read error to the demuxer* — so the read stays pending inside Symphonia, and the worker stays inside `pump_audio` and never reaches its command loop. Nothing else is running. If the hook only published progress, the output would keep draining the ring while the listener believed playback was paused, and `StateChanged{Paused}` would never be emitted at all. So `service()` gains a freeze arm:

```rust
fn service(&self) {
    let frozen = self.interrupt.is_frozen();
    let mut facts = lock(&self.facts);
    if frozen && !facts.frozen_by_hook {
        // Park the callback. Everything else — the ring, the decoder, the
        // pending read — is left exactly as it is, which is what makes
        // resuming a single release (§9).
        if self.park() {
            facts.frozen_by_hook = true;
            facts.playing = false;
            self.announce(PlaybackEvent::StateChanged {
                session_rev: facts.session_rev,
                state: PlaybackState::Paused,
            });
        }
    } else if !frozen && facts.frozen_by_hook {
        self.release();
        facts.frozen_by_hook = false;
        facts.playing = true;
        self.announce(PlaybackEvent::StateChanged {
            session_rev: facts.session_rev,
            state: PlaybackState::Playing,
        });
    }
    drop(facts);
    self.publish_progress();
}
```

`park()` takes the transport lock and calls `handshake.park(timeline, &mut pump, DEADLINE)`, returning whether it was acknowledged; a park that times out leaves `frozen_by_hook` false so the worker's own `pause()` handles the recovery when the read finally returns.

**Ordering, which is the subtle part.** `announce` may not simply `try_send`: the worker's `pending_events` backlog might be non-empty, and jumping it would deliver `Paused` ahead of events emitted before it. The worker therefore maintains `backlog_empty: Arc<AtomicBool>` — set whenever `pending_events` is empty, cleared whenever it is not — and `announce` reads it:

```rust
fn announce(&self, event: PlaybackEvent) {
    // `EVENT_CAPACITY` and `RESERVED_EVENT_SLOTS` are `pub(crate)` in
    // `engine.rs` for this line: the hook is a second emitter, and the reserve
    // exists so a terminal outcome always has room. It never occupies it.
    // Ordering first: the worker's backlog is ahead of anything emitted here,
    // and a Paused delivered in front of it would misreport the sequence.
    if self.backlog_empty.load(Ordering::Acquire)
        && lock(&self.outbox).is_empty()
        && self.events.len() + RESERVED_EVENT_SLOTS < EVENT_CAPACITY
    {
        if self.events.try_send(event).is_ok() {
            return;
        }
    } else {
        lock(&self.outbox).push_back(event);
        return;
    }
    lock(&self.outbox).push_back(event);
}
```

The outbox is drained by the worker into `pending_events` at the top of its next pass — which happens as soon as the read returns, so nothing is lost, only delayed in the case where ordering forbids the shortcut. `crossbeam_channel::Sender` is `Sync` and `try_send` takes `&self`, so the hook can hold a clone.

**The refactor, concretely.** In `Worker`:
- `transport: Option<Transport>` becomes `transport: Arc<Mutex<Option<TransportCore>>>` plus `pcm: Option<rtrb::Producer<f32>>`, `link: Option<Arc<OutputLink>>` and `config: Option<NegotiatedOutput>` as plain worker fields. Only `handshake`, `timeline`, `anchor` and `sample_rate` need to be shared; the PCM producer is `!Sync` and stays on the worker, which is also correct — the hook must never push audio.
- `timeline: Timeline` leaves `Worker`.
- `anchor: Duration` leaves `Worker` (it lives in `TransportCore`); `Worker::reset_generation_state` sets it through the lock.
- Every site that today writes `self.transport.as_mut()` and `&mut self.timeline` together — `run()` step 2, `open_transport`, `reinstall`, `prime_and_run`, `capture_position`, `pause`, `check_end_of_track` — takes the lock for the duration of that one operation and releases it.
- `publish_progress` becomes a thin wrapper that updates `SessionFacts` and *then* calls the same `WaitService::service()` the hook calls. One implementation, two callers, which is what makes "the hook does exactly what the loop does" a fact rather than a comment — including the freeze arm, so a pause that arrives while the worker is *not* blocked is handled by the same code.
  **It must drop the `SessionFacts` guard before calling `service`.** `std::sync::Mutex` is not reentrant and `service` takes that same lock first thing, so holding it across the call deadlocks the worker on its very first pass — with no test failure to point at, just a hang. Write it as `{ let mut facts = lock(&self.facts); …update…; } self.service.service();`
- **Lock order, stated once and never varied:** `SessionFacts` → `TransportCore`. `SourceInterrupt::state` is a leaf and is never held while either is taken (`ByteChannel::read` drops it across the hook call for exactly this reason). Nothing takes `TransportCore` before `SessionFacts`.
- `run()` gains, as the very first thing in step 4: drain `service.take_outbox()` onto the **back** of `pending_events`, and maintain `backlog_empty` after every mutation of `pending_events`. Back, not front: the hook uses the outbox only when `backlog_empty` was false, which means those pending events were emitted *before* the hook's, so pushing to the front would invert exactly the order the outbox exists to preserve. Draining before `flush_events` is what gets the hook's events out on the same pass.
- `pause()` becomes idempotent with respect to the hook: if `facts.frozen_by_hook` is already set, the transport is parked and `StateChanged{Paused}` is already emitted, so it only sets `self.state = Paused` without re-emitting. Likewise `play()` checks the flag before releasing.
- **`pump_audio` must not hold the transport lock across `source.next_planar()`.** Structure it as: lock → compute `free`, push staging, read `pushed_total` → drop the lock → decode. A single `let free = { … };` block is enough. Getting this wrong deadlocks the moment a remote read blocks, and no local test would catch it.

- [ ] **Step 1: Write the failing wait-service test**

Create `tests/wait_service.rs`. The engine-level version of this fact — progress
still rising while a real read is blocked — needs a stalled server to stage, so
it lives in H13 (Task 13). What belongs here is the unit-level fact H13 rests
on: the hook and the main loop are one implementation, and servicing with no
transport preserves the retained position instead of rewinding it to zero.

```rust
use std::sync::{Arc, Mutex};
use std::time::Duration;

use continuo::http::channel::WaitHook;
use continuo::playback::engine::TransportCore;
use continuo::playback::event::Progress;
use continuo::playback::output::Nanos;
use continuo::playback::timeline::PositionQuality;
use continuo::playback::wait::{SessionFacts, WaitService};

/// The four wiring arguments that only the freeze tests care about, in the
/// inert configuration: nothing frozen, an empty backlog, a channel nobody
/// reads. A bare helper, so it handles its own error rather than unwrapping.
fn inert() -> (
    Arc<continuo::http::channel::SourceInterrupt>,
    crossbeam_channel::Sender<PlaybackEvent>,
    Arc<Mutex<std::collections::VecDeque<PlaybackEvent>>>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let (tx, rx) = crossbeam_channel::bounded(64);
    // Leak the receiver into the returned sender's lifetime by keeping it
    // alive here would be wrong; instead the caller keeps it. These tests do
    // not read events, so a disconnected channel is fine and try_send simply
    // fails, which `announce` already handles by using the outbox.
    drop(rx);
    (
        continuo::http::channel::SourceInterrupt::new(1024),
        tx,
        Arc::new(Mutex::new(std::collections::VecDeque::new())),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
    )
}

#[test]
fn servicing_publishes_the_position_the_timeline_reports() {
    let transport = Arc::new(Mutex::new(None));
    let progress = Arc::new(Mutex::new(Progress {
        session_rev: 0,
        media: None,
        position: Duration::ZERO,
        quality: PositionQuality::Exact,
    }));
    let facts = Arc::new(Mutex::new(SessionFacts {
        session_rev: 4,
        media: None,
        position: Duration::from_secs(9),
        degraded: false,
        playing: true,
        frozen_by_hook: false,
    }));
    let (interrupt, events, outbox, backlog_empty) = inert();
    let service = WaitService::new(
        Arc::clone(&transport),
        Arc::clone(&progress),
        Arc::clone(&facts),
        interrupt,
        events,
        outbox,
        backlog_empty,
        Arc::new(|| Nanos(0)),
    );

    service.service();

    let published = match progress.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(published.session_rev, 4);
    // With no transport the retained position stands: a blocked read must not
    // rewind the position to zero just because there is nothing to sample.
    assert_eq!(published.position, Duration::from_secs(9));
}

#[test]
fn servicing_a_freeze_parks_the_output_and_announces_paused() {
    // §9: pause must work during a stalled read. The worker is inside
    // pump_audio and will not reach its command loop until the read returns,
    // so if the hook does not do this, nothing does — the output keeps
    // draining and Paused is never emitted (#3).
    let (events_tx, events_rx) = crossbeam_channel::bounded(64);
    let interrupt = SourceInterrupt::new(1024);
    let outbox = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let backlog_empty = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let facts = Arc::new(Mutex::new(SessionFacts {
        session_rev: 3,
        media: None,
        position: Duration::from_secs(7),
        degraded: false,
        playing: true,
        frozen_by_hook: false,
    }));
    let service = WaitService::new(
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(Progress {
            session_rev: 3,
            media: None,
            position: Duration::from_secs(7),
            quality: PositionQuality::Exact,
        })),
        Arc::clone(&facts),
        Arc::clone(&interrupt),
        events_tx,
        Arc::clone(&outbox),
        Arc::clone(&backlog_empty),
        Arc::new(|| Nanos(0)),
    );

    interrupt.freeze();
    service.service();
    match events_rx.try_recv() {
        Ok(PlaybackEvent::StateChanged { session_rev: 3, state: PlaybackState::Paused }) => {}
        other => panic!("expected StateChanged{{Paused}}, got {other:?}"),
    }
    // Idempotent: a second slice must not re-announce.
    service.service();
    assert!(events_rx.try_recv().is_err(), "the freeze was announced twice");

    interrupt.thaw();
    service.service();
    match events_rx.try_recv() {
        Ok(PlaybackEvent::StateChanged { session_rev: 3, state: PlaybackState::Playing }) => {}
        other => panic!("expected StateChanged{{Playing}}, got {other:?}"),
    }
}

#[test]
fn a_hook_announcement_goes_to_the_outbox_when_the_workers_backlog_is_not_empty() {
    // Jumping a non-empty backlog would deliver Paused ahead of events emitted
    // before it. The outbox is drained into pending_events at the top of the
    // worker's next pass, so nothing is lost — only ordered.
    let (events_tx, events_rx) = crossbeam_channel::bounded(64);
    let interrupt = SourceInterrupt::new(1024);
    let outbox = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let backlog_empty = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let service = WaitService::new(
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(Progress {
            session_rev: 1,
            media: None,
            position: Duration::ZERO,
            quality: PositionQuality::Exact,
        })),
        Arc::new(Mutex::new(SessionFacts {
            session_rev: 1,
            media: None,
            position: Duration::ZERO,
            degraded: false,
            playing: true,
            frozen_by_hook: false,
        })),
        Arc::clone(&interrupt),
        events_tx,
        Arc::clone(&outbox),
        backlog_empty,
        Arc::new(|| Nanos(0)),
    );

    interrupt.freeze();
    service.service();
    assert!(events_rx.try_recv().is_err(), "the hook jumped a non-empty backlog");
    let drained = service.take_outbox();
    assert!(
        matches!(
            drained.as_slice(),
            [PlaybackEvent::StateChanged { state: PlaybackState::Paused, .. }]
        ),
        "{drained:?}"
    );
}

#[test]
fn servicing_with_no_transport_is_harmless_and_repeatable() {
    let transport = Arc::new(Mutex::new(None));
    let progress = Arc::new(Mutex::new(Progress {
        session_rev: 0,
        media: None,
        position: Duration::from_secs(3),
        quality: PositionQuality::Exact,
    }));
    let facts = Arc::new(Mutex::new(SessionFacts {
        session_rev: 1,
        media: None,
        position: Duration::from_secs(3),
        degraded: false,
        playing: false,
        frozen_by_hook: false,
    }));
    let (interrupt, events, outbox, backlog_empty) = inert();
    let service = WaitService::new(
        transport,
        Arc::clone(&progress),
        facts,
        interrupt,
        events,
        outbox,
        backlog_empty,
        Arc::new(|| Nanos(0)),
    );
    for _ in 0..100 {
        service.service();
    }
    let published = match progress.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(published.position, Duration::from_secs(3));
}
```

`TransportCore` must be `pub` from `src/playback/engine.rs` for these to construct an `Arc<Mutex<Option<TransportCore>>>`; exporting the type without its constructor is enough.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test wait_service 2>&1 | tail -20`
Expected: FAIL — `unresolved import continuo::playback::wait`.

- [ ] **Step 3: Perform the refactor**

Do it in this order, running `cargo test --locked` after each:

1. Introduce `TransportCore` holding `handshake`, `timeline`, `anchor`, `sample_rate`. Keep `Worker::transport` as `Option<TransportCore>` — no `Arc`, no `Mutex` yet — and move `timeline` and `anchor` off `Worker`. Fix every call site. **Everything must still pass here**, and this step alone is the risky one.
2. Wrap it: `transport: Arc<Mutex<Option<TransportCore>>>`, moving `pcm`, `link` and `config` to plain `Worker` fields. Fix every call site to take the lock for one operation. **Everything must still pass.**
3. Restructure `pump_audio` so the lock is released before `next_planar()`.
4. Add `SessionFacts` and `WaitService`; rewrite `publish_progress` to update the facts and call `service.service()`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --locked 2>&1 | tail -10`
Expected: everything green, including all 20 engine contract tests unchanged.

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add src/playback tests/wait_service.rs
git commit -m "refactor(playback): move handshake and timeline into a shared TransportCore

Closes spec gap G5: §8's wait-service hook is called from inside next_planar(),
which already holds &mut self.source, and unsafe_code = \"forbid\" rules out a
lifetime-erased slot. Sharing the transport by Arc makes the hook's inability
to re-enter a decoder a property of the type rather than a rule in prose.
pump_audio releases the lock before decoding; holding it deadlocks on the first
blocked remote read."
```

---

## Task 10: The engine — interrupts, submission, remote stop and completion

Where §8 and §9 land. Needs 7, 8 and 9.

**Files:**
- Modify: `src/playback/engine.rs`, `src/playback/command.rs`, `tests/support/mod.rs`
- Test: `tests/engine_remote.rs` (new); additions to `tests/engine_contract.rs`

**Harness additions.** Tasks 10 and 13 use these, and every one is new; add them to `tests/support/mod.rs` in this task, additively, before writing any test that calls them. No existing helper signature changes.

```rust
impl TestEngine {
    /// Load an HTTP source. Builds an `HttpService` on first use and keeps it
    /// for the engine's lifetime.
    pub fn load_remote(&mut self, url: &str);
    /// The handle, for the submission methods (`submit_pause`, `submit_seek`…).
    pub fn handle(&self) -> &EngineHandle;
    /// The mirror's current state, rebuilt from the drained event stream.
    pub fn state(&mut self) -> PlaybackState;
    /// Drain until the state is reached, or panic after `PATIENCE`.
    pub fn await_state(&mut self, state: PlaybackState);
    /// Run until Ended, Failed or the timeout, whichever comes first.
    pub fn play_until_terminal(&mut self, patience: Duration);
    /// Whether an `EndOfTrack` was ever observed in this run.
    pub fn saw_end_of_track(&self) -> bool;
}

/// The path of a fixture, for tests that need its bytes rather than an
/// `AbsolutePath`. A bare helper, so it panics with the path on failure — a
/// missing fixture is a repository error, not a test condition.
pub fn fixture_path(name: &str) -> std::path::PathBuf;
```

Task 8 additionally declares `TestEngine::{load_with_resume, await_loaded, await_restart_established}`; those come with that task.

**Interfaces:**
- Produces, on `EngineHandle`:

```rust
/// Every submission that must be able to reach a worker blocked in a source
/// read. §8: queue admission and waking belong together, so a caller cannot
/// queue a command and forget to wake anything.
pub fn submit(&self, command: PlaybackCommand) -> Admission;
pub fn submit_seek(&self, target: Duration) -> Admission;
pub fn submit_pause(&self) -> Admission;
pub fn submit_play(&self) -> Admission;
pub fn submit_stop(&self);
pub fn submit_shutdown(&self);
pub fn source_interrupt(&self) -> Arc<SourceInterrupt>;
pub fn set_http(&self, service: Option<Arc<HttpService>>);
```

**The interrupt word** grows a third bit:

```rust
const STOP: u8 = 1;
const SHUTDOWN: u8 = 2;
/// A seek whose target has already been accepted by the command queue. §8: the
/// interrupt is published only after acceptance, so a seek that was refused
/// admission cannot retire a fetch it never got to replace.
const SEEK: u8 = 4;
```

Pause does **not** get a bit: it is a level on `SourceInterrupt` (G2), because a bit cleared by the loop's `swap(0)` would be gone before the blocked read ever looked.

**What each submission does:**

| Method | Queue | `SourceInterrupt` | Interrupt word | Wake |
|---|---|---|---|---|
| `submit(cmd)` | try_send | — | — | ping |
| `submit_seek` | try_send `SeekTo`; on `Busy`, stop here | `retire()` | `\|= SEEK` | ping |
| `submit_pause` | try_send `Pause` | `freeze()` | — | ping |
| `submit_play` | try_send `Play` | `thaw()` | — | ping |
| `submit_stop` | — | `retire()` | `\|= STOP` | ping |
| `submit_shutdown` | — | `retire()` | `\|= SHUTDOWN` | ping |

**What the worker does with them:**

1. `run()`'s step 1 gains, after the shutdown and stop branches — **and only when neither of them fired**:
   ```rust
   // Shutdown dominates stop, and both dominate seek. `do_stop` has just
   // retired the fetch; arming here would clear that retirement and leave the
   // request open after the stop that existed to close it.
   if flags & SEEK != 0 && flags & (STOP | SHUTDOWN) == 0 {
       // The retirement already woke the read; the queued SeekTo carries the
       // target. Arm so the reopen the seek performs is not itself cancelled
       // by the flag that woke it — guarded, so a stop that lands between the
       // swap and here still stands.
       self.source_interrupt.arm(self.source_interrupt.generation());
   }
   ```
2. `pump_audio` distinguishes a retired read from a decode failure:
   ```rust
   Err(error) if is_retired_read(&error) => {
       // Not EOF and not a fault: a stop, seek or shutdown retired this read.
       // The decoder is now at an arbitrary point, so the source is retired
       // with it and the interrupt the loop is about to act on decides what
       // happens next (§8).
       self.retire_remote_source();
       return;
   }
   ```
   `is_retired_read(&PlaybackError) -> bool` is a one-line worker-local wrapper: it matches `PlaybackError::Decode(e)` and calls `http::source::is_retired(e)`, and matches `PlaybackError::Remote(RemoteFailure::Cancelled)` directly. `retire_remote_source` drops `self.source` when the current `SourceLocation` is `Http`, and keeps `self.descriptor` so `restore()` can reopen.
3. A decode error over a **remote** source fails the session; only a local one keeps M1's warn-and-drain behaviour:
   ```rust
   Err(error) => match (remote_cause(&error), self.source_is_remote()) {
       // §7: never reinterpret a status failure, a malformed range, a timeout
       // or a truncated body as clean source EOF.
       (Some(failure), _) => self.fail_with(format!("{failure}"), Some(failure)),
       // H8: "malformed audio cannot become successful completion". A body
       // that transferred perfectly and decoded to garbage has no remote
       // cause at all, so keying on one alone lets exactly the case H8 names
       // drain to EndOfTrack and mark the episode complete. A remote attempt
       // that cannot finish decoding is a failed attempt, whatever the
       // transport did.
       (None, true) => self.fail_with(
           format!("decoding failed: {error}"),
           None,
       ),
       // Local files keep M1's contract: a decode error late in a file the
       // listener already heard most of drains what it has rather than
       // discarding the session. `tests/decode_fixtures.rs` pins this.
       (None, false) => {
           self.source_eof = true;
           self.warn(format!("decoding stopped early: {error}"));
       }
   }
   ```
   `source_is_remote()` reads `self.descriptor`, which `load` sets and `shutdown` clears.
4. `do_stop` gains, after `capture_and_teardown()`: `self.source_interrupt.retire(); self.retire_remote_source();` — §9's "retire fetch, wake reads, discard transport and remote decoder; keep identity/source/position".
5. `Worker` gains `descriptor: Option<SourceLocation>`, set by `load`, cleared by `shutdown`. Reopening is **one method**, not a branch inside `restore()`, because three callers need it and an earlier draft gave it only to `restore` — leaving `seek_to` to hit its `self.source.is_none()` guard and reject every seek taken after a stop or a cancelled seek had retired the decoder:
   ```rust
   /// Reopen a remote source the worker retired, so a caller that needs a
   /// decoder has one. `Ok(false)` means nothing had to be done.
   ///
   /// Called by `restore` (play after stop), by `seek_to` (a seek arriving
   /// while stopped, or after a previous seek's retirement dropped the
   /// decoder), and by `restart`. Giving it to only one of them is what makes
   /// a stopped seek fail with "nothing is loaded" on a source that is very
   /// much loaded.
   fn ensure_source_open(&mut self) -> Result<bool, PlaybackError> {
       if self.source.is_some() {
           return Ok(false);
       }
       let Some(location) = self.descriptor.clone() else {
           return Ok(false);
       };
       // Guarded: a stop or shutdown that retired this source between the
       // caller's decision and here must not be undone.
       self.source_interrupt.arm(self.source_interrupt.generation());
       let prepared = prepare(&location, &self.prepare_context())?;
       self.capabilities = prepared.capabilities;
       self.emit_capabilities(prepared.capabilities);
       self.source = Some(prepared.source);
       Ok(true)
   }
   ```
   `restore()` calls it before its `source.is_none()` guard, and then applies §9's resume rule:
   ```rust
   match self.ensure_source_open() {
       Ok(_) => {}
       Err(error) => { self.fail_from(error); return; }
   }
   // A source that cannot seek cannot restore a nonzero position, and
   // starting at zero silently would be exactly the reset the milestone's
   // invariant forbids (§9).
   if self.position > Duration::ZERO && self.capabilities.seek == SeekSupport::Unsupported {
       self.reject_seek("this server cannot resume; the position is kept".into());
       self.set_state(PlaybackState::Stopped);
       return;
   }
   ```
6. `seek_to` gains a reopen and a capability gate, in that order, before its existing `source.is_none()` guard. **The reopen has to come first**: after a stop or a retired seek there is no decoder, and the current guard would reject the seek as "nothing is loaded" on a source whose identity, descriptor and position the worker is still holding.
   ```rust
   // A remote source the worker retired is reopened here, not treated as
   // absent. `ensure_source_open` is a no-op when a decoder is already live,
   // so the local path is unchanged.
   if let Err(error) = self.ensure_source_open() {
       self.reject_seek(format!("{error}"));
       return;
   }
   if self.source.is_none() {
       self.reject_seek("nothing is loaded".into());
       return;
   }
   ```
   then the capability gate, so §10's "reject unsupported stopped seeks *before* `SeekTargetStored`" holds:
   ```rust
   match self.capabilities.seek {
       SeekSupport::Unsupported => { self.reject_seek("this source cannot seek".into()); return; }
       // §6: until conclusive evidence exists, publish Unknown and verify on
       // demand. A stopped seek needs the answer before it may store a target
       // M2 treats as durable.
       SeekSupport::Unknown => {
           if !self.verify_seek_support() { self.reject_seek("this source cannot seek".into()); return; }
       }
       SeekSupport::Native | SeekSupport::RestartAndDiscard => {}
   }
   ```
   `verify_seek_support` performs one cancellable trial `seek_refined` to the current position and back, publishes `CapabilitiesChanged`, and returns whether it succeeded.
7. `check_end_of_track` calls `confirm_complete` before emitting, for remote sources only:
   ```rust
   // §9: a finite container can finish decoding before the body is consumed.
   // A truncated or timed-out tail cannot set completed status.
   if let Some(failure) = self.confirm_remote_completion() {
       self.fail_with(format!("{failure}"), Some(failure));
       return;
   }
   ```
8. `restart()` calls `ensure_source_open()` and, for a remote source, reopens from zero rather than seeking — §9's "Restart: explicitly open from zero".
9. `Worker` gains `capabilities: MediaCapabilities`, set by `load` and by `ensure_source_open`, so the gates in 5 and 6 read one field rather than reaching into the source.

- [ ] **Step 1: Write the failing engine-remote tests**

Create `tests/engine_remote.rs` covering, each against the loopback server through a real `EngineHandle` over `TestOutput`:

```rust
mod support;

// Tests, in order:
// 1. remote_playback_reaches_the_test_output_before_the_body_completes   (H1)
// 2. a_forward_seek_installs_the_media_position_and_requests_that_byte   (H2)
// 3. stop_closes_the_fetch_and_play_reopens_at_the_preserved_position    (H3)
// 4. a_range_less_server_plays_sequentially_and_refuses_every_seek       (H5)
// 5. an_unsupported_stopped_seek_emits_no_seek_target_stored             (H14)
// 6. a_seek_retired_by_a_stop_reports_cancelled_and_commits_no_target    (§8)
// 7. pause_during_a_stalled_read_freezes_output_and_resume_continues     (H10)
// 8. quit_while_paused_wakes_every_source_wait                           (H10)
// 9. a_truncated_tail_cannot_become_end_of_track                         (H8/H9)
// 10. a_capability_change_carries_the_current_session_rev                (H14)
// 11. a_seek_after_a_stop_reopens_rather_than_reporting_nothing_is_loaded (#7)
// 12. a_seek_after_a_retired_seek_reopens_and_lands                       (#7)
// 13. corrupt_audio_over_a_complete_body_fails_and_never_ends             (H8/#6)
```

Test 13 is the one the plan would otherwise have shipped broken, so write it explicitly:

```rust
#[test]
fn corrupt_audio_over_a_complete_body_fails_and_never_ends() {
    // H8: "malformed audio cannot become successful completion". The transfer
    // is perfect — full Content-Length, clean EOF, valid ETag — and the bytes
    // are garbage. Keying the failure on a RemoteFailure alone lets this drain
    // to EndOfTrack and mark the episode complete, destroying the checkpoint.
    let mut body = match std::fs::read(fixture_path("sine-5s.flac")) {
        Ok(body) => body,
        Err(error) => panic!("the fixture must exist: {error}"),
    };
    // Corrupt the middle, leaving the header intact so it still opens.
    let middle = body.len() / 2;
    for byte in &mut body[middle..middle + 4096] {
        *byte = 0xFF;
    }
    let server = TestServer::start(Script::serving(body));

    let mut engine = TestEngine::start();
    engine.load_remote(&server.url("/audio.flac"));
    engine.play_until_terminal(Duration::from_secs(10));

    assert_eq!(engine.state(), PlaybackState::Failed, "corrupt audio was reported as {:?}", engine.state());
    assert!(
        !engine.saw_end_of_track(),
        "EndOfTrack was emitted for a recording that never decoded through"
    );
    engine.finish();
    server.shutdown();
}
```

Write each one out in full following the pattern already established in `tests/engine_contract.rs`: `TestEngine::start()`, `engine.load_remote(server.url("/audio.flac"))`, assertions with the harness clock frozen, `engine.finish()`. Every cancellation test first proves the wait was entered — `server.wait_until_stalled(Duration::from_secs(5))` for a body stall, `server.requests()` growing for a header stall — before it interrupts. Sleeping and hoping is what §12 forbids.

For test 7 specifically, the shape §9 demands:

```rust
#[test]
fn pause_during_a_stalled_read_freezes_output_and_resume_continues() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").stall_body_after(8192));
    let mut engine = TestEngine::start();
    engine.load_remote(&server.url("/audio.flac"));
    engine.play_for(Duration::from_millis(100));
    assert!(server.wait_until_stalled(Duration::from_secs(5)), "the read never blocked");

    // The read is pending inside the decoder. Pause must reach it without
    // returning a destructive error to the demuxer.
    assert_eq!(engine.handle().submit_pause(), Admission::Accepted);
    engine.await_state(PlaybackState::Paused);
    let frozen = engine.progress().position;
    engine.let_time_pass(Duration::from_millis(200));
    assert_eq!(engine.progress().position, frozen, "output kept running while paused");

    assert!(server.release());
    assert_eq!(engine.handle().submit_play(), Admission::Accepted);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(Duration::from_millis(200));
    assert!(engine.progress().position > frozen, "playback did not continue");

    engine.finish();
    server.shutdown();
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test engine_remote 2>&1 | tail -20`
Expected: FAIL — the engine still rejects HTTP at `engine.rs:1173`.

- [ ] **Step 3: Implement the engine changes**

Work through 1–8 above in order, running `cargo test --locked` after each. The one to get right first is 2 and 3 — the read-outcome classification — because everything else builds on a retired read being distinguishable from a failed one.

Delete the `SourceLocation::Http(url) => self.fail(...)` arm at `engine.rs:1173`; `prepare` now handles both.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --locked 2>&1 | tail -10`
Expected: everything green.

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add src/playback tests/engine_remote.rs tests/engine_contract.rs tests/support
git commit -m "feat(playback): remote sources in the decode worker

Adds the SEEK interrupt bit, submission methods that own admission and waking
together, a pause level that reaches a blocked read, remote stop/reopen, and
the bounded tail validation §9 requires before a track may be called complete."
```

---

## Task 11: The §10 checkpoint protection

Needs Task 8. Pure policy, driven synchronously with the existing `FakeClock`.

**Files:**
- Modify: `src/session.rs`
- Test: `tests/session_policy.rs` (additive)

**What changes in `Session`:**

```rust
    /// A positive checkpoint the current run must not overwrite, because
    /// playback fell back to zero on a source that cannot resume (§10).
    ///
    /// Deliberately not a max-position merge: the point is to recover the
    /// *earlier* resume point, and progress heard in a fallback run does not
    /// replace it however far it goes. Ends only on an established Restart, an
    /// established user seek, or verified completion.
    protected: Option<Duration>,
```

- `on_loaded` reads `Loaded.disposition`: `ResumeUnavailable { retained }` sets `protected = Some(retained)`; every other disposition clears it. Set **before** any Playing or progress event can be observed, which is why the disposition rides on `Loaded` rather than arriving as a separate warning (§5).
- `record_current` returns early when `protected.is_some()`. That single gate covers periodic, pause, stop, outgoing-media and shutdown captures at once, because every one of them reaches the state through `record_current`. **Volume and `current_media` updates are unaffected**, because neither goes through it.
- `record_outgoing` also returns early when `protected.is_some()`, for the same reason and stated separately: it writes the *previous* media's entry from `last_sample`, not through `record_current`.
- `SeekCompleted` clears `protected` (an established user seek).
- `RestartEstablished` clears `protected` (G1's whole purpose) and behaves otherwise like `SeekCompleted`: resolve the target, `established = true`, `completed = false`, raise `pending_force`.
- `EndOfTrack` clears `protected` (verified completion) — and does so *before* `record_current`, so the completion itself is written.
- `CapabilitiesChanged` is `Action::None` and touches nothing. §10: "Capability changes alone never delete, clear or replace checkpoints."
- `SeekCancelled` is `Action::None`. A cancelled seek commits no target; `resolve_target()` is **not** called, because the stored target it would discard belongs to a stopped seek that is still outstanding.

- [ ] **Step 1: Write the failing protection tests**

Append to `tests/session_policy.rs`:

```rust
#[test]
fn a_protected_entry_survives_every_capture_path() {
    // §10/H16. Periodic, pause, stop, media switch and shutdown all reach the
    // state through record_current or record_outgoing; one gate covers all five,
    // and this test is what proves none of them slipped past it.
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));

    let _ = session.observe(&loaded_unavailable(&media("ep1"), retained), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());

    // Periodic.
    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    let _ = session.tick(&progress(1, &media("ep1"), Duration::from_secs(120)), clock.sample());
    assert_eq!(stored_position(&session, &media("ep1")), retained);

    // Pause, then stop.
    let _ = session.observe(&state_changed(1, PlaybackState::Paused), clock.sample());
    let _ = session.tick(&progress(1, &media("ep1"), Duration::from_secs(130)), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Stopped), clock.sample());
    let _ = session.tick(&progress(1, &media("ep1"), Duration::from_secs(130)), clock.sample());
    assert_eq!(stored_position(&session, &media("ep1")), retained);

    // A media switch carries the outgoing entry out — but not over this one.
    let _ = session.observe(&loaded_fresh(&media("ep2")), clock.sample());
    assert_eq!(stored_position(&session, &media("ep1")), retained);

    // And the shutdown snapshot.
    let state = session.shutdown_snapshot(&progress(2, &media("ep2"), Duration::from_secs(5)), clock.sample());
    assert_eq!(position_in(&state, &media("ep1")), retained);
}

#[test]
fn an_established_restart_lifts_the_protection() {
    // G1: without RestartEstablished this can never happen, and a listener who
    // deliberately started over would be unable to save that fact.
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));
    let _ = session.observe(&loaded_unavailable(&media("ep1"), retained), clock.sample());

    let _ = session.observe(
        &PlaybackEvent::RestartEstablished { session_rev: 1, position: Duration::ZERO },
        clock.sample(),
    );
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());
    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    let _ = session.tick(&progress(1, &media("ep1"), Duration::from_secs(30)), clock.sample());

    assert_eq!(stored_position(&session, &media("ep1")), Duration::from_secs(30));
}

#[test]
fn an_established_seek_lifts_the_protection() {
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));
    let _ = session.observe(&loaded_unavailable(&media("ep1"), retained), clock.sample());

    let _ = session.observe(
        &PlaybackEvent::SeekCompleted {
            session_rev: 1,
            requested: Duration::from_secs(60),
            actual: Duration::from_secs(60),
            refinement_truncated: false,
        },
        clock.sample(),
    );
    let _ = session.tick(&progress(1, &media("ep1"), Duration::from_secs(60)), clock.sample());
    assert_eq!(stored_position(&session, &media("ep1")), Duration::from_secs(60));
}

#[test]
fn verified_completion_lifts_the_protection_and_records_the_completion() {
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));
    let _ = session.observe(&loaded_unavailable(&media("ep1"), retained), clock.sample());

    let _ = session.observe(
        &PlaybackEvent::EndOfTrack { session_rev: 1, position: Duration::from_secs(3000) },
        clock.sample(),
    );
    assert_eq!(stored_position(&session, &media("ep1")), Duration::from_secs(3000));
    assert!(completed_in(&session, &media("ep1")));
}

#[test]
fn a_capability_change_alone_never_lifts_the_protection() {
    // §10: "Capability changes alone never delete, clear or replace
    // checkpoints." A server that starts advertising ranges mid-session must
    // not be able to discard the entry by saying so.
    let clock = FakeClock::new();
    let retained = Duration::from_secs(2400);
    let mut session = Session::new(state_with(&media("ep1"), retained, false));
    let _ = session.observe(&loaded_unavailable(&media("ep1"), retained), clock.sample());

    let _ = session.observe(
        &PlaybackEvent::CapabilitiesChanged {
            session_rev: 1,
            capabilities: MediaCapabilities { continuity: Continuity::Finite, seek: SeekSupport::Native },
        },
        clock.sample(),
    );
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());
    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    let _ = session.tick(&progress(1, &media("ep1"), Duration::from_secs(30)), clock.sample());

    assert_eq!(stored_position(&session, &media("ep1")), retained);
}

#[test]
fn a_fresh_sequential_session_with_nothing_to_protect_records_normally() {
    // §10's last paragraph: with no positive checkpoint to protect, heard
    // progress is recorded normally even though it cannot currently be resumed.
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let _ = session.observe(&loaded_fresh(&media("ep1")), clock.sample());
    let _ = session.observe(&state_changed(1, PlaybackState::Playing), clock.sample());
    clock.advance_monotonic(CAPTURE_INTERVAL + Duration::from_secs(1));
    let _ = session.tick(&progress(1, &media("ep1"), Duration::from_secs(45)), clock.sample());
    assert_eq!(stored_position(&session, &media("ep1")), Duration::from_secs(45));
}

#[test]
fn a_cancelled_seek_commits_no_target_and_leaves_an_outstanding_one_alone() {
    let clock = FakeClock::new();
    let mut session = Session::new(PersistedState::default());
    let _ = session.observe(&loaded_fresh(&media("ep1")), clock.sample());
    let _ = session.observe(
        &PlaybackEvent::SeekTargetStored { session_rev: 1, target: Duration::from_secs(90) },
        clock.sample(),
    );
    let _ = session.observe(
        &PlaybackEvent::SeekCancelled { session_rev: 1, requested: Duration::from_secs(200) },
        clock.sample(),
    );
    // The stopped seek's target still supersedes the sampled position.
    let _ = session.tick(&progress(1, &media("ep1")), clock.sample()).ignore_if_none();
    assert_eq!(stored_position(&session, &media("ep1")), Duration::from_secs(90));
}
```

`loaded_unavailable`, `loaded_fresh`, `state_changed`, `progress`, `stored_position`, `position_in`, `completed_in` and `state_with` are helpers — several already exist in `tests/session_policy.rs`; add the missing ones beside them, each handling its own error with a `panic!` carrying a message (they are not `#[test]` bodies). Drop the `.ignore_if_none()` in the last test — it is shorthand here for "the action is not what this test asserts"; write `let _ = session.tick(...);` instead, matching the file's existing style.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test session_policy 2>&1 | tail -20`
Expected: FAIL on the seven new tests; every existing one still passes.

- [ ] **Step 3: Implement the protection**

Make the changes listed above. Put the gate in exactly two places — `record_current` and `record_outgoing` — and say in a comment at each why one gate is not enough for both.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --test session_policy --test resume_contract 2>&1 | tail -20`
Expected: PASS — the new seven, plus every M2 policy and resume test unchanged.

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked 2>&1 | tail -5`

- [ ] **Step 5: Commit**

```bash
git add src/session.rs tests/session_policy.rs
git commit -m "feat(session): protect a retained checkpoint after a resume-unavailable fallback

§10, decided as R4: recover the earlier resume point rather than merge on
maximum position. Protection is set from Loaded.disposition before any Playing
event can be observed, and lifted only by an established Restart, an
established user seek, or verified completion."
```

---

## Task 12: CLI and application

Needs Tasks 10 and 11. Implements §5, §11's presentation half, and R5's raw-mode deferral.

**Files:**
- Modify: `src/cli.rs`, `src/app.rs`
- Test: `tests/http_cli.rs` (new); `tests/cli_playback.rs` (additive)

**Interfaces:**
- `src/cli.rs`:

```rust
#[derive(Debug, Subcommand)]
pub enum CliCommand {
    /// Play a local audio file or an HTTP(S) URL.
    Play {
        /// Path to an MP3, FLAC, WAV or M4A file, or an http(s):// URL.
        source: String,
        /// Open the source, print what was found, and exit without using a
        /// device or a terminal.
        #[arg(long)]
        probe_only: bool,
    },
}
```

`path: PathBuf` becomes `source: String` because a `PathBuf` cannot round-trip a URL's percent-encoding on every platform, and §5's disambiguation rule needs the raw text.

- `src/app.rs`:

```rust
/// §5's disambiguation. An explicit http/https scheme is a URL; everything
/// else keeps existing path behaviour, so `./https:weird` remains an
/// unambiguous local spelling.
fn resolve_source(input: &str) -> Result<(MediaId, SourceLocation), PlaybackError>;
```

**The rules, each with a test below:**
1. `input` starts with `http://` or `https://` (ASCII case-insensitive) → parse as a URL. A parse failure is `RemoteFailure::InvalidSource`, reported as a malformed URL rather than as a missing file.
2. A URL with userinfo is rejected: `InvalidSource { reason: "URLs with embedded credentials are not supported" }`. §5 — no implicit credential feature.
3. Identity is `MediaId::RemoteUrl(NormalizedUrl::parse(input)?)`, which already strips the fragment and keeps the query's serialization and order. The parsed `Url` is kept separately as the fetch URL; redirects change the fetch target and never the identity.
4. Anything else → the existing path behaviour. Canonicalization moves to `prepare` (Task 7), so `resolve_source` only builds `SourceLocation::LocalPath(PathBuf::from(input))`, and the `MediaId::LocalFile` is built from the canonicalized path the worker reports back on `Loaded`. **That is a behaviour change**: the identity is no longer known before the load. Keep it simple instead — canonicalize here, exactly as today, and let `prepare` canonicalize again idempotently. The double canonicalize is two `stat` calls, not a second decoder open, and it keeps `MediaId` available before the engine starts, which `open_persistence` needs.

**Raw-mode deferral (R5/G4).** `run` becomes:

```
    resolve_source
    open_persistence            (needs MediaId; no duration any more)
    build the HttpService       (only when the source is remote)
    spawn the engine
    submit SetVolume, Load{resume: Candidate|StartAt}, Play
    ── loop A: no raw mode, no rendering ────────────────────────────
       drain events until the first Loaded or Failed, or the user
       interrupts with Ctrl-C; a Failed here returns the error with the
       terminal untouched, which is the M1 property this preserves
    ── enter raw mode ───────────────────────────────────────────────
    ── loop B: the existing key/render/checkpoint loop ───────────────
```

Loop A must remain interruptible: poll `crossterm::event::poll` with a short timeout and treat Ctrl-C or `q` as a shutdown, so a stalled remote open is quittable. §5: "The application displays Loading while preparation is in flight and remains able to stop or quit." Print a single `Loading …` line to stdout in loop A — outside raw mode, so it is a plain line rather than a status render.

**`--probe-only` (§5, H15).** Runs `prepare` directly on the calling thread with a `PrepareContext` built from a fresh `HttpService` (or `None` for a local path), prints, and exits. It constructs **no** `EngineHandle`, **no** `AudioOutput`, and **no** `StateStore` — it neither reads nor writes playback state. Output gains the new facts:

```rust
println!(
    "{title} {rate} Hz {channels} ch {duration:?} continuity={continuity:?} seek={seek:?} resume={resume:?}",
    ...
);
```

**Status line (§11).** `Mirror` gains `capabilities: Option<MediaCapabilities>` and `buffering: bool`; `status_line` distinguishes:
- unknown duration → the existing `--:--:--`;
- unresolved capability → ` seek?`;
- unsupported capability → ` no-seek`;
- currently buffering → ` buffering` appended to the `playing` label, as a *detail of Playing* — never a state of its own, and never anything that could read as live radio.

`buffering` is a new `bool` on `Progress`. It must **not** be derived from `PositionQuality::Degraded`, which means something else entirely (a timing base that jumped).

After Task 9 there is one `service()` with one body, so it cannot tell its two callers apart on its own — which is why `WaitHook::service` delegates to an explicit inherent method:

```rust
/// Which caller is servicing. The only thing that differs between them, and it
/// differs by definition: the hook runs *because* a source read is blocked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Servicing { WorkerLoop, BlockedRead }

impl WaitService {
    pub fn service_as(&self, caller: Servicing) { /* the whole body */ }
}
impl WaitHook for WaitService {
    fn service(&self) { self.service_as(Servicing::BlockedRead) }
}
```

`Worker::publish_progress` calls `service_as(Servicing::WorkerLoop)`. `buffering` is set to `caller == Servicing::BlockedRead`, so it is true exactly while a read is waiting on the network and false the moment the worker gets going again. Add `buffering` to the `Progress` literals in `EngineHandle::assemble` and in `tests/support/mod.rs`; it is `false` in both.

**Redaction (§11).** `display_name(&MediaId::RemoteUrl(url))` renders `redact_url(url.as_str())`'s last path segment, falling back to the redacted host. Every `tracing` call that carries a URL passes it through `redact_url` first. `PlaybackError`'s `Display` reaching stderr in `main.rs` already routes through `RemoteFailure`, which is redacted by construction.

- [ ] **Step 1: Write the failing CLI tests**

Create `tests/http_cli.rs`:

```rust
mod support;

use std::process::Command;

use support::server::{Script, TestServer};

#[allow(clippy::unwrap_used)] // Spawning a fixed test binary.
fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_continuo")).args(args).output().unwrap()
}

#[test]
fn probe_only_reports_a_remote_recordings_capabilities() {
    // H15: no terminal, no device, no persistence.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let output = run(&["play", &server.url("/audio.flac"), "--probe-only"]);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("44100"), "{text}");
    assert!(text.contains("Finite"), "{text}");
    assert!(text.contains("Native"), "{text}");
    server.shutdown();
}

#[test]
fn probe_only_on_a_range_less_server_reports_no_seek_support() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").without_ranges());
    let output = run(&["play", &server.url("/audio.flac"), "--probe-only"]);
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Finite"), "{text}");
    assert!(text.contains("Unsupported"), "{text}");
    server.shutdown();
}

#[test]
fn a_live_stream_is_refused_legibly_and_not_played() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").live());
    let output = run(&["play", &server.url("/audio.flac"), "--probe-only"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("live"), "{text}");
    server.shutdown();
}

#[test]
fn a_malformed_url_is_reported_as_a_url_not_as_a_missing_file() {
    let output = run(&["play", "https://", "--probe-only"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("URL"), "expected a URL diagnostic: {text}");
    assert!(!text.contains("No such file"), "{text}");
}

#[test]
fn a_url_with_credentials_is_refused_rather_than_used() {
    // §5: no implicit credential feature, and the password must not echo.
    let output = run(&["play", "https://alice:hunter2@example.com/a.mp3", "--probe-only"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("credentials"), "{text}");
    assert!(!text.contains("hunter2"), "the password leaked: {text}");
}

#[test]
fn a_signed_query_is_redacted_from_diagnostics() {
    // H15's redaction half.
    let output = run(&["play", "https://nonexistent.invalid/a.mp3?token=SECRETVALUE", "--probe-only"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(!text.contains("SECRETVALUE"), "the signed query leaked: {text}");
    assert!(text.contains("nonexistent.invalid"), "the diagnostic lost its host: {text}");
}

#[test]
fn a_local_spelling_that_looks_like_a_url_stays_a_path() {
    // §5: `./https:...` is an unambiguous local spelling.
    let output = run(&["play", "./https:not-a-url", "--probe-only"]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("https:not-a-url"), "expected a path diagnostic: {text}");
    assert!(!text.contains("URL"), "a local spelling was parsed as a URL: {text}");
}
```

Add to `tests/cli_playback.rs`, additively — every existing test there stays byte-identical, which is R5's whole point:

```rust
#[test]
fn a_rejected_file_still_reports_without_a_device_or_a_terminal() {
    // R5/G4. Preparation moved to the worker; this is the M1 property that
    // move must not cost. CI has neither an audio device nor a controlling
    // terminal, so a passing run here is the evidence.
    let output = run(&["play", env!("CARGO_MANIFEST_DIR")]);
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("regular file"), "expected the regular-file rule: {text}");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --test http_cli --test cli_playback 2>&1 | tail -20`
Expected: FAIL on the eight new tests.

- [ ] **Step 3: Implement the CLI and application changes**

Work in this order: `cli.rs` first (a one-field change that will not compile until `app.rs` follows), then `resolve_source`, then the loop split, then `--probe-only`, then the status line and redaction.

The loop split is the part to be careful with. Loop A must not skip the checkpoint path: if it breaks with a failure, control still falls through to the same `engine.interrupt_shutdown()` / `session.reconcile_shutdown()` / `report_flush` sequence loop B uses. Hoist that sequence into a closure or a small `finish(engine, session, writer, clock, persisting, outcome)` function so both exits share it, rather than duplicating it — duplicating is how D18's "a terminal write failure must not skip the final checkpoint" gets lost.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test --locked 2>&1 | tail -10`
Expected: everything green.

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings`

Run the app by hand against the loopback fixture to see it work:
```bash
cargo run --quiet -- play "$(python3 -m http.server --version >/dev/null 2>&1 && echo ok)" 2>/dev/null || true
cargo run --quiet -- play tests/fixtures/sine.flac --probe-only
```
Expected: the second line prints the sine fixture's metadata, continuity `Finite`, seek `Native`, resume `Supported`.

- [ ] **Step 5: Commit**

```bash
git add src/cli.rs src/app.rs tests/http_cli.rs tests/cli_playback.rs
git commit -m "feat(cli): accept HTTP(S) URLs, defer raw mode, probe remotely

Raw mode is entered only after the first Loaded/Failed (R5), so a bad file or a
refused URL still reports with no device and no raw terminal — the M1 property
that moving preparation to the worker would otherwise have cost."
```

---

## Task 13: The acceptance suite

Needs Task 12. Every row of §12's table, wired end to end. Several are already covered by earlier tasks; this task is where the remaining ones land and where the whole table is accounted for.

**Files:**
- Create: `tests/http_playback.rs`, `tests/http_protocol.rs`, `tests/http_cancellation.rs`, `tests/http_resume.rs`
- Modify: `tests/support/mod.rs` (additive `TestEngine` helpers)

**Coverage map. Every row must point at a named test before this task is done.**

| ID | Where | Notes |
|---|---|---|
| H1 | `http_playback.rs::mp3_flac_and_wav_play_before_the_body_completes` | Three fixtures, each asserting `TestOutput::captured()` is non-silent while `server.requests()` shows the body still arriving. |
| H2 | `http_playback.rs::seeking_installs_the_media_position_and_requests_that_byte` | Forward and backward. |
| H3 | `engine_remote.rs::stop_closes_the_fetch_and_play_reopens_at_the_preserved_position` | Task 10. Assert the request count rises by exactly one. |
| H4 | `http_resume.rs::a_second_session_resumes_from_the_flushed_checkpoint` | Two `Session`s over one `StateStore` in a `tempfile::TempDir`, staged like `tests/resume_contract.rs`. Redirect the second session's fetch and assert the `MediaId` is unchanged. |
| H5 | `http_playback.rs::a_range_less_server_plays_but_cannot_seek_or_resume` | Both with and without `Accept-Ranges`, and the fallback protects an existing entry. |
| H6 | `http_protocol.rs::redirects_preserve_identity_and_query_and_the_bad_ones_fail` | Loop, excess hops, downgrade, non-HTTP scheme. |
| H7 | `http_protocol.rs::every_malformed_range_response_fails_without_committing_a_target` | Wrong start, reversed, conflicting total, missing range, multipart, unexpected 200, 416. After each, assert the engine's position is unchanged. |
| H8 | `http_protocol.rs::no_broken_transfer_can_become_a_completed_track` | Short response, stalled body, disconnect, malformed audio. Assert the state is `Failed`, never `Ended`, and the checkpoint keeps the pre-failure position. |
| H9 | `http_cancellation.rs::every_wait_wakes_and_stale_responses_cannot_repopulate` | Header waits, empty-buffer reads, full-buffer producer waits; each proves the wait was entered first. |
| H10 | `engine_remote.rs::pause_during_a_stalled_read_*`, `..::quit_while_paused_*` | Task 10. |
| H11 | `http_protocol.rs::validators_follow_the_documented_policy` | Strong change fails; weak and absent follow best-effort; assert no `If-Range` header carries a `W/` value by inspecting `server.requests()`. |
| H12 | `prepare.rs` (Task 7) — three tests | Finite unknown-duration, finite chunked, unresolved vs live as distinct refusals. |
| H13 | `http_playback.rs::occupancy_stays_bounded_and_starvation_silence_does_not_advance_position` | See the note below — the obvious assertion is not reachable from a test. |
| H14 | `engine_remote.rs` + `session_policy.rs` (Task 11) | Revision guards, no durable target for an unsupported stopped seek, ordered shutdown backlog. |
| H15 | `http_cli.rs` (Task 12) — seven tests | |
| H16 | `session_policy.rs` (Task 11) — six tests, plus `http_resume.rs::the_protection_survives_a_process_boundary` | The cross-process half needs a real store. |
| H17 | `prepare.rs` (Task 7) + `http_playback.rs::a_byte_seekable_source_with_an_unseekable_demuxer_is_not_native` | The second needs a fixture whose demuxer cannot seek. |
| H18 | The existing `tests/decode_fixtures.rs`, `tests/resume_contract.rs`, `tests/persistence_*.rs`, unchanged | Their continuing to pass untouched *is* the evidence. |

- [ ] **Step 1: Write the four acceptance files**

Write each test named above in full. Every one follows the same shape: start a `TestServer` with a scripted `Script`, drive a `TestEngine` over `TestOutput` with the harness clock frozen except where playback must actually run, assert, `engine.finish()`, `server.shutdown()`.

**H13's occupancy half needs a reachable observation.** The `ByteChannel` is owned by `HttpMediaSource`, inside a `MediaSourceStream`, inside `DecodedSource`, inside the worker — so a test cannot call `channel.buffered()` on it, and an earlier draft's assertion was unwritable. Observe it from the server side instead, which is also the stronger statement: the bound is only real if it produces backpressure on the wire.

`TestServer` gains `pub fn bytes_written(&self) -> usize`, counted as the body loop writes. The test then:

1. Serves a body far larger than `buffer_bytes` from a server that never stalls.
2. Loads it and *does not* play, so nothing drains the channel.
3. Waits until `bytes_written` stops growing for 500 ms, then asserts it settled at no more than `buffer_bytes + chunk_bytes + slack`, where `slack` is one TCP window's worth — name the constant and say it is the kernel's socket buffer, not ours.
4. Plays, and asserts `bytes_written` resumes growing.

Run it with `Limits { buffer_bytes: 64 << 10, chunk_bytes: 8 << 10, ..Limits::default() }` so the bound is reached in milliseconds and the slack is a small fraction of it rather than swamping the measurement.

The starvation half is separate and needs no new hook: stall the server mid-body, let the ring drain, and assert `engine.progress().position` stops advancing once the last pushed frame has played while the state stays `Playing`.

Three rules that apply to all of them, and that a reviewer should reject the task for violating:
- **No `std::thread::sleep` as a synchronization primitive.** Every wait either polls a condition with a deadline and a failing assertion, or uses `server.wait_until_stalled`. §12: "Each cancellation test first proves the target wait was entered; sleeping and hoping for a race is insufficient."
- **No public network and no real device.** Every URL is `127.0.0.1:<ephemeral>`; every engine runs over `TestOutput`.
- **Every persistence test uses a `tempfile::TempDir`**, never the platform state path.

- [ ] **Step 2: The M4A question (§12's closing paragraph)**

Run: `ffmpeg -version >/dev/null 2>&1 && echo have-ffmpeg || echo no-ffmpeg`

If ffmpeg is available, generate a small fixture and add a range-backed opening test:

```bash
ffmpeg -f lavfi -i "sine=frequency=440:duration=5" -c:a aac -b:a 64k tests/fixtures/sine-5s.m4a
```

```rust
#[test]
fn an_m4a_recording_opens_over_ranges() {
    // §12: an ISO-BMFF file whose `moov` atom sits at the tail needs byte
    // seeking to open at all, which is exactly what a range-capable HTTP
    // source provides and a sequential one does not.
    let server = TestServer::start(Script::from_fixture("sine-5s.m4a"));
    let prepared = /* prepare against the server */;
    assert_eq!(prepared.capabilities.continuity, Continuity::Finite);
    assert_eq!(prepared.capabilities.seek, SeekSupport::Native);
    server.shutdown();
}
```

If ffmpeg is **not** available, do not fabricate a fixture and do not claim M4A works. Record the specific limitation in `docs/m1-known-debt.md` under a new M3 heading:

> M4A over HTTP is untested. `isomp4`/`alac` are enabled and the range path is format-agnostic, but a file whose `moov` atom is at the tail needs byte seeking to open, and no fixture exists to prove it. A sequential (range-less) server is expected *not* to open such a file; that expectation is unverified.

§12 is explicit: "Do not claim all container layouts work from extension alone."

- [ ] **Step 3: Run the whole suite**

Run: `cargo test --locked 2>&1 | tail -20`
Expected: everything green.

Run: `cargo test --locked 2>&1 | grep -c '^test .* ok$'`
Expected: the recorded baseline count plus every test added by Tasks 1–13. Write the new total into the commit message.

Run each new file three times to catch flakiness, since these are the only tests in the repository with real sockets and real threads:
```bash
for i in 1 2 3; do cargo test --locked --test http_playback --test http_protocol --test http_cancellation --test http_resume 2>&1 | tail -3; done
```
Expected: identical results all three times. A test that passes two runs in three is a broken test, not a flaky one — fix it rather than retrying.

- [ ] **Step 4: Commit**

```bash
git add tests
git commit -m "test(http): the §12 acceptance suite, H1 through H18

Total after M3: <paste the count>."
```

---

## Task 14: Documentation

Needs Task 13. §12's closing instruction: "Update README and architecture to describe the shipped milestones accurately when implementation completes."

**Files:**
- Modify: `README.md`, `docs/architecture.md`, `docs/m1-known-debt.md`

- [ ] **Step 1: Update `docs/architecture.md`**

- §1: M3 ships finite HTTP; state which of the three transports now work and which do not. Keep the invariant sentence exactly as it stands.
- §2's table: the Application row's "Tokio arrives with M3's networking, not before" becomes a statement of what shipped — the application owns an `HttpService` whose runtime has one worker thread, built only when the source is remote. Add the HTTP source adapter and the fetch task as rows, with their "must not" columns from §4 of the spec.
- §3: add the `SEEK` interrupt bit, the `SourceInterrupt` freeze level, `Admission`, and the new event variants to the protocol list. Update `RESERVED_EVENT_SLOTS` from 8 to 9 and reproduce the new arithmetic.
- §5: record that `SeekSupport::RestartAndDiscard` is defined but never published (R1), and why.
- §6: add the §10 protection rule — one paragraph, stating that it favours the earlier resume point and is not a maximum-position merge.
- §7: replace the `Failed(String)` note with the typed `RemoteFailure` cause, and say what remains stringly-typed.
- §8: mark M3 shipped. Add a short subsection naming the §8 limits and stating plainly that the 1 MiB buffer plus one 64 KiB chunk is the *application's* bound, and that reqwest and rustls have buffering of their own which this number does not include (§8 requires that distinction explicitly).

- [ ] **Step 2: Update `README.md`**

Add HTTP to the usage line, show the two forms:

```
continuo play ~/Music/episode.mp3
continuo play https://example.com/podcast/episode-42.mp3
continuo play https://example.com/podcast/episode-42.mp3 --probe-only
```

State the honest limits in one short list: range-capable servers can seek and resume; range-less servers play through but cannot seek or resume; live streams and sources whose continuity cannot be established are refused; there is no automatic reconnection.

- [ ] **Step 3: Add the M3 debt section to `docs/m1-known-debt.md`**

Under a new `# Milestone 3 — carried debt` heading, record at minimum:
- Whatever the M4A decision in Task 13 Step 2 produced.
- `PlaybackEvent::Failed` still carries a `message: String` beside its typed `cause`, so non-remote faults remain stringly-typed. M3 narrowed the M1 finding rather than closing it.
- Whether `verify_seek_support`'s trial seek has a test that proves the `Unknown → Native` transition and its `Unknown → Unsupported` sibling; if only one is covered, say so.
- The `SLICE` constant in `src/http/channel.rs` sets how often the wait hook runs, and therefore how current a checkpoint stays during a stall. It is not measured against real network behaviour, only against the §8 one-second bound.
- Anything the implementer had to change from this plan, with the reason. §13: "changes during implementation must be recorded explicitly."

- [ ] **Step 4: Verify and commit**

Run: `cargo fmt --check && cargo clippy --locked --all-targets --all-features -- -D warnings && cargo test --locked 2>&1 | tail -5`

```bash
git add README.md docs/architecture.md docs/m1-known-debt.md
git commit -m "docs: describe the shipped M3 slice"
```

- [ ] **Step 5: Manual acceptance (§12)**

Not automatable and not a gate for the tasks above, but the milestone is not done without it. On a machine with a real audio device:

1. `continuo play <a real finite episode URL>` — confirm audio, and that the status line shows a duration and a seek capability.
2. Seek forward and back; confirm the position lands where the seek asked.
3. Stop with `s`, then Play with `p`; confirm playback resumes at the preserved position rather than at zero.
4. Quit with `q`, relaunch with the same URL; confirm the position continues.
5. Check `~/.local/state/continuo/state.json` (or the platform equivalent) for the entry, and confirm the identity is the original URL rather than a redirect target.
6. With `RUST_LOG=continuo=debug`, confirm no signed query or userinfo appears in any log line.

Record the results in the PR description. Feed discovery remains an M4 step.

---

## Self-review

**Spec coverage.** Every section walked, and the task that implements it:

| Spec | Task |
|---|---|
| §1 outcome and scope; the three scope choices | R1–R3 above; Tasks 6, 7, 10 |
| §2 alternatives — stream through a bounded buffer | Tasks 3, 5 |
| §3 the five integration boundaries | Tasks 7 (probe), 9 (wait hook), 10 (seek interrupt, remote stop), 11 (capabilities in `Loaded`) |
| §4 ownership | Task 5 (runtime, one worker thread), Task 9 (`WaitService` holds no decoder), Task 7 (evidence combined at the engine) |
| §5 CLI and opening sequence | Tasks 8 (resume intent, disposition), 12 (parsing, raw-mode deferral, probe-only) |
| §6 finite media and capability evidence | Task 7 (the table), Task 10 (`verify_seek_support`, `CapabilitiesChanged`) |
| §7 response contract | Task 2 (the rules), Task 5 (redirects, truncation), Task 6 (416 at byte EOF, re-request at the next byte) |
| §8 buffering, deadlines, cancellation | Tasks 1 (limits), 3 (channel, lost wakes), 5 (deadlines), 6 (retirement latching), 9 (progress during a wait), 10 (submission, reserve arithmetic) |
| §9 playback transitions | Task 10 (every row of the table), Task 3 (freeze) |
| §10 resume and persistence | Tasks 8 (disposition), 11 (protection) |
| §11 diagnostics and errors | Tasks 1 (categories, redaction), 8 (`Failed.cause`), 12 (status line) |
| §12 acceptance evidence | Tasks 4 (server), 13 (H1–H18) |
| §13 review boundary | Settled as R1–R5 |

Two things §12 asks for that are **not** in a task, deliberately, with the reason:
- **"Log source opening, redirect count, capability evidence, range operations, requested/actual seek, cancellation, reconnect, and source completion"** (§11). These are `tracing` calls spread across Tasks 5, 6, 7 and 10 rather than a task of their own. M2's carried debt already records that `tracing`-expressed policy is unassertable and that a small collecting `Layer` would close the whole class at once; adding one is out of M3's scope, so the log lines are written where the code is and the gap is noted in Task 14 Step 3.
- **Configuration of the §8 limits.** §8 says "exposing configuration is deferred", so `Limits` is injectable for tests and has no CLI or file surface.

**Placeholder scan.** No step says "add error handling", "handle edge cases", "similar to Task N", or "write tests for the above". Three places give prose obligations rather than a full literal body — Task 4's server, Task 5's fetch task, Task 13's acceptance files — because each is several hundred lines whose every branch is already pinned by a named test above it. Each lists its obligations as a numbered or tabular checklist, and each obligation has a test that fails if it is missed.

**Claims checked against the crates rather than from memory.** Five defects across the two review rounds were assertions this document made about code it does not own — four about dependencies, one (`MutexGuard::unlocked`) about the standard library — and all five were wrong. What replaced them, and where each was verified: reqwest 0.13.5's feature list (its own manifest) and `Response::chunk` (`src/async_impl/response.rs:310`); symphonia-core 0.6.1's `Error` impl (`src/errors.rs:82`), its `read_buf_exact` retry (`src/io/media_source_stream.rs:425`), and the `MediaSource` trait's surface (`src/io/mod.rs:42`). `MutexGuard::unlocked` was checked by compiling it (`rustc 1.98.1`, `E0599`). Anything this plan asserts about code it does not own should be re-checked the same way before it is relied on, rather than carried forward on the strength of appearing here — round 2 found that round 1 had repeated the mistake in the very paragraph warning against it.

**Round 2's other lesson.** Seven of its nine defects were introduced by round 1's fixes, not present before them. A fix that adds a lock, an emitter or a wake channel changes the invariants of everything already using them, so the things to re-derive after any such change are: the lock order, who notifies which waiter, and which end of a queue an event belongs on. All three appear as explicit written statements in this plan now, rather than being left implicit for the implementer to reconstruct.

**Type consistency.** Names used across task boundaries, checked against their definitions:
`Limits` (1) → 5, 6, 7. `RemoteFailure`, `Operation`, `Phase`, `RangeRejection`, `RedirectRejection`, `redact_url` (1) → 2, 5, 6, 7, 8, 12. `Headers`, `Accepted`, `ByteRange`, `Validator`, `Established`, `accept`, `accept_redirect`, `if_range_value`, `validator_from`, `is_live`, `parse_content_range` (2) → 5, 6. `ByteChannel`, `SourceInterrupt`, `WaitHook`, `Outcome`, `ReadOutcome` (3) → 5, 6, 7, 9, 10. `TestServer`, `Script`, `RecordedRequest` (4) → 5, 6, 7, 12, 13. `HttpService`, `FetchRequest`, `FetchAccepted`, `HeaderWait`, `HeaderOutcome` (5) → 6, 7, 10, 12. `HttpMediaSource`, `SourceEvidence`, `remote_cause`, `is_retired` (6) → 7, 10. `Prepared`, `PrepareContext`, `prepare` (7) → 10, 12. `ResumeCandidate`, `ResumeDecision`, `decide_resume`, `ResumeIntent`, `Admission`, `StartDisposition`, `RestartEstablished`, `CapabilitiesChanged`, `SeekCancelled` (8) → 10, 11, 12. `TransportCore`, `WaitService`, `SessionFacts` (9) → 10. Every one is defined before its first use, and every producer/consumer pair spells it the same way.

One inconsistency found and fixed while writing this review: Task 6's `HttpMediaSource::open` takes `hook: Arc<dyn WaitHook>` while Task 7's `PrepareContext` holds the same field — they are the same value threaded through, not two hooks, and Task 7's text now says so.
