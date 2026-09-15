//! A single background worker that resolves and decodes cover art (design
//! doc M5 §9, §11, decision 24): one thread, one latest-wins request slot,
//! and every job run inside [`run_contained`] so a decoder panic becomes an
//! ordinary [`ArtworkError::Panicked`] result instead of taking the process
//! down. Remote and podcast entries never reach this worker — the TUI shows
//! a placeholder for them instead — so it only ever sees local paths.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crossbeam_channel::{Receiver, Sender, TrySendError};

use crate::artwork::decode::{ArtworkError, decode_limited, read_limited};
use crate::artwork::resolve::{ArtworkSource, find_artwork};
use crate::lifecycle::hooks::TestHook;
use crate::lifecycle::panic::run_contained;
use crate::media::id::{AbsolutePath, MediaId};
use crate::media::tags::probe_local_tags;

/// Loads the decoded cover for a local track. Shared with the worker
/// thread, so it must be callable from another thread.
pub type CoverLoader =
    Arc<dyn Fn(&AbsolutePath) -> Result<image::DynamicImage, ArtworkError> + Send + Sync>;

pub struct ArtworkResult {
    pub media: MediaId,
    pub image: Result<Arc<image::DynamicImage>, ArtworkError>,
}

struct Job {
    media: MediaId,
    path: AbsolutePath,
}

/// A single artwork worker thread with a latest-wins request slot: a new
/// request replaces whatever request is still waiting (not yet picked up by
/// the worker) rather than queuing behind it, and [`request`](Self::request)
/// never blocks the caller to make that happen.
pub struct ArtworkWorker {
    requests: Sender<Job>,
    /// A second handle onto the request channel, used only to drain a
    /// stale pending job from [`request`](Self::request); the worker thread
    /// holds its own clone to receive from.
    request_drain: Receiver<Job>,
    results: Receiver<ArtworkResult>,
}

impl ArtworkWorker {
    /// Starts the `continuo-artwork` thread. The thread is detached, never
    /// joined: dropping this handle drops the request sender, which ends
    /// the worker's loop once whatever job it holds (if any) returns.
    pub fn spawn(loader: CoverLoader) -> Self {
        let (requests_tx, requests_rx) = crossbeam_channel::bounded::<Job>(1);
        let (results_tx, results_rx) = crossbeam_channel::bounded::<ArtworkResult>(1);
        let worker_requests = requests_rx.clone();
        let spawned = thread::Builder::new()
            .name("continuo-artwork".to_string())
            .spawn(move || {
                for job in &worker_requests {
                    let outcome = match run_contained("artwork", || (loader)(&job.path)) {
                        Ok(loaded) => {
                            tracing::debug!("artwork job completed");
                            loaded.map(Arc::new)
                        }
                        Err(_) => Err(ArtworkError::Panicked),
                    };
                    let result = ArtworkResult {
                        media: job.media,
                        image: outcome,
                    };
                    if results_tx.send(result).is_err() {
                        return;
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "cannot start the artwork worker");
        }
        Self {
            requests: requests_tx,
            request_drain: requests_rx,
            results: results_rx,
        }
    }

    /// Requests a cover for `media`/`path`, replacing a request the worker
    /// has not yet picked up. Never blocks: every step is a non-blocking
    /// channel operation, and the loop below only ever repeats to retry a
    /// send that raced the worker taking the previous job, which converges
    /// in at most a couple of iterations.
    pub fn request(&self, media: MediaId, path: AbsolutePath) {
        let mut job = Job { media, path };
        loop {
            match self.requests.try_send(job) {
                Ok(()) => return,
                Err(TrySendError::Full(returned)) => {
                    job = returned;
                    let _ = self.request_drain.try_recv();
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }

    /// The most recently finished result, if any is waiting.
    pub fn try_result(&self) -> Option<ArtworkResult> {
        self.results.try_recv().ok()
    }
}

/// The production loader: probes the track's tags, resolves its artwork
/// (embedded cover first, then siblings), and decodes it within the fixed
/// limits. An embedded cover the tag probe already judged oversized
/// (decision: `LocalTags::cover_oversized`) is reported as
/// [`ArtworkError::TooLarge`] directly — it outranks any sibling, so a
/// fallback would show the wrong cover rather than a placeholder for the
/// right one. Under the `artwork-job-panic` test hook, the first call
/// panics first, inside the job, so the panic is contained.
pub fn default_loader(hook: TestHook) -> CoverLoader {
    let armed = AtomicBool::new(hook == TestHook::ArtworkJobPanic);
    Arc::new(move |path| {
        if armed.swap(false, Ordering::SeqCst) {
            hook.panic_at(TestHook::ArtworkJobPanic);
        }
        let tags = probe_local_tags(path).map_err(|_| ArtworkError::Missing)?;
        if tags.cover_oversized {
            return Err(ArtworkError::TooLarge);
        }
        let source = find_artwork(path.as_path(), tags.front_cover).ok_or(ArtworkError::Missing)?;
        let bytes = match source {
            ArtworkSource::Embedded(data) => data,
            ArtworkSource::Sibling(sibling) => read_limited(&sibling)?,
        };
        decode_limited(&bytes)
    })
}
