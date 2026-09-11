//! Feed errors.
//!
//! Parsing, caching and the HTTP document boundary land in later M4 tasks;
//! this module currently holds only [`error::FeedError`], the type every
//! later feed and subscription operation constructs from.

pub mod error;
