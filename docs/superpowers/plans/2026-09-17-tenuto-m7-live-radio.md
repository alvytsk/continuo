# Tenuto M7: Live HTTP Radio Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Play a direct live HTTP audio stream (ICY-header Icecast/Shoutcast v2) with bounded, cancellable reconnect, no seeking, no checkpoints and no completion.

**Architecture:** `response::Accepted` gains `Live` and is the only body-mode model; above `http` everything keys on `Continuity::Indefinite`. The engine owns a fresh-open sequence (prepare → validate continuity → tear down → `open_transport` → prime → run) shared by reconnect and every Play on an established station, plus a `Reconnecting` state driven from the worker loop. `EngineHandle` stops damaging a source it cannot seek or must close. `Session` gains one checkpoint gate.

**Tech Stack:** Rust 1.98.1, Symphonia, CPAL, reqwest/Tokio, crossbeam-channel, Ratatui. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-17-tenuto-live-radio-design.md` (revision 2). Section references below (`§n`) are to the spec. Test IDs `L1`–`L20` are the spec's §14 table.

## Global Constraints

- Runtime code forbids `unsafe` and denies `unwrap` and `expect` (`Cargo.toml` lints). Tests use `unwrap_or_else(|e| panic!(…))`, as the existing suites do.
- Every URL in a message or log passes `redact_url` first.
- Gates, all with `--locked`: `cargo fmt --check`; `cargo clippy --locked --all-targets --all-features -- -D warnings`; `cargo test --locked`.
- All tests run against `tests/support/server.rs`. No public-network dependency.
- Commit messages carry no `Co-authored-by` and no Claude attribution.
- `schema_version` stays 3. `media/id.rs`, `queue.rs`, `persistence/*` are not edited.
- M7 never sends `Icy-MetaData: 1`.
- Defaults (§7): backoff 1 s, 2 s, 4 s, 8 s, then 15 s; budget 5 min; stable after 30 s of played audio.
- Finite-media behavior changes only as §6.1 says: `submit_seek` on an unsupported source no longer retires it; `submit(Load)` retires the observed generation.

## Verified engine facts (read 2026-09-17; re-read before Task 6)

These are the two details the spec asserted by design. Both were read; the plan depends on them as stated.

1. **Hook thaw (`src/playback/wait.rs`, `service_as`).** On `!frozen && frozen_by_hook` the hook calls `release()`, clears the flag and **announces `StateChanged(Playing)`**. `WaitService::park()` returns `true` when there is **no transport**, so with the freeze level still raised after a live pause tore the transport down, the hook would set `frozen_by_hook`, announce a duplicate `Paused`, and on the next thaw announce `Playing` with no source. Consequence: live pause must clear `frozen_by_hook` **and** lower the freeze level (`source_interrupt.thaw()`).
2. **`open_transport` (`src/playback/engine.rs:1423`).** Builds `Converter::new(source_rate, config.sample_rate, source_channels, channels)` from the current `self.source`, anchors at `self.position`, then calls `prime_and_run(playing)`, which runs `pump_audio()` once and starts the callback only if `playing`. `pump_audio` reports failures by calling `fail_with` itself. Consequence: the fresh-open sequence calls `open_transport(false)`, needs `pump_audio` to *report* rather than transition while an attempt is in progress, and starts the callback itself afterwards.
3. `fail_with` tears the transport down without capturing position; `publish_progress` runs every loop pass, so `self.position` is at most one `TICK` stale. Acceptable.
4. `reinstall` is not used by any M7 path.

## File Structure

| File | Responsibility in M7 |
| --- | --- |
| `tests/support/server.rs` | `Script::icy_station()`, `Script::then()`, per-connection ordinal for endless bodies |
| `src/http/error.rs` | `IcyFramingUnsupported`, `LiveEnded`, `RemoteFailure::is_retryable` |
| `src/http/response.rs` | `Accepted::Live`; classification inside the 200/206 arms |
| `src/http/service.rs` | `Live` body end is `LiveEnded`; no range resume |
| `src/http/source.rs` | `Live` forces evidence; `station_name()` |
| `src/playback/prepare.rs` | Accept `Indefinite`; `PrepareContext.expected`; station title |
| `src/playback/state.rs` | `PlaybackState::Reconnecting` |
| `src/playback/reconnect.rs` (new) | `ReconnectPolicy`, `Outage` — pure, unit-tested |
| `src/playback/engine.rs` | `SourceTraits`, submission rules, `is_indefinite`, fresh-open, live pause, reconnect loop |
| `src/playback/wait.rs` | `facts.playing` doc/gating comment for `Reconnecting` |
| `src/session.rs` | `checkpointable` gate and its transition order |
| `src/application/{transport,runtime,seek,view}.rs` | `PlaybackPhase::Reconnecting`, live notices, pause direction, `live` in the view |
| `src/tui/render.rs`, `src/app.rs` | `LIVE` / `reconnecting…` rendering; probe output |
| `tests/m7_*.rs` (new) | One file per concern, listed per task |
| `docs/*`, `README.md`, `CHANGELOG.md` | §15 |

---

### Task 0: Branch and spec

**Files:** none modified.

- [ ] **Step 1: Branch**

```bash
git switch -c feat/m7-live-radio
```

- [ ] **Step 2: Commit the spec and this plan**

```bash
git add docs/superpowers/specs/2026-09-17-tenuto-live-radio-design.md docs/superpowers/plans/2026-09-17-tenuto-m7-live-radio.md
git commit -m "docs: M7 live radio design and plan"
```

---

### Task 1: Test server — station script and per-request sequencing

**Files:**
- Modify: `tests/support/server.rs`
- Test: `tests/http_server_selftest.rs`

**Interfaces:**
- Produces: `Script::icy_station(self) -> Self` — endless chunked body, headers `icy-name: Test Radio`, `icy-br: 128`, **no** `icy-metaint`. `Script::then(self, next: Script) -> Self` — connection *n* (1-based) is served by the *n*-th script in the chain; the last script serves every later connection. Existing `Script::live()` is unchanged (it sends `icy-metaint: 16000` and is now the framing-refusal fixture). `truncate_body_after` on an endless body closes **every** connection it serves after that many bytes; with `truncate_only_first_response` only connection 1.

- [ ] **Step 1: Write the failing self-tests**

Append to `tests/http_server_selftest.rs`:

```rust
#[test]
fn an_icy_station_sends_name_and_bitrate_but_no_metaint() {
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let response = raw_get(&server, "/radio", &[]);
    let head = response.to_ascii_lowercase();
    assert!(head.contains("icy-name: test radio"), "{head}");
    assert!(head.contains("icy-br: 128"), "{head}");
    assert!(!head.contains("icy-metaint"), "{head}");
    assert!(head.contains("transfer-encoding: chunked"), "{head}");
    server.shutdown();
}

#[test]
fn a_script_chain_serves_each_connection_its_own_script_and_repeats_the_last() {
    let server = TestServer::start(
        Script::serving(b"first".to_vec())
            .without_ranges()
            .then(Script::serving(Vec::new()).status(503))
            .then(Script::serving(b"third".to_vec()).without_ranges()),
    );
    assert!(raw_get(&server, "/a", &[]).ends_with("first"));
    assert!(raw_get(&server, "/a", &[]).starts_with("HTTP/1.1 503"));
    assert!(raw_get(&server, "/a", &[]).ends_with("third"));
    assert!(raw_get(&server, "/a", &[]).ends_with("third"));
    server.shutdown();
}
```

`raw_get` already exists in this file if earlier self-tests use a raw socket; if it does not, add it above the tests:

```rust
fn raw_get(server: &TestServer, path: &str, headers: &[(&str, &str)]) -> String {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", server.port()))
        .unwrap_or_else(|error| panic!("connect: {error}"));
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(300)))
        .unwrap_or_else(|error| panic!("timeout: {error}"));
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .unwrap_or_else(|error| panic!("write: {error}"));
    let mut out = Vec::new();
    let mut buffer = [0u8; 4096];
    // An endless body never closes: read what arrives inside the timeout.
    while let Ok(n) = stream.read(&mut buffer) {
        if n == 0 || out.len() > 64 * 1024 {
            break;
        }
        out.extend_from_slice(&buffer[..n]);
    }
    String::from_utf8_lossy(&out).into_owned()
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --test http_server_selftest icy_station script_chain`
Expected: compile error — no method `icy_station` / `then`.

- [ ] **Step 3: Implement**

In `Script` add two fields and keep `Default`:

```rust
    /// ICY response headers without metadata framing: a station M7 plays.
    icy_station: bool,
    /// Connection n (1-based) is served by `sequence[n - 2]` once n > 1; the
    /// last entry serves every later connection.
    sequence: Vec<Script>,
```

Builders, beside `live()`:

```rust
    pub fn icy_station(mut self) -> Self {
        self.icy_station = true;
        self
    }

    pub fn then(mut self, next: Script) -> Self {
        self.sequence.push(next);
        self
    }

    /// The script that answers connection `ordinal` (1-based).
    fn for_ordinal(&self, ordinal: usize) -> &Script {
        match ordinal.checked_sub(2) {
            None => self,
            Some(index) => self
                .sequence
                .get(index)
                .or_else(|| self.sequence.last())
                .unwrap_or(self),
        }
    }
```

In `handle_connection`, immediately after `ordinal` is computed, rebind:

```rust
    let script = script.for_ordinal(ordinal);
```

In `write_whole_body`: `let endless = script.live || script.unresolved || script.icy_station;`, and after the `if script.live { … }` header block add:

```rust
    if script.icy_station {
        header.push_str("icy-name: Test Radio\r\nicy-br: 128\r\n");
    }
```

`write_endless_body` gains an `ordinal: usize` parameter, passed from `write_whole_body`, and uses it: `BodyWriter::new(script, true, ordinal)` (it passed a literal `1`).

- [ ] **Step 4: Run to verify pass, and that nothing else moved**

Run: `cargo test --locked --test http_server_selftest && cargo test --locked --test http_fetch --test http_playback`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add tests/support/server.rs tests/http_server_selftest.rs
git commit -m "test: station script and per-connection sequencing in the test server"
```

---

### Task 2: `Accepted::Live`, new failures, `is_retryable`

**Files:**
- Modify: `src/http/error.rs`, `src/http/response.rs`
- Modify (compile-only arms): `src/http/service.rs`, `src/http/source.rs`
- Test: `tests/http_response.rs`, `tests/http_errors.rs`

**Interfaces:**
- Produces: `Accepted::Live`; `RemoteFailure::IcyFramingUnsupported`, `RemoteFailure::LiveEnded`; `RemoteFailure::is_retryable(&self) -> bool`. `response::is_live` is deleted; nothing outside `response.rs` may call it.

- [ ] **Step 1: Write the failing tests**

Append to `tests/http_response.rs` (it already imports `accept`, `Accepted`, `Headers`; add `RangeRejection`, `RemoteFailure`, `Operation` if missing):

```rust
fn icy(extra: &[(&str, &str)]) -> Headers {
    let mut pairs = vec![("icy-name", "Test Radio"), ("icy-br", "128")];
    pairs.extend_from_slice(extra);
    Headers::from_pairs(&pairs)
}

#[test]
fn an_icy_200_at_origin_is_live_whatever_length_it_declares() {
    assert_eq!(accept(200, &icy(&[]), 0, true, None), Ok(Accepted::Live));
    assert_eq!(
        accept(200, &icy(&[("content-length", "1000")]), 0, true, None),
        Ok(Accepted::Live)
    );
}

#[test]
fn an_icy_206_from_zero_is_live_and_from_anywhere_else_is_a_wrong_start() {
    let ranged = icy(&[("content-range", "bytes 0-999/1000")]);
    assert_eq!(accept(206, &ranged, 0, true, None), Ok(Accepted::Live));
    let later = icy(&[("content-range", "bytes 5-999/1000")]);
    assert_eq!(
        accept(206, &later, 5, false, None),
        Err(RemoteFailure::InvalidRange {
            reason: RangeRejection::WrongStart
        })
    );
}

#[test]
fn metadata_framing_is_refused_until_it_can_be_demultiplexed() {
    assert_eq!(
        accept(200, &icy(&[("icy-metaint", "16000")]), 0, true, None),
        Err(RemoteFailure::IcyFramingUnsupported)
    );
}

#[test]
fn an_hls_playlist_is_refused_before_any_probe() {
    let headers = Headers::from_pairs(&[("content-type", "application/vnd.apple.mpegurl")]);
    assert_eq!(
        accept(200, &headers, 0, true, None),
        Err(RemoteFailure::UnsupportedLiveMedia)
    );
}

#[test]
fn an_error_status_is_never_live_whatever_icy_headers_it_carries() {
    for status in [404u16, 429, 503] {
        assert_eq!(
            accept(status, &icy(&[]), 0, true, None),
            Err(RemoteFailure::Status {
                status,
                operation: Operation::Open
            })
        );
    }
}
```

Append to `tests/http_errors.rs`:

```rust
#[test]
fn only_failures_that_say_nothing_about_the_location_are_retryable() {
    use tenuto::http::error::{Operation, Phase, RedirectRejection, RemoteFailure};
    let status = |status| RemoteFailure::Status {
        status,
        operation: Operation::Open,
    };
    for failure in [
        RemoteFailure::LiveEnded,
        RemoteFailure::Timeout { phase: Phase::Stall },
        RemoteFailure::Transport {
            operation: Operation::Read,
            detail: "reset".into(),
        },
        status(429),
        status(500),
        status(503),
    ] {
        assert!(failure.is_retryable(), "{failure:?}");
    }
    for failure in [
        status(401),
        status(403),
        status(404),
        status(410),
        RemoteFailure::Cancelled,
        RemoteFailure::ResourceChanged,
        RemoteFailure::IcyFramingUnsupported,
        RemoteFailure::UnsupportedLiveMedia,
        RemoteFailure::ContinuityUndetermined,
        RemoteFailure::Redirect {
            reason: RedirectRejection::Loop,
        },
        RemoteFailure::TruncatedBody { missing: 1 },
    ] {
        assert!(!failure.is_retryable(), "{failure:?}");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --test http_response --test http_errors`
Expected: compile errors — `Accepted::Live`, `IcyFramingUnsupported`, `LiveEnded`, `is_retryable` undefined.

- [ ] **Step 3: Implement `error.rs`**

Add to `RemoteFailure`, after `UnsupportedLiveMedia`:

```rust
    #[error("this stream interleaves metadata, which is not supported yet")]
    IcyFramingUnsupported,
    /// A live body ended. Never completion: a station has no end (M7 §3.3).
    #[error("the live stream ended")]
    LiveEnded,
```

And after the enum:

```rust
impl RemoteFailure {
    /// Whether trying the same location again can help (M7 §4). A failure
    /// that says the location itself is unusable is never retried.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport { .. } | Self::Timeout { .. } | Self::LiveEnded => true,
            Self::Status { status, .. } => *status == 429 || (500..600).contains(status),
            _ => false,
        }
    }
}
```

- [ ] **Step 4: Implement `response.rs`**

Add the variant:

```rust
pub enum Accepted {
    Sequential { len: Option<u64> },
    Ranged { range: ByteRange },
    /// An ongoing stream: no length, no ranges, and no end that is not a
    /// disconnect (M7 §4).
    Live,
}
```

Replace `pub fn is_live` with a private pair:

```rust
fn is_hls(headers: &Headers) -> bool {
    headers
        .get("content-type")
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/vnd.apple.mpegurl"))
}

/// Explicit ICY semantics. Deliberately narrow: continuity is never inferred
/// from a missing `Content-Length`, chunked framing, a MIME type or a suffix.
fn is_icy(headers: &Headers) -> bool {
    headers.get("icy-name").is_some() || headers.get("icy-br").is_some()
}

/// The live classification of a *successful* response, `None` when it is not
/// live. Called only from `accept`'s 200 and 206 arms, after status, encoding,
/// multipart and validator checks — an error status is never audio.
fn classify_live(headers: &Headers) -> Option<Result<(), RemoteFailure>> {
    if is_hls(headers) {
        return Some(Err(RemoteFailure::UnsupportedLiveMedia));
    }
    if headers.get("icy-metaint").is_some() {
        return Some(Err(RemoteFailure::IcyFramingUnsupported));
    }
    is_icy(headers).then_some(Ok(()))
}
```

In `accept`, first line of the `200 =>` arm, after the `!at_origin` check:

```rust
            if let Some(live) = classify_live(headers) {
                return live.map(|()| Accepted::Live);
            }
```

In the `206 =>` arm, after `range.first != requested_start` is checked and before the `declared_len` comparison:

```rust
            if let Some(live) = classify_live(headers) {
                live?;
                if requested_start != 0 {
                    return Err(RemoteFailure::InvalidRange {
                        reason: RangeRejection::WrongStart,
                    });
                }
                return Ok(Accepted::Live);
            }
```

(The second test's `bytes 5-999` case reaches this with `first == requested_start == 5`, so it is this check that refuses it.)

- [ ] **Step 5: Compile-only arms**

`src/http/source.rs`, in `open`: replace the `match accepted.accepted` and the `is_live` call with

```rust
        let (byte_len, byte_seekable, live) = match accepted.accepted {
            Accepted::Sequential { len } => (len, false, false),
            Accepted::Ranged { range } => (range.total, true, false),
            Accepted::Live => (None, false, true),
        };
```

and delete `let live = response::is_live(&accepted.headers);`.

`src/http/service.rs`, in `run_fetch`:

```rust
    let (mut advertised, total, resumable) = match accepted {
        Accepted::Sequential { len } => (len, len, false),
        Accepted::Ranged { range } => (range.len(), range.total, true),
        Accepted::Live => (None, None, false),
    };
```

and in the resume arm's inner `match opened.accepted`, add `Accepted::Live => None,`.

- [ ] **Step 6: Run to verify pass; find tests that relied on the old refusal point**

Run: `cargo test --locked --test http_response --test http_errors --test http_source --test http_fetch --test prepare`
Expected: the new tests PASS. Any existing test that serves `Script::live()` and asserts `UnsupportedLiveMedia` now sees `IcyFramingUnsupported` (the fixture sends `icy-metaint`). Update those assertions to `RemoteFailure::IcyFramingUnsupported`; do not change the fixture.

- [ ] **Step 7: Commit**

```bash
git add src/http tests/http_response.rs tests/http_errors.rs tests/prepare.rs tests/http_source.rs
git commit -m "feat(http): classify live responses as Accepted::Live after status validation"
```

---

### Task 3: Live body end and live source evidence

**Files:**
- Modify: `src/http/service.rs`, `src/http/source.rs`
- Test: `tests/m7_http_live.rs` (new)

**Interfaces:**
- Consumes: `Accepted::Live`, `RemoteFailure::LiveEnded`, `Script::icy_station()`.
- Produces: `HttpMediaSource::station_name(&self) -> Option<&str>`. A live body that ends, cleanly or not, finishes the channel with `Outcome::Failed(RemoteFailure::LiveEnded)`.

- [ ] **Step 1: Write the failing tests**

Create `tests/m7_http_live.rs`:

```rust
//! M7 §4: a live response at the HTTP seam.

mod support;

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use support::server::{Script, TestServer};
use tenuto::http::channel::{NoopHook, SourceInterrupt};
use tenuto::http::error::RemoteFailure;
use tenuto::http::limits::Limits;
use tenuto::http::service::HttpService;
use tenuto::http::source::{HttpMediaSource, OpeningDeadline, remote_cause};

fn open(server: &TestServer) -> HttpMediaSource {
    let limits = Limits::brisk();
    let service = HttpService::spawn(limits).unwrap_or_else(|error| panic!("service: {error}"));
    let url = url::Url::parse(&server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let interrupt = SourceInterrupt::new(limits.buffer_bytes);
    let (source, opening) = HttpMediaSource::open(
        service,
        url,
        interrupt,
        Arc::new(NoopHook),
        limits,
        OpeningDeadline(Instant::now() + limits.open),
    )
    .unwrap_or_else(|error| panic!("open: {error}"));
    opening.finish_opening();
    source
}

#[test]
fn a_station_is_live_unsized_unseekable_and_named() {
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let source = open(&server);
    let evidence = source.evidence();
    assert!(evidence.live);
    assert_eq!(evidence.byte_len, None);
    assert!(!evidence.byte_seekable);
    assert_eq!(source.station_name(), Some("Test Radio"));
    drop(source);
    server.shutdown();
}

#[test]
fn a_live_body_that_ends_is_a_failure_never_eof() {
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .truncate_body_after(8 * 1024),
    );
    let mut source = open(&server);
    let mut sink = vec![0u8; 4096];
    let error = loop {
        match source.read(&mut sink) {
            Ok(0) => panic!("a live body must never report EOF"),
            Ok(_) => continue,
            Err(error) => break error,
        }
    };
    assert_eq!(remote_cause(&error), Some(RemoteFailure::LiveEnded));
    drop(source);
    server.shutdown();
}
```

If `NoopHook` is not the name of the existing do-nothing `WaitHook` in `src/http/channel.rs`, use the one `tests/http_source.rs` already uses; do not add a new one.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --test m7_http_live`
Expected: compile error on `station_name`; after stubbing, the second test fails with "must never report EOF" or a `Transport` cause.

- [ ] **Step 3: Implement**

`src/http/source.rs`: add field `station_name: Option<String>`, set in `open` from `accepted.headers.get("icy-name").map(str::to_string)` **only in the `Accepted::Live` arm** (extend the tuple to carry it, `None` otherwise), and:

```rust
    /// The `icy-name` of a live source. Display text: the caller escapes it.
    pub fn station_name(&self) -> Option<&str> {
        self.station_name.as_deref()
    }
```

`src/http/service.rs`: keep the mode beside `resumable`:

```rust
    let live = matches!(accepted, Accepted::Live);
```

(bind it before `accepted` is moved into `publish_headers`), and at the end of the body loop replace the final `channel.finish(…)` with:

```rust
        let outcome = if live {
            // M7 §4: every end of a live body is a disconnect, a clean one
            // included. `Eof` here would drain to `EndOfTrack`.
            Outcome::Failed(RemoteFailure::LiveEnded)
        } else {
            classify_body_end(advertised, delivered, operation, ended)
        };
        channel.finish(generation, outcome);
        return;
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --locked --test m7_http_live --test http_fetch --test http_resume`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/http/service.rs src/http/source.rs tests/m7_http_live.rs
git commit -m "feat(http): a live body that ends is LiveEnded, never EOF"
```

---

### Task 4: `prepare` accepts indefinite media and validates expected continuity

**Files:**
- Modify: `src/playback/prepare.rs`; every `PrepareContext { … }` literal (`src/playback/engine.rs` `prepare_context`, `src/app.rs` probe path, `tests/prepare.rs`, any other the compiler names)
- Test: `tests/prepare.rs`

**Interfaces:**
- Produces: `PrepareContext.expected: Option<Continuity>`. `prepare` returns `Err(RemoteFailure::ResourceChanged.into())` when `expected` is `Some` and differs from the prepared continuity; the prepared source is dropped, nothing else happens. For a live source with no decoder title, `Prepared.source.metadata().title` is the station name.
- Consumes: `HttpMediaSource::station_name`.

- [ ] **Step 1: Write the failing tests**

Append to `tests/prepare.rs`, reusing its existing `context(...)` helper (add `expected: None` to it):

```rust
#[test]
fn a_station_prepares_as_indefinite_and_unseekable_with_its_name_as_title() {
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let location = http_location(&server, "/radio");
    let prepared = prepare(&location, &context()).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(prepared.capabilities.continuity, Continuity::Indefinite);
    assert_eq!(prepared.capabilities.seek, SeekSupport::Unsupported);
    assert_eq!(prepared.source.metadata().title.as_deref(), Some("Test Radio"));
    drop(prepared);
    server.shutdown();
}

#[test]
fn a_reopen_that_changes_continuity_is_resource_changed_in_both_directions() {
    let station = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let mut expecting_finite = context();
    expecting_finite.expected = Some(Continuity::Finite);
    let error = prepare(&http_location(&station, "/radio"), &expecting_finite)
        .err()
        .unwrap_or_else(|| panic!("a live source must not satisfy an expected finite one"));
    assert_eq!(remote_cause(&error), Some(RemoteFailure::ResourceChanged));
    station.shutdown();

    let file = TestServer::start(Script::from_fixture("sine-5s.mp3"));
    let mut expecting_live = context();
    expecting_live.expected = Some(Continuity::Indefinite);
    let error = prepare(&http_location(&file, "/a.mp3"), &expecting_live)
        .err()
        .unwrap_or_else(|| panic!("a finite source must not satisfy an expected live one"));
    assert_eq!(remote_cause(&error), Some(RemoteFailure::ResourceChanged));
    file.shutdown();
}
```

If `http_location` does not exist in the file, add `fn http_location(server: &TestServer, path: &str) -> SourceLocation { SourceLocation::Http(url::Url::parse(&server.url(path)).unwrap_or_else(|e| panic!("{e}"))) }`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --test prepare`
Expected: compile error on `expected`; then the first test fails with `UnsupportedLiveMedia`.

- [ ] **Step 3: Implement**

`PrepareContext` gains:

```rust
    /// The continuity an established session already has. A prepared source
    /// that disagrees is refused before anything adopts it (M7 §3.5).
    pub expected: Option<Continuity>,
```

`prepare` becomes:

```rust
    let capabilities = source.capabilities();
    if let Some(expected) = context.expected
        && expected != capabilities.continuity
    {
        return Err(RemoteFailure::ResourceChanged.into());
    }
    match capabilities.continuity {
        // An unresolved source has proven neither that it ends nor that it
        // does not; it is still refused. A live one now plays (M7).
        Continuity::Unresolved => Err(RemoteFailure::ContinuityUndetermined.into()),
        Continuity::Finite | Continuity::Indefinite => Ok(Prepared {
            source,
            capabilities,
        }),
    }
```

In `open_http`, read the name next to `evidence` (before the source is boxed): `let station = source.station_name().map(str::to_string);`. After `DecodedSource::from_media_source(...)` succeeds, fold it in — add to `DecodedSource` (`src/playback/decode.rs`):

```rust
    /// A transport-supplied title, used only when the container gave none.
    pub fn set_fallback_title(&mut self, title: String) {
        if self.metadata.title.is_none() {
            self.metadata.title = Some(title);
        }
    }
```

and call `if let Some(name) = station { decoded.set_fallback_title(name); }`. The title is escaped where every other title is: at display. Do not escape here.

Add `expected: None` to `Worker::prepare_context` and to the `app.rs` probe context. Update the module doc comment's first paragraph: `prepare` refuses `Unresolved` only.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --locked --test prepare --test http_cli --test cli_playback`
Expected: PASS, after updating any CLI test that asserted "live streams are not supported" for `Script::live()` to the `IcyFramingUnsupported` message.

- [ ] **Step 5: Commit**

```bash
git add src/playback/prepare.rs src/playback/decode.rs src/playback/engine.rs src/app.rs tests
git commit -m "feat(prepare): accept indefinite media and refuse a changed continuity"
```

---

### Task 5: Handle-side submission rules — harmless seek, retire on Load

**Files:**
- Modify: `src/playback/engine.rs` (`EngineHandle`, `assemble`, `Worker`)
- Test: `tests/m7_submission.rs` (new)

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Default)]
  pub(crate) struct SourceTraits {
      pub indefinite: AtomicBool,
      pub seek_unsupported: AtomicBool,
  }
  ```
  shared as `Arc<SourceTraits>` by `EngineHandle` and `Worker`; `Worker::set_capabilities(&mut self, capabilities: MediaCapabilities)` is the **only** assignment to `self.capabilities` and publishes both flags. `submit_seek` with `seek_unsupported` set enqueues `SeekTo` and wakes, nothing else. `submit(Load)` retires the generation observed before the send, after admission.

- [ ] **Step 1: Write the failing tests** (L14's finite half, and the Load half of L8)

Create `tests/m7_submission.rs`:

```rust
//! M7 §6.1: what the handle may do before the worker looks.

mod support;

use std::time::Duration;

use support::server::{Script, TestServer};
use support::TestEngine;
use tenuto::playback::command::{Admission, PlaybackCommand, ResumeIntent};
use tenuto::playback::event::PlaybackEvent;

#[test]
fn a_rejected_seek_leaves_a_range_less_stream_playing_on_its_one_connection() {
    let server = TestServer::start(Script::from_fixture("sine-5s.mp3").without_ranges());
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/a.mp3"));
    engine.send(PlaybackCommand::Play);
    engine.play_for(Duration::from_millis(300));

    assert_eq!(
        engine.handle().submit_seek(Duration::from_secs(2)),
        Admission::Accepted
    );
    engine.await_event(|event| matches!(event, PlaybackEvent::SeekRejected { .. }));

    // Audio continues, on the same connection.
    engine.play_for(Duration::from_millis(900));
    assert_eq!(server.requests().len(), 1, "the seek must not cost the connection");
    assert_eq!(
        engine.count_events(|event| matches!(event, PlaybackEvent::Failed { .. })),
        0
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn a_replacement_load_interrupts_a_stalled_read_instead_of_waiting_it_out() {
    let stalled = TestServer::start(Script::from_fixture("sine-5s.mp3").stall_body_after(16 * 1024));
    let next = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&stalled.url("/a.mp3"));
    engine.send(PlaybackCommand::Play);
    assert!(stalled.wait_until_stalled(Duration::from_secs(2)));

    let started = std::time::Instant::now();
    let request = engine.next_request();
    engine.load_remote_as(request, &next.url("/b.flac"), ResumeIntent::StartAt(Duration::ZERO));
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "the load waited {:?} behind the stalled read (brisk stall is 500 ms)",
        started.elapsed()
    );
    engine.finish();
    stalled.release();
    stalled.shutdown();
    next.shutdown();
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --test m7_submission`
Expected: test 1 FAILS (`requests().len()` is 2, or a `Failed`, or `play_for` panics "position never reached"); test 2 FAILS on the elapsed assertion.

- [ ] **Step 3: Implement**

Define `SourceTraits` near `EngineHandle`. Add `traits: Arc<SourceTraits>` to `EngineHandle` and `Worker`; create one `Arc` in `assemble` and hand a clone to each.

`Worker`:

```rust
    /// The one place `capabilities` is written, so the handle's out-of-band
    /// view (M7 §6.1) can never disagree with the worker's for longer than
    /// one command.
    fn set_capabilities(&mut self, capabilities: MediaCapabilities) {
        self.capabilities = capabilities;
        self.traits.indefinite.store(
            capabilities.continuity == Continuity::Indefinite,
            Ordering::Release,
        );
        self.traits.seek_unsupported.store(
            capabilities.seek == SeekSupport::Unsupported,
            Ordering::Release,
        );
    }

    fn is_indefinite(&self) -> bool {
        self.capabilities.continuity == Continuity::Indefinite
    }
```

Replace all five `self.capabilities = …` sites (`grep -n 'self.capabilities = ' src/playback/engine.rs`) with `self.set_capabilities(…)`. At the top of `load()`, after `capture_and_teardown()`, clear both flags (`store(false, …)`): the incoming source is not yet known.

`EngineHandle`:

```rust
    pub fn submit(&self, command: PlaybackCommand) -> Admission {
        // A load tears the current source down whatever it is, so its blocked
        // read or open is woken now rather than waited out (M7 §6.1). Aimed at
        // the generation observed *before* the send, like `submit_seek`.
        let replaced = matches!(command, PlaybackCommand::Load { .. })
            .then(|| self.source_interrupt.generation());
        let admission = self.try_send(command);
        if admission == Admission::Accepted {
            if let Some(generation) = replaced {
                self.source_interrupt.retire_generation(generation);
            }
            let _ = self.wake.try_send(());
        }
        admission
    }

    pub fn submit_seek(&self, target: Duration) -> Admission {
        let generation = self.source_interrupt.generation();
        let admission = self.try_send(PlaybackCommand::SeekTo(target));
        if admission != Admission::Accepted {
            return admission;
        }
        // A source that cannot seek is going to answer `SeekRejected`; retiring
        // it first would cost the listener the stream for nothing (M7 §3.4).
        if !self.traits.seek_unsupported.load(Ordering::Acquire) {
            self.source_interrupt.retire_generation(generation);
            self.interrupt.fetch_or(SEEK, Ordering::Release);
        }
        let _ = self.wake.try_send(());
        admission
    }
```

- [ ] **Step 4: Run to verify pass, and the whole engine suite**

Run: `cargo test --locked --test m7_submission --test engine_remote --test engine_contract --test m5_engine_load_outcomes --test http_cancellation`
Expected: PASS. If `m5_engine_load_outcomes` shows a load that used to end `Failed` now ending `LoadCancelled` because a second `Load` was submitted while it was opening, that is the intended change: update the assertion and say so in the commit body.

- [ ] **Step 5: Commit**

```bash
git add src/playback/engine.rs tests/m7_submission.rs tests/m5_engine_load_outcomes.rs
git commit -m "feat(engine): a rejected seek costs nothing and a load wakes the source it replaces"
```

---

### Task 6: Live load, `Reconnecting` state, rejected Restart, no end of track

**Files:**
- Modify: `src/playback/state.rs`, `src/playback/engine.rs`, every exhaustive `match` on `PlaybackState` the compiler names (`session.rs`, `application/runtime.rs`, `app.rs`, `tui/`)
- Test: `tests/m7_live_playback.rs` (new)

**Interfaces:**
- Produces: `PlaybackState::Reconnecting` (label `"reconnecting"`, non-terminal). A station loads with `StartDisposition::Fresh` at zero and plays. `Restart` on indefinite media emits `SeekRejected { reason: "a live stream cannot restart" }`.
- In this task `Reconnecting` is never *entered*; added arms are: `session.rs on_state` → the existing no-op arm; `runtime.rs phase()` → `PlaybackPhase::Playing` (Task 11 refines it); `runtime.rs` `live` matcher → included; display sites → `state.label()`.

- [ ] **Step 1: Write the failing tests** (L1, L14 live half)

Create `tests/m7_live_playback.rs`:

```rust
//! M7 §5: a station loads, plays, and refuses what it cannot do harmlessly.

mod support;

use std::time::Duration;

use support::server::{Script, TestServer};
use support::TestEngine;
use tenuto::media::capabilities::{Continuity, SeekSupport};
use tenuto::playback::command::{Admission, PlaybackCommand};
use tenuto::playback::event::{PlaybackEvent, StartDisposition};

pub fn station() -> TestServer {
    TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station())
}

#[test]
fn a_station_loads_fresh_at_zero_as_indefinite_and_plays() {
    let server = station();
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/radio"));
    let loaded = engine.await_loaded();
    assert_eq!(loaded.capabilities.continuity, Continuity::Indefinite);
    assert_eq!(loaded.capabilities.seek, SeekSupport::Unsupported);
    assert_eq!(loaded.position, Duration::ZERO);
    assert_eq!(loaded.disposition, StartDisposition::Fresh);

    engine.send(PlaybackCommand::Play);
    engine.play_for(Duration::from_millis(800));
    engine.finish();
    server.shutdown();
}

#[test]
fn seeks_and_restart_are_rejected_and_the_stream_is_untouched() {
    let server = station();
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/radio"));
    engine.send(PlaybackCommand::Play);
    engine.play_for(Duration::from_millis(300));

    assert_eq!(engine.handle().submit_seek(Duration::from_secs(9)), Admission::Accepted);
    engine.await_event(|event| matches!(event, PlaybackEvent::SeekRejected { .. }));
    engine.send(PlaybackCommand::SeekBy(10));
    engine.await_event(|event| matches!(event, PlaybackEvent::SeekRejected { .. }));
    engine.send(PlaybackCommand::Restart);
    engine.await_event(|event| {
        matches!(event, PlaybackEvent::SeekRejected { reason, .. } if reason.contains("live"))
    });

    engine.play_for(Duration::from_millis(900));
    assert_eq!(server.requests().len(), 1);
    assert_eq!(
        engine.count_events(|event| matches!(
            event,
            PlaybackEvent::RestartEstablished { .. } | PlaybackEvent::Failed { .. }
        )),
        0
    );
    engine.finish();
    server.shutdown();
}
```

If `Loaded` (the harness struct in `tests/support/mod.rs`) lacks `capabilities` or `disposition`, add those fields and fill them in `await_loaded` from the event.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --test m7_live_playback`
Expected: test 2 FAILS — `Restart` emits `RestartEstablished`, and a second request is recorded.

- [ ] **Step 3: Implement**

`state.rs`: add `Reconnecting,` after `Paused` and `Self::Reconnecting => "reconnecting",`. Build, and add the arms listed under Interfaces wherever the compiler stops.

`engine.rs`:

- `restart()`, first statement:

```rust
        // M7 §5.1: before `ensure_source_open`. The reopened-source branch
        // below would zero listening time and report a restart.
        if self.is_indefinite() {
            self.reject_seek("a live stream cannot restart".into());
            return;
        }
```

- `check_end_of_track()`, directly after `self.drain_outbox();`:

```rust
        // M7 §3.3: a station has no end.
        if self.is_indefinite() {
            return;
        }
```

- `load()`: compute the start under a guard so a stale candidate never becomes `ResumeUnavailable`:

```rust
        let resume = if prepared.capabilities.continuity == Continuity::Indefinite {
            self.position = Duration::ZERO;
            ResumeIntent::StartAt(Duration::ZERO)
        } else {
            resume
        };
```

placed immediately after `self.set_capabilities(prepared.capabilities);`.

- `dispatch`, `TogglePause`: `PlaybackState::Playing | PlaybackState::Reconnecting => self.pause(),`.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --locked --test m7_live_playback && cargo test --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A src tests/m7_live_playback.rs tests/support/mod.rs
git commit -m "feat(engine): stations load and play; Restart is refused; Reconnecting state exists"
```

---

### Task 7: Fresh-open sequence, live Pause, recovery without seeking

**Files:**
- Modify: `src/playback/engine.rs`
- Test: `tests/m7_live_recovery.rs` (new)

**Interfaces:**
- Consumes: `PrepareContext.expected`, `is_indefinite`, `SourceTraits.indefinite`.
- Produces (all private to `Worker`):
  - `fn fresh_open(&mut self) -> Result<(), PlaybackError>` — §5.2 steps 1–7. `Ok` means `Playing` was announced. On `Err` nothing of the new source remains adopted and no transport is left open.
  - `fn source_ended(&mut self, failure: RemoteFailure)` — the single exit from `pump_audio` for indefinite media. While `self.attempting` it records `self.attempt_failure` and drops the source; otherwise, in this task, it calls `fail_with` (Task 8 replaces that arm).
  - `fn pause_indefinite(&mut self)`.
  - `fn interrupted(&self) -> bool` — `stop_or_shutdown(self.interrupt.load(Ordering::Acquire))`.
  - `fn start_running(&mut self)` — the tail of `prime_and_run`, extracted.
  - `play(&mut self, primed: bool)` — `PlayLoaded` passes `true`, everything else `false`.
- `EngineHandle::submit_pause` retires instead of freezing when `traits.indefinite` is set.

- [ ] **Step 1: Write the failing tests** (L12, L13, L17 for the Play paths, L19)

Create `tests/m7_live_recovery.rs`:

```rust
//! M7 §5.1–5.3, §6.2: recovery preserves listening time and never seeks.

mod support;

use std::time::Duration;

use support::server::{Script, TestServer};
use support::TestEngine;
use tenuto::http::error::RemoteFailure;
use tenuto::media::capabilities::Continuity;
use tenuto::playback::command::PlaybackCommand;
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::state::PlaybackState;

fn station() -> Script {
    Script::from_fixture("sine-noxing.mp3").icy_station()
}

fn playing(event: &PlaybackEvent) -> bool {
    matches!(event, PlaybackEvent::StateChanged { state: PlaybackState::Playing, .. })
}

fn paused(event: &PlaybackEvent) -> bool {
    matches!(event, PlaybackEvent::StateChanged { state: PlaybackState::Paused, .. })
}

fn no_range_above_zero(server: &TestServer) {
    for request in server.requests() {
        if let Some((first, _)) = request.range() {
            assert_eq!(first, 0, "listening time must never become a byte range");
        }
    }
}

#[test]
fn pause_closes_the_connection_and_play_rejoins_with_listening_time_kept() {
    let server = TestServer::start(station());
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/radio"));
    engine.send(PlaybackCommand::Play);
    engine.play_for(Duration::from_millis(500));

    engine.handle().submit_pause();
    engine.await_event(paused);
    let at_pause = engine.handle().progress().position;
    let written = server.bytes_written();
    engine.let_time_pass(Duration::from_millis(300));
    assert!(
        server.bytes_written() - written < 256 * 1024,
        "the server kept streaming into a paused client: the connection was not closed"
    );

    engine.handle().submit_play();
    engine.await_event(playing);
    assert_eq!(server.requests().len(), 2, "Play opens a fresh request");
    let resumed = engine.handle().progress().position;
    assert!(resumed >= at_pause, "listening time went backwards: {resumed:?} < {at_pause:?}");
    engine.play_for(at_pause + Duration::from_millis(400));
    no_range_above_zero(&server);
    assert_eq!(
        engine.count_events(|event| matches!(
            event,
            PlaybackEvent::SeekCompleted { .. } | PlaybackEvent::RestartEstablished { .. }
        )),
        0
    );
    engine.finish();
    server.shutdown();
}

#[test]
fn pause_wakes_a_stalled_live_read_and_thaw_announces_nothing() {
    let server = TestServer::start(station().stall_body_after(24 * 1024));
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/radio"));
    engine.send(PlaybackCommand::Play);
    assert!(server.wait_until_stalled(Duration::from_secs(2)));

    engine.handle().submit_pause();
    engine.await_event(paused);
    // Thaw with no source open: nothing may be released or announced.
    engine.handle().source_interrupt().thaw();
    engine.let_time_pass(Duration::from_millis(200));
    assert_eq!(engine.count_events(playing), 0);
    engine.finish();
    server.release();
    server.shutdown();
}

#[test]
fn stop_then_play_reopens_without_a_seek() {
    let server = TestServer::start(station());
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/radio"));
    engine.send(PlaybackCommand::Play);
    engine.play_for(Duration::from_millis(400));
    engine.interrupt_stop();
    engine.await_event(|event| {
        matches!(event, PlaybackEvent::StateChanged { state: PlaybackState::Stopped, .. })
    });
    let at_stop = engine.handle().progress().position;
    assert!(at_stop > Duration::ZERO);

    engine.send(PlaybackCommand::Play);
    engine.await_event(playing);
    assert_eq!(engine.count_events(|e| matches!(e, PlaybackEvent::SeekRejected { .. })), 0);
    engine.play_for(at_stop + Duration::from_millis(300));
    no_range_above_zero(&server);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_device_fault_recovers_a_station_without_seeking() {
    let server = TestServer::start(station());
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/radio"));
    engine.send(PlaybackCommand::Play);
    engine.play_for(Duration::from_millis(400));
    let before = engine.handle().progress().position;

    engine.force_device_loss();
    engine.await_event(|event| matches!(event, PlaybackEvent::DeviceRecovered { .. }));
    engine.play_for(before + Duration::from_millis(300));
    assert_eq!(server.requests().len(), 1, "a device fault keeps the open decoder");
    no_range_above_zero(&server);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_station_url_that_turns_finite_fails_every_play_path_without_leaking_finite_capabilities() {
    for path in ["pause", "stop"] {
        let server = TestServer::start(station().then(Script::from_fixture("sine-5s.mp3")));
        let mut engine = TestEngine::start_idle();
        engine.load_remote(&server.url("/radio"));
        engine.send(PlaybackCommand::Play);
        engine.play_for(Duration::from_millis(300));
        let kept = if path == "pause" {
            engine.handle().submit_pause();
            engine.await_event(paused);
            engine.handle().progress().position
        } else {
            engine.interrupt_stop();
            engine.await_event(|event| {
                matches!(event, PlaybackEvent::StateChanged { state: PlaybackState::Stopped, .. })
            });
            engine.handle().progress().position
        };

        engine.handle().submit_play();
        let failed = engine.await_event(|event| matches!(event, PlaybackEvent::Failed { .. }));
        let PlaybackEvent::Failed { cause, .. } = failed else { unreachable!() };
        assert_eq!(cause, Some(RemoteFailure::ResourceChanged), "{path}");
        assert_eq!(
            engine.count_events(|event| matches!(
                event,
                PlaybackEvent::CapabilitiesChanged { capabilities, .. }
                    if capabilities.continuity == Continuity::Finite
            )),
            0,
            "{path}: a finite capability event escaped"
        );
        assert_eq!(engine.handle().progress().position, kept, "{path}");
        engine.finish();
        server.shutdown();
    }
}

#[test]
fn a_plain_play_after_load_opens_fresh_but_play_loaded_releases_the_primed_transport() {
    let server = TestServer::start(station());
    let mut engine = TestEngine::start_idle();
    let request = engine.next_request();
    engine.load_remote_as(
        request,
        &server.url("/radio"),
        tenuto::playback::command::ResumeIntent::StartAt(Duration::ZERO),
    );
    engine.send(PlaybackCommand::PlayLoaded { request });
    engine.await_event(playing);
    assert_eq!(server.requests().len(), 1, "PlayLoaded releases what Load primed");
    engine.finish();
    server.shutdown();

    let server = TestServer::start(station());
    let mut engine = TestEngine::start_idle();
    engine.load_remote(&server.url("/radio"));
    engine.send(PlaybackCommand::Play);
    engine.await_event(playing);
    assert_eq!(server.requests().len(), 2, "a plain Play opens at the live edge");
    engine.finish();
    server.shutdown();
}
```

Note for the last test's first half: Task 6's tests send a plain `Play` after `load_remote` and therefore now cost two requests; their `requests().len() == 1` assertions in `tests/m7_live_playback.rs` and `tests/m7_submission.rs`' live cases must switch to `PlayLoaded` or assert on the request count *delta*. Make that edit in this task.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --test m7_live_recovery`
Expected: FAIL — `stop_then_play…` sees `SeekRejected` ("this server cannot resume"); `pause_closes…` sees one request; the device-fault test fails in `rebuild`'s reseek.

- [ ] **Step 3: Implement the helpers**

Add fields to `Worker`: `attempting: bool`, `attempt_failure: Option<RemoteFailure>` (both initialised `false`/`None` in `Worker::new`).

```rust
    fn interrupted(&self) -> bool {
        stop_or_shutdown(self.interrupt.load(Ordering::Acquire))
    }

    fn start_running(&mut self) {
        let mut guard = lock(&self.transport);
        if let Some(core) = guard.as_mut() {
            let generation = core.handshake.generation();
            core.start_running(generation);
        }
    }
```

and make `prime_and_run` end with `self.start_running();` instead of its inline copy.

```rust
    /// M7 §5.2. One sequence for reconnect attempts and for every Play on an
    /// established station. Never seeks; never reports a seek or a restart.
    fn fresh_open(&mut self) -> Result<(), PlaybackError> {
        let Some(location) = self.descriptor.clone() else {
            return Err(PlaybackError::UnsupportedInput {
                path: Default::default(),
                reason: "no media is loaded".into(),
            });
        };
        // 1. Prepared, not adopted. A changed continuity is refused in here.
        let mut context = self.prepare_context();
        context.expected = Some(Continuity::Indefinite);
        let prepared = prepare(&location, &context)?;
        // 2.
        if self.source_interrupt.is_retired() || self.interrupted() {
            return Err(PlaybackError::Cancelled);
        }
        // 3. The old generation's final played position becomes the anchor;
        //    its ring is discarded with it.
        self.capture_and_teardown();
        // 4. Capabilities are unchanged by construction: nothing to announce.
        self.source = Some(prepared.source);
        // 5 + 6. `open_transport(false)` builds conversion for THIS decoder
        //    and primes once. While `attempting`, a source that dies during
        //    priming reports through `attempt_failure` instead of transitioning.
        self.attempting = true;
        self.attempt_failure = None;
        let opened = self.open_transport(false);
        self.attempting = false;
        let primed = self.source.is_some() && (self.pushed_total > 0 || !self.staging.is_empty());
        let failure = self.attempt_failure.take();
        if let Err(error) = opened {
            self.abandon_attempt();
            return Err(error);
        }
        if let Some(failure) = failure {
            self.abandon_attempt();
            return Err(failure.into());
        }
        if !primed {
            self.abandon_attempt();
            return Err(RemoteFailure::LiveEnded.into());
        }
        // 7.
        if self.source_interrupt.is_retired() || self.interrupted() {
            self.abandon_attempt();
            return Err(PlaybackError::Cancelled);
        }
        self.start_running();
        self.announce_playing();
        tracing::debug!(url = %redact_url_of(&location), "live source opened fresh");
        Ok(())
    }

    fn abandon_attempt(&mut self) {
        self.teardown();
        self.source_interrupt.retire();
        self.retire_remote_source();
    }
```

`redact_url_of` is a three-line local helper: `match location { SourceLocation::Http(url) => redact_url(url.as_str()), SourceLocation::LocalPath(_) => String::new() }`.

```rust
    /// The single exit from `pump_audio` for indefinite media.
    fn source_ended(&mut self, failure: RemoteFailure) {
        if self.attempting {
            self.attempt_failure = Some(failure);
            self.source = None;
            return;
        }
        self.fail_with(format!("{failure}"), Some(failure));
    }
```

In `pump_audio`, route the indefinite cases to it:

```rust
                Ok(false) if self.is_indefinite() => {
                    self.source_ended(RemoteFailure::LiveEnded);
                    return;
                }
                Ok(false) => self.source_eof = true,
```

and at the top of the general `Err(error) =>` arm:

```rust
                Err(error) if self.is_indefinite() => {
                    let failure = remote_cause(&error).unwrap_or_else(|| RemoteFailure::Transport {
                        operation: Operation::Read,
                        detail: format!("decoding failed: {error}"),
                    });
                    self.source_ended(failure);
                    return;
                }
```

(after the existing `is_retired_read` arm, which stays first: a cancellation is never a disconnect, §6.1).

- [ ] **Step 4: Implement Pause, Play, `restore`, `rebuild`**

```rust
    /// M7 §6.2. Both pause routes end here: the dispatched `Pause`, and the
    /// one where the hook parked first and already announced `Paused`.
    fn pause_indefinite(&mut self) {
        if !matches!(self.state, PlaybackState::Playing | PlaybackState::Reconnecting) {
            return;
        }
        // No transport will remain for a thaw to release, and
        // `WaitService::park` answers `true` with none — so both the flag and
        // the level go, or the next thaw announces `Playing` over nothing.
        let announced = std::mem::take(&mut lock(&self.facts).frozen_by_hook);
        self.source_interrupt.thaw();
        self.capture_and_teardown();
        self.source_interrupt.retire();
        self.retire_remote_source();
        self.session_rev += 1;
        if announced {
            self.state = PlaybackState::Paused;
        } else {
            self.set_state(PlaybackState::Paused);
        }
    }
```

First statement of `pause()`: `if self.is_indefinite() { self.pause_indefinite(); return; }`.

`play` takes `primed: bool`. `dispatch`: `Play => self.play(false)`, `PlayLoaded` → `self.play(true)`, `TogglePause`'s other arm → `self.play(false)`. New first arms of the `match self.state`:

```rust
            // M7 §6.3: neither forces an attempt nor touches the budget.
            PlaybackState::Reconnecting => {}
            // M7 §5.3: only the PlayLoaded of the load that primed this
            // transport may release it; any other Play rejoins the live edge.
            PlaybackState::Paused if self.is_indefinite() && !primed => self.restore(),
```

`restore()`, new first block:

```rust
        if self.is_indefinite() {
            if self.requested_target.take().is_some() {
                self.warn("a stored seek target was dropped: live media cannot seek".into());
            }
            match self.fresh_open() {
                Ok(()) => {}
                Err(error) if is_cancelled(&error) => {}
                // An explicit Play is one attempt (M7 §7): it fails honestly.
                Err(error) => self.fail_from(error),
            }
            return;
        }
```

`rebuild()`: wrap the reseek so it is skipped for indefinite media:

```rust
        self.capture_and_teardown();
        if !self.is_indefinite() {
            let target = self.position;
            match self.reseek(target) { /* existing arms, unchanged */ }
        }
