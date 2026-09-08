use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use super::output::{Nanos, SpanRecord};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Run,
    Freeze,
    Discard,
    Park,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Adopted {
    Running,
    Frozen,
    Parked,
}

impl Phase {
    fn as_u8(self) -> u8 {
        match self {
            Self::Run => 0,
            Self::Freeze => 1,
            Self::Discard => 2,
            Self::Park => 3,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Freeze,
            2 => Self::Discard,
            3 => Self::Park,
            _ => Self::Run,
        }
    }
}

impl Adopted {
    fn as_u8(self) -> u8 {
        match self {
            Self::Running => 0,
            Self::Frozen => 1,
            Self::Parked => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Frozen,
            2 => Self::Parked,
            _ => Self::Running,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Control {
    pub generation: u16,
    pub epoch: u32,
    pub phase: Phase,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Diagnostics {
    pub xruns: u32,
    pub spans_dropped: u32,
}

fn pack(generation: u16, epoch: u32, tag: u8) -> u64 {
    (u64::from(generation) << 48) | (u64::from(epoch) << 8) | u64::from(tag)
}

fn unpack(word: u64) -> (u16, u32, u8) {
    (
        (word >> 48) as u16,
        ((word >> 8) & 0xFFFF_FFFF) as u32,
        (word & 0xFF) as u8,
    )
}

/// Atomics shared between the decode worker and the output callback.
///
/// Contains no locks and no ring endpoints. `rtrb::Producer`/`Consumer` are
/// `Send` but not `Sync` and need `&mut self`, so ring ownership is split
/// between the two contexts rather than shared through this struct.
#[derive(Debug)]
pub struct OutputLink {
    /// Worker-written: `(generation, epoch, phase)`.
    control: AtomicU64,
    /// Callback-written: `(generation, epoch, adopted)`.
    ack: AtomicU64,
    /// Worker-written target gain, as `f32` bits.
    gain: AtomicU32,
    xruns: AtomicU32,
    spans_dropped: AtomicU32,
    /// Callback-written last-unpublished span; read only after teardown.
    rescue_generation: AtomicU32,
    rescue_total: AtomicU64,
    rescue_t0: AtomicU64,
    rescue_frames: AtomicU32,
    rescue_valid: AtomicU8,
}

impl OutputLink {
    pub fn new() -> Self {
        Self {
            control: AtomicU64::new(pack(0, 0, Phase::Park.as_u8())),
            ack: AtomicU64::new(pack(0, 0, Adopted::Parked.as_u8())),
            gain: AtomicU32::new(1.0f32.to_bits()),
            xruns: AtomicU32::new(0),
            spans_dropped: AtomicU32::new(0),
            rescue_generation: AtomicU32::new(0),
            rescue_total: AtomicU64::new(0),
            rescue_t0: AtomicU64::new(0),
            rescue_frames: AtomicU32::new(0),
            rescue_valid: AtomicU8::new(0),
        }
    }

    pub fn publish_control(&self, control: Control) {
        self.control.store(
            pack(control.generation, control.epoch, control.phase.as_u8()),
            Ordering::Release,
        );
    }

    pub fn load_control(&self) -> Control {
        let (generation, epoch, tag) = unpack(self.control.load(Ordering::Acquire));
        Control {
            generation,
            epoch,
            phase: Phase::from_u8(tag),
        }
    }

    pub fn acknowledge(&self, generation: u16, epoch: u32, adopted: Adopted) {
        self.ack
            .store(pack(generation, epoch, adopted.as_u8()), Ordering::Release);
    }

    pub fn load_ack(&self) -> (u16, u32, Adopted) {
        let (generation, epoch, tag) = unpack(self.ack.load(Ordering::Acquire));
        (generation, epoch, Adopted::from_u8(tag))
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    pub fn note_xrun(&self) {
        self.xruns.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_dropped_span(&self) {
        self.spans_dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn take_diagnostics(&self) -> Diagnostics {
        Diagnostics {
            xruns: self.xruns.swap(0, Ordering::Relaxed),
            spans_dropped: self.spans_dropped.swap(0, Ordering::Relaxed),
        }
    }

    /// Mirror the callback's unpublished span so it survives teardown.
    pub fn stash_rescue(&self, record: SpanRecord) {
        self.rescue_generation
            .store(u32::from(record.generation), Ordering::Relaxed);
        self.rescue_total
            .store(record.media_total_after, Ordering::Relaxed);
        self.rescue_t0.store(record.t0.0, Ordering::Relaxed);
        self.rescue_frames.store(record.frames, Ordering::Relaxed);
        self.rescue_valid.store(1, Ordering::Release);
    }

    pub fn clear_rescue(&self) {
        self.rescue_valid.store(0, Ordering::Release);
    }

    /// Read the stashed span. Sound only once the callback is provably stopped:
    /// the ordering obligation is discharged by the backend thread join inside
    /// the stream's `Drop`, so this read is not concurrent.
    pub fn take_rescue_after_teardown(&self) -> Option<SpanRecord> {
        if self.rescue_valid.swap(0, Ordering::Acquire) == 0 {
            return None;
        }
        Some(SpanRecord {
            generation: self.rescue_generation.load(Ordering::Relaxed) as u16,
            media_total_after: self.rescue_total.load(Ordering::Relaxed),
            t0: Nanos(self.rescue_t0.load(Ordering::Relaxed)),
            frames: self.rescue_frames.load(Ordering::Relaxed),
        })
    }
}

impl Default for OutputLink {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn control_round_trips_through_one_atomic_word() {
        let link = OutputLink::new();
        let control = Control {
            generation: 7,
            epoch: 300_000,
            phase: Phase::Discard,
        };
        link.publish_control(control);
        assert_eq!(link.load_control(), control);
    }

    #[test]
    fn a_stale_epoch_acknowledgment_is_distinguishable() {
        // Repeated same-generation Park/Run transitions must not accept an old
        // acknowledgment, which is why every publication bumps the epoch. Vary
        // only the epoch here so its contribution to the packed word is
        // isolated from the Adopted tag.
        let link = OutputLink::new();
        link.acknowledge(3, 10, Adopted::Parked);
        assert_eq!(link.load_ack(), (3, 10, Adopted::Parked));
        link.acknowledge(3, 11, Adopted::Parked);
        assert_ne!(link.load_ack(), (3, 10, Adopted::Parked));
        assert_eq!(link.load_ack(), (3, 11, Adopted::Parked));

        // The Adopted variant is independently distinguishable too.
        link.acknowledge(3, 11, Adopted::Running);
        assert_ne!(link.load_ack(), (3, 11, Adopted::Parked));
        assert_eq!(link.load_ack(), (3, 11, Adopted::Running));
    }

    #[test]
    fn the_rescue_slot_survives_the_callback_being_dropped() {
        // Dropping a cpal stream destroys the callback closure and any span it
        // had not yet published. The rescue slot lives in the link instead.
        let link = Arc::new(OutputLink::new());
        let record = SpanRecord {
            generation: 2,
            media_total_after: 9_600,
            t0: Nanos(1_000),
            frames: 480,
        };
        let callback = {
            let link = Arc::clone(&link);
            move || link.stash_rescue(record)
        };
        callback();
        drop(callback);
        assert_eq!(link.take_rescue_after_teardown(), Some(record));
        assert_eq!(link.take_rescue_after_teardown(), None);
    }

    #[test]
    fn diagnostics_accumulate_and_drain_to_zero() {
        let link = OutputLink::new();
        link.note_xrun();
        link.note_xrun();
        link.note_dropped_span();
        assert_eq!(
            link.take_diagnostics(),
            Diagnostics {
                xruns: 2,
                spans_dropped: 1
            }
        );
        assert_eq!(
            link.take_diagnostics(),
            Diagnostics {
                xruns: 0,
                spans_dropped: 0
            }
        );
    }

    #[test]
    fn gain_round_trips_as_a_bit_pattern() {
        let link = OutputLink::new();
        assert_eq!(link.gain(), 1.0);
        link.set_gain(0.375);
        assert_eq!(link.gain(), 0.375);
    }
}
