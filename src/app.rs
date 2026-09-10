//! The `continuo play` application: argument-to-source resolution, terminal
//! setup, and the key-driven status loop around [`EngineHandle`].

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, execute};
use url::Url;

use crate::cli::{self, CliCommand};
use crate::clock::{Clock, SystemClock};
use crate::http::channel::{SourceInterrupt, WaitHook};
use crate::http::error::{RemoteFailure, redact_url};
use crate::http::limits::Limits;
use crate::http::service::HttpService;
use crate::media::capabilities::{MediaCapabilities, SeekSupport};
use crate::media::id::{AbsolutePath, MediaId, NormalizedUrl};
use crate::media::source::SourceLocation;
use crate::persistence::PersistenceError;
use crate::persistence::model::{PersistedCheckpoint, PersistedState};
use crate::persistence::store::{LoadReason, StateStore};
use crate::persistence::writer::{ShutdownOutcome, StateSink, Urgency, WriterHandle};
use crate::playback::command::{Admission, PlaybackCommand, ResumeIntent};
use crate::playback::engine::EngineHandle;
use crate::playback::error::PlaybackError;
use crate::playback::event::PlaybackEvent;
use crate::playback::prepare::{PrepareContext, prepare};
use crate::playback::provenance::PositionProvenance;
use crate::playback::state::PlaybackState;
use crate::playback::timeline::PositionQuality;
use crate::playback::volume::Volume;
use crate::resume::{restart_preference, resume_candidate};
use crate::session::{Action, Session};

const SEEK_STEP_SECS: i64 = 10;
const VOLUME_STEP: f32 = 0.05;
const HELP_LINE: &str =
    "space pause · ←/→ seek 10s · Home restart · -/+ volume · s stop · p play · q quit";

/// Runs the parsed CLI to completion.
pub fn run(cli: cli::Cli) -> Result<(), PlaybackError> {
    let CliCommand::Play { source, probe_only } = cli.command;

    if probe_only {
        return run_probe_only(&source);
    }

    let (media, location) = resolve_source(&source)?;

    // Persistence opens before the engine: the resume candidate is an
    // argument to the load, and the restored volume is a command that
    // precedes it.
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let Persistence {
        mut session,
        writer,
        resume,
        volume,
        persisting,
    } = open_persistence(platform_store(&clock), &media, &clock);

    // Built before the engine spawns: `EngineHandle::set_http`'s default is
    // `None`, which fails every remote `Load` with "no HTTP service in this
    // session" — installing it before the first command reaches the worker
    // is what makes that failure mode unreachable for a source this CLI
    // itself resolved as remote.
    let http = match &location {
        SourceLocation::Http(_) => Some(HttpService::spawn(Limits::default())?),
        SourceLocation::LocalPath(_) => None,
    };

    let engine = EngineHandle::spawn_cpal();
    if let Some(service) = http {
        engine.set_http(Some(service));
    }

    for command in resume_commands(media, location, resume, volume) {
        engine.commands().send(command).ok();
    }

    // Entered only now (R5, Ruling 1): every fallible step above can still
    // fail before a single key is read, which is what keeps a rejected
    // source reportable with no raw terminal. `None` means there is no tty —
    // the CI case — and such a session simply reads no keys rather than
    // calling into crossterm, which has nothing to open and fails outright
    // rather than reporting "nothing ready".
    let raw = RawModeGuard::enable();
    let mut mirror = Mirror::default();

    // Raw mode does not translate `\n`. §5: "The application displays
    // Loading while preparation is in flight and remains able to stop or
    // quit" — this is the one line loop A renders, before the first `Loaded`
    // or `Failed` decides whether there is anything further to show.
    print!("Loading \u{2026}\r\n");
    let _ = std::io::stdout().flush();

    // Loop A: keys are read (Ctrl-C and `q` included) but nothing but the
    // line above is rendered. `Loaded` hands off to loop B; `Failed` or a
    // quit decides the run's outcome here, before loop B ever starts.
    let phase = loop {
        if handle_keys(&engine, &mirror, raw.is_some()) {
            break Phase::Done(Ok(()));
        }

        let mut failure = None;
        let mut loaded = false;
        while let Ok(event) = engine.events().try_recv() {
            if let PlaybackEvent::Failed { message, .. } = &event {
                failure = Some(message.clone());
            }
            loaded |= matches!(event, PlaybackEvent::Loaded { .. });
            // `observe` borrows the event, so the mirror still consumes it.
            submit(&writer, session.observe(&event, clock.sample()));
            mirror.apply(event);
        }
        if let Some(message) = failure {
            break Phase::Done(Err(PlaybackError::Failed(message)));
        }
        if loaded {
            break Phase::Loaded;
        }
    };

    let outcome = match phase {
        Phase::Done(outcome) => outcome,
        // Loop B: the existing key/render/checkpoint loop.
        Phase::Loaded => loop {
            if handle_keys(&engine, &mirror, raw.is_some()) {
                break Ok(());
            }

            let mut failure = None;
            while let Ok(event) = engine.events().try_recv() {
                if let PlaybackEvent::Failed { message, .. } = &event {
                    failure = Some(message.clone());
                }
                submit(&writer, session.observe(&event, clock.sample()));
                mirror.apply(event);
            }
            if let Some(message) = failure {
                break Err(PlaybackError::Failed(message));
            }

            // Render progress only when it belongs to the session the mirror is
            // showing. The keep-latest snapshot can otherwise overtake queued
            // lifecycle events and show one track's position under another's
            // title. A `Failed` event can carry a newer `session_rev` than the
            // snapshot published a tick earlier; the guard correctly skips
            // rendering the snapshot for that tick.
            let progress = engine.progress();
            submit(&writer, session.tick(&progress, clock.sample()));
            if progress.session_rev == mirror.session_rev {
                mirror.position = progress.position;
                mirror.quality = progress.quality;
                mirror.provenance = progress.provenance;
                mirror.buffering = progress.buffering;
            }
            // A terminal write failure is not a reason to skip the final
            // checkpoint, so it becomes the loop's outcome instead of returning
            // from here and bypassing the flush path (D18).
            if let Err(error) = render(&mirror) {
                break Err(error);
            }
        },
    };

    finish(engine, session, writer, &clock, raw, persisting, outcome)
}

/// What loop A decided: hand off to loop B once loaded, or the run is
/// already over (a quit, or a `Failed` before anything ever loaded).
enum Phase {
    Loaded,
    Done(Result<(), PlaybackError>),
}

