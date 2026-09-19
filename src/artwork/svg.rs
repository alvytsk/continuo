//! Rasterizing an SVG cover within fixed bounds (design doc M7.1 §8.2).
//!
//! Every external reference is refused, in **both** halves of usvg's
//! resolver. Overriding only the string half is insufficient: usvg's
//! `default_data_resolver` routes `image/svg+xml` into `load_sub_svg`, and
//! its `text/plain` arm falls through to the same function for any payload
//! whose magic bytes are not JPEG, PNG, GIF or WebP. Disabling resvg's
//! `raster-images` does not close that path — the feature governs
//! rendering, in resvg, while `load_sub_svg` is parsing, in usvg.

use resvg::tiny_skia;
use resvg::usvg;

use super::decode::ArtworkError;

/// The longest edge an SVG is rasterized to (§8.2). The document's declared
/// size never drives this, so a `viewBox` claiming 50000×50000 costs exactly
/// what a 48×48 one costs.
pub const SVG_RASTER_BOUND: u32 = 512;

/// `true` when `bytes` look like an SVG document. Content sniffing, never
/// the URL suffix or the response's content type — both are
/// attacker-controlled (§8.2).
pub fn looks_like_svg(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    let text = String::from_utf8_lossy(head);
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && text.contains("<svg"))
}

fn options() -> usvg::Options<'static> {
    usvg::Options {
        resources_dir: None,
        // Both halves. See this module's doc comment for why one is not enough.
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_mime, _data, _options| None),
            resolve_string: Box::new(|_href, _options| None),
        },
        ..usvg::Options::default()
    }
}

/// Parses and rasterizes `bytes` within [`SVG_RASTER_BOUND`].
pub fn decode_svg(bytes: &[u8]) -> Result<image::DynamicImage, ArtworkError> {
    let tree = usvg::Tree::from_data(bytes, &options()).map_err(|_| ArtworkError::Corrupt)?;

    let size = tree.size();
    let bound = SVG_RASTER_BOUND as f32;
    let scale = f32::min(bound / size.width(), bound / size.height());
    let width = ((size.width() * scale).round() as u32).clamp(1, SVG_RASTER_BOUND);
    let height = ((size.height() * scale).round() as u32).clamp(1, SVG_RASTER_BOUND);

    let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or(ArtworkError::Encoding)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // `take()` returns PREMULTIPLIED RGBA, tiny-skia's internal
    // representation; `image::RgbaImage` expects straight alpha. Passing
    // `take()` here renders every translucent fill and every antialiased
    // edge too dark. `take_demultiplied` is the conversion, and it is what
    // tiny-skia's own PNG encoder uses.
    let buffer = image::RgbaImage::from_raw(width, height, pixmap.take_demultiplied())
        .ok_or(ArtworkError::Corrupt)?;
    Ok(image::DynamicImage::ImageRgba8(buffer))
}
