//! `continuo tui`: startup, the event loop and teardown around
//! [`PlayerRuntime`] (design doc M5 §4, §11).
//!
//! Startup runs in the order §11 fixes: cleanup state and the panic hook,
//! signals, the profile lock, state, the session log and fd-2 redirect, the
//! writer and runtime, then the terminal. A shutdown request or a worker's
//! fatal panic recorded between two stages skips the rest, and every way out
//! — quit key, signal, worker fatal, startup failure, or a panic on this
//! thread — takes the same teardown over whatever was initialized so far.
//!
//! The drawing here is deliberately minimal and temporary: a title row, the
//! queue titles, and the empty-queue notice.

use std::any::Any;
use std::io::{self, Stdout};
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Duration;

use crossterm::cursor::Hide;
use crossterm::event::{self, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};

use crate::application::runtime::{
    AppCommand, FlushReport, LibraryStores, PlayerRuntime, RuntimeParts,
};
use crate::application::transport::QUEUE_EMPTY;
use crate::application::view::PlayerView;
use crate::cli::MouseMode;
use crate::clock::{Clock, SystemClock};
use crate::error::{AppError, LifecycleError};
use crate::http::limits::Limits;
use crate::lifecycle::RunOutcome;
use crate::lifecycle::hooks::TestHook;
use crate::lifecycle::lock::{LockError, ProfileLock};
use crate::lifecycle::panic::{FatalCleanup, install_panic_hook};
use crate::lifecycle::signals::ShutdownSignals;
use crate::persistence::store::{LoadOutcome, QueueBackup, StateStore};
use crate::persistence::writer::{DisabledSink, StateSink, WriterHandle};
use crate::playback::engine::EngineHandle;
use crate::session::Session;

/// How long one loop pass waits for terminal input before pumping the
/// runtime again; also the bound on how late a signal or fatal panic is seen.
const INPUT_POLL: Duration = Duration::from_millis(50);
const VOLUME_STEP: f32 = 0.05;
const TITLE: &str = "continuo";
const STATE_NOT_SAVED: &str = "This session is not saved";

pub struct TuiOptions {
    pub mouse: MouseMode,
}

type Tty = Terminal<CrosstermBackend<Stdout>>;

/// What startup has initialized, for teardown to release.
#[derive(Default)]
struct Stages {
    lock: Option<ProfileLock>,
    runtime: Option<PlayerRuntime>,
    terminal: Option<Tty>,
}

/// Why the run is ending.
enum Ending {
    /// The quit key, the Ctrl-C key, or an OS signal.
    Requested,
    /// A panic on another thread took the fatal path.
    WorkerPanicked,
    Failed(AppError),
    /// A panic on this thread, to be resumed once teardown is done.
    Panicked(Box<dyn Any + Send>),
}

pub fn run(options: TuiOptions) -> Result<RunOutcome, AppError> {
    let hook = TestHook::from_env();
    // The loop polls `fatal_requested` at least every `INPUT_POLL`, so the
    // wake receiver only has to stay alive for the hook's `try_send`.
    let (wake, _wake_receiver) = crossbeam_channel::bounded(1);
    let cleanup = Arc::new(FatalCleanup::new(wake));
    install_panic_hook(Arc::clone(&cleanup));

    let signals = ShutdownSignals::install().map_err(LifecycleError::Signals)?;

    let mut stages = Stages::default();
    let ending = match panic::catch_unwind(AssertUnwindSafe(|| {
        start_and_loop(hook, &options, &cleanup, &signals, &mut stages)
    })) {
        Ok(ending) => ending,
        Err(payload) => Ending::Panicked(payload),
    };
    teardown(ending, stages, &cleanup, signals)
}

/// `Some` when a shutdown request or a fatal panic elsewhere means startup
/// must not continue.
fn interrupted(signals: &ShutdownSignals, cleanup: &FatalCleanup) -> Option<Ending> {
    if cleanup.fatal_requested() {
        Some(Ending::WorkerPanicked)
    } else if signals.requested() {
        Some(Ending::Requested)
    } else {
        None
    }
}

macro_rules! stage {
    ($signals:expr, $cleanup:expr) => {
        if let Some(ending) = interrupted($signals, $cleanup) {
            return ending;
        }
    };
}

macro_rules! attempt {
    ($result:expr) => {
        match $result {
            Ok(value) => value,
            Err(error) => return Ending::Failed(AppError::from(error)),
        }
    };
}