/// Both loops break into this (Ruling 2): whichever loop produced `outcome`,
/// the shutdown sequence — interrupt, join, reconcile, restore the terminal,
/// flush — is one path rather than two, which is what keeps D18's "a
/// terminal write failure must not skip the final checkpoint" true
/// regardless of which loop hit it.
fn finish(
    engine: EngineHandle,
    mut session: Session,
    mut writer: WriterHandle,
    clock: &Arc<dyn Clock>,
    raw: Option<RawModeGuard>,
    persisting: bool,
    outcome: Result<(), PlaybackError>,
) -> Result<(), PlaybackError> {
    // Out of band first, and in band only as a courtesy. The worker stops
    // reading commands while an event backlog exists, and both loops have
    // just stopped draining events, so an in-band `Shutdown` can sit unread
    // in the channel forever while `join` blocks - hanging the process with
    // the terminal still in raw mode. The interrupt is the only signal that
    // is guaranteed to be seen.
    engine.interrupt_shutdown();
    engine.commands().send(PlaybackCommand::Shutdown).ok();
    let report = engine.join();

    // The events neither loop drained are replayed through the policy before
    // the snapshot is taken, so the snapshot comes from a session that has
    // seen everything the run produced (D19).
    writer.submit(
        session.reconcile_shutdown(&report, clock.sample()),
        Urgency::Forced,
    );

    // Restore the terminal before waiting on the disk, and before returning
    // to a caller that will print a diagnostic on `outcome` — so the writer's
    // bound is never spent, and nothing is ever printed, with the terminal
    // still raw (Ruling 1).
    drop(raw);
    report_flush(writer.shutdown(), persisting);
    outcome
}

/// One pass of key handling, shared by both loops. Returns whether the loop
/// must stop: an explicit quit, Ctrl-C (`to_command` already maps it to
/// `Shutdown`), or the input stream ending or failing.
///
/// With no raw terminal (`raw` false) there are no keys to read, and calling
/// into crossterm anyway does not answer "nothing ready" — with no tty to
/// open it fails outright (verified empirically against this crossterm
/// version), which would misreport a CI run with no controlling terminal as
/// someone having pressed `q`. Waiting out one tick and reporting nothing to
/// do is what actually matches "no keys", leaving the event drain in each
/// loop as the only thing such a session can still notice.
fn handle_keys(engine: &EngineHandle, mirror: &Mirror, raw: bool) -> bool {
    if !raw {
        std::thread::sleep(Duration::from_millis(100));
        return false;
    }
    match crossterm::event::poll(Duration::from_millis(100)) {
        Ok(true) => match crossterm::event::read() {
            Ok(Event::Key(key)) => match to_command(key, mirror) {
                Some(PlaybackCommand::Shutdown) => true,
                Some(command) => {
                    route_command(
                        engine,
                        mirror.state == PlaybackState::Playing,
                        mirror.position,
                        command,
                    );
                    false
                }
                None => false,
            },
            Ok(_) => false,
            // The input stream ended or failed; there is nothing left to
            // read keys from, so shut down as cleanly as `q` would.
            Err(_) => true,
        },
        Ok(false) => false,
        Err(_) => true,
    }
}

/// Sends one decoded key command to the `EngineHandle` action §8 actually
/// built for it — the out-of-band `submit_pause`/`submit_play`/`submit_seek`,
/// or the non-blocking `submit` for everything else — rather than the
/// blocking `commands().send` every command but `Stop`/`Shutdown` used to
/// travel on (IMPORTANT 2, final review). `Shutdown` never reaches here:
/// `handle_keys` decides to end the loop itself and has nothing left to route.
///
/// `pub`, alongside the rest of this crate's engine-facing surface
/// (`EngineHandle`, `PlaybackCommand`), so a test can drive the exact routing
/// a keypress takes with no tty and no crossterm event in the loop at all —
/// `handle_keys` itself cannot be driven headlessly, since
/// `crossterm::event::read()` needs a real terminal. `playing` and `position`
/// are the two `Mirror` fields this routing actually reads, taken separately
/// so `Mirror` itself can stay private.
pub fn route_command(
    engine: &EngineHandle,
    playing: bool,
    position: Duration,
    command: PlaybackCommand,
) {
    match command {
        // Loop control, decided by `handle_keys` itself before this is ever
        // called - nothing to route.
        PlaybackCommand::Shutdown => {}
        // Out of band, like `Shutdown`. The ordinary command queue stops
        // being read while an event backlog exists, and a queued Stop cannot
        // interrupt a refinement already running, so pressing `s` would not
        // stop anything when it matters most.
        PlaybackCommand::Stop => engine.interrupt_stop(),
        // `TogglePause`'s direction has to be decided here rather than left
        // for the worker's own `dispatch` to read off `self.state`: routing
        // through `submit_pause`/`submit_play` means picking one of the two
        // *before* it is queued, since only the one actually chosen also
        // freezes or thaws the source interrupt a blocked read is waiting on
        // (§9). The mirror is this thread's freshest view of which playback
        // means "toggle" answers to; a worker that has since moved on treats
        // the resulting `Pause`/`Play` as the no-op it already is for a state
        // it is not in; the freeze/thaw level is the part that actually has
        // to be right, and the mirror lags the worker by at most one drain
        // cycle - the same staleness every other read of it in this file
        // already lives with.
        PlaybackCommand::TogglePause => {
            report_admission(if playing {
                engine.submit_pause()
            } else {
                engine.submit_play()
            });
        }
        PlaybackCommand::Play => report_admission(engine.submit_play()),
        PlaybackCommand::Pause => report_admission(engine.submit_pause()),
        // An arrow-key seek is resolved to an absolute target here, against
        // the mirror's position, so it can travel through `submit_seek` -
        // the one path that publishes the SEEK bit and retires the fetch a
        // stale read would otherwise keep running against (IMPORTANT 2).
        PlaybackCommand::SeekBy(delta) => {
            report_admission(engine.submit_seek(seek_target(position, delta)));
        }
        other => report_admission(engine.submit(other)),
    }
}

/// The absolute target an arrow-key seek asks for. `submit_seek` takes a
/// `Duration`, not a delta, so this is the same clamp-at-zero arithmetic
/// `engine.rs`'s own `SeekBy` dispatch performs, computed here instead
/// against the mirror's position now that the CLI resolves the target rather
/// than handing the worker a signed step to resolve against `self.position`.
fn seek_target(position: Duration, delta: i64) -> Duration {
    let step = Duration::from_secs(delta.unsigned_abs());
    if delta >= 0 {
        position.saturating_add(step)
    } else {
        position.saturating_sub(step)
    }
}

/// §8: queue saturation must be visible, never silently dropped. `Gone`
/// means the worker has already shut down - nothing to warn about, since the
/// run is ending anyway.
fn report_admission(admission: Admission) {
    if admission == Admission::Busy {
        tracing::warn!("command queue is busy; the key press had no effect");
    }
}

/// §5's disambiguation. An explicit http/https scheme is a URL; everything
/// else keeps existing path behaviour, so `./https:weird` remains an
/// unambiguous local spelling.
fn resolve_source(input: &str) -> Result<(MediaId, SourceLocation), PlaybackError> {
    if is_url_spelling(input) {
        return resolve_url(input);
    }
    let path = PathBuf::from(input);
    let canonical = path.canonicalize().map_err(|source| PlaybackError::Open {
        path: path.clone(),
        source,
    })?;
    let absolute =
        AbsolutePath::new(canonical.clone()).map_err(|error| PlaybackError::UnsupportedInput {
            path: path.clone(),
            reason: error.to_string(),
        })?;
    Ok((
        MediaId::LocalFile(absolute),
        SourceLocation::LocalPath(canonical),
    ))
}

