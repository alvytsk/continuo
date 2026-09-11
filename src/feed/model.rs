//! Parse-layer values (§2.1): what the XML said, and nothing about
//! subscriptions.
//!
//! A [`ParsedItem`] is deliberately not a [`crate::media::Episode`]. Binding an
//! item to an identity needs a `FeedId`, which only the subscription layer
//! holds, so keeping these types identity-free is what lets
//! [`crate::feed::parse::parse_feed`] stay a pure bytes-to-values function
//! testable against fixtures alone. [`crate::feed::episode::bind_feed`] is
//! where identity is added.

use std::time::Duration;

use time::OffsetDateTime;
use url::Url;

/// One feed document, in **feed document order** (§1.2). Items are never
/// re-sorted — not by date, not by title.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedFeed {
    pub title: Option<String>,
    pub site_link: Option<Url>,
    pub items: Vec<ParsedItem>,
}

/// One `channel/item` (RSS) or `feed/entry` (Atom). Every field starts `None`:
/// absence is the normal case for most of them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParsedItem {
    pub guid: Option<String>,
    pub link: Option<Url>,
    pub enclosure: Option<Enclosure>,
    pub title: Option<String>,
    pub published: Option<OffsetDateTime>,
    pub declared_duration: Option<Duration>,
}

/// The `<enclosure>` element, modelled faithfully rather than narrowed.
///
/// Only `url` ever reaches an `Episode`; `length` and `mime_type` are kept
/// because a declared type is a claim worth logging, not evidence worth
/// acting on.
#[derive(Clone, Debug, PartialEq)]
pub struct Enclosure {
    pub url: Url,
    pub length: Option<u64>,
    pub mime_type: Option<String>,
}
