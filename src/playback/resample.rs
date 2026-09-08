use std::path::PathBuf;

use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

use super::error::PlaybackError;

const CHUNK: usize = 1024;
/// Safety cap on the number of extra silent chunks `finish` will push through
/// the resampler while draining its delay line. The delay is a small multiple
/// of `sinc_len`, far smaller than one `CHUNK`, so a single flush chunk is
/// normally enough; this bound only guards against ever looping forever.
const MAX_FLUSH_CHUNKS: usize = 8;

/// Channel mapping plus optional band-limited rate conversion.
///
/// Resampler startup delay is trimmed **once per creation or reset** — not on
/// every anchor update — and the EOF flush recovers the delayed tail so that
/// startup trimming does not shorten playback.
pub struct Converter {
    source_channels: u16,
    target_channels: u16,
    ratio: f64,
    resampler: Option<Async<f32>>,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    /// Output frames still to be discarded from the resampler's priming delay.
    trim_remaining: usize,
    input_frames_seen: u64,
}

impl Converter {
    pub fn new(
        source_rate: u32,
        target_rate: u32,
        source_channels: u16,
        target_channels: u16,
    ) -> Result<Self, PlaybackError> {
        let ratio = f64::from(target_rate) / f64::from(source_rate);
        let channels = usize::from(source_channels.max(1));
        let resampler = if source_rate == target_rate {
            None
        } else {
            let params = SincInterpolationParameters {
                sinc_len: 256,
                f_cutoff: None,
                oversampling_factor: 256,
                interpolation: SincInterpolationType::Linear,
                window: WindowFunction::BlackmanHarris2,
            };
            Some(
                Async::<f32>::new_sinc(ratio, 2.0, &params, CHUNK, channels, FixedAsync::Input)
                    .map_err(|source| PlaybackError::UnsupportedInput {
                        path: PathBuf::default(),
                        reason: format!(
                            "cannot resample {source_rate} Hz to {target_rate} Hz: {source}"
                        ),
                    })?,
            )
        };
        let trim_remaining = resampler.as_ref().map_or(0, Resampler::output_delay);
        let output_capacity = resampler
            .as_ref()
            .map_or(CHUNK, Resampler::output_frames_max);
        Ok(Self {
            source_channels: source_channels.max(1),
            target_channels: target_channels.max(1),
            ratio,
            resampler,
            input: vec![Vec::new(); channels],
            output: vec![vec![0.0; output_capacity]; channels],
            trim_remaining,
            input_frames_seen: 0,
        })
    }

    /// Discard resampler state at a discontinuity, and re-arm the startup trim.
    pub fn reset(&mut self) {
        if let Some(resampler) = self.resampler.as_mut() {
            resampler.reset();
            self.trim_remaining = resampler.output_delay();
        }
        for plane in &mut self.input {
            plane.clear();
        }
        self.input_frames_seen = 0;
    }

    pub fn expected_output_frames(&self, input_frames: u64) -> u64 {
        (input_frames as f64 * self.ratio).round() as u64
    }

    pub fn push(&mut self, planes: &[Vec<f32>], out: &mut Vec<f32>) {
        let frames = planes.first().map_or(0, Vec::len);
        self.input_frames_seen += frames as u64;
        if self.resampler.is_none() {
            self.interleave_planes(planes, frames, out);
            return;
        }
        for (plane, buffered) in planes.iter().zip(self.input.iter_mut()) {
            buffered.extend_from_slice(plane);
        }
        while self.buffered_frames() >= CHUNK {
            self.process_chunk(CHUNK, None, out);
            for plane in &mut self.input {
                plane.drain(..CHUNK);
            }
        }
    }