/// Only an explicit prefix counts, ASCII case-insensitively: `./https:weird`
/// does not start with either spelling, so it is unaffected, and there is no
/// looser check anywhere else that could make it one.
fn is_url_spelling(input: &str) -> bool {
    let starts_with_ci = |prefix: &str| {
        input
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    };
    starts_with_ci("http://") || starts_with_ci("https://")
}

fn resolve_url(input: &str) -> Result<(MediaId, SourceLocation), PlaybackError> {
    // Ruling 5: every `RemoteFailure` built from user input here carries an
    // already-redacted URL — the raw text may itself be the secret (a
    // malformed URL that embedded a token, say), so it is never echoed back.
    let invalid = |reason: &'static str| -> PlaybackError {
        RemoteFailure::InvalidSource {
            input: redact_url(input),
            reason,
        }
        .into()
    };
    let url = Url::parse(input).map_err(|_| invalid("not a valid URL"))?;
    // §5: no implicit credential feature. Rejected here, before identity is
    // ever built from it, rather than left for the fetch to refuse later.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("URLs with embedded credentials are not supported"));
    }
    let normalized =
        NormalizedUrl::parse(input).map_err(|_| invalid("expected http(s) with a host"))?;
    // The parsed `Url` is kept separately as the fetch target — its query
    // stays whole, and a redirect changes it without ever touching identity.
    Ok((MediaId::RemoteUrl(normalized), SourceLocation::Http(url)))
}

/// §5/H15: opens and classifies `source` on the calling thread. No
/// `EngineHandle`, no `AudioOutput` and no `StateStore` are constructed —
/// this reads no playback state and writes none.
fn run_probe_only(source: &str) -> Result<(), PlaybackError> {
    let (_, location) = resolve_source(source)?;
    let http = match &location {
        SourceLocation::Http(_) => Some(HttpService::spawn(Limits::default())?),
        SourceLocation::LocalPath(_) => None,
    };
    let context = PrepareContext {
        http,
        interrupt: SourceInterrupt::new(Limits::default().buffer_bytes),
        hook: Arc::new(InertHook),
        limits: Limits::default(),
    };
    let mut prepared = prepare(&location, &context)?;

    // Preparation alone stops at `SeekSupport::Unknown` for every remote
    // source (§6): performing the trial seek here, before printing, is what
    // lets the probe tell "unresolved" from "unsupported" apart. A probe that
    // printed `Unknown` would only be reporting its own incuriosity as a
    // property of the recording.
    if prepared.capabilities.seek == SeekSupport::Unknown {
        let seeked = prepared
            .source
            .seek_refined(Duration::ZERO, None, &mut || false)
            .is_ok();
        if seeked {
            prepared.source.note_demuxer_proven();
            prepared.capabilities = prepared.source.capabilities();
        }
    }

    let title = prepared
        .source
        .metadata()
        .title
        .clone()
        .unwrap_or_else(|| "(untitled)".to_string());
    println!(
        "{title} {rate} Hz {channels} ch {duration:?} continuity={continuity:?} seek={seek:?} resume={resume:?}",
        rate = prepared.source.sample_rate(),
        channels = prepared.source.channels(),
        duration = prepared.source.metadata().duration,
        continuity = prepared.capabilities.continuity,
        seek = prepared.capabilities.seek,
        resume = prepared.capabilities.resume_capability(),
    );
    Ok(())
}

/// A `WaitHook` with nothing to do. `--probe-only` runs `prepare` on the
/// calling thread with no worker behind it, so the hook a blocked read would
/// service has no progress to publish and no freeze to act on.
struct InertHook;

impl WaitHook for InertHook {
    fn service(&self) {}
}

/// §11's initial command sequence. Volume first: the engine accepts it with no
/// transport, and a transport created later adopts the stored gain — so the
/// restored level is in force from the first buffer rather than after it.
/// Nothing about the sequence is conditional; a session with no stored
/// candidate issues the same `Load`, with a start of zero.
fn resume_commands(
    media: MediaId,
    source: SourceLocation,
    resume: Option<ResumeIntent>,
    volume: Volume,
) -> [PlaybackCommand; 3] {
    // No entry is not itself a resume intent: the worker would decide
    // `NoEntry` from an absent `Candidate` anyway (§11), so this is the same
    // outcome without asking the worker to resolve one that was never
    // there. `resume_intent_for` has already decided, for whatever entry
    // there was, between `Candidate` (§11, unchanged) and
    // `EstimatedCandidate` (§4.3) — this function's only job left is the
    // "nothing at all" case.
    let resume = resume.unwrap_or(ResumeIntent::StartAt(Duration::ZERO));
    [
        PlaybackCommand::SetVolume(volume),
        PlaybackCommand::Load {
            media,
            source,
            resume,
        },
        PlaybackCommand::Play,
    ]
}

/// Writing is off for this session — an unsupported file, a quarantine that
/// could not be performed, or no state directory at all. The session runs
/// normally with in-memory state; only the disk write is suppressed, and the
/// reason has already been logged once (D3).
struct DisabledSink;

impl StateSink for DisabledSink {
    fn write(&self, _state: &PersistedState) -> Result<(), PersistenceError> {
        Ok(())
    }
}

struct Persistence {
    session: Session,
    writer: WriterHandle,
    /// The resume intent built from the stored entry for this media,
    /// unresolved against a duration — `resume_intent_for` (§4.2, §4.3)
    /// already decided between an established candidate and an estimated
    /// one, but a `Candidate`'s own position still needs a duration to
    /// validate against, and only the worker's own decode probe has one
    /// (Ruling 5). So `open_persistence` hands this onward rather than
    /// deciding a start position itself, which would mean opening the media
    /// twice for the same answer `decide_resume` gives either time.
    resume: Option<ResumeIntent>,
    volume: Volume,
    /// Whether anything this session submits can reach the disk. A disabled
    /// sink reports every write as a success, deliberately — the writer must
    /// not count a disable as a failure (D11) — so this is what keeps the
    /// shutdown log from claiming a write that never happened.
    persisting: bool,
}

/// The store on the platform's state path, or `None` when the platform offers
/// no state directory at all. Path discovery is kept out of `open_persistence`
/// so that everything downstream of it — the load classification and the sink
/// selection — can be driven from a store in a tempdir, and so that
/// `platform_path` keeps exactly one caller in the program (§13).
fn platform_store(clock: &Arc<dyn Clock>) -> Option<StateStore> {
    match StateStore::platform_path() {
        Ok(path) => Some(StateStore::new(path, Arc::clone(clock))),
        Err(error) => {
            tracing::warn!(%error, "no state directory; this session will not be persisted");
            None
        }
    }
}