```

`EngineHandle::submit_pause`:

```rust
    pub fn submit_pause(&self) -> Admission {
        let generation = self.source_interrupt.generation();
        let admission = self.try_send(PlaybackCommand::Pause);
        if admission == Admission::Accepted {
            if self.traits.indefinite.load(Ordering::Acquire) {
                // Closing, not freezing: a frozen stalled read never wakes,
                // because freezing suspends its stall timer (M7 §6.1).
                self.source_interrupt.retire_generation(generation);
            } else {
                self.source_interrupt.freeze();
            }
            let _ = self.wake.try_send(());
        }
        admission
    }
```

`ensure_source_open()` gains the symmetric guard for finite sessions — replace its `prepare` call with:

```rust
        let mut context = self.prepare_context();
        if self.media.is_some() && self.capabilities.continuity == Continuity::Finite {
            context.expected = Some(Continuity::Finite);
        }
        let prepared = prepare(&location, &context)?;
```

- [ ] **Step 5: Run to verify pass**

Run: `cargo test --locked --test m7_live_recovery --test m7_live_playback --test m7_submission --test engine_remote --test engine_contract --test wait_service`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/playback/engine.rs tests/m7_live_recovery.rs tests/m7_live_playback.rs tests/m7_submission.rs
git commit -m "feat(engine): fresh-open sequence; live pause closes the source; recovery never seeks"
```

