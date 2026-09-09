//! Session 1 → persist → session 2. The rig runs `app::run`'s ordering —
//! drain events, then sample once — against the test engine and a real store
//! in a tempdir, writing synchronously so nothing here depends on a thread.
//!
//! The quit goes through `Session::reconcile_shutdown`, which is the same
//! handoff `app::run` performs: these tests would not be worth much if they
//! verified a copy of it that lives here.

use std::sync::Arc;
use std::time::Duration;

use continuo::clock::{Clock, FakeClock};
use continuo::media::id::{AbsolutePath, MediaId};
use continuo::persistence::model::{PersistedCheckpoint, PersistedState};
use continuo::persistence::store::StateStore;
use continuo::persistence::writer::Urgency;
use continuo::playback::checkpoint::PlaybackCheckpoint;
use continuo::playback::command::PlaybackCommand;
use continuo::playback::decode::DecodedSource;
use continuo::playback::state::PlaybackState;
use continuo::playback::volume::Volume;
use continuo::session::{Action, CAPTURE_INTERVAL, Session, decide_resume};

mod support;

use support::TestEngine;

const TRACK: &str = "sine-5s.flac";
/// The fixture's duration, which the probe would supply in `app::run`. The rig
/// checks the probe agrees with it, so it cannot drift from the fixture.
const TRACK_DURATION: Duration = Duration::from_secs(5);

/// How long `Rig::send` gives a command to be applied and its event to be
/// flushed. §3 spends one worker pass on each, and the engine's own pass is
/// paced by the harness device, so this is a budget rather than a count of
/// passes: eight periods' worth of wall time, which every command in these
/// tests has needed a small fraction of.
const SETTLE_PASSES: usize = 40;
const SETTLE_NAP: Duration = Duration::from_millis(5);

fn fixture_path() -> AbsolutePath {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(TRACK);
    let Ok(canonical) = path.canonicalize() else {
        panic!("the fixture must exist: {path:?}");
    };
    match AbsolutePath::new(canonical) {
        Ok(path) => path,
        Err(error) => panic!("the fixture path must be identifiable: {error}"),
    }
}

fn track_id() -> MediaId {
    MediaId::LocalFile(fixture_path())
}

/// A store on `dir/state.json` and the clock it stamps `updated_at` from, for
/// the tests that do not build a whole rig.
fn store_in(dir: &std::path::Path) -> (StateStore, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::new());
    // `clock.clone()`, not `Arc::clone(&clock)`: the annotation constrains the
    // argument position, and `&Arc<FakeClock>` does not coerce to
    // `&Arc<dyn Clock>`.
    let injected: Arc<dyn Clock> = clock.clone();
    (StateStore::new(dir.join("state.json"), injected), clock)
}

struct Rig {
    engine: TestEngine,
    session: Session,
    store: StateStore,
    clock: Arc<FakeClock>,
}

impl Rig {
    /// Session 1: a fresh file, playing from the top.
    fn open(dir: &std::path::Path) -> Self {
        Self::open_at(dir, PersistedState::default(), Duration::ZERO)
    }

    fn open_at(dir: &std::path::Path, state: PersistedState, start_at: Duration) -> Self {
        // §11 validates a stored position against the duration the probe
        // reports, and these tests stand `TRACK_DURATION` in for it.
        let probed = match DecodedSource::open(&fixture_path()) {
            Ok(source) => source.metadata().duration,
            Err(error) => panic!("the fixture must be decodable: {error}"),
        };
        assert_eq!(
            probed,
            Some(TRACK_DURATION),
            "TRACK_DURATION must be what the probe reports for {TRACK}"
        );

        let (store, clock) = store_in(dir);
        let mut rig = Self {
            engine: TestEngine::start_at(TRACK, start_at),
            session: Session::new(state),
            store,
            clock,
        };
        rig.pump();
        rig
    }

    /// One iteration of `app::run`: drain the events, then sample once.
    fn pump(&mut self) {
        while let Some(event) = self.engine.try_event() {
            let action = self.session.observe(&event, self.clock.sample());
            Self::write(&self.store, action);
        }
        let progress = self.engine.progress();
        let action = self.session.tick(&progress, self.clock.sample());
        Self::write(&self.store, action);
    }

    /// An associated function, not a method: `pump` already holds a mutable
    /// borrow of `session` when it calls this.
    ///
    /// The writer thread's coalescing has its own tests; what matters here is
    /// which snapshot the policy produced, so it is written straight through.
    fn write(store: &StateStore, action: Action) {
        if let Action::Submit { state, .. } = action
            && let Err(error) = store.write(&state)
        {
            panic!("the tempdir must be writable: {error}");
        }
    }

    fn send(&mut self, command: PlaybackCommand) {
        self.engine.send(command);
        // The worker applies a command on one pass and flushes the event it
        // produced on the next (§3), so run the application loop for a while
        // rather than once. Deliberately not `TestEngine::position`, which
        // settles by sending a volume command of its own and consuming the
        // answer — it would eat the very `VolumeChanged` a test is watching
        // for. The harness clock is frozen, so no playback time passes here.
        for _ in 0..SETTLE_PASSES {
            self.pump();
            std::thread::sleep(SETTLE_NAP);
        }
    }

