//! Duration provenance evidence for MP3 (§5.5): whether a Xing/Info or VBRI
//! header in the first frame declares this file's total frame count, versus
//! symphonia falling back to `estimate_num_mpeg_frames`'s ~16-frame
//! extrapolation.
//!
//! Symphonia parses exactly this information into `XingInfoTag`
//! (`symphonia-bundle-mp3-0.6.1/src/demuxer.rs:768`) — a struct marked
//! `#[allow(dead_code)]` whose fields are discarded at the crate boundary
//! except `num_frames` and `lame`. There is no API to ask "did a real
//! header establish this duration, or did symphonia extrapolate it?" — the
//! only trace is a `log::info!` line, not an API — so this module reads the
//! same bytes independently, before the source is handed to
//! `MediaSourceStream`.

use std::io::SeekFrom;

use symphonia::core::io::MediaSource;

use crate::playback::error::PlaybackError;

/// Read at least this many bytes before giving up. Chosen against a real
/// case, not a round number: the Radio-T episode this task's bug report
/// traces to carries a 37,422-byte ID3v2.4 tag, putting its first frame at
/// byte 37,432 and its Xing tag a few bytes past that — comfortably inside
/// 64 KiB, nowhere near an 8 KiB probe.
const PROBE_LEN: usize = 64 * 1024;

const MPEG_HEADER_LEN: usize = 4;
const XING_TAG_ID: [u8; 4] = *b"Xing";
const INFO_TAG_ID: [u8; 4] = *b"Info";
const VBRI_TAG_ID: [u8; 4] = *b"VBRI";
/// The VBRI tag sits at a fixed offset past the frame header, independent of
/// side info length — `symphonia-bundle-mp3-0.6.1/src/demuxer.rs:1025`.
const VBRI_TAG_OFFSET_FROM_HEADER: usize = 32;

/// What established an MP3's frame count, and therefore its duration.
///
/// Symphonia populates `Track::num_frames` from one of three paths and
/// exposes no field saying which ran — the only trace is a `log::info!`
/// line, which is not an API. So we determine it from the container bytes
/// ourselves, before the reader is built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VbrHeader {
    /// A Xing or Info tag in the first frame. `Info` is what LAME writes for
    /// a constant-bitrate file; both carry a declared total frame count.
    XingInfo,
    /// A VBRI tag (Fraunhofer encoders), at a fixed offset in the first frame.
    Vbri,
    /// Neither. Symphonia will fall back to `estimate_num_mpeg_frames`, which
    /// samples ~16 frames and extrapolates.
    Absent,
}

/// Reads the head of a seekable source and reports which header it carries.
/// Restores the source's position before returning `Ok`, so the caller may
/// hand it to `MediaSourceStream` unchanged.
///
/// Returns `Ok(None)` for a source this cannot answer for — a non-MP3
/// container, a short read, or a source that is not seekable. `None` means
/// "no evidence gathered", and the caller must not read it as `Absent`.
pub fn probe_vbr_header(source: &mut dyn MediaSource) -> Result<Option<VbrHeader>, PlaybackError> {
    if !source.is_seekable() {
        return Ok(None);
    }
    let original_pos = source.stream_position()?;
    source.seek(SeekFrom::Start(0))?;
    // On failure, deliberately skip the restoring seek below rather than
    // running it unconditionally: a remote source's read failure is latched
    // by `LatchingSource` (`playback::prepare`) so the caller can recover
    // *why* opening failed, and that latch treats any later successful
    // seek as proof the fault cleared, wiping the very failure this
    // function is about to propagate. A caller that gets `Err` here is
    // abandoning this source anyway (`DecodedSource::from_media_source`
    // never reaches `MediaSourceStream::new` on this path), so there is no
    // reader left for a restored position to matter to. Verified against
    // `tests/prepare.rs::opening_that_exceeds_the_probe_byte_cap_fails_cancellably`
    // and `::a_probe_that_outlives_its_deadline_is_refused`, both of which
    // this exact ordering mistake broke.
    let buf = gather_evidence(source)?;
    source.seek(SeekFrom::Start(original_pos))?;
    Ok(detect(&buf))
}

/// How much of the head to read unconditionally. Deliberately small: large
/// enough to hold an ID3v2 header and, for the overwhelming majority of real
/// files (no tag, or a small one), the frame header and tag-check region
/// too — so the common case never asks a source for more than this.
const INITIAL_LEN: usize = 4 * 1024;

