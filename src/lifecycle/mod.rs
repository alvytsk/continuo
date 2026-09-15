//! Cross-cutting concerns shared by every `continuo` invocation that touches
//! a state profile: the exclusive profile lock and shutdown-signal handling.

pub mod lock;
pub mod signals;

/// How a `play` session ended (design doc M5 §6.5): cleanly, or because a
/// shutdown signal was recorded. `main.rs` turns this into the process's
/// exit status; the signal case follows the shell convention of `128 + n`
/// (the same status `bash` reports for a job a signal killed), so a wrapper
/// script or `$?` check downstream sees the familiar number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunOutcome {
    Completed,
    Signalled(i32),
}

impl RunOutcome {
    /// Saturates at 255 rather than wrapping a `u8`: a signal number this
    /// large is not one Unix delivers, but the arithmetic must still produce
    /// a legal exit status rather than an arbitrary wrapped one.
    pub fn exit_status(self) -> u8 {
        match self {
            Self::Completed => 0,
            Self::Signalled(number) => {
                u8::try_from(128_i32.saturating_add(number).max(0)).unwrap_or(u8::MAX)
            }
        }
    }
}
