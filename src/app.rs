//! The `continuo play` application: argument-to-path resolution, terminal
//! setup, and the key-driven status loop around [`EngineHandle`].

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, execute};

use crate::cli::{self, CliCommand};
use crate::clock::{Clock, SystemClock};
use crate::media::id::{AbsolutePath, MediaId};
use crate::media::source::SourceLocation;
use crate::persistence::PersistenceError;
use crate::persistence::model::PersistedState;
use crate::persistence::store::{LoadReason, StateStore};
use crate::persistence::writer::{ShutdownOutcome, StateSink, Urgency, WriterHandle};
use crate::playback::command::{PlaybackCommand, ResumeIntent};
use crate::playback::decode::DecodedSource;
use crate::playback::engine::EngineHandle;
use crate::playback::error::PlaybackError;
use crate::playback::event::PlaybackEvent;
use crate::playback::state::PlaybackState;
use crate::playback::timeline::PositionQuality;
use crate::playback::volume::Volume;
use crate::resume::ResumeCandidate;
use crate::session::{Action, Session};

const SEEK_STEP_SECS: i64 = 10;
const VOLUME_STEP: f32 = 0.05;
const HELP_LINE: &str =
    "space pause · ←/→ seek 10s · Home restart · -/+ volume · s stop · p play · q quit";

/// Runs the parsed CLI to completion.
pub fn run(cli: cli::Cli) -> Result<(), PlaybackError> {
    let CliCommand::Play { path, probe_only } = cli.command;

    let canonical = path.canonicalize().map_err(|source| PlaybackError::Open {
        path: path.clone(),
        source,
    })?;
    let absolute =
        AbsolutePath::new(canonical).map_err(|error| PlaybackError::UnsupportedInput {
            path: path.clone(),
            reason: error.to_string(),
        })?;

    // Validated on the main thread, before anything touches a terminal or a
    // device. A rejected file — a directory, an unreadable format, an
    // unsupported channel layout — must be reportable without an audio
    // device or a controlling terminal, neither of which CI has, and it must
    // never leave the terminal toggled into raw mode. The worker reopens the
    // same path itself once `Load` is sent, so this open is pure validation
    // and its result is not carried forward.
    let probed = DecodedSource::open(&absolute)?;
    if probe_only {
        println!(
            "{} {} Hz {} ch {:?}",
            probed.metadata().title.as_deref().unwrap_or("(untitled)"),
            probed.sample_rate(),
            probed.channels(),
            probed.metadata().duration,
        );
        return Ok(());
    }
    // This probe's own validation is all `run` needed from it; the worker
    // reopens the same path once `Load` is sent, and it is the worker's own
    // probe — the only one with a duration — that resolves whatever resume
    // candidate persistence hands back (Ruling 5).
    drop(probed);

    // Persistence opens before the engine: the resume candidate is an
    // argument to the load, and the restored volume is a command that
    // precedes it.
    let media = MediaId::LocalFile(absolute.clone());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let Persistence {
        mut session,
        mut writer,
        candidate,
        volume,
        persisting,
    } = open_persistence(platform_store(&clock), &media, &clock);

    let engine = EngineHandle::spawn_cpal();
    // Dropped explicitly by the shutdown sequence, before the flush waits on
    // the disk; its `Drop` is what covers a panic.
    let raw = RawModeGuard::enable()?;
    let mut mirror = Mirror::default();

    for command in resume_commands(
        media,
        SourceLocation::LocalPath(absolute.as_path().to_path_buf()),
        candidate,
        volume,
    ) {
        engine.commands().send(command).ok();
    }

    let outcome = loop {
        match crossterm::event::poll(Duration::from_millis(100)) {
            Ok(true) => match crossterm::event::read() {
                Ok(Event::Key(key)) => match to_command(key, &mirror) {
                    Some(PlaybackCommand::Shutdown) => break Ok(()),
                    // Stop travels out of band. The ordinary command queue stops
                    // being read while an event backlog exists, and a queued
                    // Stop cannot interrupt a refinement already running, so
                    // pressing `s` would not stop anything when it matters most.
                    Some(PlaybackCommand::Stop) => engine.interrupt_stop(),
                    Some(command) => {
                        engine.commands().send(command).ok();
                    }
                    None => {}
                },
                Ok(_) => {}
                // The input stream ended or failed; there is nothing left to
                // read keys from, so shut down as cleanly as `q` would.
                Err(_) => break Ok(()),
            },
            Ok(false) => {}
            Err(_) => break Ok(()),
        }

        let mut failure = None;
        while let Ok(event) = engine.events().try_recv() {
            if let PlaybackEvent::Failed { message, .. } = &event {
                failure = Some(message.clone());
            }
            // `observe` borrows the event, so the mirror still consumes it.
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
        }
        // A terminal write failure is not a reason to skip the final
        // checkpoint, so it becomes the loop's outcome instead of returning
        // from here and bypassing the flush path (D18).
        if let Err(error) = render(&mirror) {
            break Err(error);
        }
    };

    // Out of band first, and in band only as a courtesy. The worker stops
    // reading commands while an event backlog exists, and this loop has just
    // stopped draining events, so an in-band `Shutdown` can sit unread in the
    // channel forever while `join` blocks - hanging the process with the
    // terminal still in raw mode. The interrupt is the only signal that is
    // guaranteed to be seen.
    engine.interrupt_shutdown();
    engine.commands().send(PlaybackCommand::Shutdown).ok();
    let report = engine.join();

    // The events the loop never drained are replayed through the policy before
    // the snapshot is taken, so the snapshot comes from a session that has seen
    // everything the run produced (D19).
    writer.submit(
        session.reconcile_shutdown(&report, clock.sample()),
        Urgency::Forced,
    );

    // Restore the terminal before waiting on the disk, so the writer's bound is
    // never spent with the terminal still raw.
    drop(raw);
    report_flush(writer.shutdown(), persisting);
    outcome
}

