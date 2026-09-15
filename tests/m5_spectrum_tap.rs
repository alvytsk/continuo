//! Task 27: the allocation-free output tap (spec §10).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use continuo::playback::output::Nanos;
use continuo::playback::spectrum::tap::tap_pair;

fn pair(
    enabled: bool,
) -> (
    continuo::playback::spectrum::tap::TapWriter,
    continuo::playback::spectrum::tap::TapReader,
) {
    tap_pair(7, 2, 48_000, Arc::new(AtomicBool::new(enabled)))
}

#[test]
fn only_whole_frames_are_committed_with_a_matching_descriptor() {
    let (mut writer, mut reader) = pair(true);
    writer.offer(&[1.0, 2.0, 3.0, 4.0, 5.0], 2, 48_000, 3, 9, Nanos(100));
    let mut out = Vec::new();
    let descriptor = reader.next_block(&mut out).expect("described");
    assert_eq!(
        (
            descriptor.samples,
            descriptor.instance,
            descriptor.generation,
            descriptor.epoch
        ),
        (4, 7, 3, 9)
    );
    assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    assert!(reader.next_block(&mut out).is_none(), "no orphan PCM");
}

#[test]
fn a_block_that_does_not_fit_is_dropped_whole_and_marks_a_discontinuity() {
    let (mut writer, mut reader) = pair(true);
    let big = vec![0.25; 48_000]; // exactly the half-second ring for stereo
    writer.offer(&big, 2, 48_000, 1, 1, Nanos(0));
    writer.offer(&[0.5, 0.5], 2, 48_000, 1, 1, Nanos(1)); // no room: dropped
    let mut out = Vec::new();
    let first = reader.next_block(&mut out).expect("first block");
    assert_eq!(first.discontinuity, 0);
    assert!(
        reader.next_block(&mut out).is_none(),
        "the dropped block left nothing behind"
    );
    out.clear();
    writer.offer(&[0.75, 0.75], 2, 48_000, 1, 1, Nanos(2));
    let next = reader.next_block(&mut out).expect("accepted");
    assert_eq!(next.discontinuity, 1);
    assert_eq!(out, [0.75, 0.75]);
}

#[test]
fn descriptor_exhaustion_also_drops_whole_blocks() {
    let (mut writer, mut reader) = pair(true);
    for n in 0..300 {
        writer.offer(&[n as f32, n as f32], 2, 48_000, 1, 1, Nanos(n));
    }
    let mut out = Vec::new();
    let mut blocks = 0;
    while reader.next_block(&mut out).is_some() {
        blocks += 1;
    }
    assert_eq!(blocks, 256);
    assert_eq!(out.len(), 512);
}

#[test]
fn data_survives_ring_wraparound_in_order() {
    // 480 frames per block against a 24 000-frame ring: 2 000 rounds wrap it
    // about forty times.
    let (mut writer, mut reader) = pair(true);
    let mut block = vec![0.0f32; 960];
    let mut out = Vec::new();
    for round in 0..2_000u64 {
        for (index, sample) in block.iter_mut().enumerate() {
            *sample = ((round as usize * 960 + index) % 16_777_216) as f32;
        }
        writer.offer(&block, 2, 48_000, 1, 1, Nanos(round));
        out.clear();
        let descriptor = reader.next_block(&mut out).expect("block");
        assert_eq!(descriptor.samples, 960);
        assert_eq!(out, block, "round {round}");
    }
}

#[test]
fn a_disabled_or_unread_tap_never_blocks_the_writer() {
    let (mut disabled, mut reader) = pair(false);
    disabled.offer(&[1.0, 1.0], 2, 48_000, 1, 1, Nanos(0));
    assert!(reader.next_block(&mut Vec::new()).is_none());

    let (mut writer, _unread) = pair(true);
    let block = vec![0.1f32; 960];
    let started = Instant::now();
    for n in 0..20_000 {
        writer.offer(&block, 2, 48_000, 1, 1, Nanos(n));
    }
    assert!(
        started.elapsed().as_millis() < 500,
        "saturation must stay cheap"
    );
}
