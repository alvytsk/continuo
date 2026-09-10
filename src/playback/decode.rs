use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::{MetadataOptions, StandardTag};
use symphonia::core::units::{TimeBase, Timestamp};

use crate::media::capabilities::{
    Continuity, DemuxerSeek, MediaCapabilities, SeekSupport, SourceEvidence,
};
use crate::media::id::AbsolutePath;
use crate::media::metadata::MediaMetadata;

use super::error::PlaybackError;
use super::provenance::PositionProvenance;

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
    /// What the caller established about this source independent of the
    /// decoder — byte length, byte seekability, liveness, and whether the
    /// demuxer's own seek has been demonstrated. `capabilities()` folds this
    /// with what the decoder alone can tell.
    evidence: SourceEvidence,
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
        let mut hint = Hint::new();
        if let Some(extension) = owned.extension().and_then(|e| e.to_str()) {
            hint.with_extension(extension);
        }
        let evidence = SourceEvidence {
            byte_len: Some(metadata_fs.len()),
            byte_seekable: true,
            live: false,
            // Not an assumption: M1 already ships this guarantee for the four
            // formats M1 supports, and `tests/decode_fixtures.rs` re-proves it
            // on every run.
            demuxer: DemuxerSeek::Proven,
        };
        Self::from_media_source(Box::new(file), hint, owned, evidence)
    }

    /// Open over any `MediaSource`, with the supplied evidence folded into the
    /// capabilities the decoder alone cannot establish.
    ///
    /// `label` is the path (local) or redacted origin (remote) carried purely
    /// for diagnostics — `UnsupportedInput`'s `path` field and this source's
    /// own `path()` accessor.
    pub fn from_media_source(
        source: Box<dyn MediaSource>,
        hint: Hint,
        label: PathBuf,
        evidence: SourceEvidence,
    ) -> Result<Self, PlaybackError> {
        let mss = MediaSourceStream::new(
            source,
            MediaSourceStreamOptions {
                buffer_len: 64 * 1024,
            },
        );
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
                path: label.clone(),
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
                path: label.clone(),
                reason: "no audio codec parameters".into(),
            })?;
        let sample_rate = params
            .sample_rate
            .ok_or_else(|| PlaybackError::UnsupportedInput {
                path: label.clone(),
                reason: "unknown sample rate".into(),
            })?;
        let channels = params
            .channels
            .as_ref()
            .map(|channels| channels.count() as u16)
            .unwrap_or(0);
        if channels == 0 || channels > 2 {
            return Err(PlaybackError::UnsupportedInput {
                path: label,
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
            path: label,
            reader,
            decoder,
            track_id,
            time_base,
            sample_rate,
            channels,
            // Task 3 boundary: this probe does not yet distinguish a real
            // index/container header from `estimate_num_mpeg_frames`'s
            // ~16-frame extrapolation (symphonia's `Track` exposes no such
            // flag) — that detection is Task 4's job, alongside
            // `SeekMode::Coarse`. Every duration this decoder reports today
            // keeps the meaning it always had.
            metadata: MediaMetadata {
                title,
                duration,
                duration_provenance: PositionProvenance::Established,
            },
            planes: vec![Vec::new(); usize::from(channels)],
            cursor: 0,
            pending: false,
            evidence,
        })
    }

    pub fn metadata(&self) -> &MediaMetadata {
        &self.metadata
    }

    /// Capabilities the decoder can establish *on its own*. The engine combines
    /// these with transport evidence: HTTP range support alone does not prove
    /// that a particular container can seek in media time (§4).
    pub fn capabilities(&self) -> MediaCapabilities {
        MediaCapabilities {
            // Live evidence is checked *first*. A shoutcast server that also
            // sends a Content-Length would otherwise come back Finite and be
            // played as a recording — and `prepare` would never see the
            // `Indefinite` it refuses live media on, so the refusal path would
            // be unreachable.
            continuity: if self.evidence.live {
                Continuity::Indefinite
            } else if self.evidence.byte_len.is_some() || self.metadata.duration.is_some() {
                Continuity::Finite
            } else {
                Continuity::Unresolved
            },
            seek: match (self.evidence.byte_seekable, self.evidence.demuxer) {
                (true, DemuxerSeek::Proven) => SeekSupport::Native,
                (true, DemuxerSeek::Unproven) => SeekSupport::Unknown,
                (false, _) => SeekSupport::Unsupported,
            },
        }
    }

    /// Called once a trial seek (Task 10) demonstrates the demuxer can seek in
    /// media time, promoting `SeekSupport::Unknown` to `Native`.
    pub fn note_demuxer_proven(&mut self) {
        self.evidence.demuxer = DemuxerSeek::Proven;
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