/// §11's initial command sequence. Volume first: the engine accepts it with no
/// transport, and a transport created later adopts the stored gain — so the
/// restored level is in force from the first buffer rather than after it.
/// Nothing about the sequence is conditional; a session with no stored
/// candidate issues the same `Load`, with a start of zero.
fn resume_commands(
    media: MediaId,
    source: SourceLocation,
    candidate: Option<ResumeCandidate>,
    volume: Volume,
) -> [PlaybackCommand; 3] {
    // No entry is not itself a `Candidate`: the worker would decide `NoEntry`
    // from it anyway (§11), so this is the same outcome without asking the
    // worker to resolve a candidate that was never there.
    let resume = match candidate {
        Some(candidate) => ResumeIntent::Candidate(candidate),
        None => ResumeIntent::StartAt(Duration::ZERO),
    };
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
    /// The stored entry for this media, unresolved. Resolving it needs a
    /// duration, and only the worker's own decode probe has one (Ruling 5) —
    /// so `open_persistence` hands the raw candidate onward rather than
    /// deciding a start position itself, which would mean opening the media
    /// twice for the same answer `decide_resume` gives either time.
    candidate: Option<ResumeCandidate>,
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
    let candidate = state.entry_for(media).map(ResumeCandidate::from);
    let volume = state.volume();
    let sink: Box<dyn StateSink> = match (store, writable) {
        (Some(store), true) => Box::new(store),
        _ => Box::new(DisabledSink),
    };

    Persistence {
        session: Session::new(state),
        writer: WriterHandle::spawn(sink, Arc::clone(clock)),
        candidate,
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

/// Installs raw mode and restores it on drop. `run` drops it explicitly, so the
/// writer's shutdown bound is never spent with the terminal still raw; the
/// `Drop` covers a panic, which is the only way out of the loop above that does
/// not reach that line.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self, PlaybackError> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
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
    volume: Volume,
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
            volume: Volume::default(),
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
                position,
                ..
            } => {
                self.session_rev = session_rev;
                self.name = Some(display_name(&media));
                self.duration = metadata.duration;
                self.position = position;
                self.quality = PositionQuality::Exact;
                self.state = PlaybackState::Loading;
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
            } => {
                self.session_rev = session_rev;
                self.position = position;
            }
            PlaybackEvent::DeviceRecovered { session_rev }
            | PlaybackEvent::SeekRejected { session_rev, .. }
            | PlaybackEvent::SeekCancelled { session_rev, .. }
            | PlaybackEvent::CapabilitiesChanged { session_rev, .. }
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
        other => other.to_string(),
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
    let suffix = if mirror.quality == PositionQuality::Degraded {
        " ~"
    } else {
        ""
    };
    let duration = mirror
        .duration
        .map(format_hms)
        .unwrap_or_else(|| "--:--:--".to_string());
    format!(
        "{name} [{}] {position}{suffix} / {duration}  vol {}%",
        mirror.state.label(),
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
            Some(candidate),
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
            persistence.candidate,
            Some(ResumeCandidate {
                position: Duration::from_secs(93),
                completed: false,
            }),
            "the entry the file held, unresolved — only the worker's probe has a duration"
        );
        assert_eq!(persistence.volume, Volume::new(0.25));
        assert!(persistence.persisting);
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
            Duration::from_secs(150),
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
            persistence.candidate, None,
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
}
