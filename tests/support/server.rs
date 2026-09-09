//! A loopback HTTP/1.1 server the tests can hold still.
//!
//! Hand-rolled rather than a mocking crate because §12's acceptance evidence
//! needs raw framing, a body that stalls at a chosen byte and stays stalled
//! until the test releases it, and a socket that closes mid-body. No mocking
//! library exposes those, and each cancellation test must first *prove* the
//! wait it targets was entered rather than sleeping and hoping.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use socket2::{Domain, Socket, Type};

#[allow(clippy::unwrap_used)] // A poisoned harness mutex means a test already failed.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap()
}

#[allow(clippy::unwrap_used)] // A poisoned harness mutex means a test already failed.
fn wait_while<'a, T, F>(
    condvar: &Condvar,
    guard: MutexGuard<'a, T>,
    condition: F,
) -> MutexGuard<'a, T>
where
    F: FnMut(&mut T) -> bool,
{
    condvar.wait_while(guard, condition).unwrap()
}

#[allow(clippy::unwrap_used)] // A poisoned harness mutex means a test already failed.
fn wait_timeout_while<'a, T, F>(
    condvar: &Condvar,
    guard: MutexGuard<'a, T>,
    timeout: Duration,
    condition: F,
) -> (MutexGuard<'a, T>, bool)
where
    F: FnMut(&mut T) -> bool,
{
    let (guard, result) = condvar
        .wait_timeout_while(guard, timeout, condition)
        .unwrap();
    (guard, result.timed_out())
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
    // Not in the brief's outline, but `trickle` is in the Interfaces block,
    // which the task ruling makes binding: pacing needs its own state.
    trickle: Option<(usize, Duration)>,
}

impl Script {
    pub fn serving(body: Vec<u8>) -> Self {
        Self {
            body,
            ranges: true,
            advertise_accept_ranges: true,
            etag: Some("\"v1\"".to_string()),
            ..Default::default()
        }
    }

    /// Reads `tests/fixtures/<name>`. A missing fixture panics with the path
    /// rather than reporting a test failure: the repository is broken, not
    /// the server under test.
    pub fn from_fixture(name: &str) -> Self {
        let path = format!("tests/fixtures/{name}");
        let body =
            std::fs::read(&path).unwrap_or_else(|error| panic!("missing fixture {path}: {error}"));
        Self::serving(body)
    }

    pub fn without_ranges(mut self) -> Self {
        self.ranges = false;
        self
    }

    pub fn without_accept_ranges_header(mut self) -> Self {
        self.advertise_accept_ranges = false;
        self
    }

    pub fn chunked(mut self) -> Self {
        self.chunked = true;
        self
    }

    pub fn live(mut self) -> Self {
        self.live = true;
        self
    }

    pub fn unresolved(mut self) -> Self {
        self.unresolved = true;
        self
    }

    pub fn weak_etag(mut self) -> Self {
        self.weak_etag = true;
        self
    }

    pub fn no_validator(mut self) -> Self {
        self.etag = None;
        self
    }

    pub fn changing_etag_after(mut self, requests: usize) -> Self {
        self.etag_changes_after = Some(requests);
        self
    }

    pub fn redirect_chain(mut self, hops: usize) -> Self {
        self.redirect_hops = hops;
        self
    }

    pub fn redirect_loop(mut self) -> Self {
        self.redirect_loop = true;
        self
    }

    pub fn redirect_to(mut self, location: &str) -> Self {
        self.redirect_to = Some(location.to_string());
        self
    }

    pub fn status(mut self, code: u16) -> Self {
        self.status_override = Some(code);
        self
    }

    pub fn gzip_encoded(mut self) -> Self {
        self.content_encoding = Some("gzip".to_string());
        self
    }

    pub fn multipart_range(mut self) -> Self {
        self.multipart = true;
        self
    }

    pub fn content_range_override(mut self, value: &str) -> Self {
        self.content_range_override = Some(value.to_string());
        self
    }

    pub fn range_answered_with_200(mut self) -> Self {
        self.answer_range_with_200 = true;
        self
    }

    pub fn truncate_body_after(mut self, bytes: usize) -> Self {
        self.truncate_after = Some(bytes);
        self
    }

    pub fn stall_body_after(mut self, bytes: usize) -> Self {
        self.stall_after = Some(bytes);
        self
    }

    pub fn stall_headers(mut self) -> Self {
        self.stall_headers = true;
        self
    }

    /// Emit `bytes` every `gap`, forever. Every individual read stays inside a
    /// generous stall budget, which is what makes an opening deadline checked
    /// only *around* probing useless.
    pub fn trickle(mut self, bytes: usize, gap: Duration) -> Self {
        self.trickle = Some((bytes, gap));
        self
    }
}

/// One releasable barrier, shared between the connection thread that parks
/// at its scripted stall point and the test thread that proves the wait was
/// entered before letting it go. Two `Mutex`+`Condvar` pairs rather than one
/// so `wait_until_stalled` never has to poll: it blocks on `stalled_cv` and
/// is woken exactly when a connection parks.
struct StallGate {
    stalled: Mutex<bool>,
    stalled_cv: Condvar,
    released: Mutex<bool>,
    released_cv: Condvar,
}

impl StallGate {
    fn new() -> Self {
        Self {
            stalled: Mutex::new(false),
            stalled_cv: Condvar::new(),
            released: Mutex::new(false),
            released_cv: Condvar::new(),
        }
    }

    /// Called by a connection thread once it has reached its stall point.
    /// Announces the stall to any `wait_until_stalled` caller, then blocks
    /// until `release()` (or `shutdown()`'s forced release) wakes it.
    fn park(&self) {
        *lock(&self.stalled) = true;
        self.stalled_cv.notify_all();

        let guard = lock(&self.released);
        let mut guard = wait_while(&self.released_cv, guard, |released| !*released);
        // Reset so the same gate can stage a fresh stall on a later request.
        *guard = false;
        drop(guard);

        *lock(&self.stalled) = false;
    }

    /// Wakes a parked connection. Returns `false` if nothing is currently
    /// parked.
    fn release(&self) -> bool {
        if !*lock(&self.stalled) {
            return false;
        }
        *lock(&self.released) = true;
        self.released_cv.notify_all();
        true
    }

    /// Blocks until a connection has parked, or `patience` elapses.
    fn wait_until_stalled(&self, patience: Duration) -> bool {
        let guard = lock(&self.stalled);
        let (_guard, timed_out) =
            wait_timeout_while(&self.stalled_cv, guard, patience, |stalled| !*stalled);
        !timed_out
    }
}

/// Tracks pacing and the two byte-count trip wires (`truncate_after`,
/// `stall_after`) across the individual writes that make up one response
/// body, and frames each write as an HTTP/1.1 chunk when `chunked` is set.
struct BodyWriter {
    chunked: bool,
    chunk_size: usize,
    gap: Duration,
    truncate_after: Option<usize>,
    stall_after: Option<usize>,
    stalled_once: bool,
    sent: usize,
}

impl BodyWriter {
    fn new(script: &Script, chunked: bool) -> Self {
        let (chunk_size, gap) = script.trickle.unwrap_or((4096, Duration::ZERO));
        Self {
            chunked,
            chunk_size: chunk_size.max(1),
            gap,
            truncate_after: script.truncate_after,
            stall_after: script.stall_after,
            stalled_once: false,
            sent: 0,
        }
    }

    /// The next write must not cross a trip wire, or the stall/truncation
    /// would only be observed after bytes past it were already on the wire.
    fn next_len(&self, remaining: usize) -> usize {
        let mut len = self.chunk_size.min(remaining);
        if let Some(limit) = self.truncate_after
            && self.sent < limit
        {
            len = len.min(limit - self.sent);
        }
        if !self.stalled_once
            && let Some(limit) = self.stall_after
            && self.sent < limit
        {
            len = len.min(limit - self.sent);
        }
        len.max(1)
    }

    /// Writes one piece of the body. Returns `false` once the connection
    /// should stop: the client went away, or a truncation point fired.
    fn write_piece(
        &mut self,
        stream: &mut TcpStream,
        gate: &StallGate,
        bytes_written: &AtomicUsize,
        data: &[u8],
    ) -> bool {
        let wrote = if self.chunked {
            let mut framed = format!("{:x}\r\n", data.len()).into_bytes();
            framed.extend_from_slice(data);
            framed.extend_from_slice(b"\r\n");
            stream.write_all(&framed)
        } else {
            stream.write_all(data)
        };
        if wrote.is_err() {
            return false;
        }
        bytes_written.fetch_add(data.len(), Ordering::SeqCst);
        self.sent += data.len();

        if let Some(limit) = self.truncate_after
            && self.sent >= limit
        {
            let _ = stream.shutdown(Shutdown::Both);
            return false;
        }

        if !self.stalled_once
            && let Some(limit) = self.stall_after
            && self.sent >= limit
        {
            self.stalled_once = true;
            gate.park();
        }

        if !self.gap.is_zero() {
            std::thread::sleep(self.gap);
        }

        true
    }

    /// The chunked terminator. An endless body never calls this — it has no
    /// end to mark.
    fn finish(&self, stream: &mut TcpStream) {
        if self.chunked {
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    }
}

fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        410 => "Gone",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Status",
    }
}

