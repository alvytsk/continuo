use std::fs::File;
use std::path::Path;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

// `clippy.toml`'s `allow-unwrap-in-tests` only covers functions literally
// marked `#[test]`; this helper is called from one but isn't one itself.
#[allow(clippy::unwrap_used)]
fn probe_fixture(name: &str, extension: &str) -> u32 {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let file = File::open(&path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(extension);
    let reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .unwrap();
    let track = reader.default_track(TrackType::Audio).unwrap();
    track
        .codec_params
        .as_ref()
        .unwrap()
        .audio()
        .unwrap()
        .sample_rate
        .unwrap()
}

#[test]
fn every_promised_format_probes() {
    assert_eq!(probe_fixture("sine.wav", "wav"), 44100);
    assert_eq!(probe_fixture("sine.flac", "flac"), 44100);
    // Fails with a probe error unless Cargo.toml enables symphonia's non-default `mp3` feature.
    assert_eq!(probe_fixture("sine.mp3", "mp3"), 44100);
}