/// Builds the worker-facing resume intent from a stored checkpoint entry,
/// §4.3's estimated-preference rule folded in beside the established path
/// left unchanged.
///
/// A completed entry never reaches `restart_preference` — its own doc says
/// so: the caller's concern, and calling it anyway would let a stray
/// estimate stored before completion redirect a replay that D1 already
/// says starts over at zero regardless. So a completed entry always goes
/// through `resume_candidate` exactly as it did before this function
/// existed, and only a live, uncompleted entry's `estimated` field is ever
/// consulted.
///
/// This is where §4.3 actually gets wired into the load path (Task 6's fix
/// round 1): `restart_preference`'s pure preference decision — implemented
/// and tested since Task 6 itself — had no production caller until this
/// function. `decide_resume` is deliberately not consulted here for the
/// estimate branch: §4.3 is a preference between two already-known
/// locations, not a duration-validated choice, and running the estimate
/// through duration validation would be inventing a rule the design doc
/// does not state. `decide_resume` keeps governing the established
/// position exactly as before wherever that path is actually taken — the
/// `resume_candidate` branch below, reached whenever there is no estimate
/// to prefer.
fn resume_intent_for(entry: Option<&PersistedCheckpoint>) -> Option<ResumeIntent> {
    let entry = entry?;
    if entry.completed {
        return resume_candidate(entry.position, entry.completed).map(ResumeIntent::Candidate);
    }
    match restart_preference(entry.position, entry.estimated) {
        Some(preference) => Some(ResumeIntent::EstimatedCandidate {
            target: preference.target,
            established: preference.established,
        }),
        None => resume_candidate(entry.position, entry.completed).map(ResumeIntent::Candidate),
    }
}

fn open_persistence(
    store: Option<StateStore>,
    media: &MediaId,
    clock: &Arc<dyn Clock>,
) -> Persistence {
    let (state, writable) = match &store {
        Some(store) => {
            let outcome = store.load();
            match &outcome.reason {
                LoadReason::Loaded => tracing::debug!(path = ?store.path(), "state restored"),
                LoadReason::Missing => tracing::debug!(path = ?store.path(), "no state yet"),
                LoadReason::Quarantined { moved_to } => {
                    tracing::warn!(
                        ?moved_to,
                        "state file was unreadable and has been moved aside"
                    );
                }
                LoadReason::QuarantineFailed => {
                    tracing::warn!(
                        "state file is unreadable and could not be moved aside; not writing"
                    );
                }
                LoadReason::UnsupportedVersion { found } => {
                    tracing::warn!(
                        found,
                        "state file is from a newer build; preserving it and not writing"
                    );
                }
                LoadReason::Unreadable => {
                    tracing::warn!("state file could not be read; preserving it and not writing");
                }
            }
            (outcome.state, outcome.writable)
        }
        None => (PersistedState::default(), false),
    };

    // No `ResumeDecision` is logged here any more: the decision needs a
    // duration, this call site has none, and logging one taken with
    // `duration: None` would misreport an ordinary resume as `Unvalidated`
    // every time. The disposition the worker reports on `Loaded` is what a
    // later task logs instead (Ruling 5).
    // `resume_intent_for` (§4.2, §4.3): a freshly loaded entry may carry
    // only an estimate with no established position at all, and that case
    // must not collapse into a fabricated `AtStart` — and, since Task 6's
    // fix round 1, an entry whose `estimated` field wins the §4.3
    // preference is resolved to `ResumeIntent::EstimatedCandidate` here
    // rather than the plain `Candidate` `resume_candidate` alone would
    // build.
    let resume = resume_intent_for(state.entry_for(media));
    let volume = state.volume();
    let sink: Box<dyn StateSink> = match (store, writable) {
        (Some(store), true) => Box::new(store),
        _ => Box::new(DisabledSink),
    };

    Persistence {
        session: Session::new(state),
        writer: WriterHandle::spawn(sink, Arc::clone(clock)),
        resume,
        volume,
        persisting: writable,
    }
}

fn submit(writer: &WriterHandle, action: Action) {
    if let Action::Submit { state, urgency } = action {
        writer.submit(state, urgency);
    }
}

/// What the flush is reported as, decided apart from the logging so that the
/// one branch that exists to prevent a dishonest line can be asserted rather
/// than read.
enum FlushReport {
    Written,
    Failed(PersistenceError),
    Unconfirmed,
    /// Nothing was ever going to reach the disk this session.
    Disabled,
}

/// A disabled sink reports every write as a success, deliberately — the writer
/// must not count a deliberate disable as a failure (D11) — so a session that
/// was not persisting reaches `Written` having written nothing. The outcome
/// alone must therefore never be reported as a checkpoint that landed.
fn classify_flush(outcome: ShutdownOutcome, persisting: bool) -> FlushReport {
    if !persisting {
        return FlushReport::Disabled;
    }
    match outcome {
        ShutdownOutcome::Written => FlushReport::Written,
        ShutdownOutcome::Failed(error) => FlushReport::Failed(error),
        ShutdownOutcome::Unconfirmed => FlushReport::Unconfirmed,
    }
}

fn report_flush(outcome: ShutdownOutcome, persisting: bool) {
    match classify_flush(outcome, persisting) {
        FlushReport::Written => tracing::debug!("final checkpoint written"),
        FlushReport::Failed(error) => tracing::warn!(%error, "final checkpoint failed"),
        FlushReport::Unconfirmed => tracing::warn!("final checkpoint UNCONFIRMED"),
        FlushReport::Disabled => {
            tracing::debug!("persistence is disabled for this session; no checkpoint was written");
        }
    }
}

/// Installs raw mode and restores it on drop. `finish` drops it explicitly, so
/// the writer's shutdown bound is never spent with the terminal still raw; the
/// `Drop` covers a panic, which is the only way out of either loop that does
/// not reach that line.
struct RawModeGuard;

