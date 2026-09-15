//! The terminal-free player: one owner for the engine, `Session`, the state
//! writer and the HTTP service, driven by [`AppCommand`]s and [`pump`]ed once
//! per front-end iteration. It never reads a key or draws; a front end turns
//! input into commands and draws the [`PlayerView`] snapshots it returns.
//!
//! [`pump`]: PlayerRuntime::pump

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use url::Url;

use crate::application::podcast::{PodcastResolution, SAVED_SOURCE_NOTICE, resolve_podcast};
use crate::application::seek::KeyRouter;
use crate::application::source::{is_url_spelling, resolve_path, resolve_source};
use crate::application::transport::{
    PlaybackPhase, TransportDecision, TransportInput, TransportSituation, decide,
};
use crate::application::view::{
    NowPlaying, PersistenceStatus, PlayerView, entry_title, queue_rows, saved_history,
};
use crate::clock::Clock;
use crate::commands::displayable;
use crate::feed::cache::CacheStore;
use crate::http::error::{RemoteFailure, redact_url};
use crate::http::limits::Limits;
use crate::http::service::HttpService;
use crate::library::EpisodeCandidate;
use crate::media::capabilities::{MediaCapabilities, SeekSupport};
use crate::media::id::MediaId;
use crate::media::source::SourceLocation;
use crate::persistence::PersistenceError;
use crate::persistence::model::PersistedState;
use crate::persistence::writer::{ShutdownOutcome, Urgency, WriterHandle};
use crate::playback::command::{Admission, LoadRequestId, PlaybackCommand};
use crate::playback::engine::EngineHandle;
use crate::playback::error::PlaybackError;
use crate::playback::event::{PlaybackEvent, Progress};
use crate::playback::provenance::PositionProvenance;
use crate::playback::state::PlaybackState;
use crate::playback::timeline::PositionQuality;
use crate::playback::volume::Volume;
use crate::queue::{
    Direction, DisplayDuration, DisplayMetadata, DurationSource, MAX_QUEUE_ENTRIES, NewQueueEntry,
    QueueEntry, QueueEntryId, QueueError, QueueSource,
};
use crate::session::{Action, Advance, LoadTarget, RegisterLoadError, Removal, Session};
use crate::subscription::store::SubscriptionStore;

/// Shown when the engine refuses a command for want of queue room.
pub const PLAYER_BUSY: &str = "Player is busy";
/// Shown when `Session` already tracks its maximum of in-flight loads.
pub const TOO_MANY_PENDING_LOADS: &str = "Too many pending loads";

/// Builds the engine on the first load. Boxed so a test can hand in a
/// deviceless output while production uses the environment's choice.
pub type EngineFactory = Box<dyn FnMut() -> EngineHandle + Send>;

/// The local library files a podcast entry resolves against before it loads.
pub struct LibraryStores {
    pub subscriptions: SubscriptionStore,
    pub cache: CacheStore,
}

pub struct RuntimeParts {
    pub session: Session,
    pub writer: WriterHandle,
    /// Whether anything `writer` accepts can reach the disk (see
    /// [`FlushReport::Disabled`]).
    pub persisting: bool,
    pub clock: Arc<dyn Clock>,
    pub engine_factory: EngineFactory,
    /// `None` when the library stores could not be opened: every podcast
    /// entry then plays its saved source.
    pub library: Option<LibraryStores>,
    pub http_limits: Limits,
}

#[derive(Clone, Debug)]
pub enum EnqueueItem {
    Path(PathBuf),
    Url(String),
    Episode(EpisodeCandidate),
}

impl EnqueueItem {
    /// An explicit http/https spelling is a URL; anything else is a path,
    /// with the same disambiguation `continuo play` applies.
    pub fn from_input(text: &str) -> Self {
        if is_url_spelling(text) {
            Self::Url(text.to_owned())
        } else {
            Self::Path(PathBuf::from(text))
        }
    }
}

