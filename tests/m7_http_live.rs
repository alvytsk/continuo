//! M7 §4: a live response at the HTTP seam.

mod support;

use std::io::Read;

use support::open_station;
use support::server::{Script, TestServer};
use tenuto::http::error::RemoteFailure;
use tenuto::http::source::remote_cause;

#[test]
fn a_station_is_live_unsized_unseekable_and_named() {
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let source = open_station(&server);
    let evidence = source.evidence();
    assert!(evidence.live);
    assert_eq!(evidence.byte_len, None);
    assert!(!evidence.byte_seekable);
    assert_eq!(
        source
            .station_identity()
            .and_then(|identity| identity.name.as_deref()),
        Some("Test Radio"),
    );
    drop(source);
    server.shutdown();
}

#[test]
fn a_station_carries_its_whole_icy_identity() {
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .icy_logo("https://radio.example/logo.svg".to_owned()),
    );
    let source = open_station(&server);
    let identity = source
        .station_identity()
        .unwrap_or_else(|| panic!("a live source has an identity"));
    assert_eq!(identity.name.as_deref(), Some("Test Radio"));
    assert_eq!(identity.genre.as_deref(), Some("Lofi"));
    assert_eq!(identity.bitrate_kbps, Some(128));
    assert_eq!(
        identity.logo.as_ref().map(url::Url::as_str),
        Some("https://radio.example/logo.svg"),
    );
    drop(source);
    server.shutdown();
}

#[test]
fn a_padded_name_and_genre_are_trimmed() {
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .icy_name("  Test Radio  ".to_owned())
            .icy_genre("  Lofi  ".to_owned()),
    );
    let source = open_station(&server);
    let identity = source
        .station_identity()
        .unwrap_or_else(|| panic!("a live source has an identity"));
    // §5/§7: `icy-br`/`icy-logo` were already trimmed; `icy-name`/`icy-genre`
    // must be too, or a padded value draws e.g. `Lofi  · 128 kbps`.
    assert_eq!(identity.name.as_deref(), Some("Test Radio"));
    assert_eq!(identity.genre.as_deref(), Some("Lofi"));
    drop(source);
    server.shutdown();
}

#[test]
fn a_station_with_no_logo_header_has_no_logo() {
    let server = TestServer::start(Script::from_fixture("sine-noxing.mp3").icy_station());
    let source = open_station(&server);
    let identity = source
        .station_identity()
        .unwrap_or_else(|| panic!("a live source has an identity"));
    assert_eq!(identity.name.as_deref(), Some("Test Radio"));
    assert!(identity.logo.is_none(), "no header, no logo");
    drop(source);
    server.shutdown();
}

#[test]
fn a_relative_logo_and_a_non_numeric_bitrate_degrade_to_none() {
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .icy_logo("/logo.svg".to_owned())
            .icy_br("not-a-number".to_owned()),
    );
    let source = open_station(&server);
    let identity = source
        .station_identity()
        .unwrap_or_else(|| panic!("a live source has an identity"));
    // A relative `icy-logo` and a non-decimal `icy-br` are both dropped, and
    // the station is still a station: neither decorative field costs it its
    // classification (§5).
    assert!(identity.logo.is_none(), "a relative logo is not a URL");
    assert!(identity.bitrate_kbps.is_none(), "not a decimal bitrate");
    assert!(source.evidence().live, "still live");
    drop(source);
    server.shutdown();
}

#[test]
fn a_file_scheme_logo_degrades_to_none() {
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .icy_logo("file:///etc/passwd".to_owned()),
    );
    let source = open_station(&server);
    let identity = source
        .station_identity()
        .unwrap_or_else(|| panic!("a live source has an identity"));
    // §5: only an absolute http(s) URL with a host is kept — a `file:` value
    // would never be fetched by `fetch_document` anyway, so admitting it
    // here would just persist a logo that can never load.
    assert!(identity.logo.is_none(), "a file: URL is not a usable logo");
    drop(source);
    server.shutdown();
}

#[test]
fn a_data_scheme_logo_degrades_to_none() {
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .icy_logo("data:image/svg+xml;base64,PHN2Zy8+".to_owned()),
    );
    let source = open_station(&server);
    let identity = source
        .station_identity()
        .unwrap_or_else(|| panic!("a live source has an identity"));
    assert!(identity.logo.is_none(), "a data: URL is not a usable logo");
    drop(source);
    server.shutdown();
}

#[test]
fn a_live_body_that_ends_is_a_failure_never_eof() {
    let server = TestServer::start(
        Script::from_fixture("sine-noxing.mp3")
            .icy_station()
            .truncate_body_after(8 * 1024),
    );
    let mut source = open_station(&server);
    let mut sink = vec![0u8; 4096];
    let error = loop {
        match source.read(&mut sink) {
            Ok(0) => panic!("a live body must never report EOF"),
            Ok(_) => continue,
            Err(error) => break error,
        }
    };
    assert_eq!(remote_cause(&error), Some(RemoteFailure::LiveEnded));
    drop(source);
    server.shutdown();
}
