//! Idempotent terminal teardown (design doc M5 §11): the TUI can leave raw
//! mode, the alternate screen, mouse capture, a hidden cursor, or placed
//! kitty images active. Every one of those has to be undone exactly once,
//! from whichever of normal shutdown, a fatal panic, or a signal gets there
//! first — and it must never fail loudly, because by the time it runs the
//! PTY on the other end may already be gone.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::ExecutableCommand;
use crossterm::cursor::Show;
use crossterm::event::DisableMouseCapture;
use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};

/// Kitty graphics protocol "delete all placed images" (`a=d,d=A`).
/// Crossterm has no `Command` for the kitty graphics protocol, so this is
/// the raw escape sequence; it is written only to the real terminal device,
/// never treated as a display string, so it does not conflict with the
/// project's ban on raw control characters in displayed text.
const KITTY_DELETE_ALL: &[u8] = b"\x1b_Ga=d,d=A\x1b\\";

/// What a running TUI has changed about the terminal, tracked so it can be
/// undone. Each flag is set by the code that made the corresponding change
/// and cleared by `restore_into`/`restore` the moment its undo is written,
/// which is what makes repeated calls safe.
#[derive(Default)]
pub struct TerminalCleanup {
    raw: AtomicBool,
    alternate: AtomicBool,
    mouse: AtomicBool,
    cursor_hidden: AtomicBool,
    kitty_images: AtomicBool,
}

impl TerminalCleanup {
    pub fn mark_raw(&self) {
        self.raw.store(true, Ordering::SeqCst);
    }

    pub fn mark_alternate(&self) {
        self.alternate.store(true, Ordering::SeqCst);
    }

    pub fn set_mouse(&self, on: bool) {
        self.mouse.store(on, Ordering::SeqCst);
    }

    pub fn mark_cursor_hidden(&self) {
        self.cursor_hidden.store(true, Ordering::SeqCst);
    }

    pub fn set_kitty_images(&self, on: bool) {
        self.kitty_images.store(on, Ordering::SeqCst);
    }

    /// Writes the undo for each flag that is currently set, in the order a
    /// well-behaved terminal expects to unwind them: delete any placed
    /// images first, then release mouse capture, show the cursor, and
    /// finally leave the alternate screen. Each flag is swapped to `false`
    /// before its undo is written, so a flag that was never set — or was
    /// already restored — writes nothing, and calling this twice in a row
    /// writes nothing the second time. Every write error is ignored: a
    /// vanished PTY must never stop teardown.
    pub fn restore_into(&self, out: &mut dyn Write) {
        if self.kitty_images.swap(false, Ordering::SeqCst) {
            let _ = out.write_all(KITTY_DELETE_ALL);
        }
        if self.mouse.swap(false, Ordering::SeqCst) {
            let _ = out.execute(DisableMouseCapture);
        }
        if self.cursor_hidden.swap(false, Ordering::SeqCst) {
            let _ = out.execute(Show);
        }
        if self.alternate.swap(false, Ordering::SeqCst) {
            let _ = out.execute(LeaveAlternateScreen);
        }
    }

    /// [`Self::restore_into`] on stdout, then leaves raw mode if it was
    /// marked. Raw mode is a terminal driver setting rather than an escape
    /// sequence, so it is restored through crossterm's own call instead of
    /// a byte sequence written alongside the rest.
    pub fn restore(&self) {
        let mut stdout = std::io::stdout();
        self.restore_into(&mut stdout);
        if self.raw.swap(false, Ordering::SeqCst) {
            let _ = disable_raw_mode();
        }
    }
}