#[derive(Clone, Debug)]
pub enum AppCommand {
    PlayPause { selected: Option<QueueEntryId> },
    Play { selected: Option<QueueEntryId> },
    PlayEntry(QueueEntryId),
    Stop,
    SeekBy(i64),
    SeekTo(Duration),
    Restart,
    AdjustVolume(f32),
    Previous { selected: Option<QueueEntryId> },
    Next { selected: Option<QueueEntryId> },
    Enqueue(Vec<EnqueueItem>),
    Remove(QueueEntryId),
    Move(QueueEntryId, Direction),
    ClearQueue,
}

/// What the final flush is reported as.
#[derive(Debug)]
pub enum FlushReport {
    Written,
    Failed(PersistenceError),
    Unconfirmed,
    /// Nothing was ever going to reach the disk this session.
    Disabled,
}

/// A disabled sink reports every write as a success, deliberately — the
/// writer must not count a deliberate disable as a failure (D11) — so a
/// session that was not persisting reaches `Written` having written nothing.
/// The outcome alone must therefore never be reported as a checkpoint that
/// landed.
pub(crate) fn classify_flush(outcome: ShutdownOutcome, persisting: bool) -> FlushReport {
    if !persisting {
        return FlushReport::Disabled;
    }
    match outcome {
        ShutdownOutcome::Written => FlushReport::Written,
        ShutdownOutcome::Failed(error) => FlushReport::Failed(error),
        ShutdownOutcome::Unconfirmed => FlushReport::Unconfirmed,
    }
}

/// Ends the engine and reconciles what it never delivered, then submits the
/// final snapshot. Out of band first, and in band only as a courtesy: the
/// worker stops reading commands while an event backlog exists, so an
/// in-band `Shutdown` can sit unread while `join` blocks; the interrupt is
/// the only signal that is guaranteed to be seen. The replayed events reach
/// the policy before the snapshot is taken, so it comes from a session that
/// has seen everything the run produced (D19).
pub(crate) fn shut_down_engine(
    engine: EngineHandle,
    session: &mut Session,
    writer: &WriterHandle,
    clock: &dyn Clock,
) {
    engine.interrupt_shutdown();
    engine.commands().send(PlaybackCommand::Shutdown).ok();
    let report = engine.join();
    writer.submit(
        session.reconcile_shutdown(&report, clock.sample()),
        Urgency::Forced,
    );
}

/// The adopted playback as the engine last reported it. Exists only while
/// the engine still holds the adopted load: any later load outcome means the
/// worker tore that playback down, and a queue release ends it too.
struct Mirror {
    load: LoadRequestId,
    session_rev: u64,
    duration: Option<Duration>,
    duration_provenance: PositionProvenance,
    capabilities: MediaCapabilities,
    state: PlaybackState,
    position: Duration,
    quality: PositionQuality,
    provenance: PositionProvenance,
    buffering: bool,
}

impl Mirror {
    fn loaded(event: &PlaybackEvent) -> Option<Self> {
        let PlaybackEvent::Loaded {
            session_rev,
            request,
            metadata,
            capabilities,
            position,
            ..
        } = event
        else {
            return None;
        };
        Some(Self {
            load: *request,
            session_rev: *session_rev,
            duration: metadata.duration,
            duration_provenance: metadata.duration_provenance,
            capabilities: *capabilities,
            // The device opens after `Loaded`; `Paused` or `Failed` follows.
            state: PlaybackState::Loading,
            position: *position,
            quality: PositionQuality::Exact,
            provenance: PositionProvenance::Established,
            buffering: false,
        })
    }

