//! The application seam a future TUI (M5) reuses (design doc §6.6):
//! read-only functions that return values, print nothing, and touch neither
//! the network nor any file this build was not explicitly asked to read.

pub mod podcast;
