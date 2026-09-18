//! M8 §8.2: SVG logos decode within fixed bounds, and neither half of
//! usvg's image-href resolver may reach outside the document.

use tenuto::artwork::decode::{ArtworkError, decode_limited};
use tenuto::artwork::svg::SVG_RASTER_BOUND;

const LOGO: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="480" height="480" viewBox="0 0 48 48"><rect width="48" height="48" fill="#5EE08A"/></svg>"##;

/// A nested document with a distinctive fill. Wherever it is referenced,
/// the outer document is otherwise empty, so the raster must stay fully
/// transparent: if the sub-tree were parsed and drawn, `#123456` would
/// appear.
const NESTED_B64: &str = "PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHdpZHRoPSI0OCIgaGVpZ2h0PSI0OCI+PHJlY3Qgd2lkdGg9IjQ4IiBoZWlnaHQ9IjQ4IiBmaWxsPSIjMTIzNDU2Ii8+PC9zdmc+";
const NESTED_PCT: &str = "%3Csvg%20xmlns%3D%22http%3A%2F%2Fwww.w3.org%2F2000%2Fsvg%22%20width%3D%2248%22%20height%3D%2248%22%3E%3Crect%20width%3D%2248%22%20height%3D%2248%22%20fill%3D%22%23123456%22%2F%3E%3C%2Fsvg%3E";

fn decode(svg: &str) -> image::DynamicImage {
    decode_limited(svg.as_bytes()).unwrap_or_else(|error| panic!("{error}"))
}

fn assert_nothing_drawn(image: &image::DynamicImage) {
    let rgba = image.to_rgba8();
    assert!(
        rgba.pixels().all(|pixel| pixel.0[3] == 0),
        "the referenced image must be dropped, not drawn",
    );
}

#[test]
fn an_svg_logo_decodes_to_a_bounded_raster() {
    let image = decode(LOGO);
    assert!(image.width() <= SVG_RASTER_BOUND && image.height() <= SVG_RASTER_BOUND);
    assert!(image.width() > 0 && image.height() > 0);
    let pixel = image
        .to_rgba8()
        .get_pixel(image.width() / 2, image.height() / 2)
        .0;
    assert_eq!(pixel, [0x5E, 0xE0, 0x8A, 255]);
}

#[test]
fn an_enormous_viewbox_still_rasterizes_within_the_bound() {
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="50000" height="50000" viewBox="0 0 50000 50000"><rect width="50000" height="50000" fill="#000"/></svg>"##;
    let image = decode(svg);
    assert!(
        image.width() <= SVG_RASTER_BOUND && image.height() <= SVG_RASTER_BOUND,
        "the declared size never drives the raster size",
    );
}

#[test]
fn aspect_ratio_is_kept_and_the_long_edge_hits_the_bound() {
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100"><rect width="200" height="100" fill="#000"/></svg>"##;
    let image = decode(svg);
    assert_eq!(
        (image.width(), image.height()),
        (SVG_RASTER_BOUND, SVG_RASTER_BOUND / 2)
    );
}

#[test]
fn an_external_href_resolves_to_nothing() {
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48"><image href="/etc/passwd" width="48" height="48"/></svg>"##;
    // Decodes (the element is simply dropped) and reads no file.
    assert_nothing_drawn(&decode(svg));
}

#[test]
fn a_nested_data_svg_is_not_parsed() {
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48"><image href="data:image/svg+xml;base64,{NESTED_B64}" width="48" height="48"/></svg>"##
    );
    assert_nothing_drawn(&decode(&svg));
}

#[test]
fn a_nested_data_text_plain_payload_is_not_parsed() {
    // THE case a feature flag does not cover: usvg's `text/plain` arm falls
    // through to `load_sub_svg` for any payload whose magic bytes are not
    // JPEG/PNG/GIF/WebP. Asserted in its own right, never inferred from the
    // `image/svg+xml` case passing.
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48"><image href="data:text/plain,{NESTED_PCT}" width="48" height="48"/></svg>"##
    );
    assert_nothing_drawn(&decode(&svg));
}

#[test]
fn a_truncated_and_a_malformed_svg_are_typed_errors() {
    assert!(matches!(
        decode_limited(b"<svg xmlns=\"http://www.w3.org/2000/svg\""),
        Err(ArtworkError::Corrupt),
    ));
    assert!(matches!(
        decode_limited(b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect></svg>"),
        Err(ArtworkError::Corrupt),
    ));
}

#[test]
fn a_gzipped_svg_is_unsupported_not_inflated() {
    // svgz is deliberately off: a gzip inflater on attacker-supplied bytes
    // is a decompression surface the 10 MiB cap does not bound, since that
    // cap measures compressed bytes.
    let gzipped = [0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0, 0];
    assert!(matches!(
        decode_limited(&gzipped),
        Err(ArtworkError::Unsupported),
    ));
}

#[test]
fn detection_is_by_content_not_by_declared_type() {
    // A BOM plus an XML prologue, and leading whitespace, both still sniff as
    // SVG; nothing consults a suffix or a content type.
    decode(&format!("\u{feff}<?xml version=\"1.0\"?>\n{LOGO}"));
    decode(&format!("\n  {LOGO}"));
    assert!(matches!(
        decode_limited(b"<html><svg/></html>"),
        Err(ArtworkError::Unsupported),
    ));
}

#[test]
fn a_translucent_fill_keeps_its_straight_alpha_colour() {
    // Guards the premultiplied/straight-alpha conversion. A 50%-opaque pure
    // red over nothing is (255, 0, 0, 128) in straight alpha and
    // (128, 0, 0, 128) premultiplied; passing `Pixmap::take()` straight
    // into RgbaImage yields the latter and every soft edge renders dark.
    let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="#FF0000" fill-opacity="0.5"/></svg>"##;
    let image = decode(svg);
    let pixel = image.to_rgba8().get_pixel(8, 8).0;
    assert_eq!(pixel[3], 128, "alpha is carried through unchanged");
    assert!(
        pixel[0] >= 250,
        "red must be ~255 (straight alpha), not ~128 (premultiplied); got {pixel:?}",
    );
}
