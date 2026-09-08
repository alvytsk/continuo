use std::path::Path;
use std::time::Duration;

use continuo::media::capabilities::{Continuity, SeekSupport};
use continuo::media::id::AbsolutePath;
use continuo::playback::decode::DecodedSource;

#[allow(clippy::unwrap_used)]
fn fixture(name: &str) -> AbsolutePath {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    AbsolutePath::new(path.canonicalize().unwrap()).unwrap()
}

#[allow(clippy::unwrap_used)]
fn open(name: &str) -> DecodedSource {
    DecodedSource::open(&fixture(name)).unwrap()
}

#[test]
fn every_promised_format_decodes_to_planar_f32() {
    for name in ["sine.wav", "sine.flac", "sine.mp3"] {
        let mut source = open(name);
        assert_eq!(source.sample_rate(), 44_100, "{name}");
        assert_eq!(source.channels(), 2, "{name}");
        let mut frames = 0usize;
        while let Some(planes) = source.next_planar().unwrap() {
            assert_eq!(planes.len(), 2, "{name}");
            frames += planes[0].len();
        }
        // 0.5 s at 44100 Hz, allowing for codec delay and padding.
        assert!(
            (20_000..30_000).contains(&frames),
            "{name} decoded {frames} frames"
        );
    }
}

#[test]
fn metadata_and_capabilities_come_from_the_file() {
    let source = open("sine.flac");
    let duration = source.metadata().duration.unwrap();
    assert!(duration >= Duration::from_millis(450) && duration <= Duration::from_millis(550));
    let capabilities = source.capabilities();
    assert_eq!(capabilities.continuity, Continuity::Finite);
    assert_eq!(capabilities.seek, SeekSupport::Native);
}

#[test]
fn refined_seek_lands_at_the_requested_frame_not_the_packet_boundary() {
    // Symphonia's accurate seek lands at or before the target, so the decoder
    // must decode and discard forward to the exact frame.
    let mut source = open("sine.flac");
    let outcome = source
        .seek_refined(Duration::from_millis(250), None, &mut || false)
        .unwrap();
    assert!(!outcome.refinement_truncated);
    let delta = outcome.actual.as_millis().abs_diff(250);
    assert!(delta <= 2, "landed at {:?}, wanted 250 ms", outcome.actual);
}

#[test]
fn an_exhausted_refinement_budget_reports_where_it_actually_arrived() {
    let mut source = open("sine.flac");
    let outcome = source
        .seek_refined(
            Duration::from_millis(400),
            Some(Duration::ZERO),
            &mut || false,
        )
        .unwrap();
    assert!(outcome.refinement_truncated);
    assert!(outcome.actual <= Duration::from_millis(400));
}

#[test]
fn cancellation_is_observed_between_decode_steps() {
    let mut source = open("sine.flac");
    let mut calls = 0;
    let result = source.seek_refined(Duration::from_millis(400), None, &mut || {
        calls += 1;
        true
    });
    assert!(matches!(
        result,
        Err(continuo::playback::error::PlaybackError::Cancelled)
    ));
}

#[test]
fn a_missing_file_reports_a_contextual_error() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/absent.wav");
    let error = DecodedSource::open(&AbsolutePath::new(path).unwrap()).unwrap_err();
    assert!(error.to_string().contains("absent.wav"), "got: {error}");
}
