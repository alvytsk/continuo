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
    Transport {
        operation: Operation,
        detail: String,
    },
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