---

### Task 8: Reconnect policy (pure)

**Files:**
- Create: `src/playback/reconnect.rs`
- Modify: `src/playback/mod.rs` (`pub mod reconnect;`)
- Test: unit tests inside `reconnect.rs`

**Interfaces:**
- Produces:
  ```rust
  #[derive(Clone, Copy, Debug, PartialEq)]
  pub struct ReconnectPolicy { pub backoff: [Duration; 5], pub budget: Duration, pub stable_after: Duration }
  impl Default for ReconnectPolicy   // 1,2,4,8,15 s; 5 min; 30 s
  #[derive(Clone, Debug)]
  pub struct Outage { /* private */ }
  pub enum Next { AttemptAt(Instant), GiveUp }
  impl Outage {
      pub fn begin(now: Instant) -> Self;
      pub fn failed(&mut self, now: Instant, policy: &ReconnectPolicy) -> Next;
      pub fn due(&self, now: Instant) -> bool;
      pub fn playing_from(&mut self, position: Duration);
      pub fn is_over(&self, position: Duration, policy: &ReconnectPolicy) -> bool;
  }
  ```

- [ ] **Step 1: Write the module with its failing tests**

```rust
//! M7 §7: reconnect timing, as pure data. No I/O, no clock of its own — the
//! worker passes `Instant`s and listening time in, so every rule here is an
//! assertion rather than a sleep.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReconnectPolicy {
    /// Delay before attempt n; the last entry repeats.
    pub backoff: [Duration; 5],
    /// Wall time from the outage's first failure, evaluated only when
    /// something fails. It never cuts an in-flight open short and never stops
    /// playback that is succeeding.
    pub budget: Duration,
    /// Listening time that must advance after a reconnect before the outage
    /// is over. Played audio only: bytes and decoded frames do not count.
    pub stable_after: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            backoff: [1, 2, 4, 8, 15].map(Duration::from_secs),
            budget: Duration::from_secs(5 * 60),
            stable_after: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Next {
    AttemptAt(Instant),
    GiveUp,
}

#[derive(Clone, Debug)]
pub struct Outage {
    started: Instant,
    failures: usize,
    next_attempt_at: Instant,
    /// Listening time when the latest reconnect started playing.
    playing_from: Option<Duration>,
}

impl Outage {
    pub fn begin(now: Instant) -> Self {
        Self {
            started: now,
            failures: 0,
            next_attempt_at: now,
            playing_from: None,
        }
    }

    /// A playing connection or an attempt failed.
    pub fn failed(&mut self, now: Instant, policy: &ReconnectPolicy) -> Next {
        self.playing_from = None;
        if now.duration_since(self.started) >= policy.budget {
            return Next::GiveUp;
        }
        let step = self.failures.min(policy.backoff.len() - 1);
        self.failures += 1;
        self.next_attempt_at = now + policy.backoff[step];
        Next::AttemptAt(self.next_attempt_at)
    }

    pub fn due(&self, now: Instant) -> bool {
        self.playing_from.is_none() && now >= self.next_attempt_at
    }

    pub fn playing_from(&mut self, position: Duration) {
        self.playing_from = Some(position);
    }

    pub fn is_over(&self, position: Duration, policy: &ReconnectPolicy) -> bool {
        self.playing_from
            .is_some_and(|from| position.saturating_sub(from) >= policy.stable_after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ReconnectPolicy {
        ReconnectPolicy::default()
    }

    #[test]
    fn backoff_steps_then_repeats_its_last_entry() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        let delays: Vec<u64> = (0..7)
            .map(|_| match outage.failed(t0, &policy()) {
                Next::AttemptAt(at) => at.duration_since(t0).as_secs(),
                Next::GiveUp => panic!("inside the budget"),
            })
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 15, 15, 15]);
    }

    #[test]
    fn the_budget_is_judged_only_when_something_fails() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        assert!(matches!(outage.failed(t0, &policy()), Next::AttemptAt(_)));
        let late = t0 + Duration::from_secs(301);
        assert!(!outage.is_over(Duration::ZERO, &policy()), "time alone ends nothing");
        assert_eq!(outage.failed(late, &policy()), Next::GiveUp);
    }

    #[test]
    fn short_connections_stay_one_outage_and_thirty_played_seconds_end_it() {
        let t0 = Instant::now();
        let mut outage = Outage::begin(t0);
        outage.failed(t0, &policy());
        outage.playing_from(Duration::from_secs(100));
        assert!(!outage.due(t0 + Duration::from_secs(60)), "no attempt while playing");
        assert!(!outage.is_over(Duration::from_secs(102), &policy()));
        // It closed after two seconds: same outage, next backoff step.
        assert_eq!(
            outage.failed(t0 + Duration::from_secs(3), &policy()),
            Next::AttemptAt(t0 + Duration::from_secs(5))
        );
        outage.playing_from(Duration::from_secs(102));
        assert!(outage.is_over(Duration::from_secs(132), &policy()));
    }
}
```

