//! The HTTP service: the application's Tokio runtime, the reqwest client,
//! and the one fetch task per source generation.
//!
//! §4's binding rule is that the decode worker must never block on the Tokio
//! runtime. `HttpService::fetch` therefore never awaits anything on the
//! calling thread: it spawns the whole request-plus-body task onto its own
//! runtime and hands back a [`HeaderWait`] the caller blocks on with the same
//! condvar every body read uses. The redirect loop, response validation and
//! body streaming all happen inside that spawned task, never on the caller.

use std::pin::pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT_ENCODING, IF_RANGE, LOCATION, RANGE};
use url::Url;

use super::channel::{ByteChannel, HeaderOutcome, Outcome, SourceInterrupt, WaitHook};
use super::error::{Operation, Phase, RangeRejection, RemoteFailure, redact_url};
use super::limits::Limits;
use super::response::{
    Accepted, Established, FetchAccepted, Headers, accept, accept_redirect, if_range_value,
    validator_from,
};

/// How often the body loop re-tests the freeze flag and re-arms its stall
/// timer. Short enough that a pause is noticed promptly; long enough that
/// this is not a poll loop.
const TICK: Duration = Duration::from_millis(100);

/// One request: where to start, what was already established about the
/// resource (for `If-Range` and validator comparison), and which operation
/// this is for diagnostics.
pub struct FetchRequest {
    pub origin: Url,
    pub start: u64,
    pub established: Option<Established>,
    pub operation: Operation,
}

/// Owns the application's Tokio runtime and the reqwest client.
///
/// One worker thread is enough: there is one active fetch per source
/// generation, never more. The runtime belongs to the application, not the
/// decode worker, so `--probe-only` and the local-file path can run with no
/// runtime at all. Storing the `Runtime` as a plain field is what makes
/// `Drop` shut it down — no bespoke teardown is needed here.
pub struct HttpService {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
    limits: Limits,
}

impl HttpService {
    pub fn spawn(limits: Limits) -> Result<Arc<Self>, RemoteFailure> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|error| RemoteFailure::Transport {
                operation: Operation::Open,
                detail: error.to_string(),
            })?;
        // `Policy::none()` is required, not a preference: §7's hop limit,
        // loop detection, scheme check and downgrade refusal are ours, and
        // reqwest's default redirect policy implements none of them. Letting
        // reqwest follow redirects on its own would silently bypass every
        // one of those rules.
        //
        // HTTP/2's receive window is the transport-level analogue of our own
        // buffer cap: `http2_adaptive_window` defaults to `false`, so without
        // setting these two explicitly a peer may buffer arbitrarily far
        // ahead of what `Limits` promises. reqwest 0.13 has no equivalent
        // knob for HTTP/1.1 (see docs/architecture.md).
        let stream_window = u32::try_from(limits.chunk_bytes).unwrap_or(u32::MAX);
        let connection_window = u32::try_from(limits.buffer_bytes).unwrap_or(u32::MAX);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(limits.connect)
            .http2_initial_stream_window_size(stream_window)
            .http2_initial_connection_window_size(connection_window)
            .build()
            .map_err(|error| RemoteFailure::Transport {
                operation: Operation::Open,
                detail: transport_detail(error),
            })?;
        Ok(Arc::new(Self {
            runtime,
            client,
            limits,
        }))
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// One request. Follows redirects manually, validates the response, and
    /// spawns the task that streams the body into `channel`.
    ///
    /// Never awaits anything on the calling thread: the whole request is
    /// handed to the runtime via `spawn`, and this returns immediately with a
    /// [`HeaderWait`] the caller blocks on synchronously, exactly as it
    /// blocks on body bytes.
    pub fn fetch(
        &self,
        request: FetchRequest,
        channel: ByteChannel,
        generation: u64,
    ) -> HeaderWait {
        let interrupt = Arc::clone(channel.interrupt());
        let client = self.client.clone();
        let limits = self.limits;
        self.runtime
            .spawn(run_fetch(client, limits, request, channel, generation));
        HeaderWait {
            interrupt,
            generation,
        }
    }
}

/// A thin handle onto the header outcome a spawned fetch task will publish.
///
/// Holds nothing but the interrupt every other wait in this generation
/// already hangs off, plus the generation it belongs to. There is
/// deliberately no private `Mutex`/`Condvar` here: a wait that hung off its
/// own condvar would sleep through `retire()`, because `SourceInterrupt`'s
/// `wake_all` only reaches waits parked on its own three wake channels.
pub struct HeaderWait {
    interrupt: Arc<SourceInterrupt>,
    generation: u64,
}

