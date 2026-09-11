//! Bounded, whole-body document GETs — the feed transport beside `service`'s
//! ranged media one (§3.1).
//!
//! A feed document is a different shape of request from finite media: no
//! `Range`, no byte-channel handoff, and a size cap enforced on the running
//! total rather than trusted to `Content-Length` (§3.4). This module reuses
//! the media transport's client and `Limits` but never its runtime, its
//! redirect loop, or its byte channel — `fetch_document` contains no
//! `block_on` and is driven entirely by whichever runtime the caller awaits
//! it on (§3.1).
//!
//! Redirects and conditional GETs are Task 6's addition to this same file;
//! here a 304 is always [`RemoteFailure::UnsolicitedNotModified`] because no
//! conditional header is ever sent yet.

use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, ETAG, LAST_MODIFIED,
};
use serde::{Deserialize, Serialize};
use url::Url;

use super::error::{Operation, Phase, RemoteFailure, redact_url};
use super::limits::Limits;

/// §3.5: an XML feed first, everything else as a low-priority fallback.
const ACCEPT_VALUE: &str =
    "application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.9, */*;q=0.1";

/// One document request: where to fetch from, and the validators (if any)
/// carried over from a prior fetch of that same URL.
///
/// `validators` is part of the type from §3.1 onward even though this task
/// never reads it: Task 6 is what starts sending it as `If-None-Match` /
/// `If-Modified-Since`.
#[derive(Debug)]
pub struct DocumentRequest {
    pub origin: Url,
    pub validators: Option<CacheValidators>,
}

/// The validators a response supplied, tied to the URL that supplied them
/// (§3.3: a validator identifies a representation of *one* resource, so it
/// must never be forwarded across a redirect to something else).
///
/// URL fields stay `Url` values rather than redacted strings so a cache can
/// compare them exactly — but that also means their `Debug` carries a live
/// URL, so nothing in this crate may log a complete `CacheValidators`,
/// `DocumentRequest` or `DocumentOutcome` `Debug` representation.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct CacheValidators {
    pub url: Url,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// The result of one document fetch.
#[derive(Debug)]
pub enum DocumentOutcome {
    Unchanged {
        final_url: Url,
        permanent_url: Option<Url>,
        validators: CacheValidators,
    },
    Fetched {
        bytes: Vec<u8>,
        final_url: Url,
        permanent_url: Option<Url>,
        validators: CacheValidators,
        content_type: Option<String>,
    },
}

/// The whole operation, timed from the first connect: every redirect hop
/// (none yet, in this task), headers, and the whole body (§3.4).
pub(super) async fn fetch_document(
    client: reqwest::Client,
    limits: Limits,
    request: DocumentRequest,
) -> Result<DocumentOutcome, RemoteFailure> {
    tokio::time::timeout(limits.open, fetch_inner(client, limits, request))
        .await
        .map_err(|_| RemoteFailure::Timeout { phase: Phase::Open })?
}

/// Reuses `resolve_source`'s policy (`src/app.rs`) as a reference, not a
/// dependency: HTTP(S) only, a host present, no embedded userinfo. Feed and
/// HTTP code must not depend on `app.rs`, so the same three checks are
/// re-expressed here directly against a parsed `Url`.
fn validate_source_policy(url: &Url) -> Result<(), RemoteFailure> {
    let invalid = |reason: &'static str| RemoteFailure::InvalidSource {
        input: redact_url(url.as_str()),
        reason,
    };
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(invalid("expected http(s) with a host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("URLs with embedded credentials are not supported"));
    }
    Ok(())
}

async fn fetch_inner(
    client: reqwest::Client,
    limits: Limits,
    request: DocumentRequest,
) -> Result<DocumentOutcome, RemoteFailure> {
    validate_source_policy(&request.origin)?;

    let built = client
        .get(request.origin.clone())
        .header(ACCEPT_ENCODING, "identity")
        .header(ACCEPT, ACCEPT_VALUE)
        .build()
        .map_err(|error| RemoteFailure::Transport {
            operation: Operation::Open,
            detail: super::service::transport_detail(error),
        })?;

    let mut response = tokio::time::timeout(limits.headers, client.execute(built))
        .await
        .map_err(|_| RemoteFailure::Timeout {
            phase: Phase::Headers,
        })?
        .map_err(|error| {
            if error.is_connect() && error.is_timeout() {
                RemoteFailure::Timeout {
                    phase: Phase::Connect,
                }
            } else {
                RemoteFailure::Transport {
                    operation: Operation::Open,
                    detail: super::service::transport_detail(error),
                }
            }
        })?;

    match response.status().as_u16() {
        200 => {
            if let Some(encoding) = response
                .headers()
                .get(CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok())
                && !encoding.trim().eq_ignore_ascii_case("identity")
            {
                return Err(RemoteFailure::NonIdentityEncoding {
                    encoding: encoding.trim().to_string(),
                });
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let etag = response
                .headers()
                .get(ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let last_modified = response
                .headers()
                .get(LAST_MODIFIED)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let final_url = response.url().clone();

            // A cheap early exit only: the header can be absent or false, so
            // the streaming check below is the authority (§3.4).
            if response
                .content_length()
                .is_some_and(|n| n > limits.document_bytes as u64)
            {
                return Err(RemoteFailure::DocumentTooLarge {
                    limit: limits.document_bytes,
                });
            }
            // No preallocation from that untrusted length: `bytes` starts
            // empty and only ever grows by what was actually received.
            let mut bytes = Vec::new();
            loop {
                let chunk = tokio::time::timeout(limits.stall, response.chunk())
                    .await
                    .map_err(|_| RemoteFailure::Timeout {
                        phase: Phase::Stall,
                    })?
                    .map_err(|error| RemoteFailure::Transport {
                        operation: Operation::Open,
                        detail: super::service::transport_detail(error),
                    })?;
                let Some(chunk) = chunk else { break };
                if chunk.len() > limits.document_bytes.saturating_sub(bytes.len()) {
                    return Err(RemoteFailure::DocumentTooLarge {
                        limit: limits.document_bytes,
                    });
                }
                bytes.extend_from_slice(&chunk);
            }

            Ok(DocumentOutcome::Fetched {
                bytes,
                final_url: final_url.clone(),
                permanent_url: None,
                validators: CacheValidators {
                    url: final_url,
                    etag,
                    last_modified,
                },
                content_type,
            })
        }
        // No conditional header is ever sent in this task, so any 304 is
        // unsolicited (§3.3). Task 6 adds the conditional success path.
        304 => Err(RemoteFailure::UnsolicitedNotModified),
        other => Err(RemoteFailure::Status {
            status: other,
            operation: Operation::Open,
        }),
    }
}
