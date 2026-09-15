//! Logarithmic spectrum band geometry (spec §10).
//!
//! Twenty-four nominal logarithmic intervals span 40 Hz to the lesser of
//! 16 kHz or Nyquist. Each interval owns the FFT bin centers that fall
//! within its edges; intervals that would own fewer than two centers are
//! merged into the following interval(s) until every emitted [`Band`] owns
//! at least two.

use std::ops::Range;

/// FFT analysis window length, in samples.
pub const WINDOW: usize = 2048;

/// Nominal number of logarithmic intervals before underfilled ones are
/// merged into their neighbor.
pub const NOMINAL_BANDS: usize = 24;

/// Lower edge of the spectrum, in Hz.
pub const LOW_HZ: f64 = 40.0;

/// Upper edge of the spectrum before clamping to Nyquist, in Hz.
pub const HIGH_HZ: f64 = 16_000.0;

/// A single displayed spectrum band: the nominal frequency edges it spans,
/// and the contiguous range of FFT bin indices `k` (bin center
/// `k * rate / WINDOW`) whose centers fall within those edges.
#[derive(Clone, Debug, PartialEq)]
pub struct Band {
    pub low_hz: f64,
    pub high_hz: f64,
    pub bins: Range<usize>,
}

/// Bin center frequency, in Hz, for FFT bin `k` at `sample_rate`.
fn bin_center(k: usize, sample_rate_hz: f64) -> f64 {
    // k never exceeds WINDOW / 2 (1024); WINDOW is a fixed small constant.
    // Both conversions are exact in f64 (well below the 2^53 mantissa limit).
    #[allow(clippy::cast_precision_loss)]
    let k_hz = k as f64;
    #[allow(clippy::cast_precision_loss)]
    let window_hz = WINDOW as f64;
    k_hz * sample_rate_hz / window_hz
}

/// The 25 edges `e_0..=e_24` of the 24 nominal logarithmic intervals, each
/// computed directly from its index so rounding never accumulates across
/// edges (controller note 2).
fn nominal_edges(top_hz: f64) -> [f64; NOMINAL_BANDS + 1] {
    let ratio = top_hz / LOW_HZ;
    std::array::from_fn(|i| {
        // i ranges over 0..=24; the conversion to f64 is exact.
        #[allow(clippy::cast_precision_loss)]
        let i_hz = i as f64;
        #[allow(clippy::cast_precision_loss)]
        let nominal_bands_hz = NOMINAL_BANDS as f64;
        LOW_HZ * ratio.powf(i_hz / nominal_bands_hz)
    })
}

/// Lay out the merged logarithmic bands for `sample_rate`, or return an
/// empty `Vec` when the rate leaves no usable range above `LOW_HZ`.
pub fn layout_bands(sample_rate: u32) -> Vec<Band> {
    let rate_hz = f64::from(sample_rate);
    let top_hz = HIGH_HZ.min(rate_hz / 2.0);
    if top_hz <= LOW_HZ {
        return Vec::new();
    }

    let edges = nominal_edges(top_hz);

    // Assign each eligible bin center to its nominal interval. Centers are
    // monotonic non-decreasing in k, and interval membership (the largest i
    // with e_i <= c, capped at the last interval) is monotonic non-decreasing
    // in c, so each interval's members form one contiguous run of k. Track
    // just the run's bounds instead of enumerating members.
    let mut interval_first: [Option<usize>; NOMINAL_BANDS] = [None; NOMINAL_BANDS];
    let mut interval_last: [usize; NOMINAL_BANDS] = [0; NOMINAL_BANDS];

    let last_bin = WINDOW / 2;
    let mut edge_index = 0usize; // largest j with edges[j] <= current center, capped below
    for k in 1..=last_bin {
        let center = bin_center(k, rate_hz);
        if center > top_hz {
            break; // centers only increase from here; nothing further is eligible
        }
        if center < LOW_HZ {
            continue;
        }
        while edge_index < NOMINAL_BANDS && edges[edge_index + 1] <= center {
            edge_index += 1;
        }
        // Interval i owns [e_i, e_{i+1}), except the last interval also owns
        // c == e_24; capping edge_index at NOMINAL_BANDS - 1 gives exactly
        // that (edge_index only reaches NOMINAL_BANDS when center == e_24).
        let interval = edge_index.min(NOMINAL_BANDS - 1);
        interval_first[interval].get_or_insert(k);
        interval_last[interval] = k;
    }

    // Walk intervals low to high, accumulating into the current band until
    // it owns at least two centers, then emit it. Merge a leftover
    // underfilled tail into the preceding band; an empty walk (no band ever
    // emitted) yields an empty result.
    let mut bands: Vec<Band> = Vec::new();
    let mut acc_open = false;
    let mut acc_low_hz = LOW_HZ;
    let mut acc_high_hz = LOW_HZ;
    let mut acc_bins_start: Option<usize> = None;
    let mut acc_bins_end = 0usize;
    let mut acc_count = 0usize;

    for i in 0..NOMINAL_BANDS {
        if !acc_open {
            acc_low_hz = edges[i];
            acc_open = true;
        }
        acc_high_hz = edges[i + 1];
        if let Some(first) = interval_first[i] {
            let last = interval_last[i];
            acc_bins_start.get_or_insert(first);
            acc_bins_end = last + 1;
            acc_count += last - first + 1;
        }
        if acc_count >= 2 {
            bands.push(Band {
                low_hz: acc_low_hz,
                high_hz: acc_high_hz,
                bins: acc_bins_start.unwrap_or(0)..acc_bins_end,
            });
            acc_open = false;
            acc_bins_start = None;
            acc_bins_end = 0;
            acc_count = 0;
        }
    }

    if acc_open {
        match bands.last_mut() {
            Some(last_band) => {
                last_band.high_hz = acc_high_hz;
                // The tail's bins are already contiguous with the band's, so
                // only the upper bound needs to move.
                if acc_bins_start.is_some() {
                    last_band.bins = last_band.bins.start..acc_bins_end;
                }
            }
            None => return Vec::new(),
        }
    }

    bands
}
