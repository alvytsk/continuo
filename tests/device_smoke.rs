//! Real-device tests. Never run in CI; run locally with
//! `cargo test --locked --test device_smoke -- --ignored --nocapture`.

use std::time::Duration;

use continuo::playback::output::cpal_output::CpalOutput;
use continuo::playback::output::{AudioOutput, OutputRequest};

#[test]
#[ignore = "requires a real audio device"]
fn a_real_device_negotiates_a_playable_configuration() {
    let mut output = CpalOutput::default();
    let negotiated = output
        .negotiate(&OutputRequest {
            preferred_rate: 44_100,
            preferred_channels: 2,
        })
        .unwrap();
    assert!(negotiated.sample_rate >= 8_000);
    assert!(negotiated.channels >= 1);
    std::thread::sleep(Duration::from_millis(10));
}
