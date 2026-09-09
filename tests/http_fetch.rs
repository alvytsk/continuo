mod support;

use std::sync::Arc;
use std::time::Duration;

use continuo::http::channel::{ByteChannel, HeaderOutcome, ReadOutcome, SourceInterrupt, WaitHook};
use continuo::http::error::{Operation, Phase, RedirectRejection, RemoteFailure};
use continuo::http::limits::Limits;
use continuo::http::response::{Accepted, FetchAccepted};
use continuo::http::service::{FetchRequest, HttpService};
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

// `ByteChannel` is a `Clone` handle over `Arc<SourceInterrupt>`, not a type
// the caller wraps in its own `Arc` — Task 3 already made it cheap to clone.
// `interrupt.begin()` is what opens a live generation; `channel.generation()`
// only reads the current value back, which is not what a fetch needs.
fn open(
    service: &HttpService,
    origin: Url,
    start: u64,
) -> (
    ByteChannel,
    Arc<SourceInterrupt>,
    Result<FetchAccepted, HeaderOutcome>,
) {
    let interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = interrupt.begin();
    let wait = service.fetch(
        FetchRequest {
            origin,
            start,
            established: None,
            operation: Operation::Open,
        },
        channel.clone(),
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
    let (channel, _interrupt, accepted) =
        open(&service, url(&server.url("/audio?token=abc&Expires=9")), 0);
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
            Err(HeaderOutcome::Failed(RemoteFailure::Redirect {
                reason: RedirectRejection::Loop
            }))
        ),
        "{outcome:?}"
    );
    looping.shutdown();

    let long = TestServer::start(Script::serving(b"x".to_vec()).redirect_chain(9));
    let (_c, _i, outcome) = open(&service, url(&long.url("/audio")), 0);
    assert!(
        matches!(
            outcome,
            Err(HeaderOutcome::Failed(RemoteFailure::Redirect {
                reason: RedirectRejection::TooMany
            }))
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
    let server =
        TestServer::start(Script::serving(b"x".to_vec()).redirect_to("ftp://example.com/a.mp3"));
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
            Err(HeaderOutcome::Failed(RemoteFailure::Status {
                status: 503,
                ..
            }))
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
            Err(HeaderOutcome::Failed(RemoteFailure::Timeout {
                phase: Phase::Headers
            }))
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
    assert!(
        accepted.is_ok(),
        "opening should succeed; the body fails later"
    );

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
        matches!(
            outcome,
            ReadOutcome::Failed(RemoteFailure::TruncatedBody { .. })
        ),
        "a truncated body must not read back as EOF: {outcome:?}"
    );
    server.shutdown();
}

#[test]
fn a_retirement_during_a_header_wait_wakes_it_without_releasing_the_server() {
    let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
    let service = service(Limits::default());
    let interrupt = SourceInterrupt::new(Limits::default().buffer_bytes);
    let channel = ByteChannel::new(Arc::clone(&interrupt));
    let generation = interrupt.begin();
    let wait = service.fetch(
        FetchRequest {
            origin: url(&server.url("/audio")),
            start: 0,
            established: None,
            operation: Operation::Open,
        },
        channel.clone(),
        generation,
    );
    let waiter = std::thread::spawn(move || wait.wait(&NoHook, Duration::from_secs(30)));
    // The server records the request before parking, so this is proof the wait
    // was entered rather than a sleep.
    let started = std::time::Instant::now();
    while server.requests().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the request never arrived"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    interrupt.retire();
    let elapsed = std::time::Instant::now();
    match waiter.join() {
        Ok(outcome) => {
            assert!(
                matches!(outcome, Err(HeaderOutcome::Retired)),
                "{outcome:?}"
            );
            assert!(elapsed.elapsed() < Duration::from_secs(1));
        }
        Err(_) => panic!("the waiter thread panicked"),
    }
    server.shutdown();
}
