//! A paced virtual output with no sound device (`CONTINUO_AUDIO_OUTPUT=null`,
//! [`crate::playback::engine::EngineHandle::spawn_for_environment`]).
//!
//! A CI machine has no audio device, but a subprocess test still needs
//! `play` to actually run the transport, decode media and advance a
//! checkpoint at something close to real speed. This drives the same
//! [`CallbackCore`] a real device would, on its own thread, paced against a
//! wall clock instead of a sound card's interrupt.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::{AudioOutput, Nanos, NegotiatedOutput, OutputRequest, SampleFormat};
use crate::playback::callback::CallbackCore;
use crate::playback::error::PlaybackError;
use crate::playback::link::OutputLink;

const BUFFER_FRAMES: u32 = 480;
const MIN_SAMPLE_RATE: u32 = 8_000;
const MAX_SAMPLE_RATE: u32 = 192_000;
/// The gap between the callback instant and cpal's own "playback" prediction
/// a real device's `OutputCallbackInfo` would report — see `CpalOutput::open`
/// for the field this stands in for.
const PLAYBACK_LEAD: u64 = 20_000_000;

pub struct NullOutput {
    sample_rate: u32,
    now: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Default for NullOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl NullOutput {
    pub fn new() -> Self {
        Self {
            sample_rate: 0,
            now: Arc::new(AtomicU64::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
        }
    }
}

impl AudioOutput for NullOutput {
    fn negotiate(&mut self, request: &OutputRequest) -> Result<NegotiatedOutput, PlaybackError> {
        let sample_rate = request
            .preferred_rate
            .clamp(MIN_SAMPLE_RATE, MAX_SAMPLE_RATE);
        let channels = request.preferred_channels.clamp(1, 2);
        self.sample_rate = sample_rate;
        Ok(NegotiatedOutput {
            sample_rate,
            channels,
            buffer_frames: BUFFER_FRAMES,
            sample_format: SampleFormat::F32,
        })
    }

    fn open(
        &mut self,
        config: &NegotiatedOutput,
        link: Arc<OutputLink>,
        mut core: CallbackCore,
    ) -> Result<(), PlaybackError> {
        let _ = link; // `core` already carries the link it needs to drive.
        let buffer_frames = config.buffer_frames;
        let sample_rate = config.sample_rate;
        let samples = buffer_frames as usize * usize::from(config.channels);
        let now = Arc::clone(&self.now);
        let stop = Arc::clone(&self.stop);
        stop.store(false, Ordering::SeqCst);

        let thread = std::thread::Builder::new()
            .name("continuo-null-output".to_string())
            .spawn(move || {
                let opened_at = Instant::now();
                let mut buffer = vec![0.0_f32; samples];
                let mut deadline = Nanos(0);
                while !stop.load(Ordering::SeqCst) {
                    let callback = Nanos::from_stream_nanos(opened_at.elapsed().as_nanos());
                    let playback = Nanos(callback.0.saturating_add(PLAYBACK_LEAD));
                    now.store(callback.0, Ordering::Relaxed);
                    core.fill(&mut buffer, callback, playback);

                    deadline =
                        deadline.saturating_add_frames(u64::from(buffer_frames), sample_rate);
                    let target = opened_at + Duration::from_nanos(deadline.0);
                    let remaining = target.saturating_duration_since(Instant::now());
                    if !remaining.is_zero() {
                        std::thread::sleep(remaining);
                    }
                }
            })?;
        self.thread = Some(thread);
        Ok(())
    }

    fn now(&self) -> Nanos {
        Nanos(self.now.load(Ordering::Relaxed))
    }

    fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