- [ ] **Step 2: Run**

Run: `cargo test --locked --lib reconnect`
Expected: PASS (module and tests are written together; the failing-first cycle for this pure module is the three assertions above run before wiring, in Task 9).

- [ ] **Step 3: Commit**

```bash
git add src/playback/reconnect.rs src/playback/mod.rs
git commit -m "feat(playback): reconnect timing as pure data"
```

---

### Task 9: The reconnect loop

**Files:**
- Modify: `src/playback/engine.rs`, `src/playback/wait.rs` (comment only), `tests/support/mod.rs` (`play_until_event`)
- Test: `tests/m7_reconnect.rs` (new)

**Interfaces:**
- Consumes: `ReconnectPolicy`, `Outage`, `Next`, `fresh_open`, `source_ended`.
- Produces: `EngineHandle::set_reconnect_policy(&self, policy: ReconnectPolicy)` (same `Arc<Mutex<…>>` pattern as `set_http`). Worker fields `outage: Option<Outage>`, `last_failure: Option<RemoteFailure>`, `reconnect_policy: Arc<Mutex<ReconnectPolicy>>`. `fn enter_reconnecting(&mut self, failure: RemoteFailure)`, `fn service_reconnect(&mut self)`.

- [ ] **Step 1: Write the failing tests** (L6, L7, L9, L10, L11, L15, L17 reconnect path, L18 engine half)

