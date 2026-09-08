//! Harness for the engine contract tests.
//!
//! The engine runs on its own thread against a virtual device, so the tests
//! control time explicitly. The driver thread has two modes:
//!
//! * *frozen* — the clock does not move, and the callback runs only while a
//!   handshake is in flight (any phase but `Run`). Transitions still complete,
//!   but no audio is consumed and no instant passes, so a position read before
//!   a command and one read after it are comparable exactly.
//! * *advancing* — one buffer period of virtual time per step, which is what
//!   playing audio looks like.
//!
//! Every assertion about preservation is made with the clock frozen; that is
//! what makes `assert_eq!` on a position honest rather than flaky.

#![allow(dead_code)]

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use continuo::media::id::{AbsolutePath, MediaId};
use continuo::media::source::SourceLocation;
use continuo::playback::callback::CallbackCore;
use continuo::playback::command::PlaybackCommand;
use continuo::playback::engine::EngineHandle;
use continuo::playback::error::PlaybackError;
use continuo::playback::event::{PlaybackEvent, Progress};
use continuo::playback::link::{OutputLink, Phase};
use continuo::playback::output::cpal_output::OutputFault;
use continuo::playback::output::test_output::TestOutput;
use continuo::playback::output::{AudioOutput, Nanos, NegotiatedOutput, OutputRequest};
use continuo::playback::state::PlaybackState;

const CHANNELS: u16 = 2;
const RATE: u32 = 48_000;
/// 2 ms at 48 kHz: one period, and the granularity of the virtual clock.
const BUFFER_FRAMES: u32 = 96;
const PERIOD: Duration = Duration::from_millis(2);
/// Deliberately generous, so that "the ring is empty" and "the last frame has
/// been heard" are far apart in time and the end-of-track rule is observable.
const LATENCY: Duration = Duration::from_millis(100);
const DRIVER_NAP: Duration = Duration::from_micros(500);
const PATIENCE: Duration = Duration::from_secs(20);

const FROZEN: u8 = 0;
const ADVANCING: u8 = 1;

#[allow(clippy::unwrap_used)]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned harness mutex means a test thread already panicked; there is
    // nothing better to do than propagate it.
    mutex.lock().unwrap()
}

#[allow(clippy::unwrap_used)]
fn fixture(name: &str) -> AbsolutePath {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    AbsolutePath::new(path.canonicalize().unwrap()).unwrap()
}

struct Device {
    output: TestOutput,
    /// Stashed on every `open`, so the harness can reach the counters the
    /// callback writes without the engine exposing its internals.
    link: Option<Arc<OutputLink>>,
}

struct HarnessOutput {
    device: Arc<Mutex<Device>>,
}

impl AudioOutput for HarnessOutput {
    fn negotiate(&mut self, request: &OutputRequest) -> Result<NegotiatedOutput, PlaybackError> {
        lock(&self.device).output.negotiate(request)
    }

    fn open(
        &mut self,
        config: &NegotiatedOutput,
        link: Arc<OutputLink>,
        core: CallbackCore,
    ) -> Result<(), PlaybackError> {
        let mut device = lock(&self.device);
        device.link = Some(Arc::clone(&link));
        device.output.open(config, link, core)
    }

    fn now(&self) -> Nanos {
        lock(&self.device).output.now()
    }

    fn close(&mut self) {
        let mut device = lock(&self.device);
        device.output.close();
        device.link = None;
    }
}

struct Driver {
    device: Arc<Mutex<Device>>,
    mode: AtomicU8,
    stop: AtomicBool,
}

impl Driver {
    fn step(&self) {
        let mut device = lock(&self.device);
        if self.mode.load(Ordering::Relaxed) == ADVANCING {
            device.output.advance(PERIOD);
            return;
        }
        // Frozen: only run the callback while a transition needs answering.
        // Running it in `Run` would replay a buffer at an instant that has
        // already been used, which is not something a device ever does.
        let running = device
            .link
            .as_ref()
            .is_some_and(|link| link.load_control().phase == Phase::Run);
        if !running && device.link.is_some() {
            device.output.pump_in_place();
        }
    }
}

