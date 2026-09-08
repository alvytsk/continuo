use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender};

use super::{AudioOutput, Nanos, NegotiatedOutput, OutputRequest};
use crate::playback::callback::CallbackCore;
use crate::playback::error::PlaybackError;
use crate::playback::link::OutputLink;

/// How a device error must be handled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFault {
    /// Playback continues; report and carry on.
    Recoverable(cpal::ErrorKind),
    /// The transport must be rebuilt at the preserved position.
    Rebuild(cpal::ErrorKind),
    /// Unrecoverable; enter `Failed`.
    Fatal(cpal::ErrorKind),
}

impl OutputFault {
    pub fn classify(kind: cpal::ErrorKind) -> Self {
        use cpal::ErrorKind::*;
        match kind {
            // Automatically rerouted; the stream stays active. The timing base
            // may jump, so the caller marks position degraded.
            DeviceChanged | Xrun | RealtimeDenied => Self::Recoverable(kind),
            DeviceNotAvailable | StreamInvalidated | DeviceBusy => Self::Rebuild(kind),
            // `Other` is cpal's own catch-all for genuinely unclassifiable
            // conditions; cpal documents it as permanent, so no retry is
            // attempted without host-specific knowledge.
            PermissionDenied | HostUnavailable | UnsupportedConfig | Other => Self::Fatal(kind),
            // Fallback: one rebuild attempt, then the caller gives up. `ErrorKind`
            // is `#[non_exhaustive]`, so this also absorbs any variant cpal adds
            // in a future release — such a variant should be classified
            // explicitly once it appears, rather than left to this fallback.
            _ => Self::Rebuild(kind),
        }
    }

    pub fn kind(self) -> cpal::ErrorKind {
        match self {
            Self::Recoverable(kind) | Self::Rebuild(kind) | Self::Fatal(kind) => kind,
        }
    }
}

pub struct CpalOutput {
    device: Option<cpal::Device>,
    stream: Option<cpal::Stream>,
    faults_tx: Sender<OutputFault>,
    faults_rx: Receiver<OutputFault>,
    wake: Option<Sender<()>>,
}

impl Default for CpalOutput {
    fn default() -> Self {
        let (faults_tx, faults_rx) = crossbeam_channel::bounded(16);
        Self {
            device: None,
            stream: None,
            faults_tx,
            faults_rx,
            wake: None,
        }
    }
}

impl CpalOutput {
    /// Asynchronous device errors arrive here; `open()`'s Result covers only
    /// synchronous failures.
    pub fn faults(&self) -> Receiver<OutputFault> {
        self.faults_rx.clone()
    }

    /// The wake channel from the engine, so a fault interrupts a blocked wait.
    pub fn set_wake(&mut self, wake: Sender<()>) {
        self.wake = Some(wake);
    }
}

impl AudioOutput for CpalOutput {
    fn negotiate(&mut self, request: &OutputRequest) -> Result<NegotiatedOutput, PlaybackError> {
        let host = cpal::default_host();
        let device =
            host.default_output_device()
                .ok_or_else(|| PlaybackError::UnsupportedInput {
                    path: Default::default(),
                    reason: "no default audio output device".into(),
                })?;
        let config = device
            .default_output_config()
            .map_err(PlaybackError::Output)?;
        // The stream is built with an `f32` buffer, so the device has to accept
        // f32. Its DEFAULT config may not be f32 while an f32 config is still
        // on offer, so fall back to searching what it supports before refusing
        // - preferring one that can run at the default's sample rate.
        let config = if config.sample_format() == cpal::SampleFormat::F32 {
            config
        } else {
            let wanted = config.sample_rate();
            // Filter on channel count as well as format. This milestone drives
            // mono or stereo only, and the engine refuses anything wider, so a
            // six-channel f32 range is not a usable answer even though it is an
            // f32 one - picking it would refuse a device that offers stereo.
            let usable = |range: &cpal::SupportedStreamConfigRange| {
                range.sample_format() == cpal::SampleFormat::F32
                    && matches!(range.channels(), 1 | 2)
            };
            let chosen = device
                .supported_output_configs()
                .map_err(PlaybackError::Output)?
                .filter(usable)
                .find_map(|range| range.try_with_sample_rate(wanted))
                .or_else(|| {
                    device
                        .supported_output_configs()
                        .ok()?
                        .filter(usable)
                        .map(|range| {
                            let rate = range.max_sample_rate();
                            range.with_sample_rate(rate)
                        })
                        .next()
                });
            match chosen {
                Some(config) => config,
                None => {
                    return Err(PlaybackError::UnsupportedInput {
                        path: Default::default(),
                        reason: format!(
                            "the audio device offers no mono or stereo f32 configuration \
                             (its default is {:?}); this build outputs f32 only",
                            config.sample_format()
                        ),
                    });
                }
            }
        };
        let negotiated = NegotiatedOutput {
            sample_rate: config.sample_rate(),
            channels: config.channels(),
            buffer_frames: 1024,
            sample_format: super::SampleFormat::F32,
        };
        let _ = request;
        self.device = Some(device);
        Ok(negotiated)
    }