impl RawModeGuard {
    /// `None` when there is no controlling terminal — `enable_raw_mode` fails
    /// for want of a tty, which is the CI case this type exists to keep out
    /// of raw-mode restoration's way (Ruling 1). Such a session reads no
    /// keys; `Loading` and any failure still print, and nothing here is left
    /// toggled for `finish` to restore.
    fn enable() -> Option<Self> {
        match crossterm::terminal::enable_raw_mode() {
            Ok(()) => Some(Self),
            Err(error) => {
                tracing::debug!(%error, "no controlling terminal; running without raw mode");
                None
            }
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// The application's read-only view of playback, rebuilt from the lossless
/// event stream and refreshed from the keep-latest progress snapshot.
struct Mirror {
    session_rev: u64,
    name: Option<String>,
    duration: Option<Duration>,
    state: PlaybackState,
    position: Duration,
    quality: PositionQuality,
    /// Whether `position` is decoder-established or a byte-offset estimate
    /// (§3), read from `Progress`/`SeekCompleted` — never derived from
    /// `quality`, which is an unrelated fact.
    provenance: PositionProvenance,
    volume: Volume,
    /// §11: carried whole, rather than as a bare `SeekSupport`, so
    /// `status_line` can tell "unresolved" from "unsupported" apart. `None`
    /// until the first `Loaded`.
    capabilities: Option<MediaCapabilities>,
    /// True exactly while a source read is blocked on the network. A detail
    /// of `Playing`, never a state of its own (§11) — `status_line` is the
    /// only place this is read.
    buffering: bool,
}

impl Default for Mirror {
    fn default() -> Self {
        Self {
            session_rev: 0,
            name: None,
            duration: None,
            state: PlaybackState::Idle,
            position: Duration::ZERO,
            quality: PositionQuality::Exact,
            provenance: PositionProvenance::Established,
            volume: Volume::default(),
            capabilities: None,
            buffering: false,
        }
    }
}

impl Mirror {
    fn apply(&mut self, event: PlaybackEvent) {
        match event {
            PlaybackEvent::Loaded {
                session_rev,
                media,
                metadata,
                capabilities,
                position,
                ..
            } => {
                self.session_rev = session_rev;
                self.name = Some(display_name(&media));
                self.duration = metadata.duration;
                self.capabilities = Some(capabilities);
                self.position = position;
                self.quality = PositionQuality::Exact;
                // `Loaded` does not carry its own provenance field (§3's
                // interfaces are scoped to `Progress`, `SeekCompleted` and
                // `MediaMetadata::duration`), so this hardcodes `Established`
                // even for a `ResumeIntent::EstimatedCandidate` launch, which
                // runs `seek_bounded` for the resume and can genuinely land
                // `Estimated` (`src/playback/engine.rs`'s resume-seek arm).
                // The mark this drives (` ~est`) is one tick late in that
                // case: `Progress` corrects `self.provenance` right after
                // (`:160` below), so the window is a single progress
                // interval, display-only — not fixed here for its own sake.
                self.provenance = PositionProvenance::Established;
                self.state = PlaybackState::Loading;
                // MINOR (final review): a fresh load starts with nothing
                // buffering. Display-only and unreachable under one load per
                // run, but leaving a stale `true` standing is a real bug,
                // not only a limitation.
                self.buffering = false;
            }
            PlaybackEvent::StateChanged { session_rev, state } => {
                self.session_rev = session_rev;
                self.state = state;
            }
            PlaybackEvent::SeekCompleted {
                session_rev,
                actual,
                ..
            } => {
                self.session_rev = session_rev;
                self.position = actual;
            }
            PlaybackEvent::SeekTargetStored {
                session_rev,
                target,
            } => {
                self.session_rev = session_rev;
                self.position = target;
            }
            PlaybackEvent::EndOfTrack {
                session_rev,
                position,
                ..
            } => {
                self.session_rev = session_rev;
                self.position = position;
                self.state = PlaybackState::Ended;
            }
            PlaybackEvent::VolumeChanged {
                session_rev,
                volume,
            } => {
                self.session_rev = session_rev;
                self.volume = volume;
            }
            // G1: the only one of these three the mirror has anything to show
            // for. `restart()` lands at zero with no `SeekCompleted`, so this
            // is where the mirror's position learns it landed at all.
            PlaybackEvent::RestartEstablished {
                session_rev,
                position,
                ..
            } => {
                self.session_rev = session_rev;
                self.position = position;
            }
            // §11: evidence that arrived after `Loaded` — an on-demand seek
            // probe resolving `Unknown` — updates the same field `Loaded`
            // itself seeds, so `status_line` reads the promotion to `Native`
            // the moment it is announced rather than only on the next load.
            PlaybackEvent::CapabilitiesChanged {
                session_rev,
                capabilities,
            } => {
                self.session_rev = session_rev;
                self.capabilities = Some(capabilities);
            }
            PlaybackEvent::DeviceRecovered { session_rev }
            | PlaybackEvent::SeekRejected { session_rev, .. }
            | PlaybackEvent::SeekCancelled { session_rev, .. }
            | PlaybackEvent::Warning { session_rev, .. }
            | PlaybackEvent::Failed { session_rev, .. } => {
                self.session_rev = session_rev;
            }
        }
    }
}

fn display_name(media: &MediaId) -> String {
    match media {
        MediaId::LocalFile(path) => path
            .as_path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| media.to_string()),
        MediaId::RemoteUrl(url) => remote_display_name(url.as_str()),
        other => other.to_string(),
    }
}

/// §11: a status line must never carry a signed query or embedded
/// credentials, so this is built from `redact_url`'s output rather than the
/// URL itself — the last path segment, or the redacted host when there is
/// none.
fn remote_display_name(url: &str) -> String {
    let redacted = redact_url(url);
    match Url::parse(&redacted) {
        Ok(parsed) => parsed
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
            .or_else(|| parsed.host_str().map(str::to_string))
            .unwrap_or(redacted),
        Err(_) => redacted,
    }
}

fn to_command(key: KeyEvent, mirror: &Mirror) -> Option<PlaybackCommand> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(PlaybackCommand::Shutdown);
    }
    match key.code {
        KeyCode::Char(' ') => Some(PlaybackCommand::TogglePause),
        KeyCode::Left => Some(PlaybackCommand::SeekBy(-SEEK_STEP_SECS)),
        KeyCode::Right => Some(PlaybackCommand::SeekBy(SEEK_STEP_SECS)),
        KeyCode::Home => Some(PlaybackCommand::Restart),
        // `+` sits behind Shift on the `=` key on most layouts, while `-` does
        // not, so binding only `+` makes the two directions asymmetric to press.
        // Accept the unshifted and shifted spelling of each.
        KeyCode::Char('-' | '_') => Some(PlaybackCommand::SetVolume(
            mirror.volume.adjusted(-VOLUME_STEP),
        )),
        KeyCode::Char('+' | '=') => Some(PlaybackCommand::SetVolume(
            mirror.volume.adjusted(VOLUME_STEP),
        )),
        KeyCode::Char('s') => Some(PlaybackCommand::Stop),
        KeyCode::Char('p') => Some(PlaybackCommand::Play),
        KeyCode::Char('q') => Some(PlaybackCommand::Shutdown),
        _ => None,
    }
}

fn render(mirror: &Mirror) -> Result<(), PlaybackError> {
    let mut out = std::io::stdout();
    execute!(
        out,
        cursor::MoveToColumn(0),
        Clear(ClearType::CurrentLine),
        Print(status_line(mirror)),
        Print("\r\n"),
        Clear(ClearType::CurrentLine),
        Print(HELP_LINE),
        cursor::MoveToColumn(0),
        cursor::MoveUp(1),
    )?;
    Ok(())
}

fn status_line(mirror: &Mirror) -> String {
    let name = mirror.name.as_deref().unwrap_or("(no media)");
    let position = format_hms(mirror.position);
    // Two independent marks for two independent facts (§3): a degraded
    // quality says the played-so-far estimate may be off, while `~est` says
    // the absolute position itself was never decoder-confirmed. Neither
    // implies the other, so both may appear together.
    let mut suffix = String::new();
    if mirror.quality == PositionQuality::Degraded {
        suffix.push_str(" ~");
    }
    if mirror.provenance == PositionProvenance::Estimated {
        suffix.push_str(" ~est");
    }
    let duration = mirror
        .duration
        .map(format_hms)
        .unwrap_or_else(|| "--:--:--".to_string());
    // §11: unresolved and unsupported are different facts about the same
    // `SeekSupport`, and an HTTP transport must never be reported in a way
    // that reads as live radio — neither note ever replaces the duration
    // fallback above, which stays exactly what it always meant: an unknown
    // duration, nothing about seeking.
    let seek_note = match mirror.capabilities.map(|capabilities| capabilities.seek) {
        Some(SeekSupport::Unknown) => " seek?",
        Some(SeekSupport::Unsupported) => " no-seek",
        _ => "",
    };
    let mut label = mirror.state.label().to_string();
    // A detail of Playing, never a state of its own (§11): this never grows
    // a fifth word beside idle/loading/playing/paused/stopped/ended/failed.
    if mirror.buffering && mirror.state == PlaybackState::Playing {
        label.push_str(" buffering");
    }
    format!(
        "{name} [{label}]{seek_note} {position}{suffix} / {duration}  vol {}%",
        mirror.volume.percent(),
    )
}