impl HeaderWait {
    /// Cancellable by the same interrupt every read obeys.
    ///
    /// Returning `Err(HeaderOutcome::Failed(Timeout { .. }))` on this call's
    /// own `deadline` does **not** retire the generation — nothing here does.
    /// `HeaderWait` has no `Drop` either, so a caller that gives up on its
    /// own deadline still leaves the spawned fetch task holding the
    /// connection open, streaming into a buffer nobody will drain, until the
    /// caller retires the generation itself. The caller owns that: it is the
    /// one that knows whether it is about to retry, reopen at a different
    /// byte, or give up for good.
    pub fn wait(
        &self,
        service: &dyn WaitHook,
        deadline: Duration,
    ) -> Result<FetchAccepted, HeaderOutcome> {
        self.interrupt
            .wait_for_headers(self.generation, service, deadline)
    }
}

/// Renders a `reqwest::Error` without the URL its `Display` would otherwise
/// embed verbatim — which, for a signed media URL, is a query string a
/// diagnostic must never carry (§11). `redact_url` reattaches a safe form of
/// the same URL when the error had one at all.
fn transport_detail(error: reqwest::Error) -> String {
    let redacted = error.url().map(|url| redact_url(url.as_str()));
    let message = error.without_url().to_string();
    match redacted {
        Some(redacted) => format!("{message} for url ({redacted})"),
        None => message,
    }
}

