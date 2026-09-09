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
use crate::playback::command::PlaybackCommand;
use crate::playback::decode::DecodedSource;
use crate::playback::engine::EngineHandle;
use crate::playback::error::PlaybackError;
use crate::playback::event::PlaybackEvent;
use crate::playback::state::PlaybackState;
use crate::playback::timeline::PositionQuality;
use crate::playback::volume::Volume;
use crate::session::{Action, ResumeDecision, Session, decide_resume};

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
    // Kept before the probe is dropped: §11 validates a stored position against
    // the duration this probe already reports, so the resume opens no file of
    // its own.
    let duration = probed.metadata().duration;
    drop(probed);

    // Persistence opens before the engine: the start position is an argument to
    // the load, and the restored volume is a command that precedes it.
    let media = MediaId::LocalFile(absolute.clone());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let Persistence {
        mut session,
        mut writer,
        start_at,
        volume,
        persisting,
    } = open_persistence(&media, duration, &clock);

    let engine = EngineHandle::spawn_cpal();
    // Dropped explicitly by the shutdown sequence, before the flush waits on
    // the disk; its `Drop` is what covers a panic.
    let raw = RawModeGuard::enable()?;
    let mut mirror = Mirror::default();

    for command in resume_commands(
        media,
        SourceLocation::LocalPath(absolute.as_path().to_path_buf()),
        start_at,
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
/// Nothing about the sequence is conditional; a session with no stored volume
/// issues the same command with the default.
fn resume_commands(
    media: MediaId,
    source: SourceLocation,
    start_at: Duration,
    volume: Volume,
) -> [PlaybackCommand; 3] {
    [
        PlaybackCommand::SetVolume(volume),
        PlaybackCommand::Load {
            media,
            source,
            start_at,
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
    start_at: Duration,
    volume: Volume,
    /// Whether anything this session submits can reach the disk. A disabled
    /// sink reports every write as a success, deliberately — the writer must
    /// not count a disable as a failure (D11) — so this is what keeps the
    /// shutdown log from claiming a write that never happened.
    persisting: bool,
}

fn open_persistence(
    media: &MediaId,
    duration: Option<Duration>,
    clock: &Arc<dyn Clock>,
) -> Persistence {
    let store = match StateStore::platform_path() {
        Ok(path) => Some(StateStore::new(path, Arc::clone(clock))),
        Err(error) => {
            tracing::warn!(%error, "no state directory; this session will not be persisted");
            None
        }
    };

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

    let decision = decide_resume(state.entry_for(media), duration);
    match decision {
        ResumeDecision::NoEntry => tracing::debug!("no stored position for this media"),
        ResumeDecision::Completed => tracing::info!("resume declined: this media is completed"),
        ResumeDecision::AtStart => {}
        ResumeDecision::Resume(position) => tracing::info!(?position, "resume position selected"),
        ResumeDecision::DegenerateEnd => {
            tracing::debug!("stored position is exactly the end; starting over");
        }
        ResumeDecision::StalePastEnd => {
            tracing::warn!("stored position is past the end of this media");
        }
        ResumeDecision::Unvalidated(position) => {
            tracing::info!(
                ?position,
                "duration unknown; stored position retained unvalidated"
            );
        }
    }

    let start_at = decision.start_at();
    let volume = state.volume();
    let sink: Box<dyn StateSink> = match (store, writable) {
        (Some(store), true) => Box::new(store),
        _ => Box::new(DisabledSink),
    };

    Persistence {
        session: Session::new(state),
        writer: WriterHandle::spawn(sink, Arc::clone(clock)),
        start_at,
        volume,
        persisting: writable,
    }
}

fn submit(writer: &WriterHandle, action: Action) {
    if let Action::Submit { state, urgency } = action {
        writer.submit(state, urgency);
    }
}

/// What the flush is reported as. A session whose writing is disabled reaches
/// `Written` having written nothing, so the outcome alone must not be logged as
/// a checkpoint that landed.
fn report_flush(outcome: ShutdownOutcome, persisting: bool) {
    if !persisting {
        tracing::debug!("persistence is disabled for this session; no checkpoint was written");
        return;
    }
    match outcome {
        ShutdownOutcome::Written => tracing::debug!("final checkpoint written"),
        ShutdownOutcome::Failed(error) => tracing::warn!(%error, "final checkpoint failed"),
        ShutdownOutcome::Unconfirmed => tracing::warn!("final checkpoint UNCONFIRMED"),
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
            PlaybackEvent::DeviceRecovered { session_rev }
            | PlaybackEvent::SeekRejected { session_rev, .. }
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
    use crate::media::capabilities::{Continuity, MediaCapabilities, SeekSupport};
    use crate::media::metadata::MediaMetadata;

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
        let commands = resume_commands(
            local("/music/sonata.flac"),
            SourceLocation::LocalPath("/music/sonata.flac".into()),
            Duration::from_secs(93),
            Volume::new(0.25),
        );

        match &commands {
            [
                PlaybackCommand::SetVolume(volume),
                PlaybackCommand::Load { start_at, .. },
                PlaybackCommand::Play,
            ] => {
                assert_eq!(*volume, Volume::new(0.25), "the stored gain, unchanged");
                assert_eq!(*start_at, Duration::from_secs(93), "the decided start");
            }
            other => panic!("volume must be issued before the load: {other:?}"),
        }
    }
}
