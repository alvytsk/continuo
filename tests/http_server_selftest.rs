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
    let request =
        format!("GET {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n{extra}\r\n");
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
    assert!(
        response.contains("Content-Range: bytes 3-5/10"),
        "{response}"
    );
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
            let request =
                format!("GET {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
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
