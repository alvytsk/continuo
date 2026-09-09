//! Finite remote media over HTTP.
//!
//! Nothing in this module owns a decoder, a resampler or a CPAL stream. It
//! produces encoded bytes and the evidence needed to classify them; the decode
//! worker does everything else.

pub mod channel;
pub mod error;
pub mod limits;
pub mod response;
