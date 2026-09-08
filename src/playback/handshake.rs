use std::sync::Arc;
use std::time::{Duration, Instant};

use rtrb::Consumer;

use super::link::{Adopted, Control, OutputLink, Phase};
use super::output::{Nanos, SpanRecord};
use super::timeline::Timeline;

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("the output callback did not acknowledge within the deadline")]
    Timeout,
}

/// Worker side of the transition protocol.
///
/// Every wait carries a deadline, and a timeout means *attempt teardown and
/// recovery* — it does not bound end-to-end recovery, because the ALSA,
/// PipeWire and PulseAudio streams join their backend threads during `Drop`
/// with no timeout of their own.
pub struct Handshake {
    link: Arc<OutputLink>,
    spans: Consumer<SpanRecord>,
    epoch: u32,
    generation: u16,
}

impl Handshake {
    pub fn new(link: Arc<OutputLink>, spans: Consumer<SpanRecord>) -> Self {
        Self {
            link,
            spans,
            epoch: 0,
            generation: 0,
        }
    }

    pub fn next_epoch(&mut self) -> u32 {
        self.epoch = self.epoch.wrapping_add(1);
        self.epoch
    }

    pub fn generation(&self) -> u16 {
        self.generation
    }

    /// Publish `Run` for an already-installed generation (used by tests and by
    /// resuming from `Park` without a flush).
    ///
    /// Advancing the generation and resetting the timeline that tracks it are
    /// the same action: a caller that bumped `self.generation` without also
    /// voiding the old timeline state would have every subsequent span
    /// silently dropped by `Timeline::accept`'s generation filter, and every
    /// capture would read back as zero.
    pub fn start_running(&mut self, generation: u16, timeline: &mut Timeline) {
        self.generation = generation;
        timeline.reset(generation);
        let epoch = self.next_epoch();
        self.link.publish_control(Control {
            generation,
            epoch,
            phase: Phase::Run,
        });
    }

    /// Resume the **current** generation: publish `Run` with a fresh epoch and
    /// leave the timeline alone.
    ///
    /// This is the counterpart to `park`, and the reason it must not touch the
    /// timeline is the same reason `start_running` must: the callback resets
    /// its cumulative media counter only when the *generation* changes. Parking
    /// and releasing keep that counter running, so voiding the timeline that
    /// tracks it would drop the floor to zero while the next span arrived
    /// carrying the full running total, and the position would jump forward by
    /// everything played since the generation was installed.
    pub fn release(&mut self) {
        let epoch = self.next_epoch();
        self.link.publish_control(Control {
            generation: self.generation,
            epoch,
            phase: Phase::Run,
        });
    }

    pub fn park(
        &mut self,
        pump: &mut dyn FnMut(),
        deadline: Duration,
    ) -> Result<(), HandshakeError> {
        let epoch = self.next_epoch();
        self.link.publish_control(Control {
            generation: self.generation,
            epoch,
            phase: Phase::Park,
        });
        self.await_ack(epoch, Adopted::Parked, None, pump, deadline)
    }

    /// Step 1 and 2: freeze submission, then capture the final played position.
    ///
    /// Drains spans **while** waiting: the callback cannot acknowledge until its
    /// pending span is published, and cannot publish into a full ring, so a
    /// non-draining wait would deadlock on a healthy device.
    pub fn freeze_and_capture(
        &mut self,
        timeline: &mut Timeline,
        clock: &mut dyn FnMut() -> Nanos,
        pump: &mut dyn FnMut(),
        deadline: Duration,
    ) -> Result<u64, HandshakeError> {
        let epoch = self.next_epoch();
        self.link.publish_control(Control {
            generation: self.generation,
            epoch,
            phase: Phase::Freeze,
        });
        // The wait must ACCEPT the spans it drains, not discard them: draining
        // is what frees the slot the callback needs, and those same records are
        // the ones the capture is computed from.
        self.await_ack(epoch, Adopted::Frozen, Some(timeline), pump, deadline)?;
        // A final drain after the acknowledgment, so no published span is missed.
        self.drain_spans(timeline);
        Ok(timeline.played_frames(clock()))
    }

    /// Step 3: the callback discards the ring in one O(1) commit and parks.
    pub fn discard(
        &mut self,
        pump: &mut dyn FnMut(),
        deadline: Duration,
    ) -> Result<(), HandshakeError> {
        let epoch = self.next_epoch();
        self.link.publish_control(Control {
            generation: self.generation,
            epoch,
            phase: Phase::Discard,
        });
        self.await_ack(epoch, Adopted::Parked, None, pump, deadline)
    }

