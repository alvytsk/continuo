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
