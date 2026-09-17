//! A copy of `tests/fixtures/sine.flac` carrying title, artist and album
//! tags and, optionally, an embedded front cover — built byte by byte so no
//! tagging crate is needed and the fixture directory stays binary-free.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");
const LAST_BLOCK: u8 = 0x80;
const VORBIS_COMMENT: u8 = 4;
const PICTURE: u8 = 6;
/// The ID3v2 picture type FLAC reuses for "Cover (front)".
const FRONT_COVER: u32 = 3;

/// Writes `tagged.flac` into `dir` and returns its canonical path.
pub fn tagged_flac(
    dir: &Path,
    title: &str,
    artist: &str,
    album: &str,
    cover_png: Option<&[u8]>,
) -> PathBuf {
    let mut bytes = std::fs::read(FIXTURE).unwrap_or_else(|error| panic!("read fixture: {error}"));
    assert_eq!(&bytes[..4], b"fLaC", "the fixture is a native FLAC stream");

    // Walk the metadata blocks to the last one and clear its last-block flag.
    let mut offset = 4;
    let audio_start = loop {
        let header = bytes[offset];
        let length = usize::from(bytes[offset + 1]) << 16
            | usize::from(bytes[offset + 2]) << 8
            | usize::from(bytes[offset + 3]);
        if header & LAST_BLOCK != 0 {
            bytes[offset] = header & !LAST_BLOCK;
            break offset + 4 + length;
        }
        offset += 4 + length;
    };

    let mut blocks = Vec::new();
    push_block(
        &mut blocks,
        VORBIS_COMMENT,
        &vorbis_comment(&[
            format!("TITLE={title}"),
            format!("ARTIST={artist}"),
            format!("ALBUM={album}"),
        ]),
    );
    if let Some(png) = cover_png {
        push_block(&mut blocks, PICTURE, &picture(png));
    }
    mark_last(&mut blocks);

    let audio = bytes.split_off(audio_start);
    bytes.extend_from_slice(&blocks);
    bytes.extend_from_slice(&audio);

    let path = dir.join("tagged.flac");
    std::fs::write(&path, &bytes).unwrap_or_else(|error| panic!("write tagged flac: {error}"));
    path.canonicalize()
        .unwrap_or_else(|error| panic!("canonical tagged flac: {error}"))
}

/// Appends one metadata block, header included, with its last-block flag
/// clear.
fn push_block(blocks: &mut Vec<u8>, block_type: u8, body: &[u8]) {
    let length = u32::try_from(body.len()).unwrap_or_else(|_| panic!("block too large"));
    assert!(length < 1 << 24, "a FLAC metadata block length is 24 bits");
    blocks.push(block_type);
    blocks.extend_from_slice(&length.to_be_bytes()[1..]);
    blocks.extend_from_slice(body);
}

/// Sets the last-block flag on the final block in `blocks`.
fn mark_last(blocks: &mut [u8]) {
    let mut offset = 0;
    let mut last = 0;
    while offset < blocks.len() {
        last = offset;
        let length = usize::from(blocks[offset + 1]) << 16
            | usize::from(blocks[offset + 2]) << 8
            | usize::from(blocks[offset + 3]);
        offset += 4 + length;
    }
    blocks[last] |= LAST_BLOCK;
}

/// Vorbis comment lengths and counts are little-endian, unlike the rest of
/// FLAC's metadata.
fn vorbis_comment(entries: &[String]) -> Vec<u8> {
    let vendor = b"tenuto tests";
    let mut body = Vec::new();
    body.extend_from_slice(&le_len(vendor.len()));
    body.extend_from_slice(vendor);
    body.extend_from_slice(&le_len(entries.len()));
    for entry in entries {
        body.extend_from_slice(&le_len(entry.len()));
        body.extend_from_slice(entry.as_bytes());
    }
    body
}

fn picture(png: &[u8]) -> Vec<u8> {
    let mime = b"image/png";
    let mut body = Vec::new();
    body.extend_from_slice(&FRONT_COVER.to_be_bytes());
    body.extend_from_slice(&be_len(mime.len()));
    body.extend_from_slice(mime);
    body.extend_from_slice(&be_len(0)); // description
    // Width and height are only a hint to readers; the tests' cover is 2×2.
    body.extend_from_slice(&2u32.to_be_bytes()); // width
    body.extend_from_slice(&2u32.to_be_bytes()); // height
    body.extend_from_slice(&32u32.to_be_bytes()); // colour depth
    body.extend_from_slice(&0u32.to_be_bytes()); // indexed colours
    body.extend_from_slice(&be_len(png.len()));
    body.extend_from_slice(png);
    body
}

fn le_len(length: usize) -> [u8; 4] {
    u32::try_from(length)
        .unwrap_or_else(|_| panic!("length fits in u32"))
        .to_le_bytes()
}

fn be_len(length: usize) -> [u8; 4] {
    u32::try_from(length)
        .unwrap_or_else(|_| panic!("length fits in u32"))
        .to_be_bytes()
}