/// `/audio` -> hop 1, `/audio-k` -> hop `k + 1` while `k < hops`, `/audio-n`
/// (the last hop) serves the body — the chain terminates by simply not
/// matching any of the redirecting rules below.
fn redirect_location(script: &Script, path: &str) -> Option<String> {
    if let Some(location) = &script.redirect_to {
        return Some(location.clone());
    }
    if script.redirect_loop {
        if path == "/audio" {
            return Some("/audio".to_string());
        }
        return None;
    }
    if script.redirect_hops > 0 {
        if path == "/audio" {
            return Some("/audio-1".to_string());
        }
        if let Some(hop) = path
            .strip_prefix("/audio-")
            .and_then(|digits| digits.parse::<usize>().ok())
            && hop < script.redirect_hops
        {
            return Some(format!("/audio-{}", hop + 1));
        }
    }
    None
}

fn current_etag(script: &Script, request_ordinal: usize) -> Option<String> {
    let base = script.etag.as_deref()?;
    let value = match script.etag_changes_after {
        Some(after) if request_ordinal > after => "\"v2\"",
        _ => base,
    };
    if script.weak_etag {
        Some(format!("W/{value}"))
    } else {
        Some(value.to_string())
    }
}

fn write_status_only(stream: &mut TcpStream, code: u16) {
    let response = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        reason = reason_phrase(code)
    );
    let _ = stream.write_all(response.as_bytes());
}

