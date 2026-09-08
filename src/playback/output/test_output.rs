use std::time::Duration;

use super::Nanos;
use crate::playback::callback::CallbackCore;

/// Drives the production `CallbackCore` on a deterministic virtual clock, with
/// no device and no threads.
pub struct TestOutput {
    channels: u16,
    sample_rate: u32,
    buffer_frames: u32,
    latency: Duration,
    now: Nanos,
    core: Option<CallbackCore>,
    scratch: Vec<f32>,
    captured: Vec<f32>,
}

impl TestOutput {
    pub fn new(channels: u16, sample_rate: u32, buffer_frames: u32, latency: Duration) -> Self {
        let samples = buffer_frames as usize * channels as usize;
        Self {
            channels: channels.max(1),
            sample_rate: sample_rate.max(1),
            buffer_frames: buffer_frames.max(1),
            latency,
            now: Nanos(0),
            core: None,
            scratch: vec![0.0; samples],
            captured: Vec::new(),
        }
    }

    pub fn attach(&mut self, core: CallbackCore) {
        self.core = Some(core);
    }

    pub fn now(&self) -> Nanos {
        self.now
    }

    pub fn captured(&self) -> &[f32] {
        &self.captured
    }

    pub fn clear_captured(&mut self) {
        self.captured.clear();
    }

    /// Invoke the callback as many whole buffer periods as fit in `duration`.
    pub fn advance(&mut self, duration: Duration) {
        let period_nanos =
            u64::from(self.buffer_frames) * 1_000_000_000 / u64::from(self.sample_rate);
        let mut remaining = duration.as_nanos() as u64;
        while remaining >= period_nanos {
            // `playback` is a prediction ahead of the callback instant, exactly
            // as cpal reports it, so `now < t0` holds for the newest span.
            let playback = Nanos(self.now.0 + self.latency.as_nanos() as u64);
            if let Some(core) = self.core.as_mut() {
                core.fill(&mut self.scratch, playback);
                self.captured.extend_from_slice(&self.scratch);
            }
            self.now = Nanos(self.now.0 + period_nanos);
            remaining -= period_nanos;
        }
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }
}

impl super::AudioOutput for TestOutput {
    fn negotiate(
        &mut self,
        _request: &super::OutputRequest,
    ) -> Result<super::NegotiatedOutput, crate::playback::error::PlaybackError> {
        Ok(super::NegotiatedOutput {
            sample_rate: self.sample_rate,
            channels: self.channels,
            buffer_frames: self.buffer_frames,
        })
    }

    fn open(
        &mut self,
        _config: &super::NegotiatedOutput,
        _link: std::sync::Arc<crate::playback::link::OutputLink>,
        core: crate::playback::callback::CallbackCore,
    ) -> Result<(), crate::playback::error::PlaybackError> {
        self.attach(core);
        Ok(())
    }

    fn now(&self) -> super::Nanos {
        self.now
    }

    fn close(&mut self) {
        self.core = None;
    }
}
