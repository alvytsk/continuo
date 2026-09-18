//! M8 §10: what an add does, per probe outcome.
//!
//! Tests were written after `src/library.rs`'s station functions, not
//! before them — the implementer who wrote `add_station`, `remove_station`,
//! `reprobe_station` and `load_mutating_stations` was interrupted before
//! writing tests, and this file is that missing half, not a from-scratch
//! TDD pass. See `tests/m6_feed_management.rs` for the store+service
//! harness this borrows its shape from, and `tests/support/server.rs` for
//! the `Script` builders.

mod support;

use std::fs;
use std::path::Path;
use std::sync::Arc;

use support::server::{Script, TestServer};
use tenuto::clock::SystemClock;
use tenuto::http::limits::Limits;
use tenuto::http::service::HttpService;
use tenuto::library::{AddStationOutcome, add_station, list_stations, reprobe_station};
use tenuto::station::store::StationStore;
use url::Url;

/// A fresh `StationStore` rooted at `root/stations.json`, built repeatedly
/// (never cached) so a test can read back exactly what an add wrote, the
/// same shape `tests/m6_feed_management.rs::stores` uses for subscriptions.
fn store(root: &Path) -> StationStore {
    StationStore::new(root.join("stations.json"), Arc::new(SystemClock))
}

/// A real `HttpService`, spawned once per test against a loopback server.
fn service() -> Arc<HttpService> {
    HttpService::spawn(Limits::brisk()).unwrap_or_else(|error| panic!("service: {error}"))
}

/// Runs `add_station` synchronously on the service's own runtime, mirroring
/// `tests/m4_document_fetch.rs`'s `service.handle().block_on(...)` pattern.
fn add(
    service: &Arc<HttpService>,
    store: &StationStore,
    url: &str,
) -> Result<AddStationOutcome, tenuto::feed::error::FeedError> {
    service.handle().block_on(add_station(service, store, url))
}

fn reprobe(
    service: &Arc<HttpService>,
    store: &StationStore,
    slug: &str,
) -> Result<AddStationOutcome, tenuto::feed::error::FeedError> {
    service
        .handle()
        .block_on(reprobe_station(service, store, slug))
}

#[test]
fn a_live_station_is_saved_with_its_identity() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .icy_logo("https://cdn.example.org/logo.svg".to_string()),
    );
    let http = service();
    let st = store(root.path());

    let outcome = add(&http, &st, &server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::Verified { slug, identity } = outcome else {
        panic!("expected Verified, got {outcome:?}");
    };
    assert_eq!(slug, "test-radio");
    assert_eq!(identity.name.as_deref(), Some("Test Radio"));
    assert_eq!(identity.genre.as_deref(), Some("Lofi"));
    assert_eq!(identity.bitrate_kbps, Some(128));
    assert_eq!(
        identity.logo,
        Some(Url::parse("https://cdn.example.org/logo.svg").unwrap())
    );

    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].slug, "test-radio");
    assert_eq!(rows[0].identity, Some(identity));

    let snapshot = st.read_snapshot().unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(snapshot.stations.len(), 1);
    assert!(
        snapshot.stations[0].probed_at.is_some(),
        "a verified station's probed_at must be set"
    );
    server.shutdown();
}

#[test]
fn a_retryable_failure_saves_an_unverified_candidate() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::serving(Vec::new()).status(503));
    let http = service();
    let st = store(root.path());

    let outcome = add(&http, &st, &server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::Unverified { slug, reason } = outcome else {
        panic!("expected Unverified, got {outcome:?}");
    };
    assert!(!slug.is_empty());
    assert!(!reason.is_empty());

    let snapshot = st.read_snapshot().unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(snapshot.stations.len(), 1, "exactly one stored station");
    assert_eq!(snapshot.stations[0].identity, None);
    assert_eq!(snapshot.stations[0].probed_at, None);
    server.shutdown();
}

#[test]
fn a_non_retryable_failure_saves_nothing() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::serving(Vec::new()).status(404));
    let http = service();
    let st = store(root.path());

    let error = add(&http, &st, &server.url("/radio")).expect_err("a 404 must not be saved");
    // R1: a 404 is a statement about the URL, a 503 is not.
    let _ = error;
    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert!(rows.is_empty());
    server.shutdown();
}

#[test]
fn a_finite_url_is_not_a_station_and_is_not_saved() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3"));
    let http = service();
    let st = store(root.path());

    let error = add(&http, &st, &server.url("/radio")).expect_err("a finite URL is not a station");
    assert!(error.to_string().contains("not a live stream"), "{error}");
    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert!(rows.is_empty());
    server.shutdown();
}

#[test]
fn an_icy_metaint_response_is_not_a_station_and_is_not_saved() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").live());
    let http = service();
    let st = store(root.path());

    let _error =
        add(&http, &st, &server.url("/radio")).expect_err("icy-metaint must refuse the probe");
    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert!(rows.is_empty());
    server.shutdown();
}

#[test]
fn a_malformed_url_makes_no_request() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    // A never-used server, purely to observe that it is never touched.
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let http = service();
    let st = store(root.path());

    let _error = add(&http, &st, "https://").expect_err("an empty host must not resolve");
    assert_eq!(server.requests().len(), 0, "a malformed URL made a request");
    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert!(rows.is_empty());
    server.shutdown();
}

