//! What a local file says about itself before anything decodes it (design
//! doc M5 §8–§9): title, artist and album tags, the container's duration and
//! the embedded front cover. Only symphonia's container probe runs — no
//! decoder is built, no packet is read and no audio device is opened — so
//! enriching a queue row costs a header read, not playback.

use std::time::Duration;

use symphonia::core::formats::TrackType;
use symphonia::core::meta::StandardVisualKey;

use crate::media::id::AbsolutePath;
use crate::playback::decode::{
    ProbedContainer, open_local_file, probe_container, standard_names, track_duration,
};
use crate::playback::error::PlaybackError;
use crate::playback::provenance::PositionProvenance;

/// The largest embedded cover kept, encoded (§9: 10 MiB).
pub const MAX_EMBEDDED_COVER_BYTES: usize = 10 * 1024 * 1024;

/// An embedded picture exactly as the file stores it, still encoded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoverBytes {
    pub data: Vec<u8>,
    /// The MIME type the tag declares; a hint, not a verified format.
    pub media_type: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LocalTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    /// Computed exactly as playback computes it, so a row enriched here
    /// never claims more certainty than the loaded track later would.
    pub duration_provenance: PositionProvenance,
    pub front_cover: Option<CoverBytes>,
    /// A front cover exists but is larger than [`MAX_EMBEDDED_COVER_BYTES`];
    /// `front_cover` is then `None`.
    pub cover_oversized: bool,
}

/// Probes `path`'s container and reads its current metadata revision. The
/// tag strings are returned as the file spells them; whoever displays them
/// escapes them.
///
/// Only a visual whose usage is `FrontCover` counts (decision 13): the spec
/// asks for front-cover artwork, and an untyped or back-cover picture must
/// not stand in for it.
pub fn probe_local_tags(path: &AbsolutePath) -> Result<LocalTags, PlaybackError> {
    let local = open_local_file(path)?;
    let ProbedContainer {
        mut reader,
        vbr_header,
    } = probe_container(Box::new(local.file), &local.hint)?;
    let (duration, duration_provenance) =
        track_duration(reader.default_track(TrackType::Audio), vbr_header);

    let metadata = reader.metadata();
    let revision = metadata.current();
    let names = standard_names(revision);
    let cover = revision.and_then(|revision| {
        revision
            .media
            .visuals
            .iter()
            .find(|visual| visual.usage == Some(StandardVisualKey::FrontCover))
    });
    let (front_cover, cover_oversized) = match cover {
        Some(visual) if visual.data.len() > MAX_EMBEDDED_COVER_BYTES => (None, true),
        Some(visual) => (
            Some(CoverBytes {
                data: visual.data.to_vec(),
                media_type: visual.media_type.clone(),
            }),
            false,
        ),
        None => (None, false),
    };

    Ok(LocalTags {
        title: names.title,
        artist: names.artist,
        album: names.album,
        duration,
        duration_provenance,
        front_cover,
        cover_oversized,
    })
}
