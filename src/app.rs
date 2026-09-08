//! The `continuo play` application: argument-to-path resolution, terminal
//! setup, and the key-driven status loop around [`EngineHandle`].

use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, execute};

use crate::cli::{self, CliCommand};
use crate::media::id::{AbsolutePath, MediaId};
use crate::media::source::SourceLocation;
use crate::playback::command::PlaybackCommand;
use crate::playback::decode::DecodedSource;
use crate::playback::engine::EngineHandle;
use crate::playback::error::PlaybackError;
use crate::playback::event::PlaybackEvent;
use crate::playback::state::PlaybackState;
use crate::playback::timeline::PositionQuality;
use crate::playback::volume::Volume;

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
    drop(probed);

    let engine = EngineHandle::spawn_cpal();
    let _raw = RawModeGuard::enable()?; // Drop restores the terminal on every exit path.
    let mut mirror = Mirror::default();

    engine
        .commands()
        .send(PlaybackCommand::Load {
            media: MediaId::LocalFile(absolute.clone()),
            source: SourceLocation::LocalPath(absolute.as_path().to_path_buf()),
            start_at: Duration::ZERO,
        })
        .ok();
    engine.commands().send(PlaybackCommand::Play).ok();

    let outcome = loop {
        match crossterm::event::poll(Duration::from_millis(100)) {
            Ok(true) => match crossterm::event::read() {
                Ok(Event::Key(key)) => match to_command(key, &mirror) {
                    Some(PlaybackCommand::Shutdown) => break Ok(()),
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
        if progress.session_rev == mirror.session_rev {
            mirror.position = progress.position;
            mirror.quality = progress.quality;
        }
        render(&mirror)?;
    };

    // Out of band first, and in band only as a courtesy. The worker stops
    // reading commands while an event backlog exists, and this loop has just
    // stopped draining events, so an in-band `Shutdown` can sit unread in the
    // channel forever while `join` blocks - hanging the process with the
    // terminal still in raw mode. The interrupt is the only signal that is
    // guaranteed to be seen.
    engine.interrupt_shutdown();
    engine.commands().send(PlaybackCommand::Shutdown).ok();
    engine.join();
    outcome
}

/// Installs raw mode and restores it on drop, so a `?` anywhere in the loop
/// above — or a panic — cannot leave the terminal raw.
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
                ..
            } => {
                self.session_rev = session_rev;
                self.name = Some(display_name(&media));
                self.duration = metadata.duration;
                self.position = Duration::ZERO;
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
}