fn format_hms(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use crate::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
    use crate::media::metadata::MediaMetadata;
    use crate::playback::checkpoint::PlaybackCheckpoint;
    use crate::playback::event::StartDisposition;
    use crate::resume::ResumeCandidate;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    /// Default volume is `FULL`, and `adjusted` clamps at 1.0, so a raise test
    /// has to start below full or it measures the clamp instead of the binding.
    fn volume_after(code: KeyCode, from: f32) -> Option<f32> {
        let mirror = Mirror {
            volume: Volume::new(from),
            ..Mirror::default()
        };
        match to_command(press(code), &mirror) {
            Some(PlaybackCommand::SetVolume(volume)) => Some(volume.as_gain()),
            _ => None,
        }
    }

    #[test]
    fn volume_up_does_not_require_shift() {
        // `+` shares a key with `=` on most layouts, so binding only `+` makes
        // turning the volume up need Shift while turning it down does not.
        let baseline = 0.5;
        let shifted = volume_after(KeyCode::Char('+'), baseline).expect("+ raises volume");
        let unshifted = volume_after(KeyCode::Char('='), baseline).expect("= raises volume");
        assert_eq!(shifted, unshifted);
        assert!(unshifted > baseline);
    }

    #[test]
    fn volume_down_accepts_both_spellings_of_its_key() {
        let baseline = 0.5;
        let unshifted = volume_after(KeyCode::Char('-'), baseline).expect("- lowers volume");
        let shifted = volume_after(KeyCode::Char('_'), baseline).expect("_ lowers volume");
        assert_eq!(shifted, unshifted);
        assert!(unshifted < baseline);
    }

    #[test]
    fn the_two_directions_are_symmetric_to_press() {
        // Whatever raises volume must be reachable with the same effort as what
        // lowers it: an unshifted key exists for each.
        assert!(volume_after(KeyCode::Char('='), 0.5).is_some());
        assert!(volume_after(KeyCode::Char('-'), 0.5).is_some());
    }

    fn local(path: &str) -> MediaId {
        match AbsolutePath::new(path.into()) {
            Ok(path) => MediaId::LocalFile(path),
            Err(error) => panic!("a literal absolute path must parse: {error}"),
        }
    }

    /// The user-visible half of a resume: the listener sees the restored
    /// position the moment the track opens, not after the first progress tick.
    /// `Loaded` is the only event that carries it.
    #[test]
    fn a_resumed_position_is_shown_as_soon_as_the_track_opens() {
        let mut mirror = Mirror::default();
        mirror.apply(PlaybackEvent::Loaded {
            session_rev: 3,
            media: local("/music/sonata.flac"),
            metadata: MediaMetadata {
                title: None,
                duration: Some(Duration::from_secs(300)),
                duration_provenance: PositionProvenance::Established,
            },
            capabilities: MediaCapabilities {
                continuity: Continuity::Finite,
                seek: SeekSupport::Native,
            },
            position: Duration::from_secs(93),
            disposition: StartDisposition::Resumed,
        });

        assert_eq!(mirror.position, Duration::from_secs(93));
        assert_eq!(mirror.quality, PositionQuality::Exact);
        assert!(
            status_line(&mirror).contains("00:01:33"),
            "the resumed position is on the first line drawn, not 00:00:00: {}",
            status_line(&mirror)
        );
    }

    /// §11: the restored level has to be in force from the first buffer, which
    /// is only true if the volume command precedes the load. Pinned on the
    /// sequence itself — two commands the engine applied in order leave no
    /// trace of that order in the events it emits, so nothing downstream can
    /// check this.
    #[test]
    fn the_resume_sequence_restores_volume_before_it_loads() {
        let candidate = ResumeCandidate {
            position: Duration::from_secs(93),
            completed: false,
        };
        let commands = resume_commands(
            local("/music/sonata.flac"),
            SourceLocation::LocalPath("/music/sonata.flac".into()),
            Some(ResumeIntent::Candidate(candidate)),
            Volume::new(0.25),
        );

        match &commands {
            [
                PlaybackCommand::SetVolume(volume),
                PlaybackCommand::Load { resume, .. },
                PlaybackCommand::Play,
            ] => {
                assert_eq!(*volume, Volume::new(0.25), "the stored gain, unchanged");
                assert_eq!(
                    *resume,
                    ResumeIntent::Candidate(candidate),
                    "the persisted candidate, carried onward for the worker to resolve"
                );
            }
            other => panic!("volume must be issued before the load: {other:?}"),
        }
    }

    /// A store in a tempdir. Nothing in these tests reaches `$HOME`:
    /// `platform_path` is called by `run` and by nothing else, which is exactly
    /// what hoisting it out of `open_persistence` buys.
    fn store_at(path: &std::path::Path) -> (StateStore, Arc<dyn Clock>) {
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        (
            StateStore::new(path.to_path_buf(), Arc::clone(&clock)),
            clock,
        )
    }

    #[test]
    fn a_stored_entry_becomes_the_resume_candidate_and_the_restored_volume() {
        let dir = tempfile::tempdir().unwrap();
        let (store, clock) = store_at(&dir.path().join("state.json"));
        let media = local("/music/sonata.flac");
        let mut stored = PersistedState::default();
        stored.set_volume(Volume::new(0.25));
        stored.record(
            &PlaybackCheckpoint {
                media: media.clone(),
                position: Duration::from_secs(93),
                updated_at: clock.sample().wall,
            },
            false,
        );
        store.write(&stored).unwrap();

        let persistence = open_persistence(Some(store), &media, &clock);

        assert_eq!(
            persistence.resume,
            Some(ResumeIntent::Candidate(ResumeCandidate {
                position: Duration::from_secs(93),
                completed: false,
            })),
            "the entry the file held, unresolved — only the worker's probe has a duration"
        );
        assert_eq!(persistence.volume, Volume::new(0.25));
        assert!(persistence.persisting);
    }

    /// §4.3 (Task 6 fix round 1): an entry carrying both an `estimated`
    /// location and its established fallback resolves to
    /// `ResumeIntent::EstimatedCandidate`, preferring the estimate and
    /// keeping the established position in reserve. Ablation: a
    /// `resume_intent_for` that never calls `restart_preference` (the
    /// pre-fix-round state) makes this fail — `persistence.resume` would
    /// read `Some(ResumeIntent::Candidate(..))` instead.
    #[test]
    fn a_stored_estimate_and_its_established_fallback_resolve_to_an_estimated_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let (store, clock) = store_at(&dir.path().join("state.json"));
        let media = local("/music/sonata.flac");
        let mut stored = PersistedState::default();
        stored.record(
            &PlaybackCheckpoint {
                media: media.clone(),
                position: Duration::from_secs(40),
                updated_at: clock.sample().wall,
            },
            false,
        );
        stored.record_estimated(
            media.clone(),
            Duration::from_secs(97),
            clock.sample().wall,
            false,
        );
        store.write(&stored).unwrap();

        let persistence = open_persistence(Some(store), &media, &clock);

        assert_eq!(
            persistence.resume,
            Some(ResumeIntent::EstimatedCandidate {
                target: Duration::from_secs(97),
                established: Some(Duration::from_secs(40)),
            })
        );
    }

    /// R8: an entry that only ever carried an estimate must resolve with
    /// `established: None`, never a fabricated zero. Ablation: the same as
    /// above, plus — a `resume_intent_for` that reads an absent `position`
    /// as `Duration::ZERO` (the exact loss `resume_candidate` was written to
    /// avoid on the established side) would make this fail on the
    /// `established` field alone while the sibling test above still passes.
    #[test]
    fn a_stored_estimate_with_no_established_position_resolves_with_no_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let (store, clock) = store_at(&dir.path().join("state.json"));
        let media = local("/music/sonata.flac");
        let mut stored = PersistedState::default();
        stored.record_estimated(
            media.clone(),
            Duration::from_secs(97),
            clock.sample().wall,
            false,
        );
        store.write(&stored).unwrap();

        let persistence = open_persistence(Some(store), &media, &clock);

        assert_eq!(
            persistence.resume,
            Some(ResumeIntent::EstimatedCandidate {
                target: Duration::from_secs(97),
                established: None,
            })
        );
    }

    /// A completed entry must ignore a stray `estimated` field entirely: D1
    /// still resumes it through the ordinary `Candidate`/`CompletedReplay`
    /// path, not `EstimatedCandidate`. `resume_intent_for`'s own doc says a
    /// completed entry never reaches `restart_preference` — this is the
    /// test that would fail if that guard were removed (a `completed`
    /// entry's `resume` would read `EstimatedCandidate` instead of
    /// `Candidate`).
    #[test]
    fn a_completed_entry_ignores_a_stray_estimate_and_stays_an_ordinary_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let (store, clock) = store_at(&dir.path().join("state.json"));
        let media = local("/music/sonata.flac");
        let mut stored = PersistedState::default();
        stored.record_estimated(
            media.clone(),
            Duration::from_secs(97),
            clock.sample().wall,
            true,
        );
        store.write(&stored).unwrap();

        let persistence = open_persistence(Some(store), &media, &clock);

        assert_eq!(
            persistence.resume,
            Some(ResumeIntent::Candidate(ResumeCandidate {
                position: Duration::ZERO,
                completed: true,
            }))
        );
    }

    /// The sink selection is the whole feature in one line: swap the store for
    /// `DisabledSink` and persistence silently never writes again. Nothing else
    /// would notice — every other writer test drives a sink of its own — so this
    /// is the one test that follows a submitted snapshot all the way to the
    /// bytes on disk, through `impl StateSink for StateStore` and through the
    /// `Written` arm of the flush report.
    #[test]
    fn a_submitted_snapshot_reaches_the_state_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let (store, clock) = store_at(&path);
        let media = local("/music/sonata.flac");
        let mut stored = PersistedState::default();
        stored.record(
            &PlaybackCheckpoint {
                media: media.clone(),
                position: Duration::from_secs(93),
                updated_at: clock.sample().wall,
            },
            false,
        );
        store.write(&stored).unwrap();

        let mut persistence = open_persistence(Some(store), &media, &clock);
        assert!(persistence.persisting);

        // The listener got another minute in, and the volume moved with them.
        let mut advanced = PersistedState::default();
        advanced.set_volume(Volume::new(0.5));
        advanced.set_current_media(media.clone());
        advanced.record(
            &PlaybackCheckpoint {
                media: media.clone(),
                position: Duration::from_secs(150),
                updated_at: clock.sample().wall,
            },
            false,
        );
        persistence.writer.submit(advanced, Urgency::Forced);

        let outcome = persistence.writer.shutdown();
        assert!(
            matches!(outcome, ShutdownOutcome::Written),
            "the store must acknowledge the final write: {outcome:?}"
        );
        assert!(matches!(
            classify_flush(outcome, persistence.persisting),
            FlushReport::Written
        ));

        let bytes = std::fs::read(&path).unwrap();
        let written: PersistedState = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            written.entry_for(&media).unwrap().position,
            Some(Duration::from_secs(150)),
            "the file must hold the snapshot that was submitted, not the one it started with"
        );
        assert_eq!(written.volume(), Volume::new(0.5));
        assert_eq!(written.current_media(), Some(&media));
    }

    /// D3: a file this build cannot read is preserved in place and writing is
    /// off for the session. The sink the disable selects has to write nowhere,
    /// or the preservation is a claim rather than a fact.
    #[test]
    fn a_state_file_from_a_newer_build_disables_writing_and_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let newer = br#"{"schema_version":99,"current_media":null,"volume":0.5,"checkpoints":{}}"#;
        std::fs::write(&path, newer).unwrap();
        let (store, clock) = store_at(&path);
        let media = local("/music/sonata.flac");

        let mut persistence = open_persistence(Some(store), &media, &clock);

        assert!(!persistence.persisting);
        assert_eq!(
            persistence.resume, None,
            "nothing is restored from a file this build cannot read"
        );
        assert_eq!(persistence.volume, Volume::FULL);

        persistence
            .writer
            .submit(PersistedState::default(), Urgency::Forced);
        let outcome = persistence.writer.shutdown();
        assert!(
            matches!(
                classify_flush(outcome, persistence.persisting),
                FlushReport::Disabled
            ),
            "a session that wrote nothing must not be reported as having written"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            newer,
            "the preserved file must come out byte for byte as it went in"
        );
    }

    #[test]
    fn a_disabled_session_never_reports_a_written_checkpoint() {
        // The sink answers `Ok` for a write it deliberately never performed, so
        // the outcome on its own cannot tell the two apart.
        assert!(matches!(
            classify_flush(ShutdownOutcome::Written, false),
            FlushReport::Disabled
        ));
        assert!(matches!(
            classify_flush(ShutdownOutcome::Written, true),
            FlushReport::Written
        ));
    }

    // ------------------------------------------------------- resolve_source

    #[test]
    fn only_an_explicit_http_or_https_prefix_is_a_url() {
        assert!(is_url_spelling("http://example.com/a.mp3"));
        assert!(is_url_spelling("https://example.com/a.mp3"));
        // ASCII case-insensitive (§5).
        assert!(is_url_spelling("HTTP://example.com/a.mp3"));
        assert!(is_url_spelling("HtTpS://example.com/a.mp3"));
        // §5: `./https:weird` is an unambiguous local spelling - the scheme
        // has to be a genuine prefix, not merely present anywhere.
        assert!(!is_url_spelling("./https:not-a-url"));
        assert!(!is_url_spelling("https:not-a-url"));
        assert!(!is_url_spelling("/music/http://weird.flac"));
        assert!(!is_url_spelling(""));
    }

    #[test]
    fn a_malformed_url_is_reported_as_invalid_source_not_a_missing_file() {
        let error = match resolve_source("https://") {
            Err(error) => error,
            Ok(_) => panic!("an empty host must not resolve"),
        };
        assert!(
            matches!(
                error,
                PlaybackError::Remote(RemoteFailure::InvalidSource { .. })
            ),
            "{error}"
        );
        assert!(error.to_string().contains("URL"), "{error}");
    }

    #[test]
    fn a_url_with_embedded_credentials_is_rejected_and_the_password_never_appears() {
        let error = match resolve_source("https://alice:hunter2@example.com/a.mp3") {
            Err(error) => error,
            Ok(_) => panic!("credentials must be refused"),
        };
        let message = error.to_string();
        assert!(message.contains("credentials"), "{message}");
        assert!(
            !message.contains("hunter2"),
            "the password leaked: {message}"
        );
    }

    #[test]
    fn a_valid_remote_url_resolves_to_a_remote_media_id_with_the_query_kept_for_fetching() {
        let (media, location) = match resolve_source("https://example.com/a.flac?token=secret") {
            Ok(resolved) => resolved,
            Err(error) => panic!("a well-formed URL must resolve: {error}"),
        };
        assert!(matches!(media, MediaId::RemoteUrl(_)));
        match location {
            SourceLocation::Http(url) => {
                assert_eq!(
                    url.query(),
                    Some("token=secret"),
                    "the fetch URL keeps the query"
                );
            }
            other => panic!("expected an HTTP source location: {other:?}"),
        }
    }

    #[test]
    fn a_local_spelling_that_looks_like_a_url_is_never_parsed_as_one() {
        // §5: `./https:...` stays a path. It will not canonicalize (there is
        // no such file), but the error must be a path error, not a URL one.
        let error = match resolve_source("./https:not-a-url") {
            Err(error) => error,
            Ok(_) => panic!("a nonexistent path must not resolve"),
        };
        assert!(matches!(error, PlaybackError::Open { .. }), "{error}");
        assert!(
            error.to_string().contains("https:not-a-url"),
            "expected the literal path in the diagnostic: {error}"
        );
    }

    // -------------------------------------------------------- display_name

    #[test]
    fn a_remote_display_name_is_the_last_path_segment_with_the_query_stripped() {
        let media = MediaId::RemoteUrl(
            NormalizedUrl::parse("https://cdn.example.com/shows/ep-1.mp3?token=secret")
                .unwrap_or_else(|error| panic!("a well-formed URL must parse: {error}")),
        );
        let name = display_name(&media);
        assert_eq!(name, "ep-1.mp3");
        assert!(
            !name.contains("token"),
            "the query leaked into the name: {name}"
        );
    }

    #[test]
    fn a_remote_display_name_falls_back_to_the_host_with_no_path() {
        let media = MediaId::RemoteUrl(
            NormalizedUrl::parse("https://cdn.example.com")
                .unwrap_or_else(|error| panic!("a well-formed URL must parse: {error}")),
        );
        assert_eq!(display_name(&media), "cdn.example.com");
    }

    // --------------------------------------------------------- status_line

    fn mirror_with_capabilities(seek: SeekSupport) -> Mirror {
        Mirror {
            capabilities: Some(MediaCapabilities {
                continuity: Continuity::Finite,
                seek,
            }),
            ..Mirror::default()
        }
    }

    #[test]
    fn an_unresolved_seek_capability_is_marked_distinctly_from_unsupported() {
        let unresolved = status_line(&mirror_with_capabilities(SeekSupport::Unknown));
        let unsupported = status_line(&mirror_with_capabilities(SeekSupport::Unsupported));
        assert!(unresolved.contains("seek?"), "{unresolved}");
        assert!(!unresolved.contains("no-seek"), "{unresolved}");
        assert!(unsupported.contains("no-seek"), "{unsupported}");
    }

    #[test]
    fn a_seekable_source_carries_no_seek_note_at_all() {
        let line = status_line(&mirror_with_capabilities(SeekSupport::Native));
        assert!(!line.contains("seek?"), "{line}");
        assert!(!line.contains("no-seek"), "{line}");
    }

    #[test]
    fn buffering_is_shown_only_as_a_detail_of_playing() {
        let mut mirror = Mirror {
            state: PlaybackState::Playing,
            buffering: true,
            ..Mirror::default()
        };
        assert!(
            status_line(&mirror).contains("buffering"),
            "{}",
            status_line(&mirror)
        );

        // Never a state of its own: a paused session that happens to be
        // servicing a blocked read (e.g. a seek's own reopen) does not print
        // "buffering" under a label that is not Playing.
        mirror.state = PlaybackState::Paused;
        assert!(
            !status_line(&mirror).contains("buffering"),
            "{}",
            status_line(&mirror)
        );
    }

    /// §11: `buffering` must never be derived from `PositionQuality::Degraded`
    /// (Ruling 4) - a degraded quality with `buffering` still false must not
    /// print "buffering" on its own account.
    #[test]
    fn degraded_quality_does_not_imply_buffering() {
        let mirror = Mirror {
            state: PlaybackState::Playing,
            quality: PositionQuality::Degraded,
            buffering: false,
            ..Mirror::default()
        };
        assert!(
            !status_line(&mirror).contains("buffering"),
            "{}",
            status_line(&mirror)
        );
    }

    // ----------------------------------------------------- Mirror::apply

    #[test]
    fn capabilities_changed_updates_the_mirror_without_disturbing_position() {
        let mut mirror = Mirror {
            position: Duration::from_secs(42),
            ..Mirror::default()
        };
        mirror.apply(PlaybackEvent::CapabilitiesChanged {
            session_rev: 7,
            capabilities: MediaCapabilities {
                continuity: Continuity::Finite,
                seek: SeekSupport::Native,
            },
        });
        assert_eq!(mirror.session_rev, 7);
        assert_eq!(
            mirror.capabilities.map(|capabilities| capabilities.seek),
            Some(SeekSupport::Native)
        );
        assert_eq!(
            mirror.position,
            Duration::from_secs(42),
            "unrelated to capability evidence"
        );
    }

    /// Minor (final review): a stale `buffering` from whatever the mirror was
    /// showing before must not survive into a fresh `Loaded` - display-only
    /// and unreachable under one load per run, but a real bug rather than
    /// only a limitation.
    #[test]
    fn a_fresh_load_clears_a_stale_buffering_flag() {
        let mut mirror = Mirror {
            buffering: true,
            ..Mirror::default()
        };
        mirror.apply(PlaybackEvent::Loaded {
            session_rev: 3,
            media: local("/music/sonata.flac"),
            metadata: MediaMetadata {
                title: None,
                duration: Some(Duration::from_secs(300)),
                duration_provenance: PositionProvenance::Established,
            },
            capabilities: MediaCapabilities {
                continuity: Continuity::Finite,
                seek: SeekSupport::Native,
            },
            position: Duration::ZERO,
            disposition: StartDisposition::Fresh,
        });
        assert!(
            !mirror.buffering,
            "a fresh load must clear a stale buffering flag"
        );
    }
}
