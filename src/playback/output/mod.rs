use std::sync::Arc;
use std::time::Duration;

use crate::playback::callback::CallbackCore;
use crate::playback::error::PlaybackError;
use crate::playback::link::OutputLink;

// `test_output` is compiled unconditionally rather than behind `cfg(test)`:
// integration tests drive the engine through it from outside the crate, so it
// has to be part of the public module tree declared here.
pub mod cpal_output;
pub mod test_output;

/// A point on the output device's clock, in nanoseconds.
///
/// This exists so `timeline` never depends on cpal: `TestOutput` synthesizes
/// these from a virtual clock, `CpalOutput` converts them from `StreamInstant`.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Nanos(pub u64);

impl Nanos {
    /// `cpal::StreamInstant::as_nanos` returns `u128`; saturate rather than wrap.
    /// The clamp is unreachable in practice (u64 nanoseconds is 585 years).
    pub fn from_stream_nanos(value: u128) -> Self {
        Self(u64::try_from(value).unwrap_or(u64::MAX))
    }

    pub fn checked_sub(self, earlier: Self) -> Option<Duration> {
        self.0.checked_sub(earlier.0).map(Duration::from_nanos)
    }

    pub fn saturating_add_frames(self, frames: u64, sample_rate: u32) -> Self {
        let rate = u64::from(sample_rate.max(1));
        Self(
            self.0
                .saturating_add(frames.saturating_mul(1_000_000_000) / rate),
        )
    }
}

/// One callback's contribution of media to the output timeline.
///
/// A callback pops the ring once, so media always occupies a prefix of the
/// output buffer: `frames` media frames at offsets `[0, frames)`, injected
/// silence afterwards. `t0` is cpal's predicted instant for offset 0.
///
/// `media_total_after` is **cumulative and absolute**, not a delta. That is what
/// makes a dropped record cost timing granularity without corrupting counts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpanRecord {
    pub generation: u16,
    pub media_total_after: u64,
    pub t0: Nanos,
    pub frames: u32,
}

impl SpanRecord {
    pub fn end(&self, sample_rate: u32) -> Nanos {
        self.t0
            .saturating_add_frames(u64::from(self.frames), sample_rate)
    }

    pub fn media_total_before(&self) -> u64 {
        self.media_total_after
            .saturating_sub(u64::from(self.frames))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputRequest {
    pub preferred_rate: u32,
    pub preferred_channels: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NegotiatedOutput {
    pub sample_rate: u32,
    pub channels: u16,
    pub buffer_frames: u32,
    /// The device's sample format. M1 writes `f32` buffers, so a device that
    /// cannot accept `f32` is refused at negotiation with a legible message
    /// rather than failing obscurely when the stream is built.
    pub sample_format: SampleFormat,
}

/// The sample formats this milestone can drive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleFormat {
    F32,
}

/// The internal device seam. Narrow by design: exactly the operations both
/// `CpalOutput` and `TestOutput` exercise, and nothing speculative.
pub trait AudioOutput: Send {
    /// Report the device's actual configuration, before any ring is sized.
    fn negotiate(&mut self, request: &OutputRequest) -> Result<NegotiatedOutput, PlaybackError>;
    /// Build the stream **parked**, so no audio flows until the worker releases it.
    fn open(
        &mut self,
        config: &NegotiatedOutput,
        link: Arc<OutputLink>,
        core: CallbackCore,
    ) -> Result<(), PlaybackError>;
    fn now(&self) -> Nanos;
    fn close(&mut self);
}
