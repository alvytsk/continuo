//! Subscription identity and value types.
//!
//! `FeedId` validation and generation, slug derivation, and the
//! `Subscription` record live in [`model`]. Durable storage of the
//! subscription list lives in [`store`]. The application layer lands in
//! later M4 tasks.

pub mod model;
pub mod store;
