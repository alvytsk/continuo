//! Terminal input read on a thread of its own, handed to the player's loop
//! over a bounded channel — `tui`'s event loop and `play`'s key loop alike
//! (design doc M5 §4).
//!
//! crossterm 0.29 retries a tty read that returns end of file or an I/O
//! error without end, inside `event::poll`, so on a hung-up terminal the
//! thread that polls never returns. Were that a player's loop, it would never
//! again see a signal, pump playback or draw: the profile lock would stay
//! held and nothing would be flushed. Reading here leaves the loop waiting
//! on the channel instead, so the hangup's SIGHUP, or the next draw's failed
//! write, still ends the run.
//!
//! The reader thread is never joined. After a hangup it stays inside
//! crossterm until the process exits; otherwise it notices within
//! [`READ_POLL`] that its [`InputReader`] is gone and returns, so it does not
//! go on taking keys meant for the shell once the player has quit.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use crossterm::event::{self, Event};

/// How long the reader waits for input before checking it is still wanted.
const READ_POLL: Duration = Duration::from_millis(50);

/// The loop's end of the reader thread. Dropping it stops the thread.
pub struct InputReader {
    events: Receiver<io::Result<Event>>,
    stop: Arc<AtomicBool>,
}

impl InputReader {
    /// Starts reading terminal input. At most `capacity` events wait unread;
    /// past that the reader stops reading until the loop catches up.
    ///
    /// Must not start before anything else has finished reading stdin — such
    /// as `tui`'s cover-art protocol query — or it would take that answer as
    /// input.
    pub fn spawn(capacity: usize) -> io::Result<Self> {
        let (sender, events) = crossbeam_channel::bounded(capacity);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("continuo-input".to_owned())
            .spawn(move || read_events(&sender, &stopping))?;
        Ok(Self { events, stop })
    }

    /// The next event, waiting up to `wait` for one. A failed terminal read
    /// is returned once, and the reader stops after it.
    pub fn next(&self, wait: Duration) -> io::Result<Option<Event>> {
        match self.events.recv_timeout(wait) {
            Ok(event) => event.map(Some),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                Err(io::Error::other("the terminal input reader stopped"))
            }
        }
    }
}

impl Drop for InputReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn read_events(sender: &Sender<io::Result<Event>>, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        let event = match event::poll(READ_POLL) {
            Ok(false) => continue,
            Ok(true) => event::read(),
            Err(error) => Err(error),
        };
        let failed = event.is_err();
        if sender.send(event).is_err() || failed {
            return;
        }
    }
}