    /// An event `Session` accepted as belonging to the adopted playback.
    fn apply(&mut self, event: &PlaybackEvent) {
        self.session_rev = event.session_rev();
        match event {
            PlaybackEvent::StateChanged { state, .. } => self.state = *state,
            PlaybackEvent::SeekCompleted {
                actual, provenance, ..
            } => {
                self.position = *actual;
                self.provenance = *provenance;
            }
            PlaybackEvent::SeekTargetStored { target, .. } => self.position = *target,
            PlaybackEvent::EndOfTrack {
                position,
                provenance,
                ..
            } => {
                self.position = *position;
                self.provenance = *provenance;
                self.state = PlaybackState::Ended;
            }
            PlaybackEvent::RestartEstablished {
                position,
                provenance,
                ..
            } => {
                self.position = *position;
                self.provenance = *provenance;
            }
            PlaybackEvent::CapabilitiesChanged { capabilities, .. } => {
                self.capabilities = *capabilities;
            }
            _ => {}
        }
    }

    /// While a seek target stands the display keeps showing it; see
    /// `app::apply_progress`.
    fn apply_progress(&mut self, progress: &Progress, seeking: bool) {
        self.buffering = progress.buffering;
        if seeking {
            return;
        }
        self.position = progress.position;
        self.quality = progress.quality;
        self.provenance = progress.provenance;
    }
}

pub struct PlayerRuntime {
    session: Session,
    writer: WriterHandle,
    persisting: bool,
    clock: Arc<dyn Clock>,
    engine_factory: EngineFactory,
    engine: Option<EngineHandle>,
    http: Option<Arc<HttpService>>,
    http_limits: Limits,
    library: Option<LibraryStores>,
    router: KeyRouter,
    mirror: Option<Mirror>,
    /// The output volume last requested, shown at once rather than after the
    /// engine echoes it.
    volume: Volume,
    status: Option<String>,
    selection_hint: Option<QueueEntryId>,
    /// The entry the latest load attempt was for; Space/p retry it.
    last_requested: Option<QueueEntryId>,
    /// The token the latest attempt was admitted under. Cleared when an
    /// attempt starts, so an outcome of any earlier attempt — or a failure
    /// before admission, which has no token — cannot be mistaken for it.
    last_attempt: Option<LoadRequestId>,
    /// Whether the latest attempt failed.
    load_failed: bool,
}

impl PlayerRuntime {
    pub fn new(parts: RuntimeParts) -> Self {
        let volume = parts.session.state().volume();
        Self {
            session: parts.session,
            writer: parts.writer,
            persisting: parts.persisting,
            clock: parts.clock,
            engine_factory: parts.engine_factory,
            engine: None,
            http: None,
            http_limits: parts.http_limits,
            library: parts.library,
            router: KeyRouter::new(),
            mirror: None,
            volume,
            status: None,
            selection_hint: None,
            last_requested: None,
            last_attempt: None,
            load_failed: false,
        }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
    }

    /// The entry a removal suggests selecting next, once.
    pub fn take_selection_hint(&mut self) -> Option<QueueEntryId> {
        self.selection_hint.take()
    }

    pub fn handle(&mut self, command: AppCommand) {
        match command {
            AppCommand::PlayPause { selected } => self.transport(TransportInput::Space, selected),
            AppCommand::Play { selected } => self.transport(TransportInput::Play, selected),
            AppCommand::PlayEntry(id) => self.transport(TransportInput::Enter, Some(id)),
            AppCommand::SeekBy(step) => self.transport(TransportInput::SeekBy(step), None),
            AppCommand::SeekTo(target) => self.transport(TransportInput::SeekTo(target), None),
            AppCommand::Restart => self.transport(TransportInput::Home, None),
            AppCommand::Previous { selected } => {
                self.transport(TransportInput::Previous, selected);
            }
            AppCommand::Next { selected } => self.transport(TransportInput::Next, selected),
            AppCommand::Stop => {
                self.router.cancel();
                if let Some(engine) = &self.engine {
                    engine.interrupt_stop();
                }
            }
            AppCommand::AdjustVolume(delta) => self.adjust_volume(delta),
            AppCommand::Enqueue(items) => self.enqueue(items),
            AppCommand::Remove(id) => self.remove(id),
            AppCommand::Move(id, direction) => match self.session.move_entry(id, direction) {
                Ok(action) => self.submit(action),
                Err(error) => self.status = Some(error.to_string()),
            },
            AppCommand::ClearQueue => self.clear_queue(),
        }
    }

