//! The allocation-free output tap (spec §10).
//!
//! [`TapWriter::offer`] runs on the real-time audio callback, alongside
//! [`crate::playback::callback::CallbackCore::fill`]: no allocation, no lock,
//! no I/O, and it never blocks. It copies post-gain interleaved PCM into a
//! bounded [`rtrb`] ring and labels each accepted block with a
//! [`TapDescriptor`] in a second, matching ring, so a reader on another
//! thread can pull exactly the samples a descriptor describes and never see
//! orphan PCM, a partial channel frame, or an unmatched descriptor.
//!
//! [`TapWriter`] and [`TapReader`] share only one atomic: `enabled`. Nothing
//! else crosses threads outside the two `rtrb` rings themselves.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rtrb::{Consumer, Producer, RingBuffer};

use super::bands::WINDOW;
use crate::playback::output::Nanos;

/// Labels one accepted tap block: the transport, its output generation and
/// control epoch, the format it was captured at, its position in the
/// discontinuity sequence, the predicted output instant, and how many
/// interleaved samples (not frames) follow it in the PCM ring.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TapDescriptor {
    pub instance: u64,
    pub generation: u16,
    pub epoch: u32,
    pub channels: u16,
    pub sample_rate: u32,
    pub discontinuity: u32,
    pub predicted: Nanos,
    pub samples: u32,
}

/// Upper bound on undelivered descriptors. A descriptor is a few words; PCM
/// is the bulk of the ring's memory, sized separately in [`tap_pair`].
pub const DESCRIPTOR_CAPACITY: usize = 256;

/// The real-time producer half of the tap. Lives on the audio callback.
pub struct TapWriter {
    pcm: Producer<f32>,
    descriptors: Producer<TapDescriptor>,
    instance: u64,
    enabled: Arc<AtomicBool>,
    /// Set when the most recent block(s) could not be committed (disabled,
    /// or either ring was too full), so the *next* accepted block bumps
    /// `discontinuity` before it is published.
    lost: bool,
    discontinuity: u32,
}

/// The consumer half of the tap. Lives on the analysis worker.
pub struct TapReader {
    pcm: Consumer<f32>,
    descriptors: Consumer<TapDescriptor>,
}

/// Builds a matched [`TapWriter`]/[`TapReader`] pair for a transport whose
/// output is `channels` at `sample_rate`, sharing `enabled` as the tap's one
/// cross-thread flag.
///
/// The PCM ring holds half a second of audio, rounded down to whole frames
/// (`sample_rate / 2` frames), or one [`WINDOW`] of frames, whichever is
/// larger - never zero, even at implausibly low sample rates. `instance`
/// labels every descriptor this writer ever publishes, so a reader can
/// reject samples from a retired or recreated transport (spec §10).
pub fn tap_pair(
    instance: u64,
    channels: u16,
    sample_rate: u32,
    enabled: Arc<AtomicBool>,
) -> (TapWriter, TapReader) {
    let channels = usize::from(channels.max(1));
    let frames = (sample_rate / 2).max(WINDOW as u32) as usize;
    let (pcm_tx, pcm_rx) = RingBuffer::<f32>::new(frames * channels);
    let (descriptors_tx, descriptors_rx) = RingBuffer::<TapDescriptor>::new(DESCRIPTOR_CAPACITY);
    (
        TapWriter {
            pcm: pcm_tx,
            descriptors: descriptors_tx,
            instance,
            enabled,
            lost: false,
            discontinuity: 0,
        },
        TapReader {
            pcm: pcm_rx,
            descriptors: descriptors_rx,
        },
    )
}

impl TapWriter {
    /// Offers one callback's worth of post-gain interleaved PCM to the tap.
    ///
    /// Real-time safe: every path below is O(1) index arithmetic over
    /// already-allocated rings, with no lock, no I/O, and no blocking -
    /// including the disabled and capacity-exhausted paths, which drop the
    /// whole block rather than partially commit it (spec §10).
    #[allow(clippy::too_many_arguments)] // mirrors the brief's descriptor fields exactly.
    pub fn offer(
        &mut self,
        samples: &[f32],
        channels: u16,
        sample_rate: u32,
        generation: u16,
        epoch: u32,
        predicted: Nanos,
    ) {
        // Relaxed: `enabled` only ever gates whether a block is copied, and a
        // stale read costs at most one block copied-then-unread or one block
        // dropped-when-it-could-have-run - never a torn read, a partial
        // frame, or an orphaned descriptor. The reader re-checks freshness on
        // every block it does receive, so a one-callback-late flag flip is
        // invisible to it.
        if !self.enabled.load(Ordering::Relaxed) {
            self.lost = true;
            return;
        }

        let channels_usize = usize::from(channels.max(1));
        let whole = samples.len() / channels_usize * channels_usize;
        if whole == 0 {
            return;
        }

        if self.descriptors.slots() == 0 || self.pcm.slots() < whole {
            self.lost = true;
            return;
        }

        let Ok(mut chunk) = self.pcm.write_chunk(whole) else {
            // Unreachable given the `slots()` check above (single producer,
            // nothing else shrinks it), but offer() must never panic.
            self.lost = true;
            return;
        };
        let (first, second) = chunk.as_mut_slices();
        let (source_first, source_second) = samples[..whole].split_at(first.len());
        first.copy_from_slice(source_first);
        second.copy_from_slice(source_second);
        chunk.commit_all(); // PCM lands before its descriptor - never the reverse.

        if self.lost {
            self.discontinuity = self.discontinuity.wrapping_add(1);
            self.lost = false;
        }

        let descriptor = TapDescriptor {
            instance: self.instance,
            generation,
            epoch,
            channels,
            sample_rate,
            discontinuity: self.discontinuity,
            predicted,
            #[allow(clippy::cast_possible_truncation)] // whole <= the ring's own usize capacity.
            samples: whole as u32,
        };
        // Guaranteed to succeed: this producer is the ring's only writer, and
        // `descriptors.slots() > 0` was just confirmed above.
        let _ = self.descriptors.push(descriptor);
    }
}

impl TapReader {
    /// Pops the next described block, appending its samples to `out`.
    ///
    /// Never allocates when `out` already has spare capacity: both halves of
    /// the PCM chunk are appended with `extend_from_slice`. Returns `None`
    /// when no block is waiting.
    pub fn next_block(&mut self, out: &mut Vec<f32>) -> Option<TapDescriptor> {
        let descriptor = self.descriptors.pop().ok()?;
        let samples = descriptor.samples as usize;
        // `offer` always commits PCM (its step 4) before pushing the
        // descriptor that describes it (its step 6), so a popped descriptor's
        // PCM is always already available - this should never be false. Guard
        // it rather than panic, per the brief's ruling.
        debug_assert!(
            self.pcm.slots() >= samples,
            "tap descriptor published without its matching PCM"
        );
        let Ok(chunk) = self.pcm.read_chunk(samples) else {
            return None;
        };
        let (first, second) = chunk.as_slices();
        out.extend_from_slice(first);
        out.extend_from_slice(second);
        chunk.commit_all();
        Some(descriptor)
    }
}