fn write_redirect(stream: &mut TcpStream, location: &str) {
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(response.as_bytes());
}

fn write_416(stream: &mut TcpStream, len: usize) {
    let response = format!(
        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(response.as_bytes());
}

/// The resolved bounds of a satisfiable `Range` request, bundled into one
/// value so `write_206` stays under clippy's argument-count ceiling.
struct RangeSlice {
    first: usize,
    last: usize,
    len: usize,
}

fn write_206(
    stream: &mut TcpStream,
    script: &Script,
    range: RangeSlice,
    ordinal: usize,
    gate: &StallGate,
    bytes_written: &AtomicUsize,
) {
    let RangeSlice { first, last, len } = range;
    let mut header = String::from("HTTP/1.1 206 Partial Content\r\n");
    if script.advertise_accept_ranges {
        header.push_str("Accept-Ranges: bytes\r\n");
    }
    if let Some(etag) = current_etag(script, ordinal) {
        header.push_str(&format!("ETag: {etag}\r\n"));
    }
    match &script.content_range_override {
        Some(value) => header.push_str(&format!("Content-Range: {value}\r\n")),
        None => header.push_str(&format!("Content-Range: bytes {first}-{last}/{len}\r\n")),
    }
    let slice = &script.body[first..=last];
    header.push_str(&format!("Content-Length: {}\r\n", slice.len()));
    header.push_str("Connection: close\r\n\r\n");

    if stream.write_all(header.as_bytes()).is_err() {
        return;
    }
    let mut writer = BodyWriter::new(script, false);
    let mut offset = 0;
    while offset < slice.len() {
        let take = writer.next_len(slice.len() - offset);
        let end = offset + take;
        if !writer.write_piece(stream, gate, bytes_written, &slice[offset..end]) {
            return;
        }
        offset = end;
    }
    writer.finish(stream);
}

fn write_whole_body(
    stream: &mut TcpStream,
    script: &Script,
    ordinal: usize,
    gate: &StallGate,
    bytes_written: &AtomicUsize,
) {
    let endless = script.live || script.unresolved;
    let chunked_framing = script.chunked || endless;

    let mut header = String::from("HTTP/1.1 200 OK\r\n");
    if script.ranges && script.advertise_accept_ranges {
        header.push_str("Accept-Ranges: bytes\r\n");
    }
    if let Some(etag) = current_etag(script, ordinal) {
        header.push_str(&format!("ETag: {etag}\r\n"));
    }
    if let Some(encoding) = &script.content_encoding {
        header.push_str(&format!("Content-Encoding: {encoding}\r\n"));
    }
    if script.multipart {
        header.push_str("Content-Type: multipart/byteranges; boundary=x\r\n");
    }
    if script.live {
        header.push_str("icy-name: Test Radio\r\nicy-metaint: 16000\r\n");
    }
    if chunked_framing {
        header.push_str("Transfer-Encoding: chunked\r\n");
    } else {
        header.push_str(&format!("Content-Length: {}\r\n", script.body.len()));
    }
    header.push_str("Connection: close\r\n\r\n");

    if stream.write_all(header.as_bytes()).is_err() {
        return;
    }

    if endless {
        write_endless_body(stream, script, gate, bytes_written);
    } else {
        let mut writer = BodyWriter::new(script, chunked_framing);
        let mut offset = 0;
        while offset < script.body.len() {
            let take = writer.next_len(script.body.len() - offset);
            let end = offset + take;
            if !writer.write_piece(stream, gate, bytes_written, &script.body[offset..end]) {
                return;
            }
            offset = end;
        }
        writer.finish(stream);
    }
}

/// Repeats `script.body` forever, chunk-framed, until the client goes away.
/// Used for `live()` and `unresolved()` — sources that are neither provably
/// finite nor provably live from framing alone.
fn write_endless_body(
    stream: &mut TcpStream,
    script: &Script,
    gate: &StallGate,
    bytes_written: &AtomicUsize,
) {
    if script.body.is_empty() {
        // Nothing to repeat; writing headers only is the best this can do.
        return;
    }
    let mut writer = BodyWriter::new(script, true);
    let mut cursor = 0usize;
    loop {
        let take = writer.next_len(script.body.len() - cursor);
        let end = cursor + take;
        if !writer.write_piece(stream, gate, bytes_written, &script.body[cursor..end]) {
            return;
        }
        cursor = end;
        if cursor >= script.body.len() {
            cursor = 0;
        }
    }
}

fn read_line(reader: &mut BufReader<TcpStream>) -> Option<String> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            Some(line)
        }
    }
}