    fn engine_state(&mut self) -> PlaybackState {
        self.engine.state()
    }

    /// `q`: interrupt, join, then the policy's half of the handoff — replay
    /// what the loop never drained and take one forced snapshot.
    fn quit(mut self) {
        let Some(report) = self.engine.shutdown_report() else {
            panic!("the engine was already gone");
        };
        let final_state = self
            .session
            .reconcile_shutdown(&report, self.clock.sample());
        if let Err(error) = self.store.write(&final_state) {
            panic!("the tempdir must be writable: {error}");
        }
    }
}

fn reload(dir: &std::path::Path) -> PersistedState {
    store_in(dir).0.load().state
}

#[test]
fn a_stop_and_a_quit_resume_where_playback_reached() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(2));
    rig.pump();
    rig.send(PlaybackCommand::Stop);
    rig.quit();

    let state = reload(dir.path());
    let entry = state
        .entry_for(&track_id())
        .expect("an entry for the track");
    assert!(
        entry.position >= Duration::from_secs(2) && entry.position < TRACK_DURATION,
        "session 2 must resume near where session 1 stopped: {:?}",
        entry.position
    );
    assert!(!entry.completed);

    let decision = decide_resume(state.entry_for(&track_id()), Some(TRACK_DURATION));
    assert!(decision.start_at() >= Duration::from_secs(2));
}

#[test]
fn a_pause_and_a_quit_resume_where_playback_reached() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(2));
    rig.pump();
    rig.send(PlaybackCommand::Pause);
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(entry.position >= Duration::from_secs(2));
}

#[test]
fn a_seek_is_persisted_from_the_canonical_position() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(1));
    rig.pump();
    rig.send(PlaybackCommand::SeekTo(Duration::from_secs(3)));
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position >= Duration::from_secs(3),
        "the seek's landing, taken from Progress rather than from the event: {:?}",
        entry.position
    );
}

/// The ordinary 5 s capture, which no other test here reaches: the rig's clock
/// is frozen, so every entry otherwise comes from a forced trigger or the
/// shutdown snapshot. Driving the interval needs nothing but the clock.
#[test]
fn an_ordinary_capture_lands_once_the_interval_has_passed() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(2));
    rig.pump();
    assert!(
        reload(dir.path()).entry_for(&track_id()).is_none(),
        "nothing is due: no clock time has passed since playback established"
    );

    rig.clock.advance(CAPTURE_INTERVAL);
    let progress = rig.engine.progress();
    let action = rig.session.tick(&progress, rig.clock.sample());
    let Action::Submit { state, urgency } = action else {
        panic!("the elapsed interval must produce a capture");
    };
    assert_eq!(
        urgency,
        Urgency::Ordinary,
        "an interval capture is not a forced one"
    );
    store_in(dir.path())
        .0
        .write(&state)
        .expect("the tempdir is writable");

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position >= Duration::from_secs(2),
        "the capture carries the tick's position: {:?}",
        entry.position
    );
    rig.quit();
}

/// play → stop → seek while stopped → quit. The sequence D7 and D17 exist for.
#[test]
fn a_stopped_seek_target_outlives_the_quit() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(3));
    rig.pump();
    rig.send(PlaybackCommand::Stop);
    rig.send(PlaybackCommand::SeekTo(Duration::from_secs(1)));
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position < Duration::from_secs(2),
        "the stored target, not the pre-seek position the engine still reports: {:?}",
        entry.position
    );
}

/// The same sequence with a `q` that gives the event no time to be drained.
#[test]
fn a_stopped_seek_target_outlives_a_quit_that_races_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(3));
    rig.pump();
    rig.send(PlaybackCommand::Stop);

    // No pump between the seek and the quit: exactly what pressing `←` and then
    // `q` inside one poll window does. The wait is for the command to be taken,
    // not for its event — `q` does not wait either, but a command the worker
    // never read is not a lost event, it is a test asking the wrong question.
    rig.engine
        .send(PlaybackCommand::SeekTo(Duration::from_secs(1)));
    rig.engine.await_commands_taken();
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position < Duration::from_secs(2),
        "the SeekTargetStored is not the application's to lose: {:?}",
        entry.position
    );
}

/// stop → seek to 1 s → Home → play → quit. `restart()` discards the target.
#[test]
fn a_restart_after_a_stopped_seek_persists_where_it_restarted() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_for(Duration::from_secs(3));
    rig.pump();
    rig.send(PlaybackCommand::Stop);
    rig.send(PlaybackCommand::SeekTo(Duration::from_secs(1)));
    rig.send(PlaybackCommand::Restart);
    rig.engine.play_for(Duration::from_secs(2));
    rig.pump();
    rig.quit();

    let entry = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry for the track");
    assert!(
        entry.position >= Duration::from_secs(2),
        "the restarted playback's position, not the target the restart threw away: {:?}",
        entry.position
    );
}

