use std::collections::VecDeque;

use super::output::{Nanos, SpanRecord};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositionQuality {
    Exact,
    Estimated,
    Degraded,
}

/// Reconstructs played media frames from the spans the output callback publishes.
///
/// Deliberately does **not** subtract an output latency from a submitted-frame
/// counter. That formulation fails across silence: one second of media followed
/// by a long underrun leaves the counter at one second while a persistent
/// latency drags the reported position backward indefinitely. Injected silence
/// is simply absent from this timeline.
#[derive(Debug)]
pub struct Timeline {
    sample_rate: u32,
    generation: u16,
    /// Media frames known to have finished playing.
    floor: u64,
    /// End instant of the most recently accepted span, for overlap validation.
    last_end: Nanos,
    last_total: u64,
    pending: VecDeque<SpanRecord>,
    dropped: u32,
    degraded: bool,
}

impl Timeline {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            generation: 0,
            floor: 0,
            last_end: Nanos(0),
            last_total: 0,
            pending: VecDeque::new(),
            dropped: 0,
            degraded: false,
        }
    }

    /// Adopt a new transport generation. Everything from the old one is void.
    pub fn reset(&mut self, generation: u16) {
        self.generation = generation;
        self.floor = 0;
        self.last_end = Nanos(0);
        self.last_total = 0;
        self.pending.clear();
        self.dropped = 0;
        self.degraded = false;
    }

    pub fn note_dropped(&mut self, count: u32) {
        if count > 0 {
            self.dropped = self.dropped.saturating_add(count);
            self.degraded = true;
        }
    }

    pub fn accept(&mut self, record: SpanRecord) {
        if record.generation != self.generation {
            return;
        }
        let contiguous = record.media_total_after >= self.last_total
            && record.t0 >= self.last_end
            && record.media_total_before() >= self.last_total;
        if !contiguous && self.last_total > 0 {
            self.degraded = true;
        }
        self.last_end = record.end(self.sample_rate);
        self.last_total = record.media_total_after;
        self.pending.push_back(record);
    }

    pub fn played_frames(&mut self, now: Nanos) -> u64 {
        while let Some(front) = self.pending.front() {
            let front_end = front.end(self.sample_rate);
            let media_total_after = front.media_total_after;
            // A subsequent span having started is itself proof the front span's
            // playback window has closed, even if its own (possibly overlapping,
            // unreliable) predicted end says otherwise: a callback only fires
            // because the previous buffer needed refilling.
            let superseded = self.pending.get(1).is_some_and(|next| now >= next.t0);
            if now >= front_end || superseded {
                self.floor = media_total_after;
                self.pending.pop_front();
            } else {
                break;
            }
        }
        let Some(front) = self.pending.front() else {
            return self.floor;
        };
        // `now < t0` is the normal case for the newest span, not an anomaly:
        // cpal's `playback` is a prediction ahead of the callback instant.
        if now < front.t0 || self.degraded {
            return self.floor;
        }
        let Some(elapsed) = now.checked_sub(front.t0) else {
            return self.floor;
        };
        let elapsed_frames = elapsed.as_nanos() * u128::from(self.sample_rate) / 1_000_000_000;
        let elapsed_frames = u64::try_from(elapsed_frames).unwrap_or(u64::from(front.frames));
        front.media_total_before() + elapsed_frames.min(u64::from(front.frames))
    }

    pub fn quality(&self) -> PositionQuality {
        if self.degraded {
            PositionQuality::Degraded
        } else {
            PositionQuality::Estimated
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    fn ms(n: u64) -> Nanos {
        Nanos(n * 1_000_000)
    }
    // 480 frames == 10 ms at 48 kHz.
    fn span(r#gen: u16, total: u64, t0_ms: u64, frames: u32) -> SpanRecord {
        SpanRecord {
            generation: r#gen,
            media_total_after: total,
            t0: ms(t0_ms),
            frames,
        }
    }

    #[test]
    fn a_span_that_has_not_begun_contributes_nothing() {
        // The normal case: cpal's `playback` is a prediction, and `now()` on the
        // PulseAudio backend is bare elapsed time, so now < t0 routinely.
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 480, 100, 480));
        assert_eq!(t.played_frames(ms(50)), 0);
    }

    #[test]
    fn a_fully_elapsed_span_is_wholly_played() {
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 480, 100, 480));
        assert_eq!(t.played_frames(ms(200)), 480);
    }

    #[test]
    fn an_in_flight_span_interpolates_and_clamps() {
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 480, 100, 480));
        assert_eq!(t.played_frames(ms(105)), 240);
        assert_eq!(t.played_frames(ms(109)), 432);
        assert_eq!(t.played_frames(ms(110)), 480);
    }

    #[test]
    fn earlier_queued_spans_are_retained_not_overwritten() {
        // Two 10 ms spans at [100,110) and [110,120). At now = 105 the newest
        // span has not started while the first is halfway played. A single
        // latest-span slot reports 0 here; a history reports 240.
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 480, 100, 480));
        t.accept(span(0, 960, 110, 480));
        assert_eq!(t.played_frames(ms(105)), 240);
        assert_eq!(t.played_frames(ms(115)), 720);
    }

    #[test]
    fn underrun_silence_never_advances_or_rewinds_position() {
        // One second of media, then a long gap of injected silence. Position
        // must stay at one second - not drift, and not be dragged backward by
        // any latency subtraction.
        let mut t = Timeline::new(RATE);
        t.accept(SpanRecord {
            generation: 0,
            media_total_after: 48_000,
            t0: ms(0),
            frames: 48_000,
        });
        assert_eq!(t.played_frames(ms(1_000)), 48_000);
        assert_eq!(t.played_frames(ms(5_000)), 48_000);
        assert_eq!(t.played_frames(ms(60_000)), 48_000);
    }

    #[test]
    fn a_dropped_span_costs_granularity_not_frame_counts() {
        // Records carry absolute cumulative totals, so a lost record cannot
        // corrupt the count. Frames in the gap are credited only once the
        // surviving span's start instant has passed - under-reporting briefly,
        // never over-reporting.
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 480, 100, 480));
        t.note_dropped(1); // the [110,120) record was lost
        t.accept(span(0, 1440, 120, 480)); // totals still absolute and correct
        assert_eq!(t.played_frames(ms(115)), 480);
        assert_eq!(t.played_frames(ms(130)), 1440);
        assert_eq!(t.quality(), PositionQuality::Degraded);
    }

    #[test]
    fn overlapping_spans_disable_interpolation_and_degrade() {
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 480, 100, 480));
        t.accept(span(0, 960, 105, 480)); // starts before its predecessor ends
        assert_eq!(t.quality(), PositionQuality::Degraded);
        assert_eq!(t.played_frames(ms(107)), 480); // floor only, no interpolation
    }

    #[test]
    fn spans_from_a_retired_generation_are_ignored() {
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 48_000, 0, 48_000));
        t.reset(1);
        assert_eq!(t.played_frames(ms(10_000)), 0);
        t.accept(span(0, 96_000, 0, 48_000)); // stale generation
        assert_eq!(t.played_frames(ms(10_000)), 0);
        t.accept(span(1, 480, 0, 480));
        assert_eq!(t.played_frames(ms(10_000)), 480);
    }

    #[test]
    fn end_of_media_waits_for_the_last_frames_predicted_play_time() {
        // EOF must not fire when the ring merely empties. The final span's last
        // frame is predicted at t0 + frames/rate, offset within its own buffer.
        let mut t = Timeline::new(RATE);
        t.accept(span(0, 960, 100, 480));
        assert!(t.played_frames(ms(105)) < 960);
        assert_eq!(t.played_frames(ms(110)), 960);
    }
}