fn handle_connection(
    stream: TcpStream,
    script: &Script,
    requests: &Mutex<Vec<RecordedRequest>>,
    gate: &StallGate,
    bytes_written: &AtomicUsize,
) {
    let mut reader = BufReader::new(stream);

    let request_line = match read_line(&mut reader) {
        Some(line) if !line.is_empty() => line,
        // Either the shutdown probe connection, or a client that hung up
        // before sending anything: either way, nothing to answer.
        _ => return,
    };

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), Some(query.to_string())),
        None => (target, None),
    };

    let mut headers = Vec::new();
    loop {
        match read_line(&mut reader) {
            Some(line) if line.is_empty() => break,
            Some(line) => {
                if let Some((name, value)) = line.split_once(':') {
                    headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
                }
            }
            None => break,
        }
    }

    let recorded = RecordedRequest {
        method,
        path,
        query,
        headers,
    };

    let ordinal = {
        let mut guard = lock(requests);
        guard.push(recorded.clone());
        guard.len()
    };

    if script.stall_headers {
        gate.park();
        return;
    }

    let stream = reader.get_mut();

    if let Some(code) = script.status_override {
        write_status_only(stream, code);
        return;
    }

    if let Some(location) = redirect_location(script, &recorded.path) {
        write_redirect(stream, &location);
        return;
    }

    if script.ranges
        && let Some((first, last)) = recorded.range()
    {
        if script.answer_range_with_200 {
            write_whole_body(stream, script, ordinal, gate, bytes_written);
            return;
        }
        let len = script.body.len();
        let first = first as usize;
        if first >= len {
            write_416(stream, len);
            return;
        }
        let last = last
            .map(|value| value as usize)
            .unwrap_or(len.saturating_sub(1))
            .min(len.saturating_sub(1));
        let range = RangeSlice { first, last, len };
        write_206(stream, script, range, ordinal, gate, bytes_written);
        return;
    }

    write_whole_body(stream, script, ordinal, gate, bytes_written);
}

