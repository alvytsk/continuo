//! Feeds: parse-layer values, the parser that produces them, and the
//! binding that turns them into real episode identities.
//!
//! Caching lands in a later M4 task. [`parse::parse_feed`] is deliberately
//! pure: it turns bytes plus a retrieval URL into [`model::ParsedFeed`], and
//! touches no subscription, cache or socket. [`episode::bind_feed`] is the
//! join — it takes a `FeedId`, which only the subscription layer holds, and
//! produces [`crate::media::Episode`] values with real
//! `MediaId::PodcastEpisode` identities.

pub mod episode;
pub mod error;
pub mod model;
pub mod parse;