First, the harness. `TestEngine::await_event` does **not** advance the virtual device clock, and a disconnect only reaches the decoder once the audio ahead of it has been played. Add to `impl TestEngine` in `tests/support/mod.rs`, beside `play_until_terminal` (it uses the same `ADVANCING`/`FROZEN` modes):

```rust
    /// `await_event` with the device clock running, for an event that only
    /// arrives once buffered audio has been played out.
    pub fn play_until_event(
        &mut self,
        predicate: impl Fn(&PlaybackEvent) -> bool,
    ) -> PlaybackEvent {
        self.set_mode(ADVANCING);
        let event = self.await_event(predicate);
        self.set_mode(FROZEN);
        event
    }
```

Rule for every M7 test from here on: wait with `play_until_event` for anything a *disconnect* causes (`Reconnecting`, the `Playing` that follows it, a budget `Failed`); wait with `await_event` for anything a *command* causes (`Paused`, `Stopped`, `SeekRejected`). `let_time_pass` settles through a command round trip, so use it only while the worker is responsive.

Create `tests/m7_reconnect.rs`:

```rust
//! M7 §7: disconnect is not completion; reconnect is bounded and cancellable.

mod support;

use std::time::Duration;

use support::server::{Script, TestServer};
use support::TestEngine;
use tenuto::http::error::RemoteFailure;
use tenuto::playback::command::PlaybackCommand;
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::reconnect::ReconnectPolicy;
use tenuto::playback::state::PlaybackState;

const CUT: usize = 48 * 1024;

fn station() -> Script {
    Script::from_fixture("sine-noxing.mp3").icy_station()
}

fn quick() -> ReconnectPolicy {
    ReconnectPolicy {
        backoff: [20, 20, 20, 20, 20].map(Duration::from_millis),
        budget: Duration::from_millis(600),
        // CUT is about three seconds of audio. Longer than that, so a
        // connection that plays out its CUT never ends the outage by itself.
        stable_after: Duration::from_secs(10),
    }
}

fn state(wanted: PlaybackState) -> impl Fn(&PlaybackEvent) -> bool {
    move |event| matches!(event, PlaybackEvent::StateChanged { state, .. } if *state == wanted)
}

fn start(server: &TestServer) -> TestEngine {
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(quick());
    let request = engine.next_request();
    engine.load_remote_as(
        request,
        &server.url("/radio"),
        tenuto::playback::command::ResumeIntent::StartAt(Duration::ZERO),
    );
    engine.send(PlaybackCommand::PlayLoaded { request });
    engine.await_event(state(PlaybackState::Playing));
    engine
}

#[test]
fn a_disconnect_reconnects_and_never_ends_the_track() {
    let server = TestServer::start(
        station().truncate_body_after(CUT).truncate_only_first_response(),
    );
    let mut engine = start(&server);
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let during = engine.handle().progress().position;
    engine.play_until_event(state(PlaybackState::Playing));
    let after = engine.handle().progress().position;
    assert!(after >= during, "anchored below the captured value: {after:?} < {during:?}");
    engine.play_for(after + Duration::from_millis(300));
    assert!(!engine.saw_end_of_track());
    assert_eq!(
        engine.count_events(|event| matches!(
            event,
            PlaybackEvent::EndOfTrack { .. }
                | PlaybackEvent::StateChanged { state: PlaybackState::Ended, .. }
        )),
        0
    );
    assert_eq!(server.requests().len(), 2);
    engine.finish();
    server.shutdown();
}

#[test]
fn listening_time_counts_drained_audio_and_then_stands_still() {
    // Every reconnect is refused, so the engine stays in Reconnecting.
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(Script::serving(Vec::new()).status(503)),
    );
    let mut engine = start(&server);
    engine.play_until_event(state(PlaybackState::Reconnecting));
    let at_disconnect = engine.handle().progress().position;
    engine.let_time_pass(Duration::from_millis(400));
    let drained = engine.handle().progress().position;
    assert!(
        drained.saturating_sub(at_disconnect) <= Duration::from_millis(350),
        "more than the ring was counted: {at_disconnect:?} -> {drained:?}"
    );
    engine.let_time_pass(Duration::from_millis(100));
    assert_eq!(engine.handle().progress().position, drained, "silence advanced listening time");
    engine.finish();
    server.shutdown();
}

#[test]
fn a_retryable_refusal_exhausts_the_budget_and_play_then_tries_exactly_once() {
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(Script::serving(Vec::new()).status(503)),
    );
    let mut engine = start(&server);
    let failed = engine.play_until_event(|event| matches!(event, PlaybackEvent::Failed { .. }));
    let PlaybackEvent::Failed { cause, .. } = failed else { unreachable!() };
    assert!(matches!(cause, Some(RemoteFailure::Status { status: 503, .. })), "{cause:?}");
    let attempts = server.requests().len();
    assert!(attempts > 2, "the budget allowed no retries");

    engine.send(PlaybackCommand::Play);
    engine.await_event(|event| matches!(event, PlaybackEvent::Failed { .. }));
    assert_eq!(server.requests().len(), attempts + 1, "an explicit Play is one attempt");
    engine.finish();
    server.shutdown();
}

#[test]
fn a_gone_station_fails_on_the_first_reconnect() {
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(Script::serving(Vec::new()).status(404)),
    );
    let mut engine = start(&server);
    let failed = engine.play_until_event(|event| matches!(event, PlaybackEvent::Failed { .. }));
    let PlaybackEvent::Failed { cause, .. } = failed else { unreachable!() };
    assert!(matches!(cause, Some(RemoteFailure::Status { status: 404, .. })), "{cause:?}");
    assert_eq!(server.requests().len(), 2);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_server_that_keeps_closing_early_is_one_outage() {
    // Every connection dies after CUT bytes: never 400 ms of played audio.
    let server = TestServer::start(station().truncate_body_after(CUT));
    let mut engine = start(&server);
    engine.play_until_event(|event| matches!(event, PlaybackEvent::Failed { .. }));
    assert!(server.requests().len() > 2);
    engine.finish();
    server.shutdown();
}

#[test]
fn an_initial_open_that_fails_is_not_retried() {
    let server = TestServer::start(Script::serving(Vec::new()).status(503));
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(quick());
    engine.load_remote_expecting_failure(&server.url("/radio"));
    engine.let_time_pass(Duration::from_millis(200));
    assert_eq!(server.requests().len(), 1);
    engine.finish();
    server.shutdown();
}

#[test]
fn a_reconnect_to_different_audio_parameters_plays() {
    // sine-22k-mono.mp3: 22050 Hz mono, against the 44.1 kHz stereo station.
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(Script::from_fixture("sine-22k-mono.mp3").icy_station()),
    );
    let mut engine = start(&server);
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    let resumed = engine.handle().progress().position;
    engine.play_for(resumed + Duration::from_millis(500));
    assert!(engine.captured_is_audible());
    engine.finish();
    server.shutdown();
}

#[test]
fn a_source_that_probes_but_dies_while_priming_is_a_failed_attempt() {
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            // Enough to probe, not enough to prime a frame past it.
            .then(station().truncate_body_after(2 * 1024))
            .then(station()),
    );
    let mut engine = start(&server);
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    assert!(server.requests().len() >= 3, "the priming failure was not retried");
    engine.finish();
    server.shutdown();
}

#[test]
fn a_reconnect_that_comes_back_finite_fails_as_resource_changed() {
    let server = TestServer::start(
        station().truncate_body_after(CUT).then(Script::from_fixture("sine-5s.mp3")),
    );
    let mut engine = start(&server);
    let failed = engine.play_until_event(|event| matches!(event, PlaybackEvent::Failed { .. }));
    let PlaybackEvent::Failed { cause, .. } = failed else { unreachable!() };
    assert_eq!(cause, Some(RemoteFailure::ResourceChanged));
    engine.finish();
    server.shutdown();
}

#[test]
fn toggle_pauses_a_reconnect_and_play_during_one_changes_nothing() {
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(Script::serving(Vec::new()).status(503)),
    );
    let mut engine = start(&server);
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.send(PlaybackCommand::Play);
    engine.let_time_pass(Duration::from_millis(50));
    assert_eq!(engine.count_events(state(PlaybackState::Playing)), 0);

    engine.send(PlaybackCommand::TogglePause);
    engine.await_event(state(PlaybackState::Paused));
    let settled = server.requests().len();
    engine.let_time_pass(Duration::from_millis(200));
    assert_eq!(server.requests().len(), settled, "attempts continued after Pause");
    engine.finish();
    server.shutdown();
}

#[test]
fn sustained_playback_ends_the_outage_so_a_later_drop_gets_a_fresh_budget() {
    // Connections 1 and 2 are cut; 3 plays on.
    let server = TestServer::start(
        station()
            .truncate_body_after(CUT)
            .then(station().truncate_body_after(CUT))
            .then(station()),
    );
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(ReconnectPolicy {
        stable_after: Duration::from_millis(500),
        ..quick()
    });
    let request = engine.next_request();
    engine.load_remote_as(
        request,
        &server.url("/radio"),
        tenuto::playback::command::ResumeIntent::StartAt(Duration::ZERO),
    );
    engine.send(PlaybackCommand::PlayLoaded { request });
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    let rejoined = engine.handle().progress().position;
    // Half a second of *played* audio ends the outage...
    engine.play_for(rejoined + Duration::from_millis(700));
    // ...so wall time well past the 600 ms budget no longer matters.
    std::thread::sleep(Duration::from_millis(700));
    engine.play_until_event(state(PlaybackState::Reconnecting));
    engine.play_until_event(state(PlaybackState::Playing));
    assert_eq!(
        engine.count_events(|event| matches!(event, PlaybackEvent::Failed { .. })),
        0,
        "the second drop was judged against the first outage's budget"
    );
    engine.finish();
    server.shutdown();
}
```