    /// One iteration: drain engine events, act on what they asked for, then
    /// sample progress.
    pub fn pump(&mut self) {
        while let Some(event) = self
            .engine
            .as_ref()
            .and_then(|engine| engine.events().try_recv().ok())
        {
            self.observe_event(&event);
        }

        if self.session.take_stop_request() {
            self.router.cancel();
            // A load still pending tears the unadopted playback down anyway
            // when it runs; stopping now could cancel that newer load instead.
            if self.session.pending_load_count() == 0
                && let Some(engine) = &self.engine
            {
                engine.interrupt_stop();
            }
        }
        if let Some(Advance::Next(id)) = self.session.take_advance() {
            self.load_entry(id);
        }
        if self.seeking_allowed()
            && let Some(engine) = &self.engine
        {
            self.router.flush(engine, Instant::now());
        }

        let Some(engine) = &self.engine else {
            return;
        };
        let progress = engine.progress();
        let action = self.session.tick(&progress, self.clock.sample());
        self.submit(action);
        if let Some(mirror) = &mut self.mirror
            && progress.session_rev == mirror.session_rev
            && progress.load == Some(mirror.load)
        {
            mirror.apply_progress(&progress, self.router.is_seeking());
        }
    }

    pub fn view(&self) -> PlayerView {
        let state = self.session.state();
        let queue = state.queue();
        let active = queue.active();
        PlayerView {
            rows: queue_rows(state),
            active,
            now_playing: active
                .and_then(|id| queue.get(id))
                .map(|entry| self.now_playing(entry, state)),
            phase: self.phase(),
            volume: self.volume,
            status: self.status.as_deref().map(displayable),
            persistence: if self.persisting {
                PersistenceStatus::Saving
            } else {
                PersistenceStatus::Unsaved
            },
            last_requested: self.last_requested,
        }
    }

    /// Stops the engine, reconciles what it never delivered, and flushes.
    pub fn shutdown(mut self) -> FlushReport {
        self.router.cancel();
        if let Some(engine) = self.engine.take() {
            shut_down_engine(engine, &mut self.session, &self.writer, self.clock.as_ref());
        }
        classify_flush(self.writer.shutdown(), self.persisting)
    }

    // ----------------------------------------------------------- transport

    fn phase(&self) -> PlaybackPhase {
        if self.session.pending_load_count() > 0 {
            return PlaybackPhase::Loading;
        }
        if self.load_failed {
            return PlaybackPhase::LoadFailed;
        }
        match self.mirror.as_ref().map(|mirror| mirror.state) {
            None => PlaybackPhase::Unloaded,
            Some(PlaybackState::Ended) => PlaybackPhase::Ended,
            Some(PlaybackState::Paused) => PlaybackPhase::Paused,
            Some(PlaybackState::Playing) => PlaybackPhase::Playing,
            Some(PlaybackState::Stopped | PlaybackState::Failed) => PlaybackPhase::Stopped,
            Some(PlaybackState::Idle | PlaybackState::Loading) => PlaybackPhase::Loading,
        }
    }

    fn decide(&self, input: TransportInput, selected: Option<QueueEntryId>) -> TransportDecision {
        decide(
            input,
            &TransportSituation {
                queue: self.session.state().queue(),
                selected,
                phase: self.phase(),
                last_requested: self.last_requested,
            },
        )
    }

    /// Whether the decision table would let a seek through right now — the
    /// gate on flushing a burst, so one never lands in a loading track.
    fn seeking_allowed(&self) -> bool {
        matches!(
            self.decide(TransportInput::SeekBy(0), None),
            TransportDecision::SeekBy(_)
        )
    }

