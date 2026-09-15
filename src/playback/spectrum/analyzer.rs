//! Post-gain frequency-spectrum analysis (spec §10): a Hann-windowed FFT
//! over a sliding, overlapping window, reduced to one level per [`Band`].

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

use super::bands::{Band, WINDOW, layout_bands};

/// Hop between successive analysis windows: 50% overlap.
pub const HOP: usize = WINDOW / 2;

/// Analyzes interleaved PCM into per-band spectrum levels over a
/// `WINDOW`-sample Hann-windowed FFT, advanced `HOP` samples at a time.
///
/// Pure and allocation-light: the FFT planner, its scratch space and the
/// Hann window are built once in [`SpectrumAnalyzer::new`]. No I/O, no
/// threads.
pub struct SpectrumAnalyzer {
    bands: Vec<Band>,
    channels: usize,
    fft: Arc<dyn Fft<f32>>,
    hann: Vec<f32>,
    /// `4 / (sum of the Hann coefficients)^2`, the power-normalization
    /// constant from spec §10, precomputed once since it depends only on
    /// the (fixed) window shape.
    power_norm: f32,
    /// Interleaved samples buffered since the last emitted window, always
    /// starting on a hop boundary.
    pending: Vec<f32>,
    /// Reused per-channel FFT input/output buffer, length `WINDOW`.
    fft_buffer: Vec<Complex32>,
    /// Reused FFT scratch space.
    fft_scratch: Vec<Complex32>,
    /// Reused per-band power accumulator (summed across channels).
    band_power: Vec<f32>,
}

impl SpectrumAnalyzer {
    /// Builds an analyzer for `sample_rate` and `channels`. `channels == 0`
    /// or a rate with no usable band range leaves the analyzer permanently
    /// inert: [`Self::push_interleaved`] then always returns `None`.
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let bands = layout_bands(sample_rate);
        let channels = usize::from(channels);

        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(WINDOW);
        let scratch_len = fft.get_inplace_scratch_len();

        let hann: Vec<f32> = (0..WINDOW)
            .map(|n| {
                // n < WINDOW (2048) and WINDOW - 1 are both far below f32's
                // 2^24 exact-integer limit; both conversions are exact.
                #[allow(clippy::cast_precision_loss)]
                let n_f = n as f32;
                #[allow(clippy::cast_precision_loss)]
                let denom = (WINDOW - 1) as f32;
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * n_f / denom).cos()
            })
            .collect();
        let window_sum: f32 = hann.iter().sum();
        let power_norm = 4.0 / (window_sum * window_sum);

        let pending_capacity = WINDOW.saturating_mul(channels.max(1));

        Self {
            band_power: vec![0.0; bands.len()],
            bands,
            channels,
            fft,
            hann,
            power_norm,
            pending: Vec::with_capacity(pending_capacity),
            fft_buffer: vec![Complex32::new(0.0, 0.0); WINDOW],
            fft_scratch: vec![Complex32::new(0.0, 0.0); scratch_len],
        }
    }

    /// The analyzer's band geometry, in ascending frequency order.
    pub fn bands(&self) -> &[Band] {
        &self.bands
    }

    /// Discards any buffered, not-yet-windowed samples.
    pub fn reset(&mut self) {
        self.pending.clear();
    }

    /// Feeds interleaved samples (whole frames only; a trailing partial
    /// frame in this call is dropped, never carried over) into the sliding
    /// analysis window. The window advances by `HOP` samples per channel
    /// each time it fills; call this from arbitrarily sized chunks and it
    /// keeps the WINDOW-vs-HOP overlap across calls.
    ///
    /// If, in a single call, enough samples arrive to complete more than one
    /// hop, only the most recent window is analyzed and returned - earlier
    /// completed windows in that same call are skipped, not computed and
    /// discarded. Returns `None` when no window has completed yet, or when
    /// the analyzer is inert (`channels == 0` or an empty band layout).
    pub fn push_interleaved(&mut self, samples: &[f32]) -> Option<Vec<f32>> {
        if self.channels == 0 || self.bands.is_empty() {
            return None;
        }

        let usable = samples.len() - samples.len() % self.channels;
        self.pending.extend_from_slice(&samples[..usable]);

        let frames_available = self.pending.len() / self.channels;
        if frames_available < WINDOW {
            return None;
        }

        // The most recent hop-aligned window start that still fits within
        // the buffered frames.
        let last_start = (frames_available - WINDOW) / HOP * HOP;
        let window_start = last_start * self.channels;
        let levels = self.analyze_window(window_start);

        // Keep only the overlap needed for the next window: everything from
        // HOP samples into this window onward.
        let keep_from = (last_start + HOP) * self.channels;
        self.pending.drain(0..keep_from);

        Some(levels)
    }

    /// Runs the Hann-windowed FFT for each channel over the `WINDOW` frames
    /// starting at interleaved sample offset `start`, averaging band power
    /// across channels (never summing samples across channels first), and
    /// returns one clamped level per band.
    fn analyze_window(&mut self, start: usize) -> Vec<f32> {
        self.band_power.fill(0.0);

        for ch in 0..self.channels {
            for n in 0..WINDOW {
                let sample = self.pending[start + n * self.channels + ch];
                self.fft_buffer[n] = Complex32::new(sample * self.hann[n], 0.0);
            }
            self.fft
                .process_with_scratch(&mut self.fft_buffer, &mut self.fft_scratch);

            for (band, power) in self.bands.iter().zip(self.band_power.iter_mut()) {
                let sum: f32 = band
                    .bins
                    .clone()
                    .map(|k| self.fft_buffer[k].norm_sqr() * self.power_norm)
                    .sum();
                // layout_bands guarantees every band owns >= 2 bins, so this
                // count is never zero.
                #[allow(clippy::cast_precision_loss)]
                let count = band.bins.len() as f32;
                *power += sum / count;
            }
        }

        // channels is a u16-derived count (<= 65535), well within f32's
        // exact-integer range.
        #[allow(clippy::cast_precision_loss)]
        let channels_f = self.channels as f32;
        self.band_power
            .iter()
            .map(|&power_sum| {
                let power = power_sum / channels_f;
                let db = 10.0 * (power + 1e-12).log10();
                ((db + 80.0) / 80.0).clamp(0.0, 1.0)
            })
            .collect()
    }
}
