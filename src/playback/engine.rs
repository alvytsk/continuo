//! The decode worker: the state machine that owns every pipeline component.
//!
//! The invariant this module exists to enforce is that stopping or recreating
//! the audio pipeline never implicitly resets the logical playback position.
//! Position lives here, in `Worker::position`, and only four things establish a
//! new one: loading media, an explicit restart, a successful seek, and a resume
//! that adopts a target stored while stopped. Everything else — pausing,
//! stopping, a device disappearing, a handshake timing out — preserves it.

use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvError, Sender, TrySendError, select};

use crate::http::error::RemoteFailure;
use crate::media::id::{AbsolutePath, MediaId};
use crate::media::source::SourceLocation;
use crate::resume::{ResumeDecision, decide_resume};

use super::callback::CallbackCore;
use super::command::{PlaybackCommand, ResumeIntent};
use super::decode::DecodedSource;
use super::error::PlaybackError;
use super::event::{PlaybackEvent, Progress, ShutdownReport, StartDisposition};
use super::handshake::Handshake;
use super::link::OutputLink;
use super::output::cpal_output::{CpalOutput, OutputFault};
use super::output::{AudioOutput, NegotiatedOutput, OutputRequest, SpanRecord};
use super::resample::Converter;
use super::state::PlaybackState;
use super::timeline::{PositionQuality, Timeline};
use super::volume::Volume;

const STOP: u8 = 1;
const SHUTDOWN: u8 = 2;
const TICK: Duration = Duration::from_millis(10);
const DEADLINE: Duration = Duration::from_millis(250);
/// Ordinary events may not occupy these; terminal outcomes may.
const RESERVED_EVENT_SLOTS: usize = 9;
const EVENT_CAPACITY: usize = 64;
const PENDING_CAP: usize = 128;

// Reserve budget. Terminal outcomes may occupy the reserved tail; ordinary
// events may not. The worst case is one loop iteration emitting, at most:
//
//   stop interrupt        1  StateChanged{Stopped}
//   a serviced fault      2  Failed + StateChanged, or DeviceRecovered + StateChanged
//   a dispatched command  4  Load is the widest: StateChanged{Loading}, Loaded,
//                            CapabilitiesChanged, StateChanged{Paused}
//   end of track          2  EndOfTrack + StateChanged{Ended}
//                        --
//                         9  <= RESERVED_EVENT_SLOTS
//
// Those four are not mutually exclusive in a single pass, so the union is the
// bound rather than the maximum of them. Command admission closes while a
// backlog exists, and `service_faults` defers a fault whose events would not
// fit, so neither source can outrun the drain.
//
// `RestartEstablished` and `SeekCancelled` both belong to commands narrower
// than `Load` (their own event plus a `StateChanged`, at most 2), so neither
// raises the bound. The resume-unavailable warning does not add a fifth to
// the dispatched-command row either: it rides on `Loaded.disposition` rather
// than an event of its own.
const COMMAND_CAPACITY: usize = 1024;
const SPAN_CAPACITY: usize = 64;
/// How much audio the PCM ring holds. Large enough that one loop iteration
/// cannot drain it, small enough that discarding it on a seek is cheap.
const RING_MILLIS: u64 = 300;
/// Idle wait inside a handshake. The device runs on its own thread, so the
/// worker only has to stop spinning while it waits for an acknowledgment.
const PUMP_NAP: Duration = Duration::from_micros(250);
/// Floor on the interval between aggregated diagnostic warnings.
const DIAGNOSTIC_INTERVAL: Duration = Duration::from_millis(500);
/// Refinement budget for an *explicit* seek. Preserving seeks pass `None`.
const SEEK_BUDGET: Duration = Duration::from_secs(5);
/// A refined seek lands on a whole *source* frame, while the position it is
/// asked to preserve is a whole *output* frame, so a preserving seek can come
/// back a fraction of a frame short of what it promised. Within this tolerance
/// the promise is kept, so that repeated stop/resume cycles cannot walk the
/// position backwards a frame at a time. Pause and resume no longer need this -
/// they never re-seek - but stop, resume-at-a-stored-target and device recovery
/// still do.
///
/// One source frame at 44.1 kHz is 22.68 us, and the residues this actually
/// absorbs measure 4.5 us and 13.6 us. 100 us is four of those frames: wide
/// enough for the quantum, and an order of magnitude below the packet-sized
/// miss a genuinely failed seek leaves, so a real miss surfaces rather than
/// being swallowed.
const RESUME_TOLERANCE: Duration = Duration::from_micros(100);

/// The event stream, handed out as one unit.
///
/// The liveness receiver never carries a message. It exists so that dropping
/// the stream is *observable*: a rendezvous send is ready only when the channel
/// is disconnected, so the worker learns from the channel's own error that
/// nobody is listening. Occupancy is never consulted for that question.
pub struct EventStream {
    events: Receiver<PlaybackEvent>,
    _liveness: Receiver<()>,
}

impl EventStream {
    fn disconnected() -> Self {
        let (_events_tx, events) = crossbeam_channel::bounded(1);
        let (_liveness_tx, liveness) = crossbeam_channel::bounded(0);
        Self {
            events,
            _liveness: liveness,
        }
    }
}

pub struct EngineHandle {
    commands: Sender<PlaybackCommand>,
    stream: EventStream,
    progress: Arc<Mutex<Progress>>,
    interrupt: Arc<AtomicU8>,
    wake: Sender<()>,
    worker: Option<JoinHandle<Vec<PlaybackEvent>>>,
}

impl EngineHandle {
    /// Spawn a worker over an output that reports no asynchronous faults.
    pub fn spawn(output: Box<dyn AudioOutput>) -> Self {
        Self::spawn_with(output, crossbeam_channel::never())
    }

    /// Spawn a worker over the default cpal device, wired for fault recovery.
    pub fn spawn_cpal() -> Self {
        let mut output = CpalOutput::default();
        let faults = output.faults();
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(64);
        output.set_wake(wake_tx.clone());
        Self::assemble(Box::new(output), faults, wake_tx, wake_rx)
    }

    /// Spawn a worker over any output, plus the fault stream it publishes to.
    pub fn spawn_with(output: Box<dyn AudioOutput>, faults: Receiver<OutputFault>) -> Self {
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(64);
        Self::assemble(output, faults, wake_tx, wake_rx)
    }

    fn assemble(
        output: Box<dyn AudioOutput>,
        faults: Receiver<OutputFault>,
        wake_tx: Sender<()>,
        wake_rx: Receiver<()>,
    ) -> Self {
        let (commands_tx, commands_rx) = crossbeam_channel::bounded(COMMAND_CAPACITY);
        let (events_tx, events_rx) = crossbeam_channel::bounded(EVENT_CAPACITY);
        let (liveness_tx, liveness_rx) = crossbeam_channel::bounded(0);
        let progress = Arc::new(Mutex::new(Progress {
            session_rev: 0,
            media: None,
            position: Duration::ZERO,
            quality: PositionQuality::Exact,
        }));
        let interrupt = Arc::new(AtomicU8::new(0));
        let worker = Worker::new(
            output,
            faults,
            Arc::clone(&progress),
            commands_rx,
            events_tx,
            liveness_tx,
            wake_rx,
            Arc::clone(&interrupt),
        );
        let join = std::thread::Builder::new()
            .name("continuo-decode".into())
            .spawn(move || worker.run())
            .ok();
        Self {
            commands: commands_tx,
            stream: EventStream {
                events: events_rx,
                _liveness: liveness_rx,
            },
            progress,
            interrupt,
            wake: wake_tx,
            worker: join,
        }
    }