    fn transport(&mut self, input: TransportInput, selected: Option<QueueEntryId>) {
        match self.decide(input, selected) {
            TransportDecision::Load(id) => self.load_entry(id),
            TransportDecision::TogglePause => self.route(PlaybackCommand::TogglePause),
            TransportDecision::Play => self.route(PlaybackCommand::Play),
            TransportDecision::SeekBy(step) => self.route(PlaybackCommand::SeekBy(step)),
            TransportDecision::Restart => self.route(PlaybackCommand::Restart),
            TransportDecision::SeekTo(target) => self.seek_to(target),
            TransportDecision::Notice(text) => self.status = Some(text.to_owned()),
            TransportDecision::Nothing => {}
        }
    }

    fn route(&mut self, command: PlaybackCommand) {
        let (Some(engine), Some(mirror)) = (&self.engine, &mut self.mirror) else {
            return;
        };
        let optimistic = self.router.route(
            engine,
            mirror.state == PlaybackState::Playing,
            mirror.position,
            mirror.duration,
            Instant::now(),
            command,
        );
        // A prediction until the seek lands, so it reaches only the display.
        if let Some(position) = optimistic {
            mirror.position = position;
            mirror.provenance = PositionProvenance::Estimated;
        }
    }

    /// An absolute seek into the loaded track. `decide` has already answered
    /// every unloaded, loading, ended and empty-queue case with its notice;
    /// a track whose length is unknown or that cannot seek gets no seek.
    fn seek_to(&mut self, target: Duration) {
        self.router.cancel();
        let seekable = self.mirror.as_ref().is_some_and(|mirror| {
            mirror.duration.is_some() && mirror.capabilities.seek != SeekSupport::Unsupported
        });
        if seekable
            && let Some(engine) = &self.engine
            && engine.submit_seek(target) != Admission::Accepted
        {
            self.status = Some(PLAYER_BUSY.to_owned());
        }
    }

    // ---------------------------------------------------------------- loads

    fn load_entry(&mut self, id: QueueEntryId) {
        self.load_entry_starting(id, |engine, request| {
            engine.submit(PlaybackCommand::PlayLoaded { request })
        });
    }

    /// `load_entry` with the automatic-start submission passed in, so a test
    /// can refuse it without filling the engine's command queue.
    fn load_entry_starting(
        &mut self,
        id: QueueEntryId,
        start: impl FnOnce(&EngineHandle, LoadRequestId) -> Admission,
    ) {
        let Some(entry) = self.session.state().queue().get(id) else {
            return;
        };
        let (media, source) = (entry.media().clone(), entry.source().clone());
        self.last_requested = Some(id);
        self.last_attempt = None;
        // A new attempt supersedes whatever the previous one reported.
        self.status = None;

        let location = match self.locate(id, &media, &source) {
            Ok(location) => location,
            Err(message) => {
                self.load_failed = true;
                self.status = Some(message);
                return;
            }
        };
        let request = match self.session.register_load(LoadTarget::Queue(id), &media) {
            Ok(request) => request,
            Err(RegisterLoadError::Busy) => {
                self.status = Some(TOO_MANY_PENDING_LOADS.to_owned());
                return;
            }
            Err(RegisterLoadError::UnknownEntry) => {
                self.status = Some(QueueError::UnknownEntry(id).to_string());
                return;
            }
            Err(RegisterLoadError::MediaMismatch) => {
                self.status = Some(QueueError::SourceMismatch.to_string());
                return;
            }
        };

        self.ensure_engine();
        if matches!(location, SourceLocation::Http(_))
            && let Err(error) = self.ensure_http()
        {
            self.session.retract_load(request);
            self.load_failed = true;
            self.status = Some(PlaybackError::from(error).to_string());
            return;
        }
        let Some(engine) = &self.engine else {
            self.session.retract_load(request);
            return;
        };
        let resume = self.session.resume_intent(&media);
        let admission = engine.submit(PlaybackCommand::Load {
            request,
            media,
            source: location,
            resume,
        });
        if admission != Admission::Accepted {
            self.session.retract_load(request);
            self.status = Some(PLAYER_BUSY.to_owned());
            return;
        }
        self.last_attempt = Some(request);
        self.router.cancel();
        self.load_failed = false;
        // Token-scoped: it starts only this load, and only once it opened.
        // Refused, the load still lands, paused; nothing retries the start
        // with an unrestricted `Play`.
        if start(engine, request) != Admission::Accepted {
            self.status = Some(PLAYER_BUSY.to_owned());
        }
    }