Fixture: add `tests/fixtures/sine-22k-mono.mp3` and a README row ("5 s, 22.05 kHz mono MP3, no Xing — a reconnect whose decoder differs from the one it replaces"):

```bash
ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=22050:duration=5" -ac 1 \
  -c:a libmp3lame -b:a 64k -write_xing 0 tests/fixtures/sine-22k-mono.mp3
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --test m7_reconnect`
Expected: compile error on `set_reconnect_policy`; once stubbed, every test fails at `await_event(Reconnecting)` with a `Failed` instead.

- [ ] **Step 3: Implement**

Plumb `reconnect_policy: Arc<Mutex<ReconnectPolicy>>` exactly as `http` is plumbed (`assemble`, `EngineHandle`, `Worker`), with:

```rust
    pub fn set_reconnect_policy(&self, policy: ReconnectPolicy) {
        *lock(&self.reconnect_policy) = policy;
    }
```

Replace `source_ended`'s last line:

```rust
        if failure.is_retryable() {
            self.enter_reconnecting(failure);
        } else {
            self.outage = None;
            self.fail_with(format!("{failure}"), Some(failure));
        }
```

```rust
    /// M7 §7. A playing connection, or an attempt, failed retryably.
    fn enter_reconnecting(&mut self, failure: RemoteFailure) {
        let now = Instant::now();
        let policy = *lock(&self.reconnect_policy);
        let outage = self.outage.get_or_insert_with(|| Outage::begin(now));
        match outage.failed(now, &policy) {
            Next::GiveUp => {
                self.outage = None;
                self.fail_with(format!("{failure}"), Some(failure));
            }
            Next::AttemptAt(_) => {
                tracing::info!(reason = %failure, "live source lost; reconnecting");
                self.last_failure = Some(failure);
                // The output transport stays up so the ring plays out; only
                // the source goes.
                self.source_interrupt.retire();
                self.retire_remote_source();
                self.set_state(PlaybackState::Reconnecting);
            }
        }
    }

    /// Run from the loop, never from a sleep: commands stay serviced through
    /// every backoff.
    fn service_reconnect(&mut self) {
        let policy = *lock(&self.reconnect_policy);
        match self.state {
            PlaybackState::Playing => {
                if self
                    .outage
                    .as_ref()
                    .is_some_and(|outage| outage.is_over(self.position, &policy))
                {
                    self.outage = None;
                }
            }
            PlaybackState::Reconnecting => {
                if !self.outage.as_ref().is_some_and(|outage| outage.due(Instant::now())) {
                    return;
                }
                match self.fresh_open() {
                    Ok(()) => {
                        let position = self.position;
                        if let Some(outage) = self.outage.as_mut() {
                            outage.playing_from(position);
                        }
                    }
                    // The command that cancelled it decides what happens next.
                    Err(error) if is_cancelled(&error) => {}
                    Err(error) => {
                        let failure = match error {
                            PlaybackError::Remote(failure) => failure,
                            other => remote_cause(&other).unwrap_or(RemoteFailure::Transport {
                                operation: Operation::Reopen,
                                detail: other.to_string(),
                            }),
                        };
                        // `fresh_open` may have torn the old transport down
                        // and announced nothing; stay in Reconnecting.
                        self.state = PlaybackState::Reconnecting;
                        if failure.is_retryable() {
                            self.enter_reconnecting(failure);
                        } else {
                            self.outage = None;
                            self.fail_with(format!("{failure}"), Some(failure));
                        }
                    }
                }
            }
            _ => {}
        }
    }
```

A device-open failure (`PlaybackError` that is not remote) maps to a non-retryable path: replace the `unwrap_or(RemoteFailure::Transport …)` fallback with an early `self.outage = None; self.fail_from(other); return;` when `remote_cause(&other)` is `None`. Write it as:

```rust
                        let failure = match error {
                            PlaybackError::Remote(failure) => failure,
                            other => match remote_cause(&other) {
                                Some(failure) => failure,
                                None => {
                                    self.outage = None;
                                    self.fail_from(other);
                                    return;
                                }
                            },
                        };
```

Call `self.service_reconnect();` in `run()` as the first statement of step 7, before the `if self.state == PlaybackState::Playing` pump.

Clear the outage wherever the listener ends the request: `self.outage = None;` in `pause_indefinite`, `do_stop`, the top of `load()`, and `shutdown()`.

`publish_progress` (engine):

```rust
            facts.playing = matches!(
                self.state,
                PlaybackState::Playing | PlaybackState::Paused | PlaybackState::Reconnecting
            );
```

`src/playback/wait.rs`: extend the comment above `if facts.playing {` — it currently says the gate is safe because only `Playing | Paused` publish new spans. Add: "M7: `Reconnecting` keeps the transport running unparked so the ring plays out, and is included in `facts.playing` for exactly the reason this comment warns about — audio heard after a disconnect must be counted."

`Play` while `Reconnecting` is already a no-op (Task 7).

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --locked --test m7_reconnect --test m7_live_recovery --test m7_live_playback && cargo test --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/playback tests/m7_reconnect.rs tests/support/mod.rs tests/fixtures/sine-22k-mono.mp3 tests/fixtures/README.md
git commit -m "feat(engine): bounded, cancellable reconnect for live media"
```

---

### Task 10: Cancellation matrix (L8)

**Files:**
- Test: `tests/m7_cancellation.rs` (new)
- Modify: `src/playback/engine.rs` only if a cell fails

**Interfaces:**
- Consumes: everything above. Produces no new API. This task is the reviewer's gate on §6: a cell that fails is a bug in Tasks 5, 7 or 9, fixed here with its cell as the regression test.

- [ ] **Step 1: Write the matrix**

Create `tests/m7_cancellation.rs`:

```rust
//! M7 §6, L8: every cancelling command, in every situation a live source can
//! be blocked in. "No request" means none for the cancelled operation; a
//! replacement Load issues its own, to its own server.

mod support;

use std::time::{Duration, Instant};

use support::server::{Script, TestServer};
use support::TestEngine;
use tenuto::playback::command::{PlaybackCommand, ResumeIntent};
use tenuto::playback::event::PlaybackEvent;
use tenuto::playback::reconnect::ReconnectPolicy;
use tenuto::playback::state::PlaybackState;

const CUT: usize = 48 * 1024;
/// The handshake deadline (250 ms) plus scheduling slack.
const PROMPT: Duration = Duration::from_millis(450);

#[derive(Clone, Copy, Debug)]
enum Situation {
    Backoff,
    AwaitingHeaders,
    StalledBody,
    DeliveringBody,
    Priming,
}

#[derive(Clone, Copy, Debug)]
enum Cancel {
    Pause,
    Stop,
    Load,
    Shutdown,
}

fn station() -> Script {
    Script::from_fixture("sine-noxing.mp3").icy_station()
}

fn script(situation: Situation) -> Script {
    let cut = station().truncate_body_after(CUT);
    match situation {
        // Refused forever, with a backoff long enough to be standing in.
        Situation::Backoff => cut.then(Script::serving(Vec::new()).status(503)),
        Situation::AwaitingHeaders => cut.then(station().stall_headers()),
        Situation::StalledBody => station().stall_body_after(24 * 1024),
        Situation::DeliveringBody => station(),
        // Probes, then stalls before a frame can be primed.
        Situation::Priming => cut.then(station().stall_body_after(3 * 1024)),
    }
}

fn policy(situation: Situation) -> ReconnectPolicy {
    let step = match situation {
        Situation::Backoff => Duration::from_secs(30),
        _ => Duration::from_millis(20),
    };
    ReconnectPolicy {
        backoff: [step; 5],
        budget: Duration::from_secs(60),
        stable_after: Duration::from_secs(30),
    }
}

fn is(state: PlaybackState) -> impl Fn(&PlaybackEvent) -> bool {
    move |event| matches!(event, PlaybackEvent::StateChanged { state: s, .. } if *s == state)
}

fn arrange(situation: Situation) -> (TestServer, TestEngine) {
    let server = TestServer::start(script(situation));
    let mut engine = TestEngine::start_idle();
    engine.handle().set_reconnect_policy(policy(situation));
    let request = engine.next_request();
    engine.load_remote_as(request, &server.url("/radio"), ResumeIntent::StartAt(Duration::ZERO));
    engine.send(PlaybackCommand::PlayLoaded { request });
    engine.await_event(is(PlaybackState::Playing));
    match situation {
        Situation::DeliveringBody => engine.play_for(Duration::from_millis(200)),
        Situation::StalledBody => assert!(server.wait_until_stalled(Duration::from_secs(2))),
        Situation::Backoff => {
            engine.play_until_event(is(PlaybackState::Reconnecting));
            // Let the first (immediate-ish) attempt be refused.
            let deadline = Instant::now() + Duration::from_secs(2);
            while server.requests().len() < 2 && Instant::now() < deadline {
                engine.let_time_pass(Duration::from_millis(10));
            }
        }
        Situation::AwaitingHeaders | Situation::Priming => {
            engine.play_until_event(is(PlaybackState::Reconnecting));
            assert!(server.wait_until_stalled(Duration::from_secs(2)));
        }
    }
    (server, engine)
}

