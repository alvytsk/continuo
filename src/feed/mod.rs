//! Feeds: parse-layer values, the parser that produces them, the binding
//! that turns them into real episode identities, and the disposable cache
//! that makes a bound feed durable.
//!
//! [`parse::parse_feed`] is deliberately pure: it turns bytes plus a
//! retrieval URL into [`model::ParsedFeed`], and touches no subscription,
//! cache or socket. [`episode::bind_feed`] is the join — it takes a
//! `FeedId`, which only the subscription layer holds, and produces
//! [`crate::media::Episode`] values with real `MediaId::PodcastEpisode`
//! identities. [`cache::CacheStore`] is where those bound values land: one
//! JSON file per feed, atomically replaced, never touched by a read.

pub mod cache;
pub mod episode;
pub mod error;
pub mod model;
pub mod parse;
