//! One poller for [`BrowseWorker`] answers, shared by every M5/M6/M8 test
//! that drives the browse worker synchronously.
//!
//! Before this existed, the same ~20-line deadline/`try_result`/sleep loop
//! was hand-rolled three times (`tests/m6_feed_management.rs`'s `answer`,
//! and `tests/m8_station_probe.rs`'s `answer` and `list`) plus a fourth,
//! differently-timed copy in `tests/m5_no_network.rs`. A copy left
//! un-migrated is a silent liability: if a deadline is ever tuned to fix a
//! flaky CI runner (see the project's own history of timing flakes on
//! hosted runners), any copy nobody remembered to update keeps the old
//! timing, and the flake resurfaces wherever that copy still lives. One
//! function, tuned once, closes that gap for every caller at once.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use tenuto::application::browse::{BrowseRequest, BrowseResult, BrowseWorker};

/// How long a test waits for the browse worker to answer before treating
/// the wait itself as the failure, distinct from whatever the worker was
/// asked to do.
const DEADLINE: Duration = Duration::from_secs(20);

/// Waits for the worker's next answer, whatever request it is for. Panics
/// past [`DEADLINE`] rather than hanging a CI run.
pub fn wait_for_result(worker: &BrowseWorker) -> BrowseResult {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(result) = worker.try_result() {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "the browse worker never answered"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Sends `request` and waits for its `Mutation` answer, panicking if the
/// worker answers with anything else — the shape every mutation test
/// (`Subscribe`/`Refresh`/`Unsubscribe`, and the M8 station mutations)
/// needs.
pub fn answer(
    worker: &BrowseWorker,
    request: BrowseRequest,
) -> (BrowseRequest, Result<String, String>) {
    worker.request(request);
    match wait_for_result(worker) {
        BrowseResult::Mutation { request, outcome } => (request, outcome),
        other => panic!("expected a mutation answer, got {other:?}"),
    }
}
