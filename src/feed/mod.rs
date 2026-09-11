//! Feeds: parse-layer values and the parser that produces them.
//!
//! Caching and the episode binding land in later M4 tasks. What is here is
//! deliberately pure: [`parse::parse_feed`] turns bytes plus a retrieval URL
//! into [`model::ParsedFeed`], and touches no subscription, cache or socket.

pub mod error;
pub mod model;
pub mod parse;