#[test]
fn a_finished_track_is_completed_and_reopens_at_zero_with_its_position_kept() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_to_end();
    rig.pump();
    rig.quit();

    let state = reload(dir.path());
    let entry = state.entry_for(&track_id()).cloned().expect("an entry");
    assert!(entry.completed);
    assert!(
        entry.position > Duration::from_secs(4),
        "D1 retains it: {:?}",
        entry.position
    );

    let decision = decide_resume(state.entry_for(&track_id()), Some(TRACK_DURATION));
    assert_eq!(
        decision.start_at(),
        Duration::ZERO,
        "and reopening starts at zero"
    );
}

#[test]
fn a_completed_entry_survives_a_launch_whose_device_refuses_to_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.engine.play_to_end();
    rig.pump();
    rig.quit();
    let kept = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("an entry");
    assert!(kept.completed);

    let after = relaunch_onto_a_refusing_device(dir.path());
    assert_eq!(
        after.position, kept.position,
        "D20: nothing established, so nothing overwrites the position D1 retains"
    );
    assert!(after.completed);
}

/// The other §11 row that opens at zero while keeping what it has: a stored
/// position past the end of the media, as a hand-edited or renamed-media file
/// would carry. D20 must retain it for the same reason it retains a completed
/// one — nothing established, so there is no validated position to write.
#[test]
fn a_position_past_the_end_survives_a_launch_whose_device_refuses_to_open() {
    let dir = tempfile::tempdir().unwrap();
    let (store, clock) = store_in(dir.path());
    let mut stale = PersistedState::default();
    stale.record(
        &PlaybackCheckpoint {
            media: track_id(),
            position: Duration::from_secs(600),
            updated_at: clock.sample().wall,
        },
        false,
    );
    store.write(&stale).expect("the tempdir is writable");
    let kept = reload(dir.path())
        .entry_for(&track_id())
        .cloned()
        .expect("the stale entry");
    assert_eq!(
        decide_resume(Some(&kept), Some(TRACK_DURATION)).start_at(),
        Duration::ZERO,
        "§11 opens a position past the end at zero"
    );

    let after = relaunch_onto_a_refusing_device(dir.path());
    assert_eq!(
        after.position, kept.position,
        "D20: nothing established, so nothing overwrites the retained position"
    );
    assert!(!after.completed);
}

/// Session 2 for the two §11 rows that open at zero. §11 starts such an entry
/// at zero, `load()` emits `Loaded` before it opens the device, and this device
/// offers six channels — which negotiation refuses, after the `Loaded` has
/// already gone out. The application therefore knows the media and reports a
/// position of zero, and playback never happened.
///
/// Deliberately not the rig: `TestEngine` always reaches `Playing`, which
/// establishes, so it cannot stage this at all.
fn relaunch_onto_a_refusing_device(dir: &std::path::Path) -> PersistedCheckpoint {
    let (store, clock) = store_in(dir);
    let mut session = Session::new(reload(dir));

    let report = support::failed_device_session(TRACK, 6, Duration::ZERO);
    let final_state = session.reconcile_shutdown(&report, clock.sample());
    if let Err(error) = store.write(&final_state) {
        panic!("the tempdir must be writable: {error}");
    }

    match reload(dir).entry_for(&track_id()).cloned() {
        Some(entry) => entry,
        None => panic!("the entry the relaunch must not have dropped is gone"),
    }
}

/// The value's half of §11's volume restore. The ordering half — `SetVolume`
/// before `Load`, so the restored level is in force from the first buffer — is
/// pinned on the sequence `app::run` issues, in `src/app.rs`: the harness
/// records the engine's events rather than the commands sent to it, so the
/// order of two commands is not observable from here.
#[test]
fn volume_survives_the_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::open(dir.path());
    rig.send(PlaybackCommand::SetVolume(Volume::new(0.25)));
    rig.quit();

    let state = reload(dir.path());
    assert_eq!(state.volume(), Volume::new(0.25));
    assert_eq!(state.current_media, Some(track_id()));
}

#[test]
fn a_position_past_the_end_is_refused_as_a_start() {
    // A stale file, as a hand-edited or renamed-media one would be.
    let dir = tempfile::tempdir().unwrap();
    let (store, clock) = store_in(dir.path());
    let mut state = PersistedState::default();
    state.record(
        &PlaybackCheckpoint {
            media: track_id(),
            position: Duration::from_secs(600),
            updated_at: clock.sample().wall,
        },
        false,
    );
    store.write(&state).unwrap();

    let reloaded = reload(dir.path());
    let decision = decide_resume(reloaded.entry_for(&track_id()), Some(TRACK_DURATION));
    assert_eq!(decision.start_at(), Duration::ZERO);

    // And the engine can be started from that decision without complaint.
    let mut rig = Rig::open_at(dir.path(), reloaded, decision.start_at());
    assert_eq!(rig.engine_state(), PlaybackState::Playing);
    rig.quit();
}
