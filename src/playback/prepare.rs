//! One path from a [`SourceLocation`] to an open decoder plus evidence-backed
//! capabilities, for local files and HTTP alike (R5).
//!
//! §6's continuity/seek table lives here, applied uniformly: `DecodedSource`
//! already folds transport evidence into `capabilities()` (Ruling 4), so this
//! module's job is only to build that evidence for each kind of location and
//! then translate the two capability facts §6 refuses — `Indefinite` and
//! `Unresolved` — into the two distinct errors R3 calls for.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSource;
use url::Url;

use crate::http::channel::{SourceInterrupt, WaitHook};
use crate::http::error::{RemoteFailure, redact_url};
use crate::http::limits::Limits;
use crate::http::service::HttpService;
use crate::http::source::{HttpMediaSource, OpeningDeadline, remote_cause};
use crate::media::capabilities::{Continuity, MediaCapabilities};
use crate::media::id::AbsolutePath;
use crate::media::source::SourceLocation;

use super::decode::DecodedSource;
use super::error::PlaybackError;

pub struct Prepared {
    pub source: DecodedSource,
    pub capabilities: MediaCapabilities,
}

/// Everything `prepare` needs beyond the location itself.
pub struct PrepareContext {
    /// `None` for local-only sessions — `--probe-only` on a file, and every
    /// test that never touches the network.
    pub http: Option<Arc<HttpService>>,
    pub interrupt: Arc<SourceInterrupt>,
    pub hook: Arc<dyn WaitHook>,
    pub limits: Limits,
}

/// Open `location` and classify it, refusing anything that is not provably
/// finite before it is handed back.
pub fn prepare(
    location: &SourceLocation,
    context: &PrepareContext,
) -> Result<Prepared, PlaybackError> {
    let source = match location {
        SourceLocation::LocalPath(path) => open_local(path)?,
        SourceLocation::Http(url) => open_http(url, context)?,
    };
    let capabilities = source.capabilities();
    match capabilities.continuity {
        // R3: two different facts, two different refusals. Live media is
        // explicitly ongoing; an unresolved source has proven neither that it
        // ends nor that it does not.
        Continuity::Indefinite => Err(RemoteFailure::UnsupportedLiveMedia.into()),
        Continuity::Unresolved => Err(RemoteFailure::ContinuityUndetermined.into()),
        Continuity::Finite => Ok(Prepared {
            source,
            capabilities,
        }),
    }
}

fn open_local(path: &Path) -> Result<DecodedSource, PlaybackError> {
    let canonical = path.canonicalize().map_err(|source| PlaybackError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let absolute =
        AbsolutePath::new(canonical).map_err(|error| PlaybackError::UnsupportedInput {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    DecodedSource::open(&absolute)
}

fn open_http(url: &Url, context: &PrepareContext) -> Result<DecodedSource, PlaybackError> {
    let service = context
        .http
        .as_ref()
        .ok_or_else(|| RemoteFailure::InvalidSource {
            input: redact_url(url.as_str()),
            reason: "no HTTP service in this session",
        })?;

    // One absolute instant every wait taken while opening — the header wait,
    // and every read the probe below performs — is clamped against, so a
    // server that trickles data can never keep opening running past
    // `limits.open` even though every individual read stays inside
    // `limits.stall` (Ruling 3).
    let opening = OpeningDeadline(Instant::now() + context.limits.open);
    let (source, opening_limits) = HttpMediaSource::open(
        Arc::clone(service),
        url.clone(),
        Arc::clone(&context.interrupt),
        Arc::clone(&context.hook),
        context.limits,
        opening,
    )?;
    // Set through the handle, not the source: `from_media_source` boxes the
    // source below, and the handle is the only way back to it once probing
    // starts (Ruling 2).
    opening_limits.set_probe_cap(Some(context.limits.probe_bytes));

    // Evidence is read off the source before it is boxed and consumed by the
    // probe — there is no way back to it afterwards.
    let evidence = source.evidence();
    let mut hint = Hint::new();
    if let Some(extension) = extension_from_url(url) {
        hint.with_extension(&extension);
    }
    let label = PathBuf::from(redact_url(url.as_str()));

    // Symphonia's own format probe does not preserve read errors on every
    // path: `Probe::next` scans byte-by-byte for a marker with `while let
    // Ok(byte) = mss.read_byte()`, and a failing read there simply ends the
    // loop and is reported as a generic "no suitable format reader found",
    // with no wrapped cause at all — verified against symphonia-core 0.6.1.
    // Latching the failure at the read itself, before Symphonia gets a
    // chance to lose it, is the only place this project can reliably recover
    // *why* a remote probe failed.
    let latched = Arc::new(Mutex::new(None));
    let latching = LatchingSource {
        inner: source,
        latched: Arc::clone(&latched),
    };
    let result = DecodedSource::from_media_source(Box::new(latching), hint, label, evidence);
    // Both the probe cap and the opening deadline bound *opening*, not
    // playback, so they come off here regardless of outcome: once this
    // returns, ordinary reads are bounded only by `limits.stall` (§8).
    opening_limits.finish_opening();
    result.map_err(|error| promote_latched(error, &latched))
}

/// A poisoned lock means a thread already panicked while holding it; there is
/// nothing better to do than carry on with the state it left.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Wraps `HttpMediaSource`, latching the first [`RemoteFailure`] any read or
/// seek produces before handing the (possibly since-mangled) error onward to
/// Symphonia. See `open_http`'s comment for why this exists.
struct LatchingSource {
    inner: HttpMediaSource,
    latched: Arc<Mutex<Option<RemoteFailure>>>,
}

impl LatchingSource {
    fn note(&self, error: &io::Error) {
        if let Some(failure) = remote_cause(error) {
            let mut slot = lock(&self.latched);
            // First failure wins: once a source has failed, whatever it
            // reports on the next call is not a *new* fact.
            if slot.is_none() {
                *slot = Some(failure);
            }
        }
    }
}

impl io::Read for LatchingSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.inner.read(out).inspect_err(|error| self.note(error))
    }
}

impl io::Seek for LatchingSource {
    fn seek(&mut self, from: io::SeekFrom) -> io::Result<u64> {
        self.inner.seek(from).inspect_err(|error| self.note(error))
    }
}

impl MediaSource for LatchingSource {
    fn is_seekable(&self) -> bool {
        self.inner.is_seekable()
    }

    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len()
    }
}

/// Override a decode failure with the latched remote cause, if one was
/// recorded. A failure that never touched the network (an unrecognised local
/// format, say) leaves the latch empty and passes through unchanged; an
/// already-typed `PlaybackError::Remote` (a failure Symphonia happened not to
/// mangle) is left alone too, since the latch can only agree with it.
fn promote_latched(error: PlaybackError, latched: &Mutex<Option<RemoteFailure>>) -> PlaybackError {
    match error {
        PlaybackError::Remote(failure) => PlaybackError::Remote(failure),
        other => match lock(latched).clone() {
            Some(failure) => PlaybackError::Remote(failure),
            None => other,
        },
    }
}

/// The last path segment's extension, if any, so the format probe gets the
/// same hint a local `open` would give it from a file extension.
fn extension_from_url(url: &Url) -> Option<String> {
    let last_segment = url.path_segments()?.next_back()?;
    let (_, extension) = last_segment.rsplit_once('.')?;
    Some(extension.to_string())
}