pub struct TestEngine {
    handle: Mutex<Option<EngineHandle>>,
    commands: Sender<PlaybackCommand>,
    faults: Sender<OutputFault>,
    wake: Sender<()>,
    device: Arc<Mutex<Device>>,
    driver: Arc<Driver>,
    thread: Mutex<Option<JoinHandle<()>>>,
    inbox: Mutex<Vec<PlaybackEvent>>,
    states: Mutex<Vec<PlaybackState>>,
    /// How far `await_state` has consumed the state history. A state the
    /// engine has already passed through must not satisfy a later wait.
    consumed_states: Mutex<usize>,
    draining: AtomicBool,
}

impl TestEngine {
    pub fn start(name: &str) -> Self {
        let device = Arc::new(Mutex::new(Device {
            output: TestOutput::new(CHANNELS, RATE, BUFFER_FRAMES, LATENCY),
            link: None,
        }));
        let (fault_tx, fault_rx) = crossbeam_channel::bounded(16);
        let handle = EngineHandle::spawn_with(
            Box::new(HarnessOutput {
                device: Arc::clone(&device),
            }),
            fault_rx,
        );
        let driver = Arc::new(Driver {
            device: Arc::clone(&device),
            mode: AtomicU8::new(FROZEN),
            stop: AtomicBool::new(false),
        });
        let thread = {
            let driver = Arc::clone(&driver);
            std::thread::Builder::new()
                .name("harness-device".into())
                .spawn(move || {
                    while !driver.stop.load(Ordering::Relaxed) {
                        driver.step();
                        std::thread::sleep(DRIVER_NAP);
                    }
                })
                .ok()
        };
        let mut engine = Self {
            commands: handle.commands().clone(),
            wake: handle.wake().clone(),
            handle: Mutex::new(Some(handle)),
            faults: fault_tx,
            device,
            driver,
            thread: Mutex::new(thread),
            inbox: Mutex::new(Vec::new()),
            states: Mutex::new(Vec::new()),
            consumed_states: Mutex::new(0),
            draining: AtomicBool::new(true),
        };
        let path = fixture(name);
        engine.send(PlaybackCommand::Load {
            media: MediaId::LocalFile(path.clone()),
            source: SourceLocation::LocalPath(path.as_path().to_path_buf()),
            start_at: Duration::ZERO,
        });
        engine.await_state(PlaybackState::Paused);
        engine.send(PlaybackCommand::Play);
        engine.await_state(PlaybackState::Playing);
        // The events a start-up emits are not what any test is looking at.
        lock(&engine.inbox).clear();
        engine
    }

    // ------------------------------------------------------------- commands

    pub fn send(&mut self, command: PlaybackCommand) {
        if self.commands.send(command).is_err() {
            panic!("the engine stopped accepting commands");
        }
    }

    pub fn interrupt_stop(&mut self) {
        if let Some(handle) = lock(&self.handle).as_ref() {
            handle.interrupt_stop();
        }
    }

    /// Inject the device fault a vanished output device reports.
    pub fn force_device_loss(&mut self) {
        let _ = self
            .faults
            .send(OutputFault::Rebuild(cpal::ErrorKind::DeviceNotAvailable));
        let _ = self.wake.try_send(());
    }

    pub fn inject_xruns(&mut self, count: usize) {
        let device = lock(&self.device);
        let Some(link) = device.link.as_ref() else {
            panic!("no transport is open, so there is nothing to inject into");
        };
        for _ in 0..count {
            link.note_xrun();
        }
    }

    /// Commands the worker has not taken off the channel yet. Admission
    /// closing is otherwise invisible from outside.
    pub fn pending_commands(&mut self) -> usize {
        lock(&self.handle)
            .as_ref()
            .map_or(0, |handle| handle.commands().len())
    }