/// Bytes needed past the frame header to check both possible tag positions:
/// Xing/Info's widest `side_info_len` (32, MPEG1 stereo) and VBRI's fixed
/// 32-byte offset, plus the 4-byte tag id itself.
const TAG_REGION_LEN: usize = 32 + 4;

/// Reads only as much of the head as the evidence actually requires: a
/// small initial chunk, extended only when that chunk itself reveals an
/// ID3v2 tag long enough to push the first frame beyond it — never the full
/// `PROBE_LEN` cap for the ordinary case of no tag or a small one.
///
/// This matters beyond efficiency: a remote source can have more bytes to
/// deliver than it has sent yet (a slow or deliberately stalled body,
/// chiefly, as `tests/app_cli.rs`'s stall-recovery tests exercise on
/// `sine-5s.flac`). Forcing a full 64 KiB read for every open — including
/// files that plainly do not need it — would block this open on exactly
/// the backpressure those tests exist to prove the engine can get out of
/// band of. Extending only when the ID3 header itself says the frame sits
/// further out keeps that blocking read confined to files that actually
/// have that much of a tag — which is also the only case §5.5's evidence
/// gathering needs it for.
fn gather_evidence(source: &mut dyn MediaSource) -> Result<Vec<u8>, PlaybackError> {
    let mut buf = read_up_to(source, INITIAL_LEN)?;
    // `first_frame_offset` never actually returns `None` (see its own
    // comment); the fallback is defensive, not a guess.
    let frame_offset = first_frame_offset(&buf).unwrap_or(0);
    let needed = frame_offset.saturating_add(MPEG_HEADER_LEN + TAG_REGION_LEN);
    if needed > buf.len() {
        let target = needed.min(PROBE_LEN);
        if target > buf.len() {
            let more = read_up_to(source, target - buf.len())?;
            buf.extend(more);
        }
    }
    Ok(buf)
}

