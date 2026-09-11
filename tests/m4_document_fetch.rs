mod support;
use std::time::Duration;

use continuo::http::{
    document::{DocumentOutcome, DocumentRequest},
    error::{Phase, RemoteFailure},
    limits::Limits,
    service::HttpService,
};
use support::server::{Script, TestServer};

#[test]
fn fetches_a_document_without_a_media_range() -> Result<(), Box<dyn std::error::Error>> {
    let xml = b"<rss><channel/></rss>".to_vec();
    let server = TestServer::start(Script::serving(xml.clone()).without_ranges());
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin: url::Url::parse(&server.url("/feed"))?,
            validators: None,
        }))?;
    let DocumentOutcome::Fetched { bytes, .. } = result else {
        panic!("expected a body")
    };
    assert_eq!(bytes, xml);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header("range"), None);
    assert_eq!(requests[0].header("accept-encoding"), Some("identity"));
    server.shutdown();
    Ok(())
}

#[test]
fn a_declared_content_length_above_the_cap_is_rejected_before_the_body_is_read()
-> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(Script::serving(vec![b'x'; 17]).without_ranges());
    let limits = Limits {
        document_bytes: 16,
        ..Limits::brisk()
    };
    let service = HttpService::spawn(limits)?;
    let origin = url::Url::parse(&server.url("/feed"))?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin,
            validators: None,
        }));
    match result {
        Err(RemoteFailure::DocumentTooLarge { limit: 16 }) => {}
        other => panic!("expected DocumentTooLarge {{ limit: 16 }}, got {other:?}"),
    }
    server.shutdown();
    Ok(())
}

#[test]
fn a_chunked_body_with_no_content_length_is_still_rejected_once_streamed_past_the_cap()
-> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(Script::serving(vec![b'x'; 17]).without_ranges().chunked());
    let limits = Limits {
        document_bytes: 16,
        ..Limits::brisk()
    };
    let service = HttpService::spawn(limits)?;
    let origin = url::Url::parse(&server.url("/feed"))?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin,
            validators: None,
        }));
    match result {
        Err(RemoteFailure::DocumentTooLarge { limit: 16 }) => {}
        other => panic!("expected DocumentTooLarge {{ limit: 16 }}, got {other:?}"),
    }
    server.shutdown();
    Ok(())
}

#[test]
fn a_body_at_exactly_the_cap_succeeds() -> Result<(), Box<dyn std::error::Error>> {
    let body = vec![b'x'; 16];
    let server = TestServer::start(Script::serving(body.clone()).without_ranges());
    let limits = Limits {
        document_bytes: 16,
        ..Limits::brisk()
    };
    let service = HttpService::spawn(limits)?;
    let origin = url::Url::parse(&server.url("/feed"))?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin,
            validators: None,
        }))?;
    let DocumentOutcome::Fetched { bytes, .. } = result else {
        panic!("expected a body")
    };
    assert_eq!(bytes, body);
    server.shutdown();
    Ok(())
}

#[test]
fn a_non_identity_content_encoding_is_refused() -> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(
        Script::serving(b"<rss/>".to_vec())
            .without_ranges()
            .gzip_encoded(),
    );
    let service = HttpService::spawn(Limits::brisk())?;
    let origin = url::Url::parse(&server.url("/feed"))?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin,
            validators: None,
        }));
    match result {
        Err(RemoteFailure::NonIdentityEncoding { .. }) => {}
        other => panic!("expected NonIdentityEncoding, got {other:?}"),
    }
    server.shutdown();
    Ok(())
}

#[test]
fn an_unsolicited_media_range_response_is_refused() -> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(Script::serving(b"<rss/>".to_vec()).status(206));
    let service = HttpService::spawn(Limits::brisk())?;
    let origin = url::Url::parse(&server.url("/feed"))?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin,
            validators: None,
        }));
    match result {
        Err(RemoteFailure::Status { status: 206, .. }) => {}
        other => panic!("expected Status {{ status: 206, .. }}, got {other:?}"),
    }
    server.shutdown();
    Ok(())
}

#[test]
fn a_trickling_body_exceeds_the_whole_body_deadline_rather_than_a_single_stall()
-> Result<(), Box<dyn std::error::Error>> {
    let server = TestServer::start(
        Script::serving(vec![b'x'; 16])
            .without_ranges()
            .trickle(1, Duration::from_millis(20)),
    );
    let limits = Limits {
        open: Duration::from_millis(80),
        stall: Duration::from_millis(100),
        ..Limits::brisk()
    };
    let service = HttpService::spawn(limits)?;
    let origin = url::Url::parse(&server.url("/feed"))?;
    let result = service
        .handle()
        .block_on(service.fetch_document(DocumentRequest {
            origin,
            validators: None,
        }));
    match result {
        Err(RemoteFailure::Timeout { phase: Phase::Open }) => {}
        other => panic!("expected Timeout {{ phase: Phase::Open }}, got {other:?}"),
    }
    server.shutdown();
    Ok(())
}
