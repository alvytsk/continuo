use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTag};
use symphonia::core::units::{TimeBase, Timestamp};

use crate::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
use crate::media::id::AbsolutePath;
use crate::media::metadata::MediaMetadata;

use super::error::PlaybackError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SeekOutcome {
    pub actual: Duration,
    pub refinement_truncated: bool,
}

pub struct DecodedSource {
    path: PathBuf,
    reader: Box<dyn FormatReader + 'static>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    sample_rate: u32,
    channels: u16,
    metadata: MediaMetadata,
    planes: Vec<Vec<f32>>,
    /// Media timestamp of the next frame `next_planar` will return.
    cursor: u64,
    /// `true` when `planes` already holds decoded audio (the tail of a packet
    /// trimmed during seek refinement) that `next_planar` should hand out
    /// before pulling a new packet from the reader.
    pending: bool,
}

// `FormatReader` and `AudioDecoder` are trait objects that do not implement
// `Debug`, so this is hand-written rather than derived. It exists so
// `Result<DecodedSource, _>::unwrap_err()` is usable in tests.
impl std::fmt::Debug for DecodedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecodedSource")
            .field("path", &self.path)
            .field("track_id", &self.track_id)
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("cursor", &self.cursor)
            .finish_non_exhaustive()
    }
}

impl DecodedSource {
    pub fn open(path: &AbsolutePath) -> Result<Self, PlaybackError> {
        let owned = path.as_path().to_path_buf();
        let metadata_fs = std::fs::metadata(&owned).map_err(|source| PlaybackError::Open {
            path: owned.clone(),
            source,
        })?;
        // Reject pipes and device files so this local-file slice cannot acquire
        // an unbounded read.
        if !metadata_fs.is_file() {
            return Err(PlaybackError::UnsupportedInput {
                path: owned,
                reason: "not a regular file".into(),
            });
        }
        let file = File::open(&owned).map_err(|source| PlaybackError::Open {
            path: owned.clone(),
            source,
        })?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(extension) = owned.extension().and_then(|e| e.to_str()) {
            hint.with_extension(extension);
        }
        let mut reader = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(PlaybackError::Decode)?;

        let track = reader.default_track(TrackType::Audio).ok_or_else(|| {
            PlaybackError::UnsupportedInput {
                path: owned.clone(),
                reason: "no audio track".into(),
            }
        })?;
        let track_id = track.id;
        let time_base = track.time_base;
        let duration = track
            .num_frames
            .zip(track.time_base)
            .and_then(|(frames, base)| {
                base.calc_time(Timestamp::new(frames as i64))
                    .map(|time| Duration::from_secs_f64(time.as_secs_f64()))
            });
        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .ok_or_else(|| PlaybackError::UnsupportedInput {
                path: owned.clone(),
                reason: "no audio codec parameters".into(),
            })?;
        let sample_rate = params
            .sample_rate
            .ok_or_else(|| PlaybackError::UnsupportedInput {
                path: owned.clone(),
                reason: "unknown sample rate".into(),
            })?;
        let channels = params
            .channels
            .as_ref()
            .map(|channels| channels.count() as u16)
            .unwrap_or(0);
        if channels == 0 || channels > 2 {
            return Err(PlaybackError::UnsupportedInput {
                path: owned,
                reason: format!("{channels} channels; M1 supports mono and stereo"),
            });
        }
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(PlaybackError::Decode)?;
        // `Tag::std`, when present, is a `StandardTag` that carries its value inline
        // (e.g. `StandardTag::TrackTitle(Arc<String>)`) rather than a separate key enum.
        let title = reader.metadata().current().and_then(|revision| {
            revision.media.tags.iter().find_map(|tag| match &tag.std {
                Some(StandardTag::TrackTitle(title)) => Some(title.to_string()),
                _ => None,
            })
        });

        Ok(Self {
            path: owned,
            reader,
            decoder,
            track_id,
            time_base,
            sample_rate,
            channels,
            metadata: MediaMetadata { title, duration },
            planes: vec![Vec::new(); usize::from(channels)],
            cursor: 0,
            pending: false,
        })
    }

    pub fn metadata(&self) -> &MediaMetadata {
        &self.metadata
    }

    /// Local files are finite and, for every format M1 ships, natively seekable.
    pub fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            continuity: if self.metadata.duration.is_some() {
                Continuity::Finite
            } else {
                Continuity::Unresolved
            },
            seek: SeekSupport::Native,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn position(&self) -> Duration {
        self.frames_to_duration(self.cursor)
    }