/// Reads up to `len` further bytes from `source`'s current position,
/// stopping early at EOF. The returned `Vec` is exactly as long as what was
/// actually read.
fn read_up_to(source: &mut dyn MediaSource, len: usize) -> Result<Vec<u8>, PlaybackError> {
    let mut buf = vec![0u8; len];
    let mut filled = 0usize;
    while filled < buf.len() {
        match source.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(PlaybackError::from(error)),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Where the first MPEG frame begins, per the leading ID3v2 tag if there is
/// one. Always `Some`: a buffer too short to hold an ID3v2 header is
/// treated the same as one with no tag at all (offset 0), since neither is
/// evidence of a tag actually being present. Kept as `Option` because
/// `detect`'s own `?` reads more naturally against it and a future refinement
/// may have a genuine "cannot tell" case.
fn first_frame_offset(buf: &[u8]) -> Option<usize> {
    if buf.len() < 10 || &buf[0..3] != b"ID3" {
        // No ID3v2 tag (or too little to tell): the stream, if this is MP3
        // at all, starts with the first frame directly.
        return Some(0);
    }
    let flags = buf[5];
    let size = (u32::from(buf[6] & 0x7f) << 21)
        | (u32::from(buf[7] & 0x7f) << 14)
        | (u32::from(buf[8] & 0x7f) << 7)
        | u32::from(buf[9] & 0x7f);
    let mut offset = 10usize + size as usize;
    // Footer flag (ID3v2.4 §3.1, bit 4 of the flags byte): an optional
    // 10-byte copy of the header repeated after the frames.
    if flags & 0x10 != 0 {
        offset += 10;
    }
    Some(offset)
}

/// The two frame-header fields this detection needs beyond `MPEG_HEADER_LEN`
/// itself: whether the frame uses MPEG1 side-info sizing, and whether it is
/// mono — both of which `side_info_len` depends on.
struct FrameShape {
    is_mpeg1: bool,
    is_mono: bool,
    is_layer3: bool,
}

/// Validates a 4-byte MPEG frame header word and extracts what this module
/// needs from it. Mirrors the field checks in
/// `symphonia-bundle-mp3-0.6.1/src/header.rs::parse_frame_header` — sync,
/// version, layer, bitrate index, sample-rate index — without needing that
/// crate's private types, since only validity and two derived booleans are
/// needed here, never a full decode.
fn parse_frame_shape(word: [u8; 4]) -> Option<FrameShape> {
    if word[0] != 0xFF || word[1] & 0xE0 != 0xE0 {
        return None;
    }
    let header = u32::from_be_bytes(word);

    let version_bits = (header & 0x18_0000) >> 19;
    if version_bits == 0b01 {
        // Reserved version.
        return None;
    }
    let is_mpeg1 = version_bits == 0b11;

    let layer_bits = (header & 0x6_0000) >> 17;
    if layer_bits == 0b00 {
        // Reserved layer.
        return None;
    }
    let is_layer3 = layer_bits == 0b01;

    let bitrate_index = (header & 0xf000) >> 12;
    if bitrate_index == 0 || bitrate_index == 15 {
        return None;
    }

    let sample_rate_index = (header & 0xc00) >> 10;
    if sample_rate_index == 0b11 {
        return None;
    }

    let channel_mode_bits = (header & 0xc0) >> 6;
    let is_mono = channel_mode_bits == 0b11;

    Some(FrameShape {
        is_mpeg1,
        is_mono,
        is_layer3,
    })
}

/// `symphonia-bundle-mp3-0.6.1/src/common.rs::FrameHeader::side_info_len`,
/// reproduced against the two fields this module tracks instead of that
/// crate's private `MpegVersion`/`ChannelMode` types.
fn side_info_len(shape: &FrameShape) -> usize {
    match (shape.is_mpeg1, shape.is_mono) {
        (true, true) => 17,
        (true, false) => 32,
        (false, true) => 9,
        (false, false) => 17,
    }
}

fn detect(buf: &[u8]) -> Option<VbrHeader> {
    let frame_offset = first_frame_offset(buf)?;
    let header_end = frame_offset.checked_add(MPEG_HEADER_LEN)?;
    let word: [u8; 4] = buf.get(frame_offset..header_end)?.try_into().ok()?;
    let shape = parse_frame_shape(word)?;

    let xing_pos = header_end + side_info_len(&shape);
    let vbri_pos = header_end + VBRI_TAG_OFFSET_FROM_HEADER;
    // Both candidate positions must be fully covered by what was read
    // before concluding `Absent` — a short read must report "no evidence",
    // never "no header present".
    let furthest_needed = xing_pos.max(vbri_pos) + 4;
    if buf.len() < furthest_needed {
        return None;
    }

    if shape.is_layer3 {
        let candidate = &buf[xing_pos..xing_pos + 4];
        if candidate == XING_TAG_ID || candidate == INFO_TAG_ID {
            return Some(VbrHeader::XingInfo);
        }
    }
    let candidate = &buf[vbri_pos..vbri_pos + 4];
    if candidate == VBRI_TAG_ID {
        return Some(VbrHeader::Vbri);
    }

    Some(VbrHeader::Absent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Seek};

    /// A trivial in-memory `MediaSource`, so `probe_vbr_header` itself can be
    /// exercised directly against a fabricated buffer, without going through
    /// a real file or an HTTP fixture. Necessary for
    /// `probe_vbr_header_finds_a_xing_tag_beyond_an_8kib_id3_tag`: routing
    /// that case through `DecodedSource::open` (as the integration test
    /// does) cannot actually pin down that this function reads far enough,
    /// because `detect`'s own "offset beyond what was read means `None`,
    /// never `Absent`" rule (correctly) maps a too-small read to
    /// `Established` too — the same answer a correct 64 KiB read would give,
    /// for a different and wrong reason. Only asserting the `VbrHeader`
    /// value itself, not the `PositionProvenance` it eventually becomes,
    /// tells the two apart.
    struct MemSource(Cursor<Vec<u8>>);

    impl Read for MemSource {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Seek for MemSource {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(pos)
        }
    }

    impl MediaSource for MemSource {
        fn is_seekable(&self) -> bool {
            true
        }

        fn byte_len(&self) -> Option<u64> {
            Some(self.0.get_ref().len() as u64)
        }
    }

    /// An ID3v2.4 tag of `padding` zero bytes of content, followed by a
    /// valid MPEG1 Layer III stereo frame header (`side_info_len` 32) with a
    /// `Xing` tag glued on immediately after its side info.
    fn id3_then_xing_frame(padding: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"ID3");
        buf.extend_from_slice(&[4, 0, 0]); // version, revision, flags
        let size = padding as u32;
        buf.extend_from_slice(&[
            ((size >> 21) & 0x7f) as u8,
            ((size >> 14) & 0x7f) as u8,
            ((size >> 7) & 0x7f) as u8,
            (size & 0x7f) as u8,
        ]);
        buf.extend(std::iter::repeat_n(0u8, padding));
        buf.extend_from_slice(&[0xff, 0xfb, 0x90, 0x00]); // MPEG1 L3 stereo
        buf.extend(std::iter::repeat_n(0u8, 32)); // side info, stereo
        buf.extend_from_slice(b"Xing");
        buf
    }

    #[test]
    fn probe_vbr_header_finds_a_xing_tag_beyond_an_8kib_id3_tag() {
        // The ID3 tag alone is comfortably past 8 KiB but still well inside
        // the 64 KiB this probe must read - the exact "8 KB trap" the
        // design doc calls out. Directly pins `PROBE_LEN`'s floor, which
        // the integration-level `Established` assertion cannot (see
        // `MemSource`'s own comment).
        let bytes = id3_then_xing_frame(20 * 1024);
        #[allow(clippy::unwrap_used)]
        let result = probe_vbr_header(&mut MemSource(Cursor::new(bytes))).unwrap();
        assert_eq!(result, Some(VbrHeader::XingInfo));
    }

    #[test]
    fn first_frame_offset_is_zero_with_no_id3_tag() {
        assert_eq!(first_frame_offset(b"\xff\xfb\x90\x00rest"), Some(0));
    }

    #[test]
    fn first_frame_offset_decodes_the_syncsafe_size() {
        // ID3 header + syncsafe size 0x22 (34) => frame at 10 + 34 = 44.
        let mut buf = vec![b'I', b'D', b'3', 4, 0, 0, 0, 0, 0, 0x22];
        buf.resize(50, 0);
        assert_eq!(first_frame_offset(&buf), Some(44));
    }

    #[test]
    fn first_frame_offset_accounts_for_the_footer_flag() {
        // Same as above but with the footer flag (bit 4) set: 10 more bytes.
        let mut buf = vec![b'I', b'D', b'3', 4, 0, 0x10, 0, 0, 0, 0x22];
        buf.resize(60, 0);
        assert_eq!(first_frame_offset(&buf), Some(54));
    }

    #[test]
    fn parse_frame_shape_rejects_a_bad_sync() {
        assert!(parse_frame_shape([0xff, 0x1b, 0x90, 0x00]).is_none());
    }

    #[test]
    fn parse_frame_shape_rejects_a_reserved_version() {
        // Version bits 0b01 are reserved; every other combination
        // (0b00, 0b10, 0b11) is a real MPEG version.
        assert!(parse_frame_shape([0xff, 0xeb, 0x90, 0x00]).is_none());
    }

    #[test]
    fn parse_frame_shape_rejects_a_reserved_layer() {
        // Layer bits 0b00 are reserved; 0b01/0b10/0b11 are Layer III/II/I.
        assert!(parse_frame_shape([0xff, 0xf9, 0x90, 0x00]).is_none());
    }

    #[test]
    fn parse_frame_shape_rejects_an_invalid_bitrate_index() {
        // Bitrate index 0b1111 (15) is invalid.
        assert!(parse_frame_shape([0xff, 0xfb, 0xf0, 0x00]).is_none());
    }

    #[test]
    fn parse_frame_shape_rejects_an_invalid_sample_rate_index() {
        // Sample-rate index 0b11 (3) is reserved.
        assert!(parse_frame_shape([0xff, 0xfb, 0x0c, 0x00]).is_none());
    }

    #[test]
    fn detect_reports_absent_for_a_valid_header_with_no_tag() {
        let mut buf = vec![0xff, 0xfb, 0x90, 0x00];
        buf.resize(200, 0);
        assert_eq!(detect(&buf), Some(VbrHeader::Absent));
    }

    #[test]
    fn detect_reports_vbri_for_a_valid_header_with_a_vbri_tag() {
        // A mono MPEG1 header: side_info_len is 17, distinct from VBRI's
        // fixed 32-byte offset, so a "VBRI" tag placed at the VBRI position
        // cannot be coincidentally matched by the Xing/Info check the way a
        // stereo header's matching offsets (32 either way) could hide a
        // broken VBRI branch behind a passing Xing/Info one.
        let mut buf = vec![0xff, 0xfb, 0x90, 0xc0]; // MPEG1 Layer III, mono
        buf.extend(std::iter::repeat_n(0u8, 32));
        buf.extend_from_slice(b"VBRI");
        assert_eq!(detect(&buf), Some(VbrHeader::Vbri));
    }

    #[test]
    fn detect_reports_none_on_a_truncated_read() {
        // A valid-looking header word but nothing after it to check either
        // tag position against.
        let buf = vec![0xff, 0xfb, 0x90, 0x00];
        assert_eq!(detect(&buf), None);
    }

    #[test]
    fn detect_reports_none_for_bytes_that_are_not_an_mpeg_frame() {
        // FLAC's own signature, standing in for "not MP3 at all".
        let mut buf = b"fLaC".to_vec();
        buf.resize(200, 0);
        assert_eq!(detect(&buf), None);
    }
}