    pub fn commands(&self) -> &Sender<PlaybackCommand> {
        &self.commands
    }

    pub fn events(&self) -> &Receiver<PlaybackEvent> {
        &self.stream.events
    }

    /// The wake channel: a device backend pings it so a fault interrupts a
    /// blocked wait rather than waiting out the tick.
    pub fn wake(&self) -> &Sender<()> {
        &self.wake
    }

    /// Release the event stream. The worker sees the disconnection and shuts
    /// down: an engine nobody listens to has no reason to hold a device open.
    pub fn release_events(&mut self) {
        self.stream = EventStream::disconnected();
        let _ = self.wake.try_send(());
    }

    pub fn progress(&self) -> Progress {
        match self.progress.lock() {
            Ok(progress) => progress.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Out-of-band stop: travels on the interrupt flag plus the wake channel,
    /// so a saturated event channel cannot delay it.
    pub fn interrupt_stop(&self) {
        self.interrupt.fetch_or(STOP, Ordering::Release);
        let _ = self.wake.try_send(());
    }

    pub fn interrupt_shutdown(&self) {
        self.interrupt.fetch_or(SHUTDOWN, Ordering::Release);
        let _ = self.wake.try_send(());
    }

    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Joins the worker, then drains what it left behind.
    ///
    /// The channel drain is safe only because the thread has already gone —
    /// nothing can send again — and that is the same happens-before the final
    /// `Progress` rests on. Channel first, backlog after: everything the worker
    /// flushed was emitted before anything it could not.
    pub fn join(mut self) -> ShutdownReport {
        let backlog = match self.worker.take() {
            Some(worker) => match worker.join() {
                Ok(events) => events,
                Err(panic) => {
                    tracing::warn!(
                        panic = %describe_panic(&panic),
                        "the decode worker panicked; its shutdown backlog is lost"
                    );
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        let mut events = Vec::new();
        while let Ok(event) = self.stream.events.try_recv() {
            events.push(event);
        }
        events.extend(backlog);
        ShutdownReport {
            progress: self.progress(),
            events,
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.interrupt_shutdown();
    }
}

/// The transport state a blocked source read may service, behind one lock
/// once Task 9 finishes wrapping it (step 2). For now, step 1, it is a plain
/// field: introducing the type and moving `timeline`/`anchor` onto it is done
/// on its own, separately from the `Arc<Mutex<...>>` wrap, so a regression is
/// bisectable to whichever change caused it.
///
/// `pcm`, `link` and `config` do NOT move onto this struct: `TransportCore`
/// only ever holds what a later wait hook needs (`handshake`, `timeline`,
/// `anchor`, `sample_rate`), and `pcm` in particular is `rtrb::Producer<f32>`,
/// which is `!Sync` and must stay on the worker regardless of this task's
/// later steps.
struct TransportCore {
    handshake: Handshake,
    timeline: Timeline,
    /// Media position the current generation's frame counting starts from.
    anchor: Duration,
    // Not read until step 4, when `WaitService`'s shared recompute needs it
    // to convert played frames back to a `Duration` without a second lookup
    // into `Worker::config`, which the hook cannot reach. Carried from step 1
    // regardless, alongside the other three fields the interface specifies,
    // rather than added piecemeal later.
    #[allow(dead_code)]
    sample_rate: u32,
}

struct Worker {
    output: Box<dyn AudioOutput>,
    faults: Receiver<OutputFault>,
    transport: Option<TransportCore>,
    pcm: Option<rtrb::Producer<f32>>,
    link: Option<Arc<OutputLink>>,
    config: Option<NegotiatedOutput>,
    source: Option<DecodedSource>,
    converter: Option<Converter>,
    /// Interleaved output samples the ring has not accepted yet.
    staging: Vec<f32>,
    state: PlaybackState,
    session_rev: u64,
    /// The logical resume point. Stop and recreation preserve it; load,
    /// restart and successful seeks establish a new one.
    position: Duration,
    requested_target: Option<Duration>,
    media: Option<MediaId>,
    volume: Volume,
    generation: u16,
    pushed_total: u64,
    source_eof: bool,
    converter_flushed: bool,
    decoder_drained: bool,
    degraded: bool,
    shutting_down: bool,
    receivers_gone: bool,
    pending_events: VecDeque<PlaybackEvent>,
    progress: Arc<Mutex<Progress>>,
    commands: Receiver<PlaybackCommand>,
    events: Sender<PlaybackEvent>,
    liveness: Sender<()>,
    wake: Receiver<()>,
    interrupt: Arc<AtomicU8>,
    xruns: u64,
    lost_spans: u64,
    dropped_events: u64,
    /// A collapsed fault held back because the event backlog had no room for
    /// the events acting on it would produce.
    deferred_fault: Option<OutputFault>,
    device_warnings: u64,
    reported_diagnostics: u64,
    last_diagnostic: Instant,
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        output: Box<dyn AudioOutput>,
        faults: Receiver<OutputFault>,
        progress: Arc<Mutex<Progress>>,
        commands: Receiver<PlaybackCommand>,
        events: Sender<PlaybackEvent>,
        liveness: Sender<()>,
        wake: Receiver<()>,
        interrupt: Arc<AtomicU8>,
    ) -> Self {
        Self {
            output,
            faults,
            transport: None,
            pcm: None,
            link: None,
            config: None,
            source: None,
            converter: None,
            staging: Vec::new(),
            state: PlaybackState::Idle,
            session_rev: 0,
            position: Duration::ZERO,
            requested_target: None,
            media: None,
            volume: Volume::FULL,
            generation: 0,
            pushed_total: 0,
            source_eof: false,
            converter_flushed: false,
            decoder_drained: false,
            degraded: false,
            shutting_down: false,
            receivers_gone: false,
            pending_events: VecDeque::new(),
            progress,
            commands,
            events,
            liveness,
            wake,
            interrupt,
            xruns: 0,
            lost_spans: 0,
            dropped_events: 0,
            deferred_fault: None,
            device_warnings: 0,
            reported_diagnostics: 0,
            last_diagnostic: Instant::now()
                .checked_sub(DIAGNOSTIC_INTERVAL)
                .unwrap_or_else(Instant::now),
        }
    }

    fn run(mut self) -> Vec<PlaybackEvent> {
        let commands = self.commands.clone();
        let wake = self.wake.clone();
        let liveness = self.liveness.clone();
        loop {
            // 1. Out-of-band interrupts. Shutdown dominates stop.
            let flags = self.interrupt.swap(0, Ordering::Acquire);
            if flags & SHUTDOWN != 0 {
                self.shutdown();
                return Vec::from(self.pending_events);
            }
            if flags & STOP != 0 {
                self.do_stop();
            }

            // 2. Spans -> timeline -> keep-latest progress snapshot.
            self.collect_diagnostics();
            if let Some(transport) = self.transport.as_mut() {
                transport.handshake.drain_spans(&mut transport.timeline);
            }
            self.publish_progress();

            // 3. Asynchronous device faults.
            self.service_faults();

            // 4. Flush the event backlog. Admission stays closed while it is
            //    non-empty, which bounds further command-driven generation.
            self.flush_events();

            // 5. Diagnostics coalesce; they never enter pending_events.
            self.emit_aggregated_warning_if_slot_free();

            // 6. Wait. Never a bare sleep - stop and shutdown must wake it.
            let admit = self.pending_events.is_empty();
            let mut received: Option<Result<PlaybackCommand, RecvError>> = None;
            let mut disconnected = false;
            if admit {
                select! {
                    recv(commands) -> command => received = Some(command),
                    recv(wake) -> _ => {}
                    send(liveness, ()) -> result => disconnected = result.is_err(),
                    default(TICK) => {}
                }
            } else {
                select! {
                    recv(wake) -> _ => {}
                    send(liveness, ()) -> result => disconnected = result.is_err(),
                    default(TICK) => {}
                }
            }
            self.receivers_gone |= disconnected;
            match received {
                Some(Ok(command)) => self.dispatch(command),
                // Nobody can drive this engine any more.
                Some(Err(_)) => {
                    self.shutdown();
                    return Vec::from(self.pending_events);
                }
                None => {}
            }
            if self.shutting_down {
                self.shutdown();
                return Vec::from(self.pending_events);
            }

            // 7. Decode -> convert -> ring, with interruptible backpressure.
            if self.state == PlaybackState::Playing {
                self.pump_audio();
            }
            self.check_end_of_track();

            // A disconnected event receiver is shutdown. Detected from the
            // channel's own errors - `SendError` when flushing events, and
            // `RecvError` above when the command sender is gone - never from
            // channel occupancy, which says nothing about connectedness.
            if self.receivers_gone {
                self.shutdown();
                return Vec::from(self.pending_events);
            }
        }
    }

    // ----------------------------------------------------------------- events

    fn emit(&mut self, event: PlaybackEvent) {
        if self.pending_events.len() >= PENDING_CAP {
            if !event.is_terminal() {
                self.dropped_events += 1;
                return;
            }
            // A terminal outcome displaces the oldest ordinary event rather
            // than being dropped: nothing that follows implies it.
            match self.pending_events.iter().position(|e| !e.is_terminal()) {
                Some(index) => {
                    self.pending_events.remove(index);
                    self.dropped_events += 1;
                }
                None => {
                    self.dropped_events += 1;
                    return;
                }
            }
        }
        self.pending_events.push_back(event);
    }

    fn free_event_slots(&self) -> usize {
        EVENT_CAPACITY.saturating_sub(self.events.len())
    }

    fn flush_events(&mut self) {
        while let Some(front) = self.pending_events.front() {
            let free = self.free_event_slots();
            if free == 0 || (!front.is_terminal() && free <= RESERVED_EVENT_SLOTS) {
                break;
            }
            let Some(event) = self.pending_events.pop_front() else {
                break;
            };
            match self.events.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    self.pending_events.push_front(event);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.receivers_gone = true;
                    return;
                }
            }
        }
    }

    /// Counters, not a queue: an aggregated warning goes out only when an
    /// ordinary slot is free, and it reports the running totals. A receiver
    /// that stays connected without draining therefore bounds diagnostics by
    /// the channel itself, not by a rate limit alone.
    fn emit_aggregated_warning_if_slot_free(&mut self) {
        let total = self.xruns + self.lost_spans + self.dropped_events + self.device_warnings;
        if total == self.reported_diagnostics
            || !self.pending_events.is_empty()
            || self.free_event_slots() <= RESERVED_EVENT_SLOTS
            || self.last_diagnostic.elapsed() < DIAGNOSTIC_INTERVAL
        {
            return;
        }
        let message = format!(
            "audio glitches: {} underruns, {} lost spans, {} dropped events, {} device warnings",
            self.xruns, self.lost_spans, self.dropped_events, self.device_warnings
        );
        self.reported_diagnostics = total;
        self.last_diagnostic = Instant::now();
        let event = PlaybackEvent::Warning {
            session_rev: self.session_rev,
            message,
        };
        if let Err(TrySendError::Disconnected(_)) = self.events.try_send(event) {
            self.receivers_gone = true;
        }
    }

    fn warn(&mut self, message: String) {
        let session_rev = self.session_rev;
        self.emit(PlaybackEvent::Warning {
            session_rev,
            message,
        });
    }

    fn reject_seek(&mut self, reason: String) {
        let session_rev = self.session_rev;
        self.emit(PlaybackEvent::SeekRejected {
            session_rev,
            reason,
        });
    }

    /// Announce playback only when there is a transport to play it.
    ///
    /// Recovery can abandon a rebuild without failing - a stop arriving while
    /// it re-seeks, say - and the caller must not report `Playing` over a
    /// pipeline that no longer exists.
    fn announce_playing(&mut self) {
        if self.transport.is_some() {
            self.set_state(PlaybackState::Playing);
        }
    }

    fn set_state(&mut self, state: PlaybackState) {
        if self.state == state {
            return;
        }
        self.state = state;
        let session_rev = self.session_rev;
        self.emit(PlaybackEvent::StateChanged { session_rev, state });
    }

    fn fail(&mut self, message: String) {
        self.fail_with(message, None);
    }

    /// `fail`'s general form. A remote failure carries a typed `cause`
    /// alongside `message`, so a policy can act on what went wrong rather
    /// than only read a string a human wrote; M1's local failures have none,
    /// which is why `fail` is still the entry point every local call site
    /// uses (Ruling 1).
    fn fail_with(&mut self, message: String, cause: Option<RemoteFailure>) {
        if self.state == PlaybackState::Failed {
            // A repeating fatal fault must not re-announce the same failure
            // every iteration.
            return;
        }
        self.teardown();
        let session_rev = self.session_rev;
        self.emit(PlaybackEvent::Failed {
            session_rev,
            message,
            cause,
        });
        self.set_state(PlaybackState::Failed);
    }

    // --------------------------------------------------------------- progress

    fn quality(&self) -> PositionQuality {
        if self.degraded {
            return PositionQuality::Degraded;
        }
        match self.state {
            PlaybackState::Playing | PlaybackState::Paused => self
                .transport
                .as_ref()
                .map(|t| t.timeline.quality())
                .unwrap_or(PositionQuality::Exact),
            _ => PositionQuality::Exact,
        }
    }

    fn publish_progress(&mut self) {
        // Paused counts as well: parking silences the callback, but the frames
        // it already handed to the device still play out, so the position goes
        // on rising for one output latency after the park and only then
        // settles. Freezing the number at the instant of the park would report
        // a position slightly behind what the listener actually heard.
        if matches!(self.state, PlaybackState::Playing | PlaybackState::Paused)
            && let Some(rate) = self.config.as_ref().map(|c| c.sample_rate)
        {
            let now = self.output.now();
            if let Some(transport) = self.transport.as_mut() {
                let played = transport.timeline.played_frames(now);
                self.position = transport.anchor + frames_to_duration(played, rate);
            }
        }
        let snapshot = Progress {
            session_rev: self.session_rev,
            media: self.media.clone(),
            position: self.position,
            quality: self.quality(),
        };
        // Keep-latest: nothing but the assignment happens under the lock.
        match self.progress.lock() {
            Ok(mut slot) => *slot = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        }
    }

    /// Read the link's counters before anything else can swap them away, and
    /// forward the span losses to the timeline that has to account for them.
    fn collect_diagnostics(&mut self) {
        let Some(link) = self.link.as_ref() else {
            return;
        };
        let diagnostics = link.take_diagnostics();
        if let Some(transport) = self.transport.as_mut() {
            transport.timeline.note_dropped(diagnostics.spans_dropped);
        }
        self.lost_spans += u64::from(diagnostics.spans_dropped);
        // Underruns after the decoder has run dry are the expected sound of a
        // track ending, not a glitch worth reporting.
        if !self.decoder_drained {
            self.xruns += u64::from(diagnostics.xruns);
        }
    }

    // ----------------------------------------------------------------- faults

    /// Room a fault must leave behind before it is acted on.
    ///
    /// Two for the fault's own pair (`Failed` + `StateChanged`, or
    /// `DeviceRecovered` + `StateChanged`), plus the terminal work that can
    /// still follow it: end of track is another two, and a load in the same
    /// pass is three (`StateChanged{Loading}`, `Loaded`, `StateChanged{Paused}`).
    /// Reserving only its own pair let a fault land at 126 pending, leaving no
    /// room for the EOF that followed - which then evicted a lifecycle event.
    const FAULT_EVENT_BUDGET: usize = 7;

    fn service_faults(&mut self) {
        let mut worst: Option<OutputFault> = self.deferred_fault.take();
        let mut seen = 0u64;
        // Collapse the queue to its most severe entry: acting on each one in
        // turn would let a burst of faults generate a burst of events.
        while let Ok(fault) = self.faults.try_recv() {
            seen += 1;
            if worst.is_none_or(|current| severity(fault) > severity(current)) {
                worst = Some(fault);
            }
        }
        let Some(fault) = worst else {
            return;
        };
        // Command admission already stops the loop creating events faster than
        // they drain. Faults arrive asynchronously and answer to no such gate,
        // so apply the same rule here: hold the fault until its events fit,
        // rather than acting now and dropping the report at the cap.
        if self.pending_events.len() + Self::FAULT_EVENT_BUDGET > PENDING_CAP {
            self.deferred_fault = Some(fault);
            self.device_warnings += seen;
            return;
        }
        self.device_warnings += seen;
        match fault {
            // Rerouted or refused realtime: playback continues, but the timing
            // base underneath the timeline may have jumped.
            OutputFault::Recoverable(_) => self.degraded = true,
            OutputFault::Rebuild(kind) => {
                let resume = self.state == PlaybackState::Playing;
                // Nothing to report here: `rebuild` has already emitted
                // whatever the outcome warrants.
                let _ = self.rebuild(&format!("{kind:?}"), resume);
            }
            OutputFault::Fatal(kind) => self.fail(format!("audio device failed: {kind:?}")),
        }
    }

    /// Capture, tear down, then rebuild at the preserved position.
    ///
    /// `resume` is the state the caller *intends*, not the state the worker
    /// happens to be in: a recovery reached from `resume` or `restart` runs
    /// before either has announced `Playing`, and reading it off `self.state`
    /// would bring the transport back parked while the caller announced that
    /// playback had started.
    ///
    /// Three outcomes, and callers must tell them apart:
    ///
    /// * `Ok(())` — recovered; the transport is live again.
    /// * `Err(Cancelled)` — a stop or shutdown arrived during the recovery
    ///   re-seek. Nothing failed and nothing was emitted: the interrupt is
    ///   still set, and the loop's next pass turns it into the transition the
    ///   user actually asked for. A caller that reported this as a failure
    ///   would turn a requested stop into a spurious `Failed`, which `do_stop`
    ///   would then refuse to correct.
    /// * any other `Err` — recovery failed, `Failed` has already been emitted
    ///   with the position preserved, and the caller must announce nothing.
    fn rebuild(&mut self, reason: &str, resume: bool) -> Result<(), PlaybackError> {
        if self.source.is_none() {
            self.teardown();
            return Err(PlaybackError::UnsupportedInput {
                path: Default::default(),
                reason: "no media is loaded".into(),
            });
        }
        self.capture_and_teardown();
        let target = self.position;
        match self.reseek(target) {
            Ok(actual) => self.position = adopt_preserved(target, actual),
            Err(error) => {
                if !is_cancelled(&error) {
                    self.fail(format!("cannot recover after {reason}: {error}"));
                }
                return Err(error);
            }
        }
        self.session_rev += 1;
        match self.open_transport(resume) {
            Ok(()) => {
                let session_rev = self.session_rev;
                self.emit(PlaybackEvent::DeviceRecovered { session_rev });
                Ok(())
            }
            Err(error) => {
                self.fail(format!("cannot reopen the audio device: {error}"));
                Err(error)
            }
        }
    }

    // -------------------------------------------------------------- transport

    fn open_transport(&mut self, playing: bool) -> Result<(), PlaybackError> {
        let (source_rate, source_channels) = match self.source.as_ref() {
            Some(source) => (source.sample_rate(), source.channels()),
            None => {
                return Err(PlaybackError::UnsupportedInput {
                    path: Default::default(),
                    reason: "no media is loaded".into(),
                });
            }
        };
        let config = self.output.negotiate(&OutputRequest {
            preferred_rate: source_rate,
            preferred_channels: source_channels,
        })?;
        let channels = config.channels.max(1);
        // M1 is scoped to mono and stereo, and `Converter` only knows how to
        // fan a source out to one or two device channels. A 6- or 8-channel
        // default is routine on HDMI and PipeWire, and the interleave fallback
        // would quietly emit two samples per frame into a buffer the engine
        // strides at six: 3x-fast playback and a 3x-inflated position, with no
        // error anywhere. Refuse the device instead. Downmixing is a later
        // milestone.
        if !matches!(channels, 1 | 2) {
            return Err(PlaybackError::UnsupportedInput {
                path: Default::default(),
                reason: format!(
                    "the audio device negotiated {channels} channels; \
                     only mono and stereo output is supported"
                ),
            });
        }
        let link = Arc::new(OutputLink::new());
        link.set_gain(self.volume.as_gain());

        let ring_frames = (u64::from(config.sample_rate) * RING_MILLIS / 1_000)
            .max(u64::from(config.buffer_frames) * 4) as usize;
        // A whole number of frames, so the ring can never come to hold a
        // partial frame: the callback pops sample by sample and would emit one
        // channel of a frame while its siblings stayed silent.
        let (pcm_tx, pcm_rx) = rtrb::RingBuffer::<f32>::new(ring_frames * usize::from(channels));
        let (span_tx, span_rx) = rtrb::RingBuffer::<SpanRecord>::new(SPAN_CAPACITY);
        let core = CallbackCore::new(
            Arc::clone(&link),
            pcm_rx,
            span_tx,
            channels,
            config.sample_rate,
        );
        self.output.open(&config, Arc::clone(&link), core)?;

        let mut timeline = Timeline::new(config.sample_rate);
        self.converter = Some(Converter::new(
            source_rate,
            config.sample_rate,
            source_channels,
            channels,
        )?);
        let mut handshake = Handshake::new(Arc::clone(&link), span_rx);
        self.reset_generation_state();
        let generation = self.generation;
        // The anchor this generation counts from: captured now, before the
        // new `TransportCore` exists to hold it, and handed straight to its
        // constructor below.
        let anchor = self.position;
        let installed = {
            let mut pump = || std::thread::sleep(PUMP_NAP);
            handshake.install(generation, false, &mut timeline, &mut pump, DEADLINE)
        };
        if installed.is_err() {
            self.output.close();
            return Err(PlaybackError::Timeout);
        }
        self.link = Some(link);
        self.pcm = Some(pcm_tx);
        let sample_rate = config.sample_rate;
        self.config = Some(config);
        self.transport = Some(TransportCore {
            handshake,
            timeline,
            anchor,
            sample_rate,
        });
        self.prime_and_run(playing);
        Ok(())
    }

    /// Re-adopt the existing transport under a fresh generation: discard the
    /// ring the old generation filled, install, prime, and release.
    fn reinstall(&mut self, playing: bool) -> Result<(), PlaybackError> {
        if self.transport.is_none() {
            return self.open_transport(playing);
        }
        if let Some(converter) = self.converter.as_mut() {
            converter.reset();
        }
        // The discard comes first, and the generation state is reset only once
        // it has succeeded. Both are recoveries into `rebuild`, but they meet
        // the timeline in opposite conditions: `install` resets the timeline as
        // its first action, so a timeout there leaves `capture_position` with a
        // voided timeline and nothing to misread. A timeout in `discard` leaves
        // the *previous* generation's spans standing, and if the anchor had
        // already been moved to the freshly seeked target, the capture would
        // read back `anchor_new + played_old` - a silent forward jump of
        // everything played since the last install.
        let discarded = {
            let Some(transport) = self.transport.as_mut() else {
                return Ok(());
            };
            let mut pump = || std::thread::sleep(PUMP_NAP);
            transport.handshake.discard(&mut pump, DEADLINE)
        };
        // The device stopped answering. Recreate it rather than run on against
        // a transport whose state can no longer be established - and pass the
        // outcome through unchanged, so the caller can tell a failed recovery
        // from a cancelled one and announce neither.
        if discarded.is_err() {
            return self.rebuild("handshake timeout", playing);
        }
        self.reset_generation_state();
        let generation = self.generation;
        let anchor = self.position;
        let installed = {
            let Some(transport) = self.transport.as_mut() else {
                return Ok(());
            };
            // Set here, through the field, rather than at construction:
            // unlike `open_transport`, this generation reuses an
            // already-existing `TransportCore` rather than building a new one.
            transport.anchor = anchor;
            let mut pump = || std::thread::sleep(PUMP_NAP);
            transport.handshake.install(
                generation,
                false,
                &mut transport.timeline,
                &mut pump,
                DEADLINE,
            )
        };
        if installed.is_err() {
            return self.rebuild("handshake timeout", playing);
        }
        self.prime_and_run(playing);
        Ok(())
    }

    /// Everything that a new generation invalidates, in one place.
    ///
    /// The anchor - `self.position` at the moment this generation starts
    /// counting from - is not among them: it lives on `TransportCore` now, so
    /// a caller building a brand-new one (`open_transport`) passes it straight
    /// to the constructor, and `reinstall`, which reuses an existing one,
    /// writes it back through the field itself, right after this returns.
    fn reset_generation_state(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pushed_total = 0;
        self.staging.clear();
        self.source_eof = false;
        self.converter_flushed = false;
        self.decoder_drained = false;
        self.degraded = false;
    }

    /// Fill the ring before releasing the callback, so that starting playback
    /// does not begin with a self-inflicted underrun.
    ///
    /// The ring is primed even when the transport stays parked: a seek taken
    /// while paused installs a new generation, and the `release` that resumes
    /// it would otherwise start against an empty ring.
    fn prime_and_run(&mut self, playing: bool) {
        self.pump_audio();
        if !playing {
            return;
        }
        if let Some(transport) = self.transport.as_mut() {
            let generation = transport.handshake.generation();
            transport
                .handshake
                .start_running(generation, &mut transport.timeline);
        }
    }

    /// Freeze the callback and read back the frames it really played.
    ///
    /// Returns `false` when the device did not answer, in which case the
    /// caller must fall back to the link's rescue record.
    fn capture_position(&mut self) -> bool {
        let Some(rate) = self.config.as_ref().map(|c| c.sample_rate) else {
            return true;
        };
        let captured = {
            let Self {
                transport, output, ..
            } = self;
            let Some(transport) = transport.as_mut() else {
                return true;
            };
            let anchor = transport.anchor;
            let mut clock = || output.now();
            let mut pump = || std::thread::sleep(PUMP_NAP);
            transport
                .handshake
                .freeze_and_capture(&mut transport.timeline, &mut clock, &mut pump, DEADLINE)
                .map(|frames| anchor + frames_to_duration(frames, rate))
        };
        match captured {
            Ok(position) => {
                self.position = position;
                true
            }
            Err(_) => false,
        }
    }

    fn capture_and_teardown(&mut self) {
        let rate = self
            .config
            .as_ref()
            .map(|c| c.sample_rate)
            .unwrap_or(48_000);
        let exact = self.capture_position();
        // Read before `teardown` clears the `TransportCore` this comes from.
        // Unused when `exact` is true (the common case) or when there was
        // never a transport to begin with - the rescue branch below is the
        // only place it matters, and it is reachable only when both a
        // transport existed and its capture timed out.
        let anchor = self
            .transport
            .as_ref()
            .map(|t| t.anchor)
            .unwrap_or(self.position);
        let generation = self.generation;
        let link = self.teardown();
        if exact {
            return;
        }
        // The capture timed out. The callback's last unpublished span survives
        // in the link, so read it once the teardown has joined the backend
        // thread; if it is unusable, keep the last validated position rather
        // than fabricating one.
        self.degraded = true;
        if let Some(link) = link
            && let Some(record) = link.take_rescue_after_teardown()
            && record.generation == generation
        {
            self.position = anchor + frames_to_duration(record.media_total_after, rate);
        }
    }

    fn teardown(&mut self) -> Option<Arc<OutputLink>> {
        // Close first: dropping the stream joins the backend thread, which is
        // what makes the link's rescue slot safe to read afterwards.
        self.output.close();
        retire_faults(&mut self.deferred_fault, &self.faults);
        let link = self.link.take();
        self.transport = None;
        self.pcm = None;
        self.config = None;
        self.converter = None;
        self.staging.clear();
        self.pushed_total = 0;
        self.source_eof = false;
        self.converter_flushed = false;
        self.decoder_drained = false;
        link
    }

    fn shutdown(&mut self) {
        let captured_exactly = self.capture_position();
        self.teardown();
        self.source = None;
        self.state = PlaybackState::Idle;
        if !captured_exactly {
            // quality() checks `degraded` before it looks at `state`, so this is
            // what keeps a capture that timed out from being published as
            // Exact on the strength of `state` having just become `Idle`.
            self.degraded = true;
        }
        // With the transport gone the recompute branch is skipped, so this
        // publishes precisely the captured position (D14). Without it, the last
        // Progress a reader can see is the one from the previous pass and the
        // capture is unreachable.
        self.publish_progress();
    }

    // ------------------------------------------------------------------ audio

    fn pump_audio(&mut self) {
        loop {
            let Some(config) = self.config.as_ref() else {
                return;
            };
            let channels = usize::from(config.channels.max(1));
            let Some(pcm) = self.pcm.as_mut() else {
                return;
            };
            // Whole frames only, in a ring sized as a multiple of the channel
            // count: a partial frame here would emit one channel of a frame
            // while its siblings stayed silent.
            let free = pcm.slots() / channels * channels;
            let ready = self.staging.len() / channels * channels;
            let count = free.min(ready);
            if count > 0 {
                // Publish the whole batch with ONE tail advance. Pushing sample
                // by sample makes each one visible immediately, so the callback
                // can pop a left channel whose right sibling has not been
                // written yet - a half frame, mid-push, even though `count` is a
                // whole number of frames.
                if pcm.push_entire_slice(&self.staging[..count]).is_ok() {
                    self.staging.drain(..count);
                    self.pushed_total += (count / channels) as u64;
                }
            }
            if !self.staging.is_empty() {
                // The ring is full; the loop's wait provides the backpressure.
                return;
            }
            if self.source_eof {
                if !self.converter_flushed {
                    self.converter_flushed = true;
                    if let Some(converter) = self.converter.as_mut() {
                        converter.finish(&mut self.staging);
                    }
                    continue;
                }
                self.decoder_drained = true;
                return;
            }
            let decoded = {
                let Self {
                    source,
                    converter,
                    staging,
                    ..
                } = self;
                let (Some(source), Some(converter)) = (source.as_mut(), converter.as_mut()) else {
                    self.source_eof = true;
                    continue;
                };
                match source.next_planar() {
                    Ok(Some(planes)) => {
                        converter.push(planes, staging);
                        Ok(true)
                    }
                    Ok(None) => Ok(false),
                    Err(error) => Err(error),
                }
            };
            match decoded {
                Ok(true) => {}
                Ok(false) => self.source_eof = true,
                Err(error) => {
                    self.source_eof = true;
                    self.warn(format!("decoding stopped early: {error}"));
                }
            }
        }
    }

    /// End of track fires only once every pushed frame has been played, which
    /// the timeline reports only after the final span's predicted play time
    /// has passed - not when the ring merely empties.
    fn check_end_of_track(&mut self) {
        if self.state != PlaybackState::Playing || !self.decoder_drained {
            return;
        }
        let Some(rate) = self.config.as_ref().map(|c| c.sample_rate) else {
            return;
        };
        let now = self.output.now();
        let landed_anchor = {
            let Some(transport) = self.transport.as_mut() else {
                return;
            };
            if transport.timeline.played_frames(now) < self.pushed_total {
                return;
            }
            let mut pump = || std::thread::sleep(PUMP_NAP);
            // The outcome is deliberately ignored: a timed-out park does not
            // prevent EndOfTrack from being reported - the decoder has
            // genuinely run dry either way.
            let _ = transport
                .handshake
                .park(&mut transport.timeline, &mut pump, DEADLINE);
            transport.anchor
        };
        self.position = landed_anchor + frames_to_duration(self.pushed_total, rate);
        let session_rev = self.session_rev;
        let position = self.position;
        self.emit(PlaybackEvent::EndOfTrack {
            session_rev,
            position,
        });
        self.set_state(PlaybackState::Ended);
    }

    // --------------------------------------------------------------- commands

    fn dispatch(&mut self, command: PlaybackCommand) {
        match command {
            PlaybackCommand::Load {
                media,
                source,
                resume,
            } => self.load(media, source, resume),
            PlaybackCommand::Play => self.play(),
            PlaybackCommand::Pause => self.pause(),
            PlaybackCommand::TogglePause => match self.state {
                PlaybackState::Playing => self.pause(),
                _ => self.play(),
            },
            PlaybackCommand::SeekTo(target) => self.seek_to(target),
            PlaybackCommand::SeekBy(delta) => {
                let base = self.position;
                let step = Duration::from_secs(delta.unsigned_abs());
                let target = if delta >= 0 {
                    base.saturating_add(step)
                } else {
                    base.saturating_sub(step)
                };
                self.seek_to(target);
            }
            PlaybackCommand::Restart => self.restart(),
            PlaybackCommand::SetVolume(volume) => {
                self.volume = volume;
                if let Some(link) = self.link.as_ref() {
                    link.set_gain(volume.as_gain());
                }
                let session_rev = self.session_rev;
                self.emit(PlaybackEvent::VolumeChanged {
                    session_rev,
                    volume,
                });
            }
            PlaybackCommand::Stop => self.do_stop(),
            PlaybackCommand::Shutdown => self.shutting_down = true,
        }
    }

    fn load(&mut self, media: MediaId, source: SourceLocation, resume: ResumeIntent) {
        self.capture_and_teardown();
        self.source = None;
        self.session_rev += 1;
        self.media = Some(media.clone());
        self.requested_target = None;
        // A caller-decided start is already the position that will be asked
        // for, so it is pinned BEFORE opening: if the load fails, Failed must
        // carry it so a retry can resume there. Zeroing here loses it for
        // every failure path below. A `Candidate` has no position to pin yet -
        // deciding one needs the duration only the decoder can report, so it
        // waits until the source has actually opened, below.
        //
        // `self.position` alone is enough: no transport exists at this point
        // (the teardown above just cleared it), and `open_transport` reads
        // `self.position` as the new generation's anchor when it builds one,
        // below.
        if let ResumeIntent::StartAt(target) = resume {
            self.position = target;
        }
        self.degraded = false;
        self.set_state(PlaybackState::Loading);

        let path = match &source {
            SourceLocation::LocalPath(path) => AbsolutePath::new(path.clone()),
            SourceLocation::Http(url) => {
                self.fail(format!("this build plays local files only, not {url}"));
                return;
            }
        };
        let path = match path {
            Ok(path) => path,
            Err(error) => {
                self.fail(format!("{error}"));
                return;
            }
        };
        let mut decoded = match DecodedSource::open(&path) {
            Ok(decoded) => decoded,
            Err(error) => {
                self.fail(format!("{error}"));
                return;
            }
        };

        // A `Candidate` is decided now, against the duration this decoder just
        // reported - the same rule `decide_resume` applies wherever a
        // duration is in hand (G3), so a worker-resolved resume and an
        // application-resolved one can never land somewhere the checkpoint
        // never said. Task 10 adds the `ResumeUnavailable` branch, for a
        // positive candidate a non-seekable source cannot establish.
        let (start_at, disposition) = match resume {
            ResumeIntent::StartAt(target) => (target, StartDisposition::Fresh),
            ResumeIntent::Candidate(candidate) => {
                let decision = decide_resume(Some(candidate), decoded.metadata().duration);
                let target = decision.start_at();
                self.position = target;
                let disposition = match decision {
                    ResumeDecision::Completed => StartDisposition::CompletedReplay,
                    ResumeDecision::Resume(_) | ResumeDecision::Unvalidated(_) => {
                        StartDisposition::Resumed
                    }
                    ResumeDecision::NoEntry
                    | ResumeDecision::AtStart
                    | ResumeDecision::DegenerateEnd
                    | ResumeDecision::StalePastEnd => StartDisposition::Fresh,
                };
                (target, disposition)
            }
        };

        if start_at > Duration::ZERO {
            // Cancellable for the same reason `reseek` is: the worker must not
            // be blind to a stop or a shutdown for the length of a refinement.
            let interrupt = Arc::clone(&self.interrupt);
            let seek = decoded.seek_refined(start_at, None, &mut || {
                interrupt.load(Ordering::Acquire) != 0
            });
            match seek {
                Ok(outcome) => self.position = adopt_preserved(start_at, outcome.actual),
                // Abandon the load, leaving no decoder open. The interrupt
                // still stands and the loop's next pass acts on it.
                Err(error) if is_cancelled(&error) => return,
                Err(error) => {
                    self.fail(format!("{error}"));
                    return;
                }
            }
        }
        let session_rev = self.session_rev;
        let position = self.position;
        self.emit(PlaybackEvent::Loaded {
            session_rev,
            media,
            metadata: decoded.metadata().clone(),
            capabilities: decoded.capabilities(),
            position,
            disposition,
        });
        self.source = Some(decoded);
        match self.open_transport(false) {
            Ok(()) => self.set_state(PlaybackState::Paused),
            Err(error) => self.fail(format!("cannot open the audio device: {error}")),
        }
    }

    fn play(&mut self) {
        match self.state {
            PlaybackState::Playing => {}
            // Play from Ended never restarts implicitly: the application has
            // to say `Restart`, so that "play" cannot silently lose the fact
            // that the track finished.
            PlaybackState::Ended => {
                self.warn("the track has ended; restart it to play it again".into())
            }
            PlaybackState::Failed => {
                self.warn("playback failed; load the media again before playing".into())
            }
            PlaybackState::Idle => self.warn("nothing is loaded".into()),
            // Resuming a parked transport is the same generation released
            // again: nothing was discarded, so nothing has to be rebuilt.
            PlaybackState::Paused
                if self.transport.is_some() && self.requested_target.is_none() =>
            {
                if let Some(transport) = self.transport.as_mut() {
                    transport.handshake.release();
                }
                self.set_state(PlaybackState::Playing);
            }
            PlaybackState::Loading | PlaybackState::Paused | PlaybackState::Stopped => {
                self.restore()
            }
        }
    }

    /// Restore playback from a torn-down transport, at the preserved position
    /// or at a target stored while stopped - the one case where resuming
    /// establishes a new position rather than preserving one.
    fn restore(&mut self) {
        if self.source.is_none() {
            self.warn("nothing is loaded".into());
            return;
        }
        if self.transport.is_some() {
            self.capture_position();
        }
        // A target stored while stopped was never validated against the decoder,
        // so resuming is where it gets confirmed - and where the SeekCompleted
        // the caller is still waiting for must finally be emitted.
        let stored = self.requested_target.take();
        let target = stored.unwrap_or(self.position);
        let landed;
        match self.reseek(target) {
            Ok(actual) => {
                landed = actual;
                self.position = adopt_preserved(target, actual);
            }
            // A stop or a shutdown arrived mid-refinement. The preserved
            // position still stands; the interrupt is handled by the loop.
            Err(PlaybackError::Cancelled) => return,
            Err(error) => {
                self.fail(format!("cannot resume at {target:?}: {error}"));
                return;
            }
        }
        match self.reinstall(true) {
            Ok(()) => {
                self.announce_playing();
                // Only now is a stored target both validated and installed,
                // which is what SeekCompleted asserts. Emitting at store time
                // would claim a landing no decoder had confirmed.
                if let Some(requested) = stored {
                    let actual = landed;
                    let session_rev = self.session_rev;
                    self.emit(PlaybackEvent::SeekCompleted {
                        session_rev,
                        requested,
                        actual,
                        refinement_truncated: false,
                    });
                }
            }
            Err(error) if is_cancelled(&error) => {}
            Err(error) => self.fail(format!("cannot start the audio device: {error}")),
        }
    }

    /// Park the callback and leave everything else exactly as it is. The ring
    /// keeps its audio, the callback keeps counting from where it stopped, and
    /// the timeline keeps its floor, so resuming is a single `release`.
    fn pause(&mut self) {
        if self.state != PlaybackState::Playing {
            return;
        }
        let parked = {
            let mut pump = || std::thread::sleep(PUMP_NAP);
            match self.transport.as_mut() {
                Some(transport) => {
                    transport
                        .handshake
                        .park(&mut transport.timeline, &mut pump, DEADLINE)
                }
                None => Ok(()),
            }
        };
        if parked.is_err() {
            // A park that never gets acknowledged means the device is not
            // running. Announcing Paused here would leave a dead transport
            // behind a state that claims it can resume, so recover instead and
            // land parked - or fail with the position preserved.
            match self.rebuild("the audio device stopped responding while pausing", false) {
                Ok(()) => self.set_state(PlaybackState::Paused),
                Err(error) if is_cancelled(&error) => {}
                Err(error) => self.fail(format!("cannot pause: {error}")),
            }
            return;
        }
        self.set_state(PlaybackState::Paused);
    }

    fn do_stop(&mut self) {
        if matches!(
            self.state,
            PlaybackState::Idle | PlaybackState::Stopped | PlaybackState::Failed
        ) {
            return;
        }
        self.capture_and_teardown();
        self.session_rev += 1;
        self.set_state(PlaybackState::Stopped);
    }

    fn seek_to(&mut self, requested: Duration) {
        if self.source.is_none() {
            self.reject_seek("nothing is loaded".into());
            return;
        }
        let target = self.clamp_target(requested);
        match self.state {
            PlaybackState::Failed => {
                self.reject_seek("playback failed; load the media again".into());
                return;
            }
            // Deliberately not `SeekCompleted`: with no transport running,
            // nothing has validated this target yet.
            PlaybackState::Idle | PlaybackState::Stopped => {
                self.requested_target = Some(target);
                let session_rev = self.session_rev;
                self.emit(PlaybackEvent::SeekTargetStored {
                    session_rev,
                    target,
                });
                return;
            }
            _ => {}
        }
        let playing = self.state == PlaybackState::Playing;
        self.capture_position();
        let preserved = self.position;
        let interrupt = Arc::clone(&self.interrupt);
        let outcome = {
            let Some(source) = self.source.as_mut() else {
                return;
            };
            source.seek_refined(target, Some(SEEK_BUDGET), &mut || {
                interrupt.load(Ordering::Acquire) != 0
            })
        };
        match outcome {
            Ok(outcome) => {
                // `refinement_truncated` is false when refinement ran into the
                // end of the media, so a short landing is checked separately.
                let truncated = outcome.refinement_truncated
                    || outcome.actual.saturating_add(RESUME_TOLERANCE) < target;
                let actual = outcome.actual;
                self.position = actual;
                self.requested_target = None;
                if let Err(error) = self.reinstall(playing) {
                    if !is_cancelled(&error) {
                        self.fail(format!("cannot restart the audio device: {error}"));
                    }
                    return;
                }
                if self.state == PlaybackState::Ended {
                    self.set_state(PlaybackState::Paused);
                }
                let session_rev = self.session_rev;
                self.emit(PlaybackEvent::SeekCompleted {
                    session_rev,
                    requested: target,
                    actual,
                    refinement_truncated: truncated,
                });
            }
            Err(error) => {
                // The captured value stands. After `Cancelled` the decoder is
                // parked at an arbitrary point inside the refinement, so the
                // position has to be re-established explicitly, never assumed.
                self.position = preserved;
                match self.reseek(preserved) {
                    // Cancelled again: the stop or shutdown that cancelled the
                    // seek is about to be handled, and it tears the decoder
                    // down anyway. The preserved position stands.
                    Err(PlaybackError::Cancelled) => {}
                    Ok(actual) => {
                        self.position = adopt_preserved(preserved, actual);
                        if let Err(error) = self.reinstall(playing) {
                            if !is_cancelled(&error) {
                                self.fail(format!("cannot restart the audio device: {error}"));
                            }
                            return;
                        }
                        self.reject_seek(format!("{error}"));
                    }
                    Err(restore) => self.fail(format!(
                        "seek failed ({error}) and the decoder could not be restored ({restore})"
                    )),
                }
            }
        }
    }

    fn restart(&mut self) {
        if self.source.is_none() {
            self.warn("nothing is loaded".into());
            return;
        }
        if self.transport.is_some() {
            self.capture_position();
        }
        // Validate first: the transport is only started once the decoder has
        // actually landed at zero.
        match self.reseek(Duration::ZERO) {
            Ok(actual) => {
                self.position = actual;
                self.requested_target = None;
            }
            Err(PlaybackError::Cancelled) => return,
            Err(error) => {
                self.reject_seek(format!("{error}"));
                return;
            }
        }
        match self.reinstall(true) {
            Ok(()) => {
                // G1: `restart()` is the only establishment that discards a
                // stored target and lands at zero with no `SeekCompleted`
                // ever emitted (D17), so it needs an event of its own for a
                // policy that has to tell an explicit restart apart from any
                // other establishment (Task 11 lifts checkpoint protection on
                // exactly this). Emitted only here, on the `Ok` arm: a
                // cancelled or failed `reinstall` announced nothing to begin
                // with, and must not announce a restart that did not land.
                let session_rev = self.session_rev;
                let position = self.position;
                self.emit(PlaybackEvent::RestartEstablished {
                    session_rev,
                    position,
                });
                self.announce_playing();
            }
            Err(error) if is_cancelled(&error) => {}
            Err(error) => self.fail(format!("cannot start the audio device: {error}")),
        }
    }

    /// A preserving seek: no budget, because it promises to land exactly where
    /// it was told to, but cancellable, so that a stop or a shutdown is not
    /// left waiting for a refinement to finish. A cancelled one leaves the
    /// decoder at an arbitrary point, which is why every caller either
    /// re-establishes it or abandons the operation entirely.
    fn reseek(&mut self, target: Duration) -> Result<Duration, PlaybackError> {
        let interrupt = Arc::clone(&self.interrupt);
        let Some(source) = self.source.as_mut() else {
            return Ok(target);
        };
        if source.position() == target {
            return Ok(target);
        }
        source
            .seek_refined(target, None, &mut || interrupt.load(Ordering::Acquire) != 0)
            .map(|outcome| outcome.actual)
    }

    fn clamp_target(&self, requested: Duration) -> Duration {
        match self
            .source
            .as_ref()
            .and_then(|source| source.metadata().duration)
        {
            Some(duration) => requested.min(duration),
            None => requested,
        }
    }
}

/// A cancelled operation is not a failure. The interrupt that cancelled it is
/// still set, so the loop's next pass turns it into the stop or the shutdown
/// the user actually asked for.
fn is_cancelled(error: &PlaybackError) -> bool {
    matches!(error, PlaybackError::Cancelled)
}

fn severity(fault: OutputFault) -> u8 {
    match fault {
        OutputFault::Recoverable(_) => 0,
        OutputFault::Rebuild(_) => 1,
        OutputFault::Fatal(_) => 2,
    }
}

/// Retire every fault belonging to a transport being torn down.
///
/// A fault describes the transport that produced it, so carrying one past a
/// teardown lets an obsolete fatal fault fire later and turn a completed `Stop`
/// into `Failed`. Both sources have to go: the one held back for want of event
/// room, and anything still queued in the channel — an interrupt arriving
/// before the queue was drained leaves the fault there rather than deferred.
fn retire_faults(deferred: &mut Option<OutputFault>, faults: &Receiver<OutputFault>) {
    *deferred = None;
    while faults.try_recv().is_ok() {}
}

fn frames_to_duration(frames: u64, rate: u32) -> Duration {
    Duration::from_secs_f64(frames as f64 / f64::from(rate.max(1)))
}

/// Best-effort text for a `std::thread::JoinHandle::join` panic payload. Panic
/// payloads are almost always a `&'static str` or a `String`; anything else
/// reports as opaque rather than being dropped silently.
fn describe_panic(panic: &(dyn Any + Send)) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Keep the promised value when the shortfall is sub-frame quantization, and
/// adopt the decoder's answer when it is a real difference. Dropping this makes
/// both `play_from_stopped_resumes_at_the_preserved_position_without_resetting`
/// and `transport_recreation_preserves_position` fail on their `>=`.
fn adopt_preserved(promised: Duration, actual: Duration) -> Duration {
    if actual <= promised && promised - actual <= RESUME_TOLERANCE {
        promised
    } else {
        actual
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retiring_faults_clears_the_deferred_slot_and_the_queue() {
        // Both halves matter. Clearing only the deferred slot leaves a fault
        // that was still queued when the interrupt arrived, and that one fires
        // after the stop completes.
        let (tx, rx) = crossbeam_channel::bounded(8);
        tx.send(OutputFault::Fatal(cpal::ErrorKind::PermissionDenied))
            .expect("queue accepts a fault");
        tx.send(OutputFault::Rebuild(cpal::ErrorKind::DeviceNotAvailable))
            .expect("queue accepts a fault");
        let mut deferred = Some(OutputFault::Fatal(cpal::ErrorKind::HostUnavailable));

        retire_faults(&mut deferred, &rx);

        assert!(
            deferred.is_none(),
            "a deferred fault outlived its transport"
        );
        assert!(
            rx.try_recv().is_err(),
            "a queued fault outlived its transport"
        );
    }

    #[test]
    fn retiring_faults_is_safe_when_there_is_nothing_to_retire() {
        let (_tx, rx) = crossbeam_channel::bounded::<OutputFault>(8);
        let mut deferred = None;
        retire_faults(&mut deferred, &rx);
        assert!(deferred.is_none());
    }
}
