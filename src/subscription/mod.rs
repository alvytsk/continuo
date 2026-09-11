//! Subscription identity and value types.
//!
//! `FeedId` validation and generation, slug derivation, and the
//! `Subscription` record live in [`model`]. Durable storage and the
//! application layer land in later M4 tasks.

pub mod model;