/// The request-plus-body task `HttpService::fetch` spawns: one per source
/// generation, running entirely on the service's runtime.
async fn run_fetch(
    client: reqwest::Client,
    limits: Limits,
    request: FetchRequest,
    channel: ByteChannel,
    generation: u64,
) {
    let interrupt = Arc::clone(channel.interrupt());
    let FetchRequest {
        origin,
        start,
        established,
        operation,
    } = request;

    // The header phase's cancellation wrapper. Dropping the future on
    // timeout is fine here — the request is being abandoned outright — and
    // it does not re-test the freeze, which is also fine: a pause cannot
    // arrive before the source it would pause exists yet.
    macro_rules! cancellable {
        ($fut:expr, $timeout:expr, $phase:expr) => {
            tokio::select! {
                biased;
                () = interrupt.cancelled(generation) => return,
                result = $fut => result,
                () = tokio::time::sleep($timeout) => {
                    interrupt.publish_headers(
                        generation,
                        Err(RemoteFailure::Timeout { phase: $phase }),
                    );
                    return;
                }
            }
        };
    }

    let mut current = origin;
    let mut seen: Vec<Url> = Vec::new();
    let mut redirects: u8 = 0;

    // Manual redirect loop: reqwest's own policy is disabled (`spawn`'s
    // client is built with `Policy::none()`), so every hop is validated here
    // before it is followed, and the count is bounded by `limits.max_redirects`.
    let mut response = loop {
        let mut builder = client
            .get(current.clone())
            .header(RANGE, format!("bytes={start}-"))
            .header(ACCEPT_ENCODING, "identity");
        if let Some(established) = &established
            && let Some(if_range) = if_range_value(&established.validator)
        {
            builder = builder.header(IF_RANGE, if_range);
        }
        let built = match builder.build() {
            Ok(built) => built,
            Err(error) => {
                interrupt.publish_headers(
                    generation,
                    Err(RemoteFailure::Transport {
                        operation,
                        detail: transport_detail(error),
                    }),
                );
                return;
            }
        };

        let outcome = cancellable!(client.execute(built), limits.headers, Phase::Headers);
        let response = match outcome {
            Ok(response) => response,
            Err(error) => {
                interrupt.publish_headers(
                    generation,
                    Err(RemoteFailure::Transport {
                        operation,
                        detail: transport_detail(error),
                    }),
                );
                return;
            }
        };

        let status = response.status().as_u16();
        let location = if (300..400).contains(&status) {
            response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        } else {
            None
        };

        let Some(location) = location else {
            break response;
        };

        match accept_redirect(&current, &location, redirects + 1, &seen, &limits) {
            Ok(target) => {
                seen.push(current.clone());
                current = target;
                redirects += 1;
            }
            Err(failure) => {
                interrupt.publish_headers(generation, Err(failure));
                return;
            }
        }
    };

    let status = response.status().as_u16();
    let headers = Headers::from_map(response.headers());
    let accepted = match accept(status, &headers, start, start == 0, established.as_ref()) {
        Ok(accepted) => accepted,
        Err(failure) => {
            interrupt.publish_headers(generation, Err(failure));
            return;
        }
    };

    // The interval this response promises, so the body loop can tell a
    // short body (§7: `TruncatedBody`) from a body that simply ended a
    // smaller-than-total interval exactly where it said it would (`Eof`,
    // not media EOF).
    let advertised: Option<u64> = match accepted {
        Accepted::Sequential { len } => len,
        Accepted::Ranged { range } => range.len(),
    };

    let validator = validator_from(&headers);
    interrupt.publish_headers(
        generation,
        Ok(FetchAccepted {
            accepted,
            validator,
            headers,
            redirects,
        }),
    );

    let mut delivered: u64 = 0;
    // Never zero: `bytes.chunks(0)` panics, and `Limits` is injectable with
    // no validating constructor, so a caller-supplied zero must not reach it.
    let chunk_cap = limits.chunk_bytes.max(1);
    loop {
        // Pinned once per chunk, polled across many timer slices. `&mut
        // chunk` in the select leaves the future in place when another
        // branch wins, so nothing is dropped mid-poll and no delivered bytes
        // are lost — `Response::chunk` is not documented cancel-safe, and
        // rebuilding it on every slice could lose buffered data.
        let mut chunk = pin!(response.chunk());
        let mut demanded = Duration::ZERO;
        let next = loop {
            // Re-tested every slice, not once at the top: this is what makes
            // a pause arriving mid-await suspend the budget rather than
            // watch it run out (§8, "time spent paused ... does not count as
            // a server stall").
            interrupt.wait_while_frozen(generation).await;
            let slice = TICK.min(limits.stall - demanded);
            let started = Instant::now();
            tokio::select! {
                biased;
                () = interrupt.cancelled(generation) => return,
                result = &mut chunk => break result,
                () = tokio::time::sleep(slice) => {
                    // Only unfrozen time is charged. A freeze that lands
                    // inside the sleep is caught by the next pass's
                    // `wait_while_frozen`.
                    if !interrupt.is_frozen() {
                        demanded += started.elapsed();
                    }
                    if demanded >= limits.stall {
                        channel.finish(generation, Outcome::Failed(
                            RemoteFailure::Timeout { phase: Phase::Stall },
                        ));
                        return;
                    }
                }
            }
        };

        let ended = match next {
            Ok(Some(bytes)) => {
                let mut offset = 0usize;
                while offset < bytes.len() {
                    let (piece, exceeded) =
                        clamp_to_advertised(chunk_cap, advertised, delivered, &bytes[offset..]);
                    let took = piece.len();
                    // Bytes past the advertised interval belong to different
                    // media and must never reach a reader — clamped and
                    // refused *before* the push, not pushed and checked
                    // after, which would have already handed a decoder up to
                    // `chunk_bytes` of the wrong recording.
                    if !piece.is_empty() && !channel.push(generation, piece).await {
                        return;
                    }
                    delivered += took as u64;
                    offset += took;
                    if exceeded {
                        channel.finish(
                            generation,
                            Outcome::Failed(RemoteFailure::InvalidRange {
                                reason: RangeRejection::LengthMismatch,
                            }),
                        );
                        return;
                    }
                }
                drop(bytes);
                continue;
            }
            Ok(None) => None,
            Err(error) => Some(transport_detail(error)),
        };

        channel.finish(
            generation,
            classify_body_end(advertised, delivered, operation, ended),
        );
        return;
    }
}

/// Clamp one raw piece of body to at most `chunk_cap` bytes and, when a
/// total is advertised, to no more than what remains of it.
///
/// Pure and pinned by a direct unit test rather than only by what a real
/// transport happens to be willing to hand a decoder: a `Content-Length`
/// body can never legitimately overshoot what its own header promised — a
/// standards-observing client enforces that as a hard cap itself, which two
/// throwaway probes against hyper 1.11.1 confirmed (a body that fully
/// satisfies its declared length never yields another chunk or an error, and
/// a server that lies with a *shorter* `Content-Length` than it writes never
/// hands the excess to `chunk()` at all). So this boundary — the second half
/// of it, specifically, "flag and stop before pushing the excess" — cannot be
/// driven by any conformant origin, and a unit test is the only sound way to
/// pin it.
///
/// Returns the piece to push and whether the advertised total was reached or
/// crossed by it, in which case nothing past `piece` may be processed.
fn clamp_to_advertised(
    chunk_cap: usize,
    advertised: Option<u64>,
    delivered: u64,
    raw: &[u8],
) -> (&[u8], bool) {
    let want = chunk_cap.min(raw.len());
    match advertised {
        Some(total) => {
            let remaining = total.saturating_sub(delivered);
            if remaining == 0 {
                return (&raw[..0], true);
            }
            // `remaining` is capped against `want`, itself a `usize`, before
            // the cast back — it never truncates.
            let allowed = remaining.min(want as u64) as usize;
            (&raw[..allowed], allowed < want)
        }
        None => (&raw[..want], false),
    }
}