fn run(situation: Situation, cancel: Cancel) {
    let (server, mut engine) = arrange(situation);
    let before = server.requests().len();
    let other = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let started = Instant::now();
    match cancel {
        Cancel::Pause => {
            engine.handle().submit_pause();
            engine.await_event(is(PlaybackState::Paused));
        }
        Cancel::Stop => {
            engine.interrupt_stop();
            engine.await_event(is(PlaybackState::Stopped));
        }
        Cancel::Load => {
            let request = engine.next_request();
            engine.load_remote_as(
                request,
                &other.url("/b.flac"),
                ResumeIntent::StartAt(Duration::ZERO),
            );
        }
        Cancel::Shutdown => {
            let _ = engine.shutdown_report();
        }
    }
    assert!(
        started.elapsed() < PROMPT,
        "{situation:?}/{cancel:?}: took {:?}",
        started.elapsed()
    );
    if !matches!(cancel, Cancel::Shutdown) {
        engine.let_time_pass(Duration::from_millis(150));
        assert_eq!(
            server.requests().len(),
            before,
            "{situation:?}/{cancel:?}: the cancelled operation issued another request"
        );
        if matches!(cancel, Cancel::Pause | Cancel::Stop) {
            assert_eq!(
                engine.count_events(is(PlaybackState::Playing)),
                0,
                "{situation:?}/{cancel:?}: Playing announced after the cancel"
            );
        }
        engine.finish();
    }
    server.release();
    server.shutdown();
    other.shutdown();
}

macro_rules! cell {
    ($name:ident, $situation:ident, $cancel:ident) => {
        #[test]
        fn $name() {
            run(Situation::$situation, Cancel::$cancel);
        }
    };
}

cell!(backoff_pause, Backoff, Pause);
cell!(backoff_stop, Backoff, Stop);
cell!(backoff_load, Backoff, Load);
cell!(backoff_shutdown, Backoff, Shutdown);
cell!(headers_pause, AwaitingHeaders, Pause);
cell!(headers_stop, AwaitingHeaders, Stop);
cell!(headers_load, AwaitingHeaders, Load);
cell!(headers_shutdown, AwaitingHeaders, Shutdown);
cell!(stalled_pause, StalledBody, Pause);
cell!(stalled_stop, StalledBody, Stop);
cell!(stalled_load, StalledBody, Load);
cell!(stalled_shutdown, StalledBody, Shutdown);
cell!(delivering_pause, DeliveringBody, Pause);
cell!(delivering_stop, DeliveringBody, Stop);
cell!(delivering_load, DeliveringBody, Load);
cell!(delivering_shutdown, DeliveringBody, Shutdown);
cell!(priming_pause, Priming, Pause);
cell!(priming_stop, Priming, Stop);
cell!(priming_load, Priming, Load);
cell!(priming_shutdown, Priming, Shutdown);
```

`count_events` counts from the events the harness has buffered since the last `await_event`; if its contract is "all events ever", record the count before the cancel and assert on the difference.

- [ ] **Step 2: Run**

Run: `cargo test --locked --test m7_cancellation`
Expected: all 20 PASS. For any failing cell: reproduce it alone (`cargo test --locked --test m7_cancellation headers_pause -- --nocapture` with `RUST_LOG=tenuto=debug`), fix the cause in `engine.rs`, and keep the cell. The likely causes, in order: a wait inside `fresh_open` that does not observe `source_interrupt` retirement (`retire_generation` aimed at a generation `prepare` had not yet begun — re-read the generation *after* `try_send` in `submit_pause` if so, as `e51bae3` did for seeks); `pause_indefinite` rejected because `self.state` was still `Playing` while `attempting`.

- [ ] **Step 3: Commit**

```bash
git add tests/m7_cancellation.rs src/playback/engine.rs
git commit -m "test(engine): cancellation matrix for live sources"
```

---

### Task 11: Session checkpoint gate

**Files:**
- Modify: `src/session.rs`
- Test: `tests/m7_session.rs` (new)

**Interfaces:**
- Produces: private `Session.checkpointable: bool` (initially `true`). No public API change.
- Order in `on_loaded` (§9): `record_outgoing` under the **outgoing** gate → reset and adopt → set the gate from this `Loaded`.

- [ ] **Step 1: Write the failing tests** (L16)

Create `tests/m7_session.rs`:

```rust
//! M7 §9: a station never checkpoints, and the gate changes hands in order.

mod support;

use std::time::Duration;

use support::media;
use tenuto::clock::{Clock, FakeClock};
use tenuto::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use tenuto::media::id::MediaId;
use tenuto::persistence::model::PersistedState;
use tenuto::playback::event::{PlaybackEvent, Progress, StartDisposition};
use tenuto::playback::provenance::PositionProvenance;
use tenuto::playback::state::PlaybackState;
use tenuto::playback::timeline::PositionQuality;
use tenuto::session::{CAPTURE_INTERVAL, LoadTarget, Session};

const FINITE: MediaCapabilities = MediaCapabilities {
    continuity: Continuity::Finite,
    seek: SeekSupport::Native,
};
const LIVE: MediaCapabilities = MediaCapabilities {
    continuity: Continuity::Indefinite,
    seek: SeekSupport::Unsupported,
};

fn load(session: &mut Session, rev: u64, id: &MediaId, caps: MediaCapabilities) -> Progress {
    let request = session
        .register_load(LoadTarget::Legacy, id)
        .unwrap_or_else(|error| panic!("room: {error:?}"));
    let clock = FakeClock::new();
    session.observe(
        &PlaybackEvent::Loaded {
            session_rev: rev,
            request,
            media: id.clone(),
            metadata: Default::default(),
            capabilities: caps,
            position: Duration::ZERO,
            disposition: StartDisposition::Fresh,
        },
        clock.sample(),
    );
    session.observe(
        &PlaybackEvent::StateChanged {
            session_rev: rev,
            state: PlaybackState::Playing,
            request: None,
        },
        clock.sample(),
    );
    Progress {
        session_rev: rev,
        media: Some(id.clone()),
        position: Duration::ZERO,
        quality: PositionQuality::Estimated,
        provenance: PositionProvenance::Established,
        buffering: false,
        load: Some(request),
    }
}

fn listen(session: &mut Session, clock: &FakeClock, progress: &mut Progress, to: Duration) {
    progress.position = to;
    clock.advance(CAPTURE_INTERVAL + Duration::from_secs(1));
    session.tick(progress, clock.sample());
}

fn checkpoint(session: &Session, id: &MediaId) -> Option<Duration> {
    session.state().checkpoint_for(id).and_then(|entry| entry.position)
}

#[test]
fn a_station_is_never_checkpointed_by_tick_pause_stop_or_shutdown() {
    let station = media("radio");
    let mut session = Session::new(PersistedState::default());
    let clock = FakeClock::new();
    let mut progress = load(&mut session, 1, &station, LIVE);
    listen(&mut session, &clock, &mut progress, Duration::from_secs(40));
    for state in [PlaybackState::Paused, PlaybackState::Stopped] {
        session.observe(
            &PlaybackEvent::StateChanged { session_rev: 1, state, request: None },
            clock.sample(),
        );
        session.tick(&progress, clock.sample());
    }
    let snapshot = session.shutdown_snapshot(&progress, clock.sample());
    assert_eq!(checkpoint(&session, &station), None);
    assert!(snapshot.checkpoint_for(&station).is_none());
    assert_eq!(snapshot.current_media(), Some(&station), "the station is still current");
}

#[test]
fn loading_a_station_still_checkpoints_the_finite_track_on_its_way_out() {
    let (track, station) = (media("a"), media("radio"));
    let mut session = Session::new(PersistedState::default());
    let clock = FakeClock::new();
    let mut progress = load(&mut session, 1, &track, FINITE);
    listen(&mut session, &clock, &mut progress, Duration::from_secs(12));
    // Advance past the last capture so only `record_outgoing` can write this.
    progress.position = Duration::from_secs(14);
    session.tick(&progress, clock.sample());
    load(&mut session, 2, &station, LIVE);
    assert_eq!(checkpoint(&session, &track), Some(Duration::from_secs(14)));
    assert_eq!(checkpoint(&session, &station), None);
}

#[test]
fn loading_a_finite_track_writes_nothing_for_the_station_on_its_way_out() {
    let (station, track) = (media("radio"), media("a"));
    let mut session = Session::new(PersistedState::default());
    let clock = FakeClock::new();
    let mut progress = load(&mut session, 1, &station, LIVE);
    listen(&mut session, &clock, &mut progress, Duration::from_secs(90));
    let mut next = load(&mut session, 2, &track, FINITE);
    assert_eq!(checkpoint(&session, &station), None);
    listen(&mut session, &clock, &mut next, Duration::from_secs(7));
    assert_eq!(checkpoint(&session, &track), Some(Duration::from_secs(7)), "the gate reopened");
}

#[test]
fn a_url_that_was_finite_and_is_now_live_keeps_its_old_checkpoint_untouched() {
    let id = media("was-a-file");
    let mut session = Session::new(PersistedState::default());
    let clock = FakeClock::new();
    let mut progress = load(&mut session, 1, &id, FINITE);
    listen(&mut session, &clock, &mut progress, Duration::from_secs(33));
    let before = session.state().checkpoint_for(&id).cloned();

    let mut live = load(&mut session, 2, &id, LIVE);
    listen(&mut session, &clock, &mut live, Duration::from_secs(500));
    session.shutdown_snapshot(&live, clock.sample());
    assert_eq!(session.state().checkpoint_for(&id).cloned(), before);
}
```

If `PersistedState` exposes checkpoints under another accessor than `checkpoint_for` / `current_media`, use the one `tests/session_policy.rs` uses; do not add accessors.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --test m7_session`
Expected: tests 1, 3 and 4 FAIL with a checkpoint written for the station.

- [ ] **Step 3: Implement**

Add `checkpointable: bool` to `Session` (doc: "Whether the adopted media may be checkpointed. `false` for `Continuity::Indefinite`: listening time is not a resume point (M7 §9). Set last in `on_loaded`, so `record_outgoing` always runs under the outgoing media's value."), initialised `true` in `new`.

`on_loaded` gains a `capabilities: &MediaCapabilities` parameter (pass it from the `Loaded` arm of `observe`), and sets the gate as its **last** statement before returning `switching`:

```rust
        // Last, deliberately: everything above that writes for the outgoing
        // media has already run under the outgoing media's gate.
        self.checkpointable = capabilities.continuity != Continuity::Indefinite;
```

Gate the writers — first statement of `record_current` and of `record_current_estimated`:

```rust
        if !self.checkpointable {
            return;
        }
```

and of `checkpoint_from_progress`, before the `established` check: `if !self.checkpointable { return false; }`. `record_outgoing` and `capture_current` write only through those two, so they need no edit — verify by reading them, and add the same guard if either writes to `self.state` directly.

In `observe`'s `CapabilitiesChanged` arm, inside the existing `accepts_media_event` acceptance, tighten only — never loosen (§9):

```rust
                if capabilities.continuity == Continuity::Indefinite {
                    self.checkpointable = false;
                }
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --locked --test m7_session --test session_policy --test m5_session_adoption --test provenance_policy --test resume_contract`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/session.rs tests/m7_session.rs
git commit -m "feat(session): indefinite media is never checkpointed"
```

---

### Task 12: Application layer — phase, notices, pause direction, view

**Files:**
- Modify: `src/application/transport.rs`, `src/application/runtime.rs`, `src/application/seek.rs`, `src/application/view.rs`, `src/app.rs` (its `route` call only)
- Test: `tests/m5_transport_rules.rs`, `tests/m7_runtime.rs` (new), `tests/m5_no_network.rs`

**Interfaces:**
- Produces: `PlaybackPhase::Reconnecting`; `TransportSituation.live: bool`; `pub const LIVE_NO_SEEK: &str = "live stream: seeking is unavailable";`; `PlayerView.live: bool`, `PlayerView.reconnecting: bool`. `KeyRouter::route`'s `playing: bool` parameter is renamed `pausable: bool` and callers pass `matches!(state, Playing | Reconnecting)`.

- [ ] **Step 1: Write the failing tests**

Append to `tests/m5_transport_rules.rs` (every existing `TransportSituation { … }` literal in the file gains `live: false`):

```rust
#[test]
fn a_live_entry_answers_every_seek_and_home_with_a_notice() {
    let (queue, _) = with_active();
    for phase in [PlaybackPhase::Playing, PlaybackPhase::Reconnecting, PlaybackPhase::Paused, PlaybackPhase::Stopped] {
        for input in [
            TransportInput::SeekBy(10),
            TransportInput::SeekTo(Duration::from_secs(5)),
            TransportInput::Home,
        ] {
            let situation = TransportSituation {
                queue: &queue,
                selected: None,
                phase,
                last_requested: None,
                live: true,
            };
            assert_eq!(
                decide(input, &situation),
                TransportDecision::Notice(LIVE_NO_SEEK),
                "{phase:?} {input:?}"
            );
        }
    }
}

