//! M7 §4: a live response at the HTTP seam.

mod support;

use std::io::Read;
use std::sync::Arc;
use std::time::Instant;

use support::server::{Script, TestServer};
use tenuto::http::channel::{SourceInterrupt, WaitHook};
use tenuto::http::error::RemoteFailure;
use tenuto::http::limits::Limits;
use tenuto::http::service::HttpService;
use tenuto::http::source::{HttpMediaSource, OpeningDeadline, remote_cause};

// `NoopHook` does not exist in `src/http/channel.rs` (verified); mirrors the
// local `NoHook` that `tests/http_source.rs` already defines for the same
// purpose, since a `mod support` item cannot be shared across test binaries.
struct NoopHook;
impl WaitHook for NoopHook {
    fn service(&self) {}
}

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
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .without_ranges(),
    );
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
            .without_ranges()
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