fn start_and_loop(
    hook: TestHook,
    options: &TuiOptions,
    cleanup: &FatalCleanup,
    signals: &ShutdownSignals,
    stages: &mut Stages,
) -> Ending {
    stage!(signals, cleanup);

    hook.panic_at(TestHook::PanicBeforeRedirect);
    let state_path = attempt!(
        StateStore::platform_path().map_err(|_| LifecycleError::from(LockError::NoStateDirectory))
    );
    stages.lock = Some(attempt!(
        ProfileLock::acquire(&state_path).map_err(LifecycleError::from)
    ));
    stage!(signals, cleanup);

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let store = StateStore::new(state_path.clone(), Arc::clone(&clock));
    let loaded = store.load();
    stage!(signals, cleanup);

    attempt!(redirect_to_session_log(&state_path, cleanup));
    hook.panic_at(TestHook::PanicAfterRedirect);
    stage!(signals, cleanup);

    let runtime = stages.runtime.insert(start_runtime(store, loaded, clock));
    stage!(signals, cleanup);

    let terminal = stages.terminal.insert(attempt!(
        enter_terminal(options.mouse, cleanup).map_err(LifecycleError::Terminal)
    ));
    hook.panic_at(TestHook::PanicAfterTerminal);
    if hook == TestHook::StderrProbe {
        probe_stderr();
    }
    stage!(signals, cleanup);

    run_loop(runtime, terminal, cleanup, signals)
}

/// Opens this run's log, hands a clone to the panic hook for contained-panic
/// diagnostics, and points fd 2 at it. The redirect goes straight into the
/// cleanup slot with nothing fallible in between, so a panic at any later
/// point restores fd 2.
#[cfg(unix)]
fn redirect_to_session_log(
    state_path: &std::path::Path,
    cleanup: &FatalCleanup,
) -> Result<(), LifecycleError> {
    use crate::lifecycle::stderr::{open_session_log, redirect_stderr};

    let (file, _path) = open_session_log(state_path, time::OffsetDateTime::now_utc())
        .map_err(LifecycleError::Log)?;
    cleanup.set_diagnostic_log(file.try_clone().map_err(LifecycleError::Log)?);
    let redirect = redirect_stderr(file).map_err(LifecycleError::Redirect)?;
    // The slot starts empty and nothing else publishes into it, so this
    // cannot be refused; a refused value would be dropped here, restoring
    // fd 2 at once rather than leaking the redirect.
    let _ = cleanup.stderr_slot().publish(Box::new(redirect));
    Ok(())
}

/// Without fd-2 redirection, C libraries and stray diagnostics would write
/// over the interface, so the terminal player refuses to start rather than
/// run without it.
#[cfg(not(unix))]
fn redirect_to_session_log(
    _state_path: &std::path::Path,
    _cleanup: &FatalCleanup,
) -> Result<(), LifecycleError> {
    Err(LifecycleError::Redirect(io::Error::new(
        io::ErrorKind::Unsupported,
        "the terminal player needs stderr redirection, which is available only on Unix",
    )))
}

fn start_runtime(store: StateStore, loaded: LoadOutcome, clock: Arc<dyn Clock>) -> PlayerRuntime {
    let LoadOutcome {
        state,
        writable,
        queue_repair,
        ..
    } = loaded;
    let sink: Box<dyn StateSink> = if writable {
        Box::new(store)
    } else {
        Box::new(DisabledSink)
    };
    let library =
        crate::commands::platform_subscription_stores()
            .ok()
            .map(|(subscriptions, cache)| LibraryStores {
                subscriptions,
                cache,
            });
    let mut runtime = PlayerRuntime::new(RuntimeParts {
        session: Session::new(state),
        writer: WriterHandle::spawn(sink, Arc::clone(&clock)),
        persisting: writable,
        clock,
        engine_factory: Box::new(EngineHandle::spawn_for_environment),
        library,
        http_limits: Limits::default(),
    });
    let status = match queue_repair {
        Some(repair) => Some(match repair.backup {
            QueueBackup::Saved(path) => format!(
                "Queue data was reset ({}); backup at {}",
                repair.reset.fields_reset(),
                path.display()
            ),
            QueueBackup::Failed => format!(
                "Queue data was reset ({}); this session is not saved",
                repair.reset.fields_reset()
            ),
        }),
        None if !writable => Some(STATE_NOT_SAVED.to_owned()),
        None => None,
    };
    if let Some(status) = status {
        runtime.set_status(status);
    }
    runtime
}

