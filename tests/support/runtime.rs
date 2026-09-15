//! Shared rig for suites that drive a [`PlayerRuntime`] headlessly: a real
//! engine over the paced, deviceless `NullOutput`, a state writer into a
//! temporary directory, and pumping helpers with a generous deadline.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use continuo::application::runtime::{EngineFactory, LibraryStores, PlayerRuntime, RuntimeParts};
use continuo::application::view::PlayerView;
use continuo::clock::{Clock, SystemClock};
use continuo::http::limits::Limits;
use continuo::persistence::model::PersistedState;
use continuo::persistence::store::StateStore;
use continuo::persistence::writer::WriterHandle;
use continuo::playback::engine::EngineHandle;
use continuo::playback::output::null_output::NullOutput;
use continuo::queue::QueueEntryId;
use continuo::session::Session;

pub struct Rig {
    pub _dir: tempfile::TempDir,
    pub runtime: PlayerRuntime,
    pub state_path: std::path::PathBuf,
}

pub fn null_engine() -> EngineFactory {
    Box::new(|| EngineHandle::spawn(Box::new(NullOutput::new())))
}

pub fn rig_with(state: PersistedState) -> Rig {
    rig_with_parts(state, None, null_engine())
}

pub fn rig_with_parts(
    state: PersistedState,
    library: Option<LibraryStores>,
    engine_factory: EngineFactory,
) -> Rig {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let state_path = dir.path().join("state.json");
    let writer = WriterHandle::spawn(
        Box::new(StateStore::new(state_path.clone(), clock.clone())),
        clock.clone(),
    );
    let runtime = PlayerRuntime::new(parts(state, writer, clock, library, engine_factory));
    Rig {
        _dir: dir,
        runtime,
        state_path,
    }
}

/// The one `RuntimeParts` literal every rig builds from.
pub fn parts(
    state: PersistedState,
    writer: WriterHandle,
    clock: Arc<dyn Clock>,
    library: Option<LibraryStores>,
    engine_factory: EngineFactory,
) -> RuntimeParts {
    RuntimeParts {
        session: Session::new(state),
        writer,
        persisting: true,
        clock,
        engine_factory,
        library,
        http_limits: Limits::default(),
    }
}

pub fn pump_until(runtime: &mut PlayerRuntime, what: &str, done: impl Fn(&PlayerView) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        runtime.pump();
        if done(&runtime.view()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "never reached: {what}; view {:?}",
            runtime.view()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn pump_for(runtime: &mut PlayerRuntime, span: Duration) {
    let until = Instant::now() + span;
    while Instant::now() < until {
        runtime.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn row_ids(runtime: &PlayerRuntime) -> Vec<QueueEntryId> {
    runtime.view().rows.iter().map(|row| row.id).collect()
}
