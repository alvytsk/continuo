//! Cross-cutting concerns shared by every `continuo` invocation that touches
//! a state profile: today, the exclusive lock (Task 11); later tasks add
//! signal installation, session logging, and terminal setup here.

pub mod lock;