    fn open(
        &mut self,
        config: &NegotiatedOutput,
        link: Arc<OutputLink>,
        mut core: CallbackCore,
    ) -> Result<(), PlaybackError> {
        let device = self.device.as_ref().ok_or(PlaybackError::Timeout)?;
        let stream_config = cpal::StreamConfig {
            channels: config.channels,
            sample_rate: config.sample_rate,
            buffer_size: cpal::BufferSize::Default,
        };
        let faults = self.faults_tx.clone();
        let wake = self.wake.clone();
        let _ = link;
        let stream = device
            .build_output_stream(
                stream_config,
                move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    let playback = Nanos::from_stream_nanos(info.timestamp().playback.as_nanos());
                    core.fill(out, playback);
                },
                move |error: cpal::Error| {
                    // Never blocks: a full fault queue means the worker already
                    // has one to act on.
                    let _ = faults.try_send(OutputFault::classify(error.kind()));
                    if let Some(wake) = wake.as_ref() {
                        let _ = wake.try_send(());
                    }
                },
                None,
            )
            .map_err(PlaybackError::Output)?;
        // The stream runs continuously; pause is the Park phase, so the callback
        // stays live and every handshake deadline signals real device trouble.
        stream.play().map_err(PlaybackError::Output)?;
        self.stream = Some(stream);
        Ok(())
    }

    fn now(&self) -> Nanos {
        match self.stream.as_ref() {
            Some(stream) => Nanos::from_stream_nanos(stream.now().as_nanos()),
            None => Nanos(0),
        }
    }

    fn close(&mut self) {
        // Dropping joins the backend thread, which is what makes the link's
        // rescue slot safe to read afterwards. The join has no timeout.
        self.stream = None;
    }
}

impl CpalOutput {
    pub fn drain_faults(&self, timeout: Duration) -> Option<OutputFault> {
        self.faults_rx.recv_timeout(timeout).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::ErrorKind;

    #[test]
    fn a_rerouted_device_is_recoverable_without_a_rebuild() {
        // cpal documents DeviceChanged as "automatically rerouted; the stream
        // remains active and no rebuild is required".
        assert!(matches!(
            OutputFault::classify(ErrorKind::DeviceChanged),
            OutputFault::Recoverable(_)
        ));
    }

    #[test]
    fn an_xrun_is_recoverable_and_never_triggers_a_rebuild() {
        assert!(matches!(
            OutputFault::classify(ErrorKind::Xrun),
            OutputFault::Recoverable(_)
        ));
    }

    #[test]
    fn refused_realtime_scheduling_does_not_stop_playback() {
        assert!(matches!(
            OutputFault::classify(ErrorKind::RealtimeDenied),
            OutputFault::Recoverable(_)
        ));
    }

    #[test]
    fn a_lost_device_requires_rebuilding_at_the_preserved_position() {
        for kind in [ErrorKind::DeviceNotAvailable, ErrorKind::StreamInvalidated] {
            assert!(
                matches!(OutputFault::classify(kind), OutputFault::Rebuild(_)),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn authorization_and_host_failures_are_fatal() {
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::HostUnavailable,
            ErrorKind::UnsupportedConfig,
        ] {
            assert!(
                matches!(OutputFault::classify(kind), OutputFault::Fatal(kind2) if kind2 == kind)
            );
        }
    }

    #[test]
    fn an_unclassifiable_other_error_is_permanent_and_not_retried() {
        // cpal documents `Other` as its own catch-all for genuinely
        // unclassifiable conditions and states it is permanent: no retry
        // strategy is possible without host-specific knowledge.
        assert!(matches!(
            OutputFault::classify(ErrorKind::Other),
            OutputFault::Fatal(ErrorKind::Other)
        ));
    }

    #[test]
    fn unclassifiable_backend_errors_fall_back_to_one_rebuild_attempt() {
        for kind in [
            ErrorKind::ResourceExhausted,
            ErrorKind::UnsupportedOperation,
            ErrorKind::InvalidInput,
            ErrorKind::BackendError,
        ] {
            assert!(
                matches!(OutputFault::classify(kind), OutputFault::Rebuild(_)),
                "{kind:?}"
            );
        }
    }
}