    /// Where `media` plays from right now. `Err` carries the message to show.
    fn locate(
        &mut self,
        id: QueueEntryId,
        media: &MediaId,
        source: &QueueSource,
    ) -> Result<SourceLocation, String> {
        let fallback = match source {
            QueueSource::LocalFile(path) => {
                return Ok(SourceLocation::LocalPath(path.as_path().to_path_buf()));
            }
            QueueSource::RemoteUrl(url) => {
                return Url::parse(url.as_str())
                    .map(SourceLocation::Http)
                    .map_err(|_| {
                        PlaybackError::from(RemoteFailure::InvalidSource {
                            input: redact_url(url.as_str()),
                            reason: "not a valid URL",
                        })
                        .to_string()
                    });
            }
            QueueSource::Podcast { fallback } => fallback,
        };
        let resolution = match &self.library {
            Some(library) => {
                resolve_podcast(&library.subscriptions, &library.cache, media, fallback)
                    .map_err(|error| error.to_string())?
            }
            None => PodcastResolution::Saved {
                location: SourceLocation::Http(fallback.clone()),
            },
        };
        match resolution {
            PodcastResolution::Current {
                location,
                refreshed_fallback,
            } => {
                if let Some(url) = refreshed_fallback {
                    match self.session.update_podcast_fallback(id, url) {
                        Ok(action) => self.submit(action),
                        Err(error) => {
                            tracing::warn!(%error, "cannot refresh a queued episode's source");
                        }
                    }
                }
                Ok(location)
            }
            PodcastResolution::Saved { location } => {
                self.status = Some(SAVED_SOURCE_NOTICE.to_owned());
                Ok(location)
            }
        }
    }

    fn ensure_engine(&mut self) {
        if self.engine.is_some() {
            return;
        }
        let engine = (self.engine_factory)();
        let _ = engine.submit(PlaybackCommand::SetVolume(self.session.state().volume()));
        if let Some(service) = &self.http {
            engine.set_http(Some(Arc::clone(service)));
        }
        self.engine = Some(engine);
    }

    fn ensure_http(&mut self) -> Result<(), RemoteFailure> {
        if self.http.is_some() {
            return Ok(());
        }
        let service = HttpService::spawn(self.http_limits)?;
        if let Some(engine) = &self.engine {
            engine.set_http(Some(Arc::clone(&service)));
        }
        self.http = Some(service);
        Ok(())
    }

    // --------------------------------------------------------------- events

    fn observe_event(&mut self, event: &PlaybackEvent) {
        // Decided before `observe` retires a registration or moves ownership.
        let accepted = self.session.accepts_media_event(event);
        let action = self.session.observe(event, self.clock.sample());
        self.submit(action);
        self.router.observe(event);

        if let Some(request) = event.load_outcome() {
            self.observe_load_outcome(event, request, accepted);
            return;
        }
        // Profile-wide: `Session` already took it; never the mirror's.
        if matches!(event, PlaybackEvent::VolumeChanged { .. }) || !accepted {
            return;
        }
        if let PlaybackEvent::Failed { message, .. } = event {
            self.status = Some(message.clone());
        }
        if let Some(mirror) = &mut self.mirror {
            mirror.apply(event);
        }
    }

    fn observe_load_outcome(
        &mut self,
        event: &PlaybackEvent,
        request: LoadRequestId,
        accepted: bool,
    ) {
        let latest = self.last_attempt == Some(request);
        let adopted = accepted
            && matches!(event, PlaybackEvent::Loaded { .. })
            && self
                .session
                .adopted()
                .is_some_and(|load| load.request == request);
        if adopted {
            self.mirror = Mirror::loaded(event);
            if latest {
                self.load_failed = false;
            }
            return;
        }
        // Any other outcome of a newer load means the worker already tore
        // the mirrored playback down to run that load.
        if self.mirror.as_ref().is_some_and(|mirror| {
            mirror.load != request && event.session_rev() >= mirror.session_rev
        }) {
            self.mirror = None;
        }
        if latest && let PlaybackEvent::Failed { message, .. } = event {
            self.load_failed = true;
            self.status = Some(message.clone());
        }
    }

