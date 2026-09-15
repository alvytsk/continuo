//! Where a track's cover art comes from (design doc M5 §9): an embedded
//! front cover always wins; failing that, a sibling file in the track's own
//! directory, tried in a fixed, documented order.

use std::path::{Path, PathBuf};

use crate::media::tags::CoverBytes;

/// Sibling file names tried, in order, when a track carries no embedded
/// front cover. Exactly the spec's list (§9).
pub const SIBLING_NAMES: [&str; 4] = ["cover.jpg", "cover.png", "folder.jpg", "folder.png"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtworkSource {
    /// The still-encoded bytes of an embedded front cover.
    Embedded(Vec<u8>),
    /// The absolute path to a sibling cover file.
    Sibling(PathBuf),
}

/// Resolves the artwork for `track`: `embedded`, if present, always wins;
/// otherwise the first of [`SIBLING_NAMES`] that names a regular file next
/// to `track`. A symlink whose target is a regular file counts too, because
/// `std::fs::metadata` follows symlinks; a symlink to a directory or a
/// broken symlink does not, since it then reports something other than a
/// regular file (or an error). Directories and other non-regular entries
/// (FIFOs, sockets, device nodes) never count.
pub fn find_artwork(track: &Path, embedded: Option<CoverBytes>) -> Option<ArtworkSource> {
    if let Some(cover) = embedded {
        return Some(ArtworkSource::Embedded(cover.data));
    }
    let dir = track.parent()?;
    SIBLING_NAMES
        .into_iter()
        .map(|name| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .map(|metadata| metadata.is_file())
                .unwrap_or(false)
        })
        .map(ArtworkSource::Sibling)
}
