use std::sync::Arc;

use rtrb::{Consumer, Producer};

use super::link::{Adopted, Control, OutputLink, Phase};
use super::output::{Nanos, SpanRecord};

/// The production output callback, shared by `CpalOutput` and `TestOutput`.
///
/// Allocation-free, lock-free, and I/O-free. Owns its ring endpoints outright
/// because `rtrb`'s `Producer`/`Consumer` are not `Sync`.
pub struct CallbackCore {
    link: Arc<OutputLink>,
    pcm: Consumer<f32>,
    spans: Producer<SpanRecord>,
    channels: u16,
    generation: u16,
    media_total: u64,
    /// Part of the documented constructor interface; unused by this task's own
    /// logic (`t0` arrives pre-computed), reserved for a future consumer such
    /// as position or resampling math.
    #[allow(dead_code)]
    sample_rate: u32,
    /// A span that could not be published because the ring was full. Republished
    /// on every later invocation, including while frozen or parked, and it gates
    /// the freeze and park acknowledgments.
    pending: Option<SpanRecord>,
    current_gain: f32,
}

impl CallbackCore {
    pub fn new(
        link: Arc<OutputLink>,
        pcm: Consumer<f32>,
        spans: Producer<SpanRecord>,
        channels: u16,
        sample_rate: u32,
    ) -> Self {
        let gain = link.gain();
        Self {
            link,
            pcm,
            spans,
            channels: channels.max(1),
            sample_rate: sample_rate.max(1),
            generation: 0,
            media_total: 0,
            pending: None,
            current_gain: gain,
        }
    }

    pub fn fill(&mut self, out: &mut [f32], playback: Nanos) {
        self.flush_pending();
        let control = self.link.load_control();
        if control.generation != self.generation {
            self.generation = control.generation;
            self.media_total = 0;
        }
        match control.phase {
            Phase::Run => self.run(out, playback, control),
            Phase::Freeze => {
                silence(out);
                self.acknowledge_when_drained(control, Adopted::Frozen);
            }
            Phase::Discard => {
                silence(out);
                self.discard_all();
                self.acknowledge_when_drained(control, Adopted::Parked);
            }
            Phase::Park => {
                silence(out);
                self.acknowledge_when_drained(control, Adopted::Parked);
            }
        }
    }

    fn run(&mut self, out: &mut [f32], playback: Nanos, control: Control) {
        let channels = usize::from(self.channels);
        let wanted = out.len();
        let mut written = 0;
        while written < wanted {
            match self.pcm.pop() {
                Ok(sample) => {
                    out[written] = sample;
                    written += 1;
                }
                Err(_) => break,
            }
        }
        if written < wanted {
            silence(&mut out[written..]);
            self.link.note_xrun();
        }
        // Media always occupies a prefix, so partial frames cannot straddle.
        let frames = (written / channels) as u32;
        self.apply_gain(out);
        if frames > 0 {
            self.media_total += u64::from(frames);
            let record = SpanRecord {
                generation: control.generation,
                media_total_after: self.media_total,
                t0: playback,
                frames,
            };
            self.publish(record);
        }
        self.link
            .acknowledge(control.generation, control.epoch, Adopted::Running);
    }

    fn publish(&mut self, record: SpanRecord) {
        if self.spans.push(record).is_err() {
            // Absolute cumulative totals make this a granularity loss only.
            if self.pending.is_some() {
                self.link.note_dropped_span();
            }
            self.pending = Some(record);
            self.link.stash_rescue(record);
        }
    }

    fn flush_pending(&mut self) {
        if let Some(record) = self.pending
            && self.spans.push(record).is_ok()
        {
            self.pending = None;
            self.link.clear_rescue();
        }
    }

    fn acknowledge_when_drained(&mut self, control: Control, adopted: Adopted) {
        if self.pending.is_none() {
            self.link
                .acknowledge(control.generation, control.epoch, adopted);
        }
    }