    // ---------------------------------------------------------------- queue

    fn enqueue(&mut self, items: Vec<EnqueueItem>) {
        if items.is_empty() {
            return;
        }
        let mut batch = Vec::with_capacity(items.len());
        for item in items {
            match new_entry(item) {
                Ok(entry) => batch.push(entry),
                Err(message) => {
                    self.status = Some(message);
                    return;
                }
            }
        }
        match self.session.enqueue(batch) {
            Ok((_, action)) => self.submit(action),
            Err(QueueError::Capacity { .. }) => {
                self.status = Some(format!("Queue is full ({MAX_QUEUE_ENTRIES} entries)"));
            }
            Err(error) => self.status = Some(error.to_string()),
        }
    }

    fn remove(&mut self, id: QueueEntryId) {
        if self.session.state().queue().active() == Some(id) {
            self.router.cancel();
        }
        let progress = self.latest_progress();
        match self
            .session
            .remove_entry(id, &progress, self.clock.sample())
        {
            Ok(removal) => self.apply_removal(removal),
            Err(error) => self.status = Some(error.to_string()),
        }
    }

    fn clear_queue(&mut self) {
        self.router.cancel();
        let progress = self.latest_progress();
        let removal = self.session.clear_queue(&progress, self.clock.sample());
        self.apply_removal(removal);
    }

    fn apply_removal(&mut self, removal: Removal) {
        self.submit(removal.action);
        // Only playback the engine still holds for the released entry is
        // stopped: a displaced one is already gone, and stopping then would
        // cancel whatever newer load displaced it.
        if removal.stop_playback
            && self.mirror.is_some()
            && let Some(engine) = &self.engine
        {
            engine.interrupt_stop();
        }
        if self.session.adopted().is_none() {
            self.mirror = None;
        }
        self.selection_hint = removal.selection;
    }

    fn latest_progress(&self) -> Progress {
        match &self.engine {
            Some(engine) => engine.progress(),
            None => Progress {
                session_rev: 0,
                media: None,
                position: Duration::ZERO,
                quality: PositionQuality::Exact,
                provenance: PositionProvenance::Established,
                buffering: false,
                load: None,
            },
        }
    }

    fn adjust_volume(&mut self, delta: f32) {
        let target = self.volume.adjusted(delta);
        if target == self.volume {
            return;
        }
        match &self.engine {
            None => {
                self.volume = target;
                let action = self.session.set_volume(target);
                self.submit(action);
            }
            // `VolumeChanged` carries it into `Session`.
            Some(engine) => {
                if engine.submit(PlaybackCommand::SetVolume(target)) == Admission::Accepted {
                    self.volume = target;
                } else {
                    self.status = Some(PLAYER_BUSY.to_owned());
                }
            }
        }
    }

    // ----------------------------------------------------------------- view

    fn now_playing(&self, entry: &QueueEntry, state: &PersistedState) -> NowPlaying {
        let display = entry.display();
        let unloaded = NowPlaying {
            entry: Some(entry.id()),
            title: entry_title(entry),
            artist: display.artist.as_deref().map(displayable),
            album: display.album.as_deref().map(displayable),
            loaded: false,
            state: PlaybackState::Idle,
            position: Duration::ZERO,
            duration: display.duration,
            estimated_position: false,
            degraded: false,
            buffering: false,
            seek: None,
            saved: saved_history(state.entry_for(entry.media())),
            session_rev: 0,
            load: None,
        };
        let Some(mirror) = &self.mirror else {
            return unloaded;
        };
        NowPlaying {
            loaded: true,
            state: mirror.state,
            position: mirror.position,
            duration: mirror
                .duration
                .map(|value| DisplayDuration {
                    value,
                    source: DurationSource::Decoded(mirror.duration_provenance),
                })
                .or(display.duration),
            estimated_position: mirror.provenance == PositionProvenance::Estimated,
            degraded: mirror.quality == PositionQuality::Degraded,
            buffering: mirror.buffering,
            seek: Some(mirror.capabilities.seek),
            session_rev: mirror.session_rev,
            load: Some(mirror.load),
            ..unloaded
        }
    }

