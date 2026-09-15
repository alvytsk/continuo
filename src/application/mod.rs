//! Playback logic with no terminal underneath it: resolving what to play -
//! a podcast episode's enclosure, or a `play` argument's source - and
//! routing a decoded key command to the engine, coalescing an arrow-key
//! burst into one seek. Nothing here prints, reads a key, or draws a frame,
//! so any front end can call into it and decide for itself how to show the
//! result.

pub mod podcast;
pub mod seek;
pub mod source;