    fn discard_all(&mut self) {
        let available = self.pcm.slots();
        if available > 0
            && let Ok(chunk) = self.pcm.read_chunk(available)
        {
            // O(1): f32 is Copy with no Drop, so this is an index advance.
            chunk.commit_all();
        }
    }

    fn apply_gain(&mut self, out: &mut [f32]) {
        let channels = usize::from(self.channels);
        let target = self.link.gain();
        let frames = out.len() / channels;
        if frames == 0 {
            return;
        }
        let step = (target - self.current_gain) / frames as f32;
        let mut gain = self.current_gain;
        for frame in out.chunks_exact_mut(channels) {
            gain += step;
            for sample in frame.iter_mut() {
                *sample *= gain;
            }
        }
        self.current_gain = target;
    }
}

fn silence(out: &mut [f32]) {
    for sample in out.iter_mut() {
        *sample = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::output::test_output::TestOutput;
    use std::time::Duration;

    const RATE: u32 = 48_000;

    fn harness(
        pcm_frames: usize,
    ) -> (
        Arc<OutputLink>,
        rtrb::Producer<f32>,
        rtrb::Consumer<SpanRecord>,
        TestOutput,
    ) {
        let link = Arc::new(OutputLink::new());
        let (pcm_tx, pcm_rx) = rtrb::RingBuffer::<f32>::new(pcm_frames * 2);
        let (span_tx, span_rx) = rtrb::RingBuffer::<SpanRecord>::new(64);
        let core = CallbackCore::new(Arc::clone(&link), pcm_rx, span_tx, 2, RATE);
        let mut output = TestOutput::new(2, RATE, 480, Duration::from_millis(20));
        output.attach(core);
        (link, pcm_tx, span_rx, output)
    }

    fn push_frames(tx: &mut rtrb::Producer<f32>, frames: usize, value: f32) {
        for _ in 0..frames * 2 {
            tx.push(value).unwrap();
        }
    }

    #[test]
    fn running_publishes_a_span_whose_start_is_the_predicted_playback_instant() {
        let (link, mut pcm, mut spans, mut out) = harness(4_800);
        push_frames(&mut pcm, 480, 0.5);
        link.publish_control(Control {
            generation: 1,
            epoch: 1,
            phase: Phase::Run,
        });
        out.advance(Duration::from_millis(10));
        let record = spans.pop().unwrap();
        assert_eq!(record.generation, 1);
        assert_eq!(record.frames, 480);
        assert_eq!(record.media_total_after, 480);
        // 20 ms of virtual latency ahead of the callback instant.
        assert_eq!(record.t0, Nanos(20_000_000));
    }

    #[test]
    fn an_underrun_emits_silence_and_publishes_only_the_media_prefix() {
        let (link, mut pcm, mut spans, mut out) = harness(4_800);
        push_frames(&mut pcm, 200, 0.5); // less than one 480-frame buffer
        link.publish_control(Control {
            generation: 1,
            epoch: 1,
            phase: Phase::Run,
        });
        out.advance(Duration::from_millis(10));
        let record = spans.pop().unwrap();
        assert_eq!(record.frames, 200);
        assert_eq!(record.media_total_after, 200);
        let captured = out.captured();
        assert!(captured[..400].iter().all(|s| *s != 0.0));
        assert!(captured[400..960].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn park_emits_silence_consumes_nothing_and_acknowledges() {
        let (link, mut pcm, mut spans, mut out) = harness(4_800);
        push_frames(&mut pcm, 480, 0.5);
        link.publish_control(Control {
            generation: 1,
            epoch: 4,
            phase: Phase::Park,
        });
        out.advance(Duration::from_millis(10));
        assert_eq!(link.load_ack(), (1, 4, Adopted::Parked));
        assert!(spans.pop().is_err());
        assert!(out.captured().iter().all(|s| *s == 0.0));
    }

    #[test]
    fn freeze_acknowledges_only_after_the_pending_span_is_published() {
        // The callback may not acknowledge while a span is unpublished, so a
        // saturated span ring holds the acknowledgment back until the worker
        // drains. Task 5 relies on this being the only thing that gates it.
        let link = Arc::new(OutputLink::new());
        let (mut pcm_tx, pcm_rx) = rtrb::RingBuffer::<f32>::new(48_000);
        let (span_tx, mut span_rx) = rtrb::RingBuffer::<SpanRecord>::new(1);
        let core = CallbackCore::new(Arc::clone(&link), pcm_rx, span_tx, 2, RATE);
        let mut out = TestOutput::new(2, RATE, 480, Duration::from_millis(20));
        out.attach(core);
        for _ in 0..480 * 2 * 3 {
            pcm_tx.push(0.5).unwrap();
        }
        link.publish_control(Control {
            generation: 1,
            epoch: 1,
            phase: Phase::Run,
        });
        // Three callbacks: the first publishes, the second is retained as
        // `pending`, the third displaces that retained record - the first
        // actual loss. A retained span is not a lost one.
        out.advance(Duration::from_millis(30));
        assert!(link.take_diagnostics().spans_dropped >= 1);

        link.publish_control(Control {
            generation: 1,
            epoch: 2,
            phase: Phase::Freeze,
        });
        out.advance(Duration::from_millis(10));
        assert_ne!(link.load_ack(), (1, 2, Adopted::Frozen)); // still holding a pending span

        span_rx.pop().unwrap(); // the worker drains
        out.advance(Duration::from_millis(10));
        assert_eq!(link.load_ack(), (1, 2, Adopted::Frozen));
    }

    #[test]
    fn discard_drops_the_whole_ring_without_advancing_progress() {
        let (link, mut pcm, mut spans, mut out) = harness(4_800);
        push_frames(&mut pcm, 480, 0.5);
        link.publish_control(Control {
            generation: 1,
            epoch: 1,
            phase: Phase::Run,
        });
        out.advance(Duration::from_millis(10));
        let before = spans.pop().unwrap().media_total_after;

        push_frames(&mut pcm, 2_000, 0.5);
        link.publish_control(Control {
            generation: 1,
            epoch: 2,
            phase: Phase::Discard,
        });
        out.advance(Duration::from_millis(10));
        assert_eq!(link.load_ack(), (1, 2, Adopted::Parked));
        assert!(spans.pop().is_err()); // discarded frames publish nothing
        assert_eq!(before, 480);
    }

    #[test]
    fn adopting_a_new_generation_zeroes_the_media_counter() {
        let (link, mut pcm, mut spans, mut out) = harness(4_800);
        push_frames(&mut pcm, 480, 0.5);
        link.publish_control(Control {
            generation: 1,
            epoch: 1,
            phase: Phase::Run,
        });
        out.advance(Duration::from_millis(10));
        assert_eq!(spans.pop().unwrap().media_total_after, 480);

        push_frames(&mut pcm, 480, 0.5);
        link.publish_control(Control {
            generation: 2,
            epoch: 2,
            phase: Phase::Run,
        });
        out.advance(Duration::from_millis(10));
        let record = spans.pop().unwrap();
        assert_eq!(record.generation, 2);
        assert_eq!(record.media_total_after, 480);
    }

    #[test]
    fn gain_ramps_once_per_frame_across_both_channels() {
        let (link, mut pcm, _spans, mut out) = harness(4_800);
        push_frames(&mut pcm, 480, 1.0);
        link.set_gain(0.0);
        link.publish_control(Control {
            generation: 1,
            epoch: 1,
            phase: Phase::Run,
        });
        out.advance(Duration::from_millis(10));
        let captured = out.captured();
        for frame in captured.as_chunks::<2>().0 {
            assert_eq!(
                frame[0], frame[1],
                "one gain value per frame, across channels"
            );
        }
        // current_gain is captured from the link at construction time (1.0,
        // the OutputLink default) and set_gain(0.0) only sets the target
        // afterward, so the ramp descends from ~1.0 toward 0.0 across the
        // buffer: loud first, silent last.
        assert!(
            captured[0].abs() > captured[958].abs(),
            "gain ramps rather than stepping"
        );
    }
}