    pub fn drop_event_receiver(&self) {
        if let Some(handle) = lock(&self.handle).as_mut() {
            handle.release_events();
        }
    }

    // ---------------------------------------------------------------- clock

    /// Held under the device lock, which the driver also takes before reading
    /// the mode: without it a step already in flight advances the clock after
    /// the test believes it froze, and a preserved position drifts by a period.
    fn set_mode(&self, mode: u8) {
        let _device = lock(&self.device);
        self.driver.mode.store(mode, Ordering::Relaxed);
    }

    /// Let the clock run until the reported position reaches `target`.
    pub fn play_for(&mut self, target: Duration) {
        self.set_mode(ADVANCING);
        let deadline = Instant::now() + PATIENCE;
        while self.raw_position() < target {
            if Instant::now() >= deadline {
                self.set_mode(FROZEN);
                panic!(
                    "position never reached {target:?}; it stalled at {:?}",
                    self.raw_position()
                );
            }
            self.pump_events();
            std::thread::sleep(Duration::from_millis(1));
        }
        self.set_mode(FROZEN);
        self.settle();
    }

    pub fn play_to_end(&mut self) {
        self.set_mode(ADVANCING);
        let deadline = Instant::now() + PATIENCE;
        while !self.take_state(PlaybackState::Ended) {
            if Instant::now() >= deadline {
                self.set_mode(FROZEN);
                panic!("the track never ended");
            }
            self.pump_events();
            std::thread::sleep(Duration::from_millis(1));
        }
        self.set_mode(FROZEN);
        self.settle();
    }

    /// Advance one period at a time until the callback has been handed nothing
    /// but silence twice running, which is what an empty ring sounds like, and
    /// stop there. The output latency means the final span's predicted play
    /// time is still far ahead, so end of track must not have fired yet.
    pub fn drain_ring_without_advancing_clock(&mut self) {
        let deadline = Instant::now() + PATIENCE;
        let mut silent = 0;
        while silent < 2 {
            if Instant::now() >= deadline {
                panic!("the ring never drained");
            }
            {
                let mut device = lock(&self.device);
                device.output.clear_captured();
                device.output.advance(PERIOD);
                if device.output.captured().iter().all(|sample| *sample == 0.0) {
                    silent += 1;
                } else {
                    silent = 0;
                }
            }
            // Paced so the worker is never the reason the ring runs dry.
            std::thread::sleep(Duration::from_millis(1));
        }
        self.pump_events();
    }

    /// Let virtual time pass without asking anything of the engine. Used to
    /// show that a parked transport does not move the position.
    pub fn let_time_pass(&mut self, span: Duration) {
        let until = Instant::now() + Duration::from_secs(5);
        let mut advanced = Duration::ZERO;
        while advanced < span && Instant::now() < until {
            lock(&self.device).output.advance(PERIOD);
            advanced += PERIOD;
            std::thread::sleep(Duration::from_micros(200));
        }
        self.settle();
    }

    pub fn advance_past_output_latency(&mut self) {
        let until = Instant::now() + Duration::from_secs(5);
        let mut advanced = Duration::ZERO;
        let target = LATENCY + LATENCY;
        while advanced < target && Instant::now() < until {
            lock(&self.device).output.advance(PERIOD);
            advanced += PERIOD;
            std::thread::sleep(Duration::from_micros(200));
        }
        self.pump_events();
    }

    // --------------------------------------------------------------- events

    fn pump_events(&self) {
        if !self.draining.load(Ordering::Relaxed) {
            return;
        }
        let handle = lock(&self.handle);
        let Some(handle) = handle.as_ref() else {
            return;
        };
        while let Ok(event) = handle.events().try_recv() {
            if let PlaybackEvent::StateChanged { state, .. } = &event {
                lock(&self.states).push(*state);
            }
            lock(&self.inbox).push(event);
        }
    }