    /// Feed the final partial chunk with `partial_len` so the resampler emits
    /// its delayed valid output, then drain any remaining delay with silent
    /// chunks and trim back to the expected extent.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        if self.resampler.is_none() {
            return;
        }
        let remaining = self.buffered_frames();
        if remaining > 0 {
            for plane in &mut self.input {
                plane.resize(CHUNK, 0.0);
            }
            self.process_chunk(CHUNK, Some(remaining), out);
            for plane in &mut self.input {
                plane.clear();
            }
        }
        let expected = self.expected_output_frames(self.input_frames_seen) as usize;
        let wanted = expected * usize::from(self.target_channels);
        let mut flushes = 0;
        while out.len() < wanted && flushes < MAX_FLUSH_CHUNKS {
            for plane in &mut self.input {
                plane.resize(CHUNK, 0.0);
            }
            self.process_chunk(CHUNK, Some(0), out);
            for plane in &mut self.input {
                plane.clear();
            }
            flushes += 1;
        }
        if out.len() > wanted {
            out.truncate(wanted);
        }
    }

    fn buffered_frames(&self) -> usize {
        self.input.first().map_or(0, Vec::len)
    }

    fn process_chunk(&mut self, len: usize, partial: Option<usize>, out: &mut Vec<f32>) {
        let channels = self.input.len();
        let output_capacity = self.output.first().map_or(0, Vec::len);
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: partial,
            active_channels_mask: None,
        };
        let Some(resampler) = self.resampler.as_mut() else {
            return;
        };
        let Ok(input) = SequentialSliceOfVecs::new(&self.input[..], channels, len) else {
            return;
        };
        let Ok(mut output) =
            SequentialSliceOfVecs::new_mut(&mut self.output[..], channels, output_capacity)
        else {
            return;
        };
        let Ok((_, produced)) = resampler.process_into_buffer(&input, &mut output, Some(&indexing))
        else {
            return;
        };
        let start = self.trim_remaining.min(produced);
        self.trim_remaining -= start;
        let usable = produced - start;
        if usable == 0 {
            return;
        }
        let planes: Vec<Vec<f32>> = self
            .output
            .iter()
            .map(|plane| plane[start..start + usable].to_vec())
            .collect();
        self.interleave_planes(&planes, usable, out);
    }

    fn interleave_planes(&self, planes: &[Vec<f32>], frames: usize, out: &mut Vec<f32>) {
        match (self.source_channels, self.target_channels) {
            (1, 2) => {
                for &sample in planes[0].iter().take(frames) {
                    out.push(sample);
                    out.push(sample);
                }
            }
            (2, 1) => {
                for (&a, &b) in planes[0].iter().zip(planes[1].iter()).take(frames) {
                    out.push((a + b) * 0.5);
                }
            }
            _ => {
                let channels = usize::from(self.target_channels).min(planes.len());
                for i in 0..frames {
                    for plane in planes.iter().take(channels) {
                        out.push(plane[i]);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(frames: usize, channels: usize) -> Vec<Vec<f32>> {
        (0..channels)
            .map(|_| (0..frames).map(|i| (i as f32 * 0.05).sin()).collect())
            .collect()
    }

    #[test]
    fn a_matching_rate_passes_through_interleaved_without_a_resampler() {
        let mut converter = Converter::new(48_000, 48_000, 2, 2).unwrap();
        let mut out = Vec::new();
        converter.push(&sine(480, 2), &mut out);
        assert_eq!(out.len(), 960);
    }

    #[test]
    fn mono_sources_are_duplicated_into_a_stereo_device() {
        let mut converter = Converter::new(48_000, 48_000, 1, 2).unwrap();
        let mut out = Vec::new();
        converter.push(&sine(100, 1), &mut out);
        assert_eq!(out.len(), 200);
        for frame in out.as_chunks::<2>().0 {
            assert_eq!(frame[0], frame[1]);
        }
    }

    #[test]
    fn stereo_sources_are_averaged_into_a_mono_device() {
        let mut converter = Converter::new(48_000, 48_000, 2, 1).unwrap();
        let mut out = Vec::new();
        converter.push(&[vec![1.0; 10], vec![-1.0; 10]], &mut out);
        assert_eq!(out.len(), 10);
        assert!(out.iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn startup_delay_is_trimmed_exactly_once_per_reset() {
        // Trimming on every logical anchor update would repeatedly swallow audio.
        let mut converter = Converter::new(44_100, 48_000, 2, 2).unwrap();
        let mut first = Vec::new();
        for _ in 0..20 {
            converter.push(&sine(1024, 2), &mut first);
        }
        let mut second = Vec::new();
        for _ in 0..20 {
            converter.push(&sine(1024, 2), &mut second);
        }
        // The second run trims nothing, so it yields at least as much output.
        assert!(second.len() >= first.len());
    }

    #[test]
    fn eof_flush_recovers_the_delayed_tail_without_synthetic_padding() {
        // Startup trimming must not shorten playback: the flush returns the
        // delayed valid output, and padding is trimmed to the expected extent.
        let mut converter = Converter::new(44_100, 48_000, 2, 2).unwrap();
        let mut out = Vec::new();
        let input_frames = 44_100u64;
        for _ in 0..43 {
            converter.push(&sine(1024, 2), &mut out);
        }
        converter.push(&sine(44_100 - 43 * 1024, 2), &mut out);
        let before_flush = out.len();
        converter.finish(&mut out);
        assert!(
            out.len() > before_flush,
            "the flush must recover delayed output"
        );
        let expected = converter.expected_output_frames(input_frames) as usize * 2;
        let delta = out.len().abs_diff(expected);
        assert!(
            delta <= 2 * 2,
            "got {} samples, expected about {expected}",
            out.len()
        );
    }

    #[test]
    fn reset_discards_resampler_state_for_a_seek() {
        let mut converter = Converter::new(44_100, 48_000, 2, 2).unwrap();
        let mut out = Vec::new();
        converter.push(&sine(1024, 2), &mut out);
        converter.reset();
        let mut after = Vec::new();
        converter.push(&sine(1024, 2), &mut after);
        assert!(!after.is_empty());
    }
}