#[test]
fn reconnecting_is_controlled_like_playing() {
    let (queue, _) = with_active();
    let situation = TransportSituation {
        queue: &queue,
        selected: None,
        phase: PlaybackPhase::Reconnecting,
        last_requested: None,
        live: true,
    };
    assert_eq!(decide(TransportInput::Space, &situation), TransportDecision::TogglePause);
    assert_eq!(decide(TransportInput::Play, &situation), TransportDecision::Play);
}
```

Create `tests/m7_runtime.rs`, using the harness `tests/m5_runtime.rs` uses (`support::runtime`); mirror its construction exactly:

```rust
//! M7 §10: the runtime keeps a reconnecting station pausable and never
//! submits a seek for it. L18 (runtime half), L20.

mod support;

use std::time::Duration;

use support::runtime::RuntimeHarness;
use support::server::{Script, TestServer};
use tenuto::application::runtime::AppCommand;
use tenuto::playback::reconnect::ReconnectPolicy;
use tenuto::playback::state::PlaybackState;

fn station() -> Script {
    Script::from_fixture("sine-noxing.mp3").icy_station()
}

#[test]
fn space_during_a_reconnect_pauses_and_arrow_keys_never_reach_the_engine() {
    let server = TestServer::start(
        station()
            .truncate_body_after(48 * 1024)
            .then(Script::serving(Vec::new()).status(503)),
    );
    let mut harness = RuntimeHarness::with_reconnect_policy(ReconnectPolicy {
        backoff: [Duration::from_millis(20); 5],
        budget: Duration::from_secs(60),
        stable_after: Duration::from_secs(30),
    });
    harness.enqueue_url(&server.url("/radio"));
    harness.command(AppCommand::PlaySelected);
    harness.pump_until(|view| view.reconnecting);
    assert!(harness.view().live);

    harness.command(AppCommand::SeekBy(10));
    harness.pump_for(Duration::from_millis(350)); // past KeyRouter's 250 ms quiet window
    assert!(harness.view().status.as_deref().is_some_and(|s| s.contains("live stream")));

    harness.command(AppCommand::TogglePause);
    harness.pump_until(|view| view.state == Some(PlaybackState::Paused));
    let settled = server.requests().len();
    harness.pump_for(Duration::from_millis(200));
    assert_eq!(server.requests().len(), settled);
    server.shutdown();
}
```

Adapt the harness method names (`command`, `pump_until`, `pump_for`, `enqueue_url`, `view`) to what `tests/support/runtime.rs` really offers; add `with_reconnect_policy` there — it builds the runtime as the default constructor does and wraps `engine_factory` so each engine gets `handle.set_reconnect_policy(policy)` before it is returned. The exact `AppCommand` variant names are in `src/application/runtime.rs:114`.

Append to `tests/m5_no_network.rs`, following its existing restore-a-queue test line for line, with a station URL as the active entry, asserting `server.requests().is_empty()` after startup, a full `pump`, and a render.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --test m5_transport_rules --test m7_runtime --test m5_no_network`
Expected: compile errors (`live`, `Reconnecting`, `LIVE_NO_SEEK`, `view.live`).

- [ ] **Step 3: Implement**

`transport.rs`: add `Reconnecting` to `PlaybackPhase`, `live: bool` to `TransportSituation`, the const, and — first in `decide`, before the empty-queue branch:

```rust
    // M7 §3.4: a rejected seek must be harmless, and the cheapest way is not
    // to submit one. No `KeyRouter` burst opens either.
    if situation.live
        && matches!(
            input,
            TransportInput::Home | TransportInput::SeekBy(_) | TransportInput::SeekTo(_)
        )
        && !matches!(situation.phase, PlaybackPhase::Unloaded | PlaybackPhase::LoadFailed)
    {
        return TransportDecision::Notice(LIVE_NO_SEEK);
    }
```

Then widen every `Playing | Paused | Stopped` pattern in the table to `Playing | Reconnecting | Paused | Stopped`, and the empty-queue branch's `Playing | Paused` to include `Reconnecting`.

`runtime.rs`: `phase()` maps `Some(PlaybackState::Reconnecting) => PlaybackPhase::Reconnecting`; its `live` (engine-is-answering) matcher includes `Reconnecting`. `decide()` passes `live: self.mirror.as_ref().is_some_and(|m| m.capabilities.continuity == Continuity::Indefinite)`. `route()` passes `matches!(mirror.state, PlaybackState::Playing | PlaybackState::Reconnecting)`.

`seek.rs`: rename `route`/`route_command`'s `playing` parameter to `pausable` and update the doc comment: "whether `TogglePause` means *pause*: true while playing **or reconnecting** (M7 §6.3)". `app.rs`: pass the same expression at its call site.

`view.rs`: add `pub live: bool` and `pub reconnecting: bool` to `PlayerView`, filled in `PlayerRuntime::view` from the mirror (`continuity == Indefinite`, `state == Reconnecting`); for a live mirror, `duration` is `None` in the view regardless of metadata.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --locked --test m5_transport_rules --test m7_runtime --test m5_no_network --test m5_runtime --test m5_tui_input`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/application src/app.rs tests
git commit -m "feat(app): reconnecting is pausable; live entries never submit a seek"
```

---

### Task 13: Front ends — TUI and `tenuto play`

**Files:**
- Modify: `src/tui/render.rs`, `src/app.rs`
- Test: `tests/m5_tui_render.rs`, `src/app.rs` unit tests, `tests/http_cli.rs`

**Interfaces:**
- Consumes: `PlayerView.live`, `PlayerView.reconnecting`, `PlaybackState::Reconnecting`.

- [ ] **Step 1: Write the failing tests**

`tests/m5_tui_render.rs` — using its existing `render(view) -> String` helper and view builder (`support::views`), add:

```rust
#[test]
fn a_live_entry_shows_live_and_listening_time_with_no_bar_or_duration() {
    let mut view = support::views::playing("Test Radio", Duration::from_secs(754));
    view.live = true;
    view.now_playing.duration = None;
    let screen = render(&view);
    assert!(screen.contains("LIVE"), "{screen}");
    assert!(screen.contains("12:34"), "{screen}");
    assert!(!screen.contains(" / "), "a live entry has no total: {screen}");
}

#[test]
fn a_reconnecting_station_says_so() {
    let mut view = support::views::playing("Test Radio", Duration::from_secs(5));
    view.live = true;
    view.reconnecting = true;
    assert!(render(&view).contains("reconnecting…"));
}
```

Adapt `support::views::playing` and the field path of the duration to what `tests/support/views.rs` really provides; extend that builder rather than constructing a `PlayerView` by hand.

`src/app.rs` unit tests, beside `an_unresolved_seek_capability_is_marked_distinctly_from_unsupported`:

```rust
    #[test]
    fn a_live_source_reads_live_not_as_a_duration() {
        let mut mirror = mirror_with_capabilities(SeekSupport::Unsupported);
        mirror.capabilities.continuity = Continuity::Indefinite;
        let line = status_line(&mirror);
        assert!(line.contains("live"), "{line}");
    }
```

`tests/http_cli.rs` — beside the existing `--probe-only` tests:

```rust
#[test]
fn probe_only_reports_a_station_as_indefinite_and_exits_zero() {
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let output = run_tenuto(&["play", "--probe-only", &server.url("/radio")]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("continuity: indefinite"), "{stdout}");
    assert!(stdout.contains("Test Radio"), "{stdout}");
    server.shutdown();
}
```

(`run_tenuto` is whatever that file already calls the binary with.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --locked --test m5_tui_render --test http_cli && cargo test --locked --lib app::`
Expected: FAIL on each new assertion.

- [ ] **Step 3: Implement**

`tui/render.rs`, in the progress-line function: when `view.live`, draw `LIVE  <listening time>` with the existing time formatter and skip the gauge and the total; when `view.reconnecting`, replace the state label with `reconnecting…`. Follow the file's existing layout-tier branches; a live line is never wider than the finite one, so no tier changes.

`app.rs`: in `status_line`, where the duration is rendered, `Continuity::Indefinite` prints `live`; the state label comes from `PlaybackState::label()` and needs no edit. In the probe printer, `Continuity::Indefinite` prints `continuity: indefinite` (match the existing spelling of `finite`). Remove the comment at ~`app.rs:944` that says a note must never read as live radio only if it no longer describes the code; otherwise leave it.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --locked --test m5_tui_render --test http_cli --test app_cli --test cli_playback && cargo test --locked --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tui/render.rs src/app.rs tests
git commit -m "feat(ui): LIVE and reconnecting in the player and the play status line"
```

---

### Task 14: Documentation and acceptance record

**Files:**
- Modify: `docs/architecture.md`, `README.md`, `docs/reference.md`, `docs/m1-known-debt.md`, `CHANGELOG.md`
- Create: `docs/m7-acceptance.md`

- [ ] **Step 1: `docs/architecture.md`**

- Line 3 and §1 first paragraph: "…local files, finite remote audio over HTTP, **live HTTP radio**, and podcast episodes…"; replace "Tenuto refuses a live stream instead of playing it." with "A live stream plays as indefinite media: it cannot seek, never completes, and is never checkpointed."
- §1 position contract, new paragraph after the blockquote: "For indefinite media, position is listening time: audio heard since the load. It survives reconnect, pause and stop, excludes silence, and is never a resume point or a seek target. `capabilities.continuity` says which meaning applies."
- §1 fixed rules, replace the first bullet with: "Nothing plays, fetches or refreshes on its own. Every network request follows an explicit user action. Reconnect attempts for a live stream continue a current Play: they are cancellable at once, bounded, and never start or resume after a process restart."
- §7.1: add `Reconnecting` to the states named; add a paragraph: "Out-of-band submission. `submit_seek` retires the source only when it can seek. `submit_pause` closes an indefinite source instead of freezing it. An admitted `Load` retires the source it replaces."
- §7, new §7.5 "Live media": the fresh-open sequence (seven steps, one line each), the disconnect definition, and the policy table from spec §7.
- §8 table: the `Indefinite` row's meaning — "plays; resume capability `Unsupported`". Note `response::Accepted::Live` as the only body-mode model.
- §12: move "live radio" out of limits; add "Shoutcast v1 (`ICY 200 OK`) and streams without ICY headers are not playable."; mention M7.1 (ICY titles) as next.

- [ ] **Step 2: `README.md`, `docs/reference.md`**

Replace "A live stream is refused. Tenuto plays finite media only." and "A dropped connection fails rather than reconnecting…" (README ~149–150; reference ~26–27) with:

```markdown
- A live stream (Icecast, Shoutcast v2) plays without a position bar. It cannot seek or restart, and is never resumed: pausing closes the connection and playing rejoins the live edge.
- If a live stream drops, Tenuto reconnects with backoff for up to five minutes, then fails; Space tries once more. Stop, pause, or another track cancels it immediately.
- A dropped connection on a finite track fails. Playing again makes one attempt to reopen at the saved position.
- A stream that interleaves ICY metadata, an HLS playlist, or a source whose continuity cannot be established is refused.
```

Add a usage line: `tenuto play https://example.org/stream`.

- [ ] **Step 3: `docs/m1-known-debt.md`, `CHANGELOG.md`, `docs/m7-acceptance.md`**

Known debt: two rows — Shoutcast v1 status line; header-less live streams refused (upgrade: a station library with a `--live` override).

Changelog, under a new unreleased heading: "Added: live HTTP radio with bounded reconnect. Changed: a seek on a source that cannot seek no longer drops its connection; loading another track interrupts a stalled one immediately."

`docs/m7-acceptance.md`: a table of L1–L20 → test file and function name (fill from the tests written above), then the manual checklist from spec §14 marked **pending**, in the form `m6-acceptance.md` uses.

- [ ] **Step 4: Gates**

Run:
```bash
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
RUSTDOCFLAGS=-D\ warnings cargo doc --locked --no-deps
```
Expected: all clean.

- [ ] **Step 5: Commit**

```bash
git add docs README.md CHANGELOG.md
git commit -m "docs: live radio replaces the live-stream refusal (M7)"
```

---

## Spec coverage

| Spec | Task |
| --- | --- |
| §3.1 recovery continues a request | 9, 10, 12 (no-network), 14 |
| §3.2 / §5.1 never a seek target | 7 (`no_range_above_zero`, no `SeekCompleted`), 6 (Restart) |
| §3.3 never completes | 3, 6, 9 |
| §3.4 harmless rejections | 5, 6, 12 |
| §3.5 continuity fixed | 4, 7, 9 |
| §3.6 / §4 HTTP rules, L1–L5 | 2, 3, 4 |
| §5.2 fresh-open, L15 | 7, 9 |
| §5.3 initial load, L19 | 7 |
| §6.1–6.3, L8, L12, L18 | 5, 7, 9, 10, 12 |
| §7 policy, L6, L7, L9–L11 (L10: pure in 8, end to end in 9) | 8, 9 |
| §9 session, L16, L20 | 11, 12 |
| §10 front ends | 12, 13 |
| §15 docs | 14 |
| §12 (M7.1), §13 (providers) | not in this plan, by design |

L13's counting-`MediaSource` half is asserted at the wire instead (`no_range_above_zero` plus zero `SeekCompleted`/`RestartEstablished` events): a live source is `byte_seekable = false`, so any decoder seek that moved would fail loudly as `SeekUnavailable` and surface as a `Failed`, which every Task 7 test would catch. If review wants the literal counter, add `seeks: Arc<AtomicUsize>` to `HttpMediaSource` behind `#[cfg(test)]`-free plain code and assert it is zero in Task 7.
