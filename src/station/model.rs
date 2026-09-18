//! The station domain type and its slug derivation (M7.1 §4).

use std::collections::BTreeSet;

use time::OffsetDateTime;
use url::Url;

use crate::feed::error::FeedError;
use crate::http::source::StationIdentity;
use crate::media::id::MediaId;
use crate::subscription::model::choose_slug;

/// A saved station. `identity` is `None` for a station added after a
/// retryable probe failure — an unverified *candidate*, never a claim that
/// the URL is a station (§3 R1). Only a probe returning `Accepted::Live`
/// promotes it.
///
/// A station carries no opaque id: unlike a feed it owns no files, so its
/// identity is its URL and its handle is its slug (§4).
#[derive(Clone, Debug, PartialEq)]
pub struct Station {
    pub slug: String,
    pub url: Url,
    /// The queue identity for `url` (`MediaId::RemoteUrl`), derived once by
    /// `application::source::resolve_source` when the station is added and
    /// stored rather than recomputed. `sync_queue` matches browser rows to
    /// queue entries by this alone, so a second derivation that ever
    /// disagreed would silently break the queued tick (§7).
    pub media: MediaId,
    pub identity: Option<StationIdentity>,
    pub added_at: OffsetDateTime,
    pub probed_at: Option<OffsetDateTime>,
}

/// Chooses a slug for a station (§4), deriving from the station's ICY name
/// and falling back to the URL's host. Delegates to
/// [`crate::subscription::model::choose_slug`] so both lists spell a slug
/// the same way; `occupied` is the *station* namespace alone, so a station
/// and a feed may share a slug without either noticing.
pub fn choose_station_slug(
    name: Option<&str>,
    url: &Url,
    occupied: &BTreeSet<String>,
) -> Result<String, FeedError> {
    choose_slug(name, url, None, occupied)
}

/// Re-exported so `store` validates slugs exactly as `subscriptions.json`
/// does.
pub(crate) use crate::subscription::model::validate_slug as validate_station_slug;