fn accept_loop(
    listener: TcpListener,
    script: Arc<Script>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    gate: Arc<StallGate>,
    stop: Arc<AtomicBool>,
    bytes_written: Arc<AtomicUsize>,
) {
    loop {
        let stream = match listener.accept() {
            Ok((stream, _addr)) => stream,
            Err(_) => {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                continue;
            }
        };

        // A real connection accepted after `stop` was set only happens if it
        // raced the shutdown probe connection into the backlog; dropping it
        // here is fine because `shutdown()` is only ever called once the
        // test has no more requests to make.
        if stop.load(Ordering::SeqCst) {
            return;
        }

        let script = Arc::clone(&script);
        let requests = Arc::clone(&requests);
        let gate = Arc::clone(&gate);
        let bytes_written = Arc::clone(&bytes_written);
        // One thread per connection: a stalled or endless-body connection
        // must not block the accept loop from serving the next one.
        std::thread::spawn(move || {
            handle_connection(stream, &script, &requests, &gate, &bytes_written);
        });
    }
}

fn bind_ephemeral() -> TcpListener {
    TcpListener::bind("127.0.0.1:0")
        .unwrap_or_else(|error| panic!("failed to bind an ephemeral loopback port: {error}"))
}

/// Binds `port` with `SO_REUSEADDR` set, which `std::net::TcpListener` has no
/// way to request. Needed so a healed server can rebind the exact port a
/// shut-down one just released without waiting out the kernel's TIME_WAIT —
/// the URL, and therefore the `MediaId` derived from it, must not change.
fn bind_reuseaddr(port: u16) -> TcpListener {
    let address: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .unwrap_or_else(|error| panic!("invalid loopback address 127.0.0.1:{port}: {error}"));

    let socket = Socket::new(Domain::IPV4, Type::STREAM, None)
        .unwrap_or_else(|error| panic!("failed to create a loopback socket: {error}"));
    socket
        .set_reuse_address(true)
        .unwrap_or_else(|error| panic!("failed to set SO_REUSEADDR on 127.0.0.1:{port}: {error}"));
    socket
        .bind(&address.into())
        .unwrap_or_else(|error| panic!("failed to bind 127.0.0.1:{port}: {error}"));
    socket
        .listen(128)
        .unwrap_or_else(|error| panic!("failed to listen on 127.0.0.1:{port}: {error}"));
    TcpListener::from(socket)
}

pub struct TestServer {
    port: u16,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    gate: Arc<StallGate>,
    stop: Arc<AtomicBool>,
    bytes_written: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    pub fn start(script: Script) -> Self {
        Self::spawn(bind_ephemeral(), script)
    }

    /// Bind a *specific* port instead of an ephemeral one.
    ///
    /// This is how a test heals a server without changing its URL, and
    /// therefore without changing the `MediaId` derived from it: shut the
    /// first server down, then start a second on the same port.
    pub fn start_on(port: u16, script: Script) -> Self {
        Self::spawn(bind_reuseaddr(port), script)
    }

    fn spawn(listener: TcpListener, script: Script) -> Self {
        let port = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("failed to read the bound loopback address: {error}"))
            .port();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(StallGate::new());
        let stop = Arc::new(AtomicBool::new(false));
        let bytes_written = Arc::new(AtomicUsize::new(0));
        let script = Arc::new(script);

        let handle = std::thread::spawn({
            let requests = Arc::clone(&requests);
            let gate = Arc::clone(&gate);
            let stop = Arc::clone(&stop);
            let bytes_written = Arc::clone(&bytes_written);
            let script = Arc::clone(&script);
            move || accept_loop(listener, script, requests, gate, stop, bytes_written)
        });

        Self {
            port,
            requests,
            gate,
            stop,
            bytes_written,
            handle: Some(handle),
        }
    }

    /// The ephemeral port this server bound.
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Release a stalled body. Returns false if nothing was stalled.
    pub fn release(&self) -> bool {
        self.gate.release()
    }

    /// Blocks until the server has begun writing a body and is stalled there.
    pub fn wait_until_stalled(&self, patience: Duration) -> bool {
        self.gate.wait_until_stalled(patience)
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        lock(&self.requests).clone()
    }

    /// Body bytes this server has actually written to the socket.
    ///
    /// This is how H13 observes the buffer bound: the channel lives inside the
    /// worker's decoder and no test can reach it, and backpressure visible on
    /// the wire is the stronger claim anyway.
    pub fn bytes_written(&self) -> usize {
        self.bytes_written.load(Ordering::SeqCst)
    }

    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Any connection currently parked on a stall would otherwise hold
        // its thread forever with nothing left to wake it.
        let _ = self.gate.release();
        // Unblock the accept() the worker thread is parked in; the loop
        // checks `stop` right after accept() returns and exits without
        // serving this connection.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            // A panicked worker thread means a test already failed; there is
            // nothing further to clean up here.
            let _ = handle.join();
        }
    }
}
