//! Reading and decoding a candidate cover within fixed limits (design doc
//! M5 §9): at most 10 MiB encoded, at most 16 million decoded pixels, JPEG
//! or PNG only. Every rejection is a typed [`ArtworkError`] rather than a
//! panic or an unbounded allocation, so a hostile or merely oversized file
//! next to a track can never cost more than these limits allow.

use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use image::ImageFormat;

/// The largest encoded artwork file this reads, whether embedded or a
/// sibling file (§9: 10 MiB).
pub const MAX_ENCODED_BYTES: u64 = 10 * 1024 * 1024;
/// The largest decoded image, in pixels (§9: 16 million).
pub const MAX_PIXELS: u64 = 16_000_000;
/// The decoder's own allocation ceiling; independent of `MAX_ENCODED_BYTES`
/// because a small encoded file can still expand into a large buffer.
const MAX_ALLOC_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ArtworkError {
    #[error("artwork file is larger than 10 MiB")]
    TooLarge,
    #[error("artwork is larger than 16 million pixels")]
    TooManyPixels,
    #[error("artwork is not JPEG or PNG")]
    Unsupported,
    #[error("artwork could not be decoded")]
    Corrupt,
    #[error("artwork could not be read")]
    Io,
    #[error("artwork decoding panicked")]
    Panicked,
    /// Resizing or encoding for the terminal's image protocol failed.
    #[error("artwork could not be encoded for the terminal")]
    Encoding,
    #[error("no artwork")]
    Missing,
}

/// Reads `path` fully, refusing anything larger than [`MAX_ENCODED_BYTES`]
/// without reading it. The size is checked twice: once via `metadata`
/// before opening the file (the common case, cheap), and again by reading
/// through `File::take(MAX_ENCODED_BYTES + 1)` and rejecting a result that
/// long — which also catches a file that grows between the two checks.
/// Only regular files are read; anything else (a directory, FIFO, or other
/// non-regular entry) is `Io`.
pub fn read_limited(path: &Path) -> Result<Vec<u8>, ArtworkError> {
    let metadata = std::fs::metadata(path).map_err(|_| ArtworkError::Io)?;
    if !metadata.is_file() {
        return Err(ArtworkError::Io);
    }
    if metadata.len() > MAX_ENCODED_BYTES {
        return Err(ArtworkError::TooLarge);
    }
    let file = File::open(path).map_err(|_| ArtworkError::Io)?;
    let mut bytes = Vec::new();
    file.take(MAX_ENCODED_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ArtworkError::Io)?;
    if bytes.len() as u64 > MAX_ENCODED_BYTES {
        return Err(ArtworkError::TooLarge);
    }
    Ok(bytes)
}

/// Decodes `bytes` as a bounded JPEG or PNG. Dimensions are checked, and
/// rejected as [`ArtworkError::TooManyPixels`], *before* any pixel data is
/// decoded; only then does decoding proceed, with the decoder's own limits
/// set to exactly those dimensions plus a fixed allocation ceiling.
pub fn decode_limited(bytes: &[u8]) -> Result<image::DynamicImage, ArtworkError> {
    if bytes.len() as u64 > MAX_ENCODED_BYTES {
        return Err(ArtworkError::TooLarge);
    }

    let format = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ArtworkError::Corrupt)?
        .format();
    if !matches!(format, Some(ImageFormat::Jpeg) | Some(ImageFormat::Png)) {
        return Err(ArtworkError::Unsupported);
    }

    let (width, height) = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ArtworkError::Corrupt)?
        .into_dimensions()
        .map_err(|_| ArtworkError::Corrupt)?;
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(ArtworkError::TooManyPixels);
    }

    let mut limits = image::Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(MAX_ALLOC_BYTES);

    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ArtworkError::Corrupt)?;
    reader.limits(limits);
    reader.decode().map_err(|_| ArtworkError::Corrupt)
}