/// Each change is marked for cleanup as soon as it is made, so a failure
/// part-way leaves teardown exactly what needs undoing.
fn enter_terminal(mouse: MouseMode, cleanup: &FatalCleanup) -> io::Result<Tty> {
    let terminal = cleanup.terminal();
    enable_raw_mode()?;
    terminal.mark_raw();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    terminal.mark_alternate();
    execute!(stdout, Hide)?;
    terminal.mark_cursor_hidden();
    if mouse == MouseMode::On {
        execute!(stdout, EnableMouseCapture)?;
        terminal.set_mouse(true);
    }
    Terminal::new(CrosstermBackend::new(io::stdout()))
}

/// A write to fd 2 from a child process and one from Rust, so a process test
/// can check both reach the session log instead of the terminal.
fn probe_stderr() {
    let _ = std::process::Command::new("sh")
        .args(["-c", "printf continuo-stderr-probe >&2"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status();
    eprintln!("continuo-stderr-probe-rust");
}

fn run_loop(
    runtime: &mut PlayerRuntime,
    terminal: &mut Tty,
    cleanup: &FatalCleanup,
    signals: &ShutdownSignals,
) -> Ending {
    loop {
        if let Err(error) = handle_input(runtime, signals) {
            return Ending::Failed(LifecycleError::Terminal(error).into());
        }
        runtime.pump();
        if let Some(ending) = interrupted(signals, cleanup) {
            return ending;
        }
        if !cleanup.rendering_disabled()
            && let Err(error) = terminal.draw(|frame| draw(frame, &runtime.view()))
        {
            return Ending::Failed(LifecycleError::Terminal(error).into());
        }
    }
}

fn handle_input(runtime: &mut PlayerRuntime, signals: &ShutdownSignals) -> io::Result<()> {
    if !event::poll(INPUT_POLL)? {
        return Ok(());
    }
    let Event::Key(key) = event::read()? else {
        return Ok(());
    };
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }
    match key.code {
        KeyCode::Char('q') => signals.request(),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => signals.request(),
        KeyCode::Char('-') => runtime.handle(AppCommand::AdjustVolume(-VOLUME_STEP)),
        _ => {}
    }
    Ok(())
}

fn draw(frame: &mut Frame<'_>, view: &PlayerView) {
    let title = match &view.status {
        Some(status) => format!("{TITLE} · {status}"),
        None => TITLE.to_owned(),
    };
    let mut lines = vec![Line::from(title)];
    if view.rows.is_empty() {
        lines.push(Line::from(QUEUE_EMPTY));
    } else {
        lines.extend(view.rows.iter().map(|row| Line::from(row.title.clone())));
    }
    frame.render_widget(Paragraph::new(lines), frame.area());
}

/// Releases what startup initialized, in §11's order: the engine and writer
/// (while fd 2 still points at the log), then the terminal and fd 2, then
/// the signal listener, then the profile lock, and only then anything
/// printed for the user.
fn teardown(
    ending: Ending,
    stages: Stages,
    cleanup: &FatalCleanup,
    signals: ShutdownSignals,
) -> Result<RunOutcome, AppError> {
    let Stages {
        lock,
        runtime,
        terminal,
    } = stages;
    // Ratatui shows the cursor when its terminal drops; doing that now keeps
    // any complaint about a vanished PTY in the log.
    drop(terminal);
    let flush = runtime.map(PlayerRuntime::shutdown);
    cleanup.restore_now();
    let outcome = signals.outcome();
    signals.close();
    drop(lock);

    match flush {
        Some(FlushReport::Failed(error)) => eprintln!("State was not saved: {error}"),
        Some(FlushReport::Unconfirmed) => {
            eprintln!("State was not saved: the final write was not confirmed in time");
        }
        Some(FlushReport::Written | FlushReport::Disabled) | None => {}
    }

    match ending {
        Ending::Panicked(payload) => panic::resume_unwind(payload),
        Ending::WorkerPanicked => Err(LifecycleError::WorkerPanicked.into()),
        // As in `play`, a recorded signal is what the run reports, even when
        // it arrived alongside a failure (a hung-up pane, say).
        Ending::Failed(error) => match outcome {
            RunOutcome::Signalled(_) => Ok(outcome),
            RunOutcome::Completed => Err(error),
        },
        Ending::Requested => Ok(outcome),
    }
}