    /// Steps 4 and 5: adopt the new generation parked, and release to `Run` only
    /// when the desired state is playing.
    ///
    /// Like `start_running`, this advances the generation and resets the
    /// timeline together so the two can never drift apart.
    pub fn install(
        &mut self,
        generation: u16,
        playing: bool,
        timeline: &mut Timeline,
        pump: &mut dyn FnMut(),
        deadline: Duration,
    ) -> Result<(), HandshakeError> {
        self.generation = generation;
        timeline.reset(generation);
        let epoch = self.next_epoch();
        self.link.publish_control(Control {
            generation,
            epoch,
            phase: Phase::Park,
        });
        self.await_ack(epoch, Adopted::Parked, None, pump, deadline)?;
        if playing {
            let epoch = self.next_epoch();
            self.link.publish_control(Control {
                generation,
                epoch,
                phase: Phase::Run,
            });
        }
        Ok(())
    }

    pub fn drain_spans(&mut self, timeline: &mut Timeline) {
        let diagnostics = self.link.take_diagnostics();
        timeline.note_dropped(diagnostics.spans_dropped);
        while let Ok(record) = self.spans.pop() {
            timeline.accept(record);
        }
    }

    /// `timeline` is `Some` only while freezing, where the drained records are
    /// the ones the capture is computed from. Every other transition is about to
    /// void this generation, so its records are discarded.
    fn await_ack(
        &mut self,
        epoch: u32,
        expected: Adopted,
        mut timeline: Option<&mut Timeline>,
        pump: &mut dyn FnMut(),
        deadline: Duration,
    ) -> Result<(), HandshakeError> {
        let started = Instant::now();
        loop {
            // Draining is what frees the slot the callback needs to publish its
            // pending span, which is what unblocks the acknowledgment.
            while let Ok(record) = self.spans.pop() {
                if let Some(timeline) = timeline.as_deref_mut() {
                    timeline.accept(record);
                }
            }
            let (generation, acked_epoch, adopted) = self.link.load_ack();
            if generation == self.generation && acked_epoch == epoch && adopted == expected {
                return Ok(());
            }
            if started.elapsed() >= deadline {
                return Err(HandshakeError::Timeout);
            }
            pump();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::callback::CallbackCore;
    use crate::playback::output::test_output::TestOutput;
    use std::cell::RefCell;
    use std::rc::Rc;

    const RATE: u32 = 48_000;
    const DEADLINE: Duration = Duration::from_millis(250);

    struct Rig {
        link: Arc<OutputLink>,
        pcm: rtrb::Producer<f32>,
        output: Rc<RefCell<TestOutput>>,
        handshake: Handshake,
        timeline: Timeline,
    }

    fn rig(span_capacity: usize) -> Rig {
        let link = Arc::new(OutputLink::new());
        let (pcm_tx, pcm_rx) = rtrb::RingBuffer::<f32>::new(48_000);
        let (span_tx, span_rx) = rtrb::RingBuffer::<SpanRecord>::new(span_capacity);
        let core = CallbackCore::new(Arc::clone(&link), pcm_rx, span_tx, 2, RATE);
        let mut output = TestOutput::new(2, RATE, 480, Duration::from_millis(20));
        output.attach(core);
        Rig {
            link: Arc::clone(&link),
            pcm: pcm_tx,
            output: Rc::new(RefCell::new(output)),
            handshake: Handshake::new(link, span_rx),
            timeline: Timeline::new(RATE),
        }
    }

    fn push(rig: &mut Rig, frames: usize) {
        for _ in 0..frames * 2 {
            rig.pcm.push(0.5).unwrap();
        }
    }

    #[test]
    fn freeze_captures_the_final_position_before_anything_resets() {
        let mut rig = rig(64);
        push(&mut rig, 960);
        rig.handshake.start_running(1, &mut rig.timeline);
        let out = Rc::clone(&rig.output);
        out.borrow_mut().advance(Duration::from_millis(20));

        let out_clock = Rc::clone(&rig.output);
        let out_pump = Rc::clone(&rig.output);
        let played = rig
            .handshake
            .freeze_and_capture(
                &mut rig.timeline,
                &mut || out_clock.borrow().now(),
                &mut || out_pump.borrow_mut().advance(Duration::from_millis(10)),
                DEADLINE,
            )
            .unwrap();
        assert!(
            played > 0,
            "frames submitted before the freeze must be captured"
        );
        assert_eq!(rig.link.load_ack().2, Adopted::Frozen);
    }

    #[test]
    fn freeze_completes_against_a_saturated_span_ring() {
        // Regression: the callback cannot acknowledge while a span is pending,
        // and cannot publish into a full ring. A worker that waits without
        // draining deadlocks and times out on a perfectly healthy device.
        let mut rig = rig(1);
        push(&mut rig, 4_800);
        rig.handshake.start_running(1, &mut rig.timeline);
        let out = Rc::clone(&rig.output);
        out.borrow_mut().advance(Duration::from_millis(50)); // saturates the 1-slot ring

        let out_clock = Rc::clone(&rig.output);
        let out_pump = Rc::clone(&rig.output);
        let result = rig.handshake.freeze_and_capture(
            &mut rig.timeline,
            &mut || out_clock.borrow().now(),
            &mut || out_pump.borrow_mut().advance(Duration::from_millis(10)),
            DEADLINE,
        );
        assert!(
            result.is_ok(),
            "worker must drain spans while awaiting the ack"
        );
    }

    #[test]
    fn discard_waits_for_the_parked_acknowledgment_not_an_empty_ring() {
        let mut rig = rig(64);
        push(&mut rig, 4_800);
        rig.handshake.start_running(1, &mut rig.timeline);
        let out = Rc::clone(&rig.output);
        out.borrow_mut().advance(Duration::from_millis(10));

        let out_pump = Rc::clone(&rig.output);
        rig.handshake
            .discard(
                &mut || out_pump.borrow_mut().advance(Duration::from_millis(10)),
                DEADLINE,
            )
            .unwrap();
        assert_eq!(rig.link.load_ack().2, Adopted::Parked);
        assert_eq!(rig.link.load_control().phase, Phase::Discard);
    }

    #[test]
    fn a_paused_install_never_passes_through_run() {
        let mut rig = rig(64);
        let out_pump = Rc::clone(&rig.output);
        let seen = Rc::new(RefCell::new(Vec::new()));
        let seen_probe = Rc::clone(&seen);
        let link_probe = Arc::clone(&rig.link);
        rig.handshake
            .install(
                2,
                false, // desired state is paused
                &mut rig.timeline,
                &mut || {
                    seen_probe
                        .borrow_mut()
                        .push(link_probe.load_control().phase);
                    out_pump.borrow_mut().advance(Duration::from_millis(10));
                },
                DEADLINE,
            )
            .unwrap();
        assert!(
            !seen.borrow().contains(&Phase::Run),
            "no audio may escape while paused"
        );
        assert_eq!(rig.link.load_control().phase, Phase::Park);
    }

    #[test]
    fn a_playing_install_ends_in_run() {
        let mut rig = rig(64);
        let out_pump = Rc::clone(&rig.output);
        rig.handshake
            .install(
                2,
                true,
                &mut rig.timeline,
                &mut || out_pump.borrow_mut().advance(Duration::from_millis(10)),
                DEADLINE,
            )
            .unwrap();
        assert_eq!(rig.link.load_control().phase, Phase::Run);
    }

    #[test]
    fn park_then_release_keeps_the_generation_and_what_it_has_played() {
        // Pause and resume are the same generation, so the callback's counter
        // keeps running and the timeline must keep its floor. A release that
        // reset the timeline would report the next span's absolute total as if
        // it had all been played since the resume.
        let mut rig = rig(64);
        push(&mut rig, 4_800);
        rig.handshake.start_running(1, &mut rig.timeline);
        let out = Rc::clone(&rig.output);
        out.borrow_mut().advance(Duration::from_millis(50));
        rig.handshake.drain_spans(&mut rig.timeline);
        let played = rig.timeline.played_frames(out.borrow().now());
        assert!(played > 0);

        let out_pump = Rc::clone(&rig.output);
        rig.handshake
            .park(
                &mut || out_pump.borrow_mut().advance(Duration::from_millis(10)),
                DEADLINE,
            )
            .unwrap();
        rig.handshake.release();
        assert_eq!(rig.link.load_control().phase, Phase::Run);
        assert_eq!(rig.handshake.generation(), 1);
        assert!(
            rig.timeline.played_frames(out.borrow().now()) >= played,
            "releasing must not void what the generation already played"
        );
    }

    #[test]
    fn a_stale_epoch_acknowledgment_does_not_satisfy_a_wait() {
        let mut rig = rig(64);
        rig.handshake.start_running(1, &mut rig.timeline);
        let stale = rig.link.load_control();
        rig.link
            .acknowledge(stale.generation, stale.epoch, Adopted::Parked);
        // A fresh Park publication bumps the epoch, so the pre-existing ack must
        // not satisfy it. With no pump the wait can only time out.
        let result = rig.handshake.discard(&mut || {}, Duration::from_millis(20));
        assert!(matches!(result, Err(HandshakeError::Timeout)));
    }

    #[test]
    fn a_silent_output_times_out_rather_than_hanging() {
        let mut rig = rig(64);
        rig.handshake.start_running(1, &mut rig.timeline);
        let out_clock = Rc::clone(&rig.output);
        let result = rig.handshake.freeze_and_capture(
            &mut rig.timeline,
            &mut || out_clock.borrow().now(),
            &mut || {}, // the device never runs
            Duration::from_millis(20),
        );
        assert!(matches!(result, Err(HandshakeError::Timeout)));
    }
}
