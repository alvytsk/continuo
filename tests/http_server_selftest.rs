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

#[test]
fn a_multipart_range_response_carries_the_multipart_content_type() {
    let server = TestServer::start(Script::serving(b"0123456789".to_vec()).multipart_range());
    let response = raw(&server, "/audio", "Range: bytes=3-5\r\n");
    assert!(response.starts_with("HTTP/1.1 206"), "{response}");
    assert!(
        response.contains("Content-Type: multipart/byteranges; boundary=x"),
        "{response}"
    );
    server.shutdown();
}

#[test]
fn a_reversed_range_is_answered_with_416_rather_than_panicking() {
    let server = TestServer::start(Script::serving(b"0123456789".to_vec()));
    let response = raw(&server, "/audio", "Range: bytes=5-2\r\n");
    assert!(response.starts_with("HTTP/1.1 416"), "{response}");
    assert!(response.contains("Content-Range: bytes */10"), "{response}");
    server.shutdown();
}

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