    pub fn stop_draining_events(&mut self) {
        self.draining.store(false, Ordering::Relaxed);
    }

    pub fn resume_draining_events(&mut self) {
        self.draining.store(true, Ordering::Relaxed);
        self.pump_events();
    }

    pub fn try_event(&mut self) -> Option<PlaybackEvent> {
        self.pump_events();
        let mut inbox = lock(&self.inbox);
        if inbox.is_empty() {
            None
        } else {
            Some(inbox.remove(0))
        }
    }

    pub fn await_event(&mut self, predicate: impl Fn(&PlaybackEvent) -> bool) -> PlaybackEvent {
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.pump_events();
            {
                let mut inbox = lock(&self.inbox);
                if let Some(index) = inbox.iter().position(&predicate) {
                    return inbox.remove(index);
                }
            }
            if Instant::now() >= deadline {
                panic!("no matching event arrived; saw {:?}", lock(&self.inbox));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn count_events(&mut self, predicate: impl Fn(&PlaybackEvent) -> bool) -> usize {
        // Give anything still in flight a moment to arrive.
        let until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < until {
            self.pump_events();
            std::thread::sleep(Duration::from_millis(5));
        }
        lock(&self.inbox).iter().filter(|e| predicate(e)).count()
    }

    /// Consume the next occurrence of `state` from the history, if it has
    /// happened yet.
    fn take_state(&self, state: PlaybackState) -> bool {
        let states = lock(&self.states);
        let mut consumed = lock(&self.consumed_states);
        match states[*consumed..].iter().position(|s| *s == state) {
            Some(offset) => {
                *consumed += offset + 1;
                true
            }
            None => false,
        }
    }

    pub fn await_state(&mut self, state: PlaybackState) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.pump_events();
            if self.take_state(state) {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "the engine never reached {state:?}; it went through {:?}",
                    lock(&self.states)
                );
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn state(&mut self) -> PlaybackState {
        self.pump_events();
        lock(&self.states)
            .last()
            .copied()
            .unwrap_or(PlaybackState::Idle)
    }

    // ------------------------------------------------------------- progress

    pub fn progress(&mut self) -> Progress {
        match lock(&self.handle).as_ref() {
            Some(handle) => handle.progress(),
            None => panic!("the engine is gone"),
        }
    }

    fn raw_position(&mut self) -> Duration {
        self.progress().position
    }

    /// The published position, once the worker has had a chance to publish
    /// everything the frozen clock implies. Reading straight after a command
    /// would otherwise see the snapshot from before it was dispatched.
    pub fn position(&mut self) -> Duration {
        self.settle();
        self.raw_position()
    }

    /// Wait until the published position stops changing. Only meaningful with
    /// the clock frozen, which is the only time a test compares positions.
    fn settle(&mut self) {
        let deadline = Instant::now() + PATIENCE;
        let mut last = self.raw_position();
        loop {
            std::thread::sleep(Duration::from_millis(12));
            self.pump_events();
            let current = self.raw_position();
            if current == last {
                return;
            }
            if Instant::now() >= deadline {
                panic!("the published position never settled");
            }
            last = current;
        }
    }

    pub fn join_within(&self, patience: Duration) -> bool {
        let deadline = Instant::now() + patience;
        loop {
            {
                let handle = lock(&self.handle);
                match handle.as_ref() {
                    Some(handle) if handle.is_finished() => return true,
                    None => return true,
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for TestEngine {
    fn drop(&mut self) {
        // Shut the worker down first: it needs the device thread alive to
        // answer the handshakes its teardown performs.
        let handle = lock(&self.handle).take();
        if let Some(handle) = handle {
            handle.interrupt_shutdown();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            handle.join();
        }
        self.driver.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = lock(&self.thread).take() {
            let _ = thread.join();
        }
    }
}
