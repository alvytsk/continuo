use continuo::playback::spectrum::analyzer::SpectrumAnalyzer;
use continuo::playback::spectrum::bands::{WINDOW, layout_bands};

#[test]
fn the_band_table_matches_the_spec_at_ordinary_and_high_rates() {
    for (rate, count, first_high) in [
        (44_100, 21, 65.9),
        (48_000, 21, 84.6),
        (96_000, 19, 108.6),
        (192_000, 16, 229.6),
    ] {
        let bands = layout_bands(rate);
        assert_eq!(bands.len(), count, "{rate}");
        assert!((bands[0].low_hz - 40.0).abs() < 1e-9);
        assert!(
            (bands[0].high_hz - first_high).abs() < 0.05,
            "{rate}: {}",
            bands[0].high_hz
        );
    }
    let sizes: Vec<_> = layout_bands(44_100).iter().map(|b| b.bins.len()).collect();
    assert_eq!(
        sizes,
        [
            2, 2, 3, 2, 3, 4, 5, 6, 9, 10, 14, 17, 22, 29, 37, 47, 60, 78, 99, 128, 165
        ]
    );
}

#[test]
fn every_band_owns_two_or_more_centers_and_every_center_is_assigned_once() {
    for rate in [44_100u32, 48_000, 96_000, 192_000] {
        let bands = layout_bands(rate);
        let top = (rate as f64 / 2.0).min(16_000.0);
        let eligible: Vec<usize> = (1..=WINDOW / 2)
            .filter(|k| {
                let c = *k as f64 * rate as f64 / WINDOW as f64;
                c >= 40.0 && c <= top
            })
            .collect();
        let assigned: Vec<usize> = bands.iter().flat_map(|b| b.bins.clone()).collect();
        assert!(bands.iter().all(|b| b.bins.len() >= 2), "{rate}");
        assert!(
            bands
                .windows(2)
                .all(|p| p[0].bins.end == p[1].bins.start && p[0].high_hz <= p[1].low_hz + 1e-9),
            "{rate}: contiguous, no overlap"
        );
        assert_eq!(assigned, eligible, "{rate}: each center exactly once");
    }
}

#[test]
fn a_rate_with_no_usable_range_is_unavailable() {
    assert!(layout_bands(64).is_empty());
}

fn tone(rate: u32, hz: f32, frames: usize, channels: usize, phase_flip: bool) -> Vec<f32> {
    (0..frames)
        .flat_map(|n| {
            let s = (2.0 * std::f32::consts::PI * hz * n as f32 / rate as f32).sin() * 0.5;
            (0..channels).map(move |ch| if phase_flip && ch % 2 == 1 { -s } else { s })
        })
        .collect()
}

#[allow(clippy::expect_used)] // `levels` is always nonempty here: test helper only.
fn loudest(levels: &[f32]) -> usize {
    levels
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i)
        .expect("bands")
}

#[test]
fn a_known_tone_peaks_in_its_own_band_for_mono_stereo_and_multichannel() {
    for channels in [1u16, 2, 6] {
        let mut analyzer = SpectrumAnalyzer::new(48_000, channels);
        let levels = analyzer
            .push_interleaved(&tone(48_000, 1_000.0, WINDOW, usize::from(channels), false))
            .expect("window");
        let band = &analyzer.bands()[loudest(&levels)];
        assert!(
            band.low_hz <= 1_000.0 && 1_000.0 < band.high_hz,
            "{channels} ch: {band:?}"
        );
    }
}

#[test]
fn the_merged_low_band_reacts_to_a_low_tone() {
    let mut analyzer = SpectrumAnalyzer::new(48_000, 1);
    let levels = analyzer
        .push_interleaved(&tone(48_000, 60.0, WINDOW, 1, false))
        .expect("window");
    assert_eq!(loudest(&levels), 0);
}

#[test]
fn opposite_phase_channels_do_not_cancel() {
    let mut in_phase = SpectrumAnalyzer::new(48_000, 2);
    let mut opposite = SpectrumAnalyzer::new(48_000, 2);
    let a = in_phase
        .push_interleaved(&tone(48_000, 1_000.0, WINDOW, 2, false))
        .expect("window");
    let b = opposite
        .push_interleaved(&tone(48_000, 1_000.0, WINDOW, 2, true))
        .expect("window");
    for (x, y) in a.iter().zip(&b) {
        assert!((x - y).abs() < 1e-4);
    }
}

#[test]
fn silence_is_silent_and_reset_discards_a_partial_window() {
    let mut analyzer = SpectrumAnalyzer::new(44_100, 2);
    let levels = analyzer
        .push_interleaved(&vec![0.0; WINDOW * 2])
        .expect("window");
    assert!(levels.iter().all(|l| *l < 0.01));
    let mut analyzer = SpectrumAnalyzer::new(44_100, 1);
    assert!(analyzer.push_interleaved(&vec![0.1; 1_000]).is_none());
    analyzer.reset();
    assert!(
        analyzer
            .push_interleaved(&vec![0.1; WINDOW - 1_000])
            .is_none(),
        "reset dropped the first 1000"
    );
}