    fn submit(&self, action: Action) {
        if let Action::Submit { state, urgency } = action {
            self.writer.submit(state, urgency);
        }
    }
}

/// Resolves one enqueue item into a queue entry, or the message rejecting it.
fn new_entry(item: EnqueueItem) -> Result<NewQueueEntry, String> {
    let (media, display) = match item {
        EnqueueItem::Path(path) => (resolve_path(&path), DisplayMetadata::default()),
        EnqueueItem::Url(url) => (resolve_source(&url), DisplayMetadata::default()),
        EnqueueItem::Episode(candidate) => {
            let fallback = candidate.enclosure.ok_or_else(|| {
                crate::application::podcast::PodcastResolveError::NotPlayable.to_string()
            })?;
            let display = DisplayMetadata {
                title: candidate.title,
                duration: candidate.declared_duration.map(|value| DisplayDuration {
                    value,
                    source: DurationSource::Declared,
                }),
                ..DisplayMetadata::default()
            };
            return NewQueueEntry::new(candidate.media, QueueSource::Podcast { fallback }, display)
                .map_err(|error| error.to_string());
        }
    };
    let (media, _) = media.map_err(|error| error.to_string())?;
    let source = match &media {
        MediaId::LocalFile(path) => QueueSource::LocalFile(path.clone()),
        MediaId::RemoteUrl(url) => QueueSource::RemoteUrl(url.clone()),
        MediaId::PodcastEpisode { .. } => return Err(QueueError::SourceMismatch.to_string()),
    };
    NewQueueEntry::new(media, source, display).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use crate::persistence::writer::StateSink;
    use crate::playback::output::null_output::NullOutput;

    const FIVE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");

    struct NowhereSink;

    impl StateSink for NowhereSink {
        fn write(&self, _: &PersistedState) -> Result<(), PersistenceError> {
            Ok(())
        }
    }

    fn runtime() -> PlayerRuntime {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        PlayerRuntime::new(RuntimeParts {
            session: Session::new(PersistedState::default()),
            writer: WriterHandle::spawn(Box::new(NowhereSink), Arc::clone(&clock)),
            persisting: false,
            clock,
            engine_factory: Box::new(|| EngineHandle::spawn(Box::new(NullOutput::new()))),
            library: None,
            http_limits: Limits::default(),
        })
    }

    #[test]
    fn a_refused_automatic_start_leaves_the_loaded_track_paused_and_says_so() {
        let mut runtime = runtime();
        runtime.handle(AppCommand::Enqueue(vec![EnqueueItem::Path(FIVE.into())]));
        let id = runtime.view().rows[0].id;

        runtime.load_entry_starting(id, |_, _| Admission::Busy);
        assert_eq!(runtime.view().status.as_deref(), Some(PLAYER_BUSY));

        let deadline = Instant::now() + Duration::from_secs(20);
        while runtime.view().phase != PlaybackPhase::Paused {
            runtime.pump();
            assert!(
                Instant::now() < deadline,
                "never paused: {:?}",
                runtime.view()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let settle = Instant::now() + Duration::from_millis(300);
        while Instant::now() < settle {
            runtime.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        let view = runtime.view();
        assert_eq!(view.active, Some(id));
        assert_eq!(view.phase, PlaybackPhase::Paused, "{view:?}");
        assert_eq!(view.status.as_deref(), Some(PLAYER_BUSY));
        let _ = runtime.shutdown();
    }
}