    /// Decode the next packet into planar `f32`. `Ok(None)` means end of media.
    ///
    /// If seek refinement trimmed a packet to land on the exact frame, the
    /// trimmed remainder is handed out first, before any new packet is pulled
    /// from the reader.
    pub fn next_planar(&mut self) -> Result<Option<&[Vec<f32>]>, PlaybackError> {
        if self.pending {
            self.pending = false;
            self.cursor += self.planes[0].len() as u64;
            return Ok(Some(&self.planes));
        }
        loop {
            let packet = match self.reader.next_packet().map_err(PlaybackError::Decode)? {
                Some(packet) => packet,
                None => return Ok(None),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let decoded = self
                .decoder
                .decode(&packet)
                .map_err(PlaybackError::Decode)?;
            let frames = decoded.frames() as u64;
            if frames == 0 {
                continue;
            }
            copy_planar(&decoded, &mut self.planes);
            self.cursor += frames;
            return Ok(Some(&self.planes));
        }
    }

    /// Seek, then decode-and-discard forward to the exact frame.
    ///
    /// Symphonia's accurate seek lands at or before the target because the
    /// reader seeks to a packet boundary, so refinement is required for an exact
    /// landing. `budget` bounds refinement for an *explicit* seek; pass `None`
    /// for stop-resume and device recovery, which promise preservation.
    pub fn seek_refined(
        &mut self,
        target: Duration,
        budget: Option<Duration>,
        cancelled: &mut dyn FnMut() -> bool,
    ) -> Result<SeekOutcome, PlaybackError> {
        let seeked = self
            .reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: duration_to_time(target),
                    track_id: Some(self.track_id),
                },
            )
            .map_err(|source| PlaybackError::SeekFailed { target, source })?;
        self.decoder.reset();
        // `actual_ts` is signed and MP3 readers report a NEGATIVE timestamp when
        // seeking into an encoder's delay region, so a bare `as u64` wraps to
        // ~1.8e19 and the next `cursor += frames` overflows. A frame before the
        // first playable one is position zero.
        self.cursor = seeked.actual_ts.get().max(0) as u64;
        self.pending = false;

        let target_frames = self.duration_to_frames(target);
        let budget_frames = budget.map(|b| self.duration_to_frames(b));
        let start = self.cursor;
        let mut truncated = false;
        while self.cursor < target_frames {
            if cancelled() {
                return Err(PlaybackError::Cancelled);
            }
            if let Some(limit) = budget_frames
                && self.cursor.saturating_sub(start) >= limit
            {
                truncated = true;
                break;
            }
            match self.next_planar()? {
                Some(_) => {
                    // The reader can only seek to a packet boundary, so a
                    // decoded packet may run past the target frame. Trim its
                    // leading frames so the cursor lands exactly on target and
                    // the remainder is preserved as pending audio, rather than
                    // being decoded again or silently skipped.
                    if self.cursor > target_frames {
                        let overshoot = (self.cursor - target_frames) as usize;
                        let discard = self.planes[0].len() - overshoot;
                        for plane in &mut self.planes {
                            plane.drain(0..discard);
                        }
                        self.cursor = target_frames;
                        self.pending = !self.planes[0].is_empty();
                    }
                }
                None => break,
            }
        }
        Ok(SeekOutcome {
            actual: self.position(),
            refinement_truncated: truncated,
        })
    }

    fn duration_to_frames(&self, value: Duration) -> u64 {
        (value.as_secs_f64() * f64::from(self.sample_rate)) as u64
    }

    fn frames_to_duration(&self, frames: u64) -> Duration {
        Duration::from_secs_f64(frames as f64 / f64::from(self.sample_rate))
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn time_base(&self) -> Option<TimeBase> {
        self.time_base
    }
}

fn copy_planar(decoded: &GenericAudioBufferRef<'_>, planes: &mut Vec<Vec<f32>>) {
    decoded.copy_to_vecs_planar::<f32>(planes);
}

fn duration_to_time(value: Duration) -> symphonia::core::units::Time {
    // `subsec_nanos()` is always < 1_000_000_000, so `try_new` never actually
    // returns `None`; the fallback exists only to avoid `unwrap`.
    symphonia::core::units::Time::try_new(value.as_secs() as i64, value.subsec_nanos())
        .unwrap_or(symphonia::core::units::Time::ZERO)
}