/// Classify how the body loop's outstanding read ended, once it is known to
/// have ended: how many bytes this response advertised (if any), how many
/// were actually delivered, and — if an error ended it — its already-redacted
/// detail text.
///
/// Pure, and unit-tested directly for the same reason as
/// [`clamp_to_advertised`]: a real `Content-Length` body can only ever end in
/// `Err` when short (confirmed empirically — see that function's doc), never
/// in a clean `Ok(None)`, which makes the `delivered == total` side of this
/// match — an error with nothing left owed — impossible to drive through a
/// real socket. Merging the `Ok(None)` and `Err` signals here, rather than
/// mapping every `Err` straight to `Transport`, is what makes a short body
/// report `TruncatedBody` at all: a real HTTP/1.1 origin signals a shortfall
/// as an `Err`, never as a clean `Ok(None)`.
fn classify_body_end(
    advertised: Option<u64>,
    delivered: u64,
    operation: Operation,
    error: Option<String>,
) -> Outcome {
    match advertised {
        Some(total) if delivered < total => Outcome::Failed(RemoteFailure::TruncatedBody {
            missing: total - delivered,
        }),
        _ => match error {
            None => Outcome::Eof,
            Some(detail) => Outcome::Failed(RemoteFailure::Transport { operation, detail }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_piece_within_the_advertised_total_is_pushed_whole_and_not_flagged() {
        let (piece, exceeded) = clamp_to_advertised(64, Some(10), 4, b"abcdef");
        assert_eq!(piece, b"abcdef");
        assert!(!exceeded);
    }

    #[test]
    fn a_piece_that_would_cross_the_advertised_total_is_clamped_and_flagged() {
        let (piece, exceeded) = clamp_to_advertised(64, Some(10), 8, b"abcdef");
        assert_eq!(piece, b"ab", "only the two bytes still owed may be pushed");
        assert!(exceeded);
    }

    #[test]
    fn nothing_is_pushed_once_the_advertised_total_is_already_reached() {
        let (piece, exceeded) = clamp_to_advertised(64, Some(10), 10, b"abcdef");
        assert!(
            piece.is_empty(),
            "not one byte past the total may reach a reader"
        );
        assert!(exceeded);
    }

    #[test]
    fn without_an_advertised_total_a_piece_is_bounded_only_by_the_chunk_cap() {
        let (piece, exceeded) = clamp_to_advertised(4, None, 1_000_000, b"abcdefgh");
        assert_eq!(piece, b"abcd");
        assert!(!exceeded);
    }

    #[test]
    fn a_shortfall_is_truncated_whether_or_not_an_error_carried_it() {
        assert_eq!(
            classify_body_end(Some(100), 40, Operation::Open, Some("reset".to_string())),
            Outcome::Failed(RemoteFailure::TruncatedBody { missing: 60 })
        );
        // A real Content-Length body never signals a shortfall this way —
        // the classifier must not depend on that to stay correct.
        assert_eq!(
            classify_body_end(Some(100), 40, Operation::Open, None),
            Outcome::Failed(RemoteFailure::TruncatedBody { missing: 60 })
        );
    }

    #[test]
    fn an_error_once_the_advertised_total_is_fully_delivered_is_transport_not_truncation() {
        assert_eq!(
            classify_body_end(Some(100), 100, Operation::Open, Some("reset".to_string())),
            Outcome::Failed(RemoteFailure::Transport {
                operation: Operation::Open,
                detail: "reset".to_string(),
            })
        );
    }

    #[test]
    fn an_error_with_nothing_advertised_is_transport() {
        assert_eq!(
            classify_body_end(None, 40, Operation::Open, Some("reset".to_string())),
            Outcome::Failed(RemoteFailure::Transport {
                operation: Operation::Open,
                detail: "reset".to_string(),
            })
        );
    }

    #[test]
    fn a_clean_end_at_or_without_an_advertised_total_is_eof() {
        assert_eq!(
            classify_body_end(Some(100), 100, Operation::Open, None),
            Outcome::Eof
        );
        assert_eq!(
            classify_body_end(None, 40, Operation::Open, None),
            Outcome::Eof
        );
    }
}