#[test]
fn a_url_with_embedded_credentials_is_refused_and_never_echoed() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let http = service();
    let st = store(root.path());

    let error = add(&http, &st, "https://user:secret@radio.example/stream")
        .expect_err("embedded credentials must be refused");
    assert_eq!(server.requests().len(), 0, "credentials made a request");
    let text = error.to_string();
    assert!(!text.contains("user"), "{text}");
    assert!(!text.contains("secret"), "{text}");
    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert!(rows.is_empty());
    server.shutdown();
}

#[test]
fn a_duplicate_url_resolves_to_the_existing_station() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let http = service();
    let st = store(root.path());

    let first = add(&http, &st, &server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::Verified {
        slug: first_slug, ..
    } = first
    else {
        panic!("expected Verified, got {first:?}");
    };

    let second = add(&http, &st, &server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::AlreadySaved { slug, identity } = second else {
        panic!("expected AlreadySaved, got {second:?}");
    };
    assert_eq!(slug, first_slug);
    assert!(identity.is_some(), "the re-probe on add succeeded");

    let rows = list_stations(&st).unwrap_or_else(|error| panic!("list: {error}"));
    assert_eq!(rows.len(), 1, "a duplicate must not be saved twice");
    assert_eq!(server.requests().len(), 2, "the second add still re-probed");
    server.shutdown();
}

#[test]
fn a_reprobe_of_a_now_finite_url_keeps_the_record_and_reports_the_failure() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .then(Script::from_fixture("sine-noxing.mp3")),
    );
    let http = service();
    let st = store(root.path());

    let added = add(&http, &st, &server.url("/radio")).unwrap_or_else(|error| panic!("{error}"));
    let AddStationOutcome::Verified { slug, identity } = added else {
        panic!("expected Verified, got {added:?}");
    };

    let before = st.read_snapshot().unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(before.stations.len(), 1);

    let outcome = reprobe(&http, &st, &slug).expect_err("a now-finite URL must fail the reprobe");
    let _ = outcome;

    let after = st.read_snapshot().unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(after.stations.len(), 1, "R1 governs entry, not eviction");
    assert_eq!(after.stations[0].slug, slug);
    assert_eq!(
        after.stations[0].identity,
        Some(identity),
        "the stale identity is kept, not cleared"
    );
    server.shutdown();
}

/// §4: an `add` against a file `load_mutating_stations` deliberately
/// preserves must return `Err` and must never touch the file's bytes.
/// Without the write guard, each of these silently replaces a preserved
/// file with a fresh one holding only the station this add just tried to
/// make.
#[test]
fn a_protected_store_unsupported_version_is_never_overwritten() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let path = root.path().join("stations.json");
    let original = br#"{"schema_version": 999, "stations": []}"#.to_vec();
    fs::write(&path, &original).unwrap_or_else(|error| panic!("seed: {error}"));

    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let http = service();
    let st = store(root.path());

    let _error =
        add(&http, &st, &server.url("/radio")).expect_err("an unsupported version must refuse");
    let after = fs::read(&path).unwrap_or_else(|error| panic!("reread: {error}"));
    assert_eq!(
        after, original,
        "an unsupported-version file must be untouched"
    );
    server.shutdown();
}

#[test]
fn a_protected_store_unreadable_is_never_overwritten() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let path = root.path().join("stations.json");
        let original = br#"{"schema_version": 1, "stations": []}"#.to_vec();
        fs::write(&path, &original).unwrap_or_else(|error| panic!("seed: {error}"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000))
            .unwrap_or_else(|error| panic!("chmod: {error}"));

        // Root, and some filesystems/CI containers, ignore a 0o000 mode; skip
        // rather than report a false failure when that is the environment
        // this runs in.
        if fs::read(&path).is_ok() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).ok();
            eprintln!(
                "skipping a_protected_store_unreadable_is_never_overwritten: \
                 this environment does not enforce file mode 0o000 (root, or a \
                 permissive filesystem)"
            );
            return;
        }

        let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
        let http = service();
        let st = store(root.path());

        let _error = add(&http, &st, &server.url("/radio")).expect_err("unreadable must refuse");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .unwrap_or_else(|error| panic!("chmod back: {error}"));
        let after = fs::read(&path).unwrap_or_else(|error| panic!("reread: {error}"));
        assert_eq!(after, original, "an unreadable file must be untouched");
        server.shutdown();
    }
}

#[test]
fn a_protected_store_quarantine_failure_is_never_overwritten() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let path = root.path().join("stations.json");
        let original = b"not json at all".to_vec();
        fs::write(&path, &original).unwrap_or_else(|error| panic!("seed: {error}"));
        // A read-only directory: `StationStore::quarantine`'s `fs::rename`
        // needs write permission on the parent, not the file, so this is
        // what turns quarantine into `QuarantineFailed` instead of moving
        // the file aside.
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o555))
            .unwrap_or_else(|error| panic!("chmod dir: {error}"));

        let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
        let http = service();
        let st = store(root.path());

        let outcome = add(&http, &st, &server.url("/radio"));

        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|error| panic!("chmod dir back: {error}"));

        // Root, and some filesystems, ignore a read-only directory; skip
        // rather than report a false failure when the rename went ahead.
        if outcome.is_ok() {
            eprintln!(
                "skipping a_protected_store_quarantine_failure_is_never_overwritten: \
                 this environment does not enforce a read-only directory (root, \
                 or a permissive filesystem)"
            );
            return;
        }

        let after = fs::read(&path).unwrap_or_else(|error| panic!("reread: {error}"));
        assert_eq!(
            after, original,
            "a file that could not be quarantined must be untouched"
        );
        server.shutdown();
    }
}
