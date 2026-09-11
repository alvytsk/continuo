//! The application seam a future TUI (M5) reuses (design doc §6.6): it
//! returns values, prints nothing, and contains no `block_on`. Read-only
//! functions take no [`crate::http::service::HttpService`] at all — only
//! `subscribe`, `unsubscribe` and the two `refresh` functions (Tasks 12 and
//! 13, added to this same file) ever touch the network.
//!
//! This task builds the three read-only functions: [`list_feeds`],
//! [`list_episodes`] and [`resolve_episode`]. Each calls
//! [`SubscriptionStore::read_snapshot`], never [`SubscriptionStore::load`]
//! — that split is what keeps §8.4's "listings change no files" true, since
//! `load` is free to quarantine a malformed `subscriptions.json` and a
//! listing must never do that merely by being asked to display something.

use std::num::NonZeroUsize;
use std::time::Duration;

use time::OffsetDateTime;

use crate::feed::cache::{CacheStore, CachedEpisode};
use crate::feed::error::FeedError;
use crate::media::id::MediaId;
use crate::media::source::SourceLocation;
use crate::persistence::model::PersistedCheckpoint;
use crate::persistence::store::StateSnapshot;
use crate::subscription::model::Subscription;
use crate::subscription::store::{SubscriptionSnapshot, SubscriptionStore};

/// One row of `continuo feeds` (§6.1, §6.6). `episodes` and
/// `last_refreshed_at` are both `None` for a subscription that has never
/// been refreshed — the normal state right after `subscribe`, not an error.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedSummary {
    pub slug: String,
    pub title: Option<String>,
    /// `None` when there is no usable cache yet.
    pub episodes: Option<usize>,
    pub last_refreshed_at: Option<OffsetDateTime>,
}

/// One row of `continuo episodes` (§6.1, §6.6). `index` is 1-based and
/// contiguous over the retained list (§1.2) — the same index
/// [`resolve_episode`] accepts.
#[derive(Clone, Debug, PartialEq)]
pub struct EpisodeRow {
    pub index: usize,
    pub media: MediaId,
    pub title: Option<String>,
    pub published: Option<OffsetDateTime>,
    /// The feed's own claim (`itunes:duration`), never decoder-confirmed
    /// (§2.2, §6.2).
    pub declared_duration: Option<Duration>,
    /// A separate column from [`Progress`] (§6.2): `false` for an item with
    /// identity but no usable enclosure, so a removed enclosure never hides
    /// existing progress.
    pub playable: bool,
    pub progress: Progress,
}

/// The progress cell's precedence, computed once as a value rather than
/// re-derived from rendered text (§6.2, §6.6). This is a **new** type,
/// distinct from [`crate::playback::event::Progress`].
#[derive(Clone, Debug, PartialEq)]
pub enum Progress {
    /// No checkpoint entry at all.
    None,
    Played,
    /// An estimated position — never presented as confirmed (§4's
    /// provenance rule).
    Estimated(Duration),
    /// A decoder-confirmed position.
    Established(Duration),
    /// An entry exists but carries neither `position` nor `estimated`.
    /// Distinct from [`Progress::None`] and must never collapse into it.
    Unknown,
}

/// §6.2's precedence table, exactly as the design spec's Step 3 code block
/// binds it: `completed` outranks `estimated`, which outranks `position`;
/// an entry with neither is [`Progress::Unknown`], distinct from no entry
/// at all ([`Progress::None`]).
fn progress_for(entry: Option<&PersistedCheckpoint>) -> Progress {
    let Some(c) = entry else {
        return Progress::None;
    };
    if c.completed {
        return Progress::Played;
    }
    if let Some(value) = c.estimated {
        return Progress::Estimated(value);
    }
    if let Some(value) = c.position {
        return Progress::Established(value);
    }
    Progress::Unknown
}

/// Looks up `slug` in an already-decoded snapshot, returning an owned clone
/// or [`FeedError::UnknownSlug`]. A missing `subscriptions.json` decodes to
/// an empty snapshot (§5.1), so a slug that cannot exist reaches this
/// function rather than a storage fault (§6.3). Private: Tasks 12 and 13
/// consume it inside this same file.
fn find_subscription(
    snapshot: &SubscriptionSnapshot,
    slug: &str,
) -> Result<Subscription, FeedError> {
    snapshot
        .subscriptions
        .iter()
        .find(|subscription| subscription.slug == slug)
        .cloned()
        .ok_or_else(|| FeedError::UnknownSlug {
            slug: slug.to_string(),
        })
}

/// `continuo feeds` (§6.1, §6.6). Iterates subscription order (never
/// re-sorted), uses each subscription's own durable `title` — never the
/// cache's, which can go stale the moment a feed's title changes upstream
/// and is only refreshed on the next `refresh` — and reports a missing
/// cache as a zero-information row rather than an error, since `feeds` must
/// stay usable the instant a feed is subscribed and before its first
/// refresh (§6.4: "`feeds` exits zero for a merely missing cache"). Any
/// other cache failure (corrupt, parser-mismatched) propagates, since those
/// are not "never refreshed" and must not be silently downgraded into that
/// state.
pub fn list_feeds(
    subs: &SubscriptionStore,
    cache: &CacheStore,
) -> Result<Vec<FeedSummary>, FeedError> {
    let snapshot = subs.read_snapshot()?;
    snapshot
        .subscriptions
        .iter()
        .map(|subscription| match cache.read(subscription) {
            Ok(cached) => Ok(FeedSummary {
                slug: subscription.slug.clone(),
                title: subscription.title.clone(),
                episodes: Some(cached.episodes.len()),
                last_refreshed_at: Some(cached.last_refreshed_at),
            }),
            Err(FeedError::CacheMissing { .. }) => Ok(FeedSummary {
                slug: subscription.slug.clone(),
                title: subscription.title.clone(),
                episodes: None,
                last_refreshed_at: None,
            }),
            Err(error) => Err(error),
        })
        .collect()
}

/// `continuo episodes <slug> [-n N]` (§6.1, §6.6). Rows are built by
/// enumerating the cache's episodes in stored order — never re-sorted by
/// date or title (§1.2) — assigning contiguous 1-based indices before
/// `limit` ever truncates the list, so `-n N` changes what is displayed,
/// never what an index means (§6.3).
pub fn list_episodes(
    subs: &SubscriptionStore,
    cache: &CacheStore,
    state: &StateSnapshot,
    slug: &str,
    limit: Option<NonZeroUsize>,
) -> Result<Vec<EpisodeRow>, FeedError> {
    let snapshot = subs.read_snapshot()?;
    let subscription = find_subscription(&snapshot, slug)?;
    let cached = cache.read(&subscription)?;

    let rows = cached
        .episodes
        .iter()
        .enumerate()
        .map(|(position, cached_episode)| {
            let episode = cached_episode.episode();
            let progress = progress_for(state.entry_for(&episode.id));
            EpisodeRow {
                index: position + 1,
                media: episode.id,
                title: episode.title,
                published: episode.published,
                declared_duration: episode.declared_duration,
                playable: episode.source.is_some(),
                progress,
            }
        })
        .take(limit.map_or(usize::MAX, NonZeroUsize::get))
        .collect();
    Ok(rows)
}

/// `continuo play <slug> <index>`'s resolution step (§6.4, §6.5, §6.6): the
/// only thing this function does is decide *which* `(MediaId,
/// SourceLocation)` pair to play, or that none exists — no `EngineHandle`,
/// no `AudioOutput` and no `HttpService` are constructed here or by any
/// caller of an `Err` from this function. The identity returned is always
/// the cached podcast [`MediaId`], exactly as `bind_feed` produced it;
/// nothing here ever calls `resolve_source` on the enclosure to derive one.
pub fn resolve_episode(
    subs: &SubscriptionStore,
    cache: &CacheStore,
    slug: &str,
    index: usize,
) -> Result<(MediaId, SourceLocation), FeedError> {
    let snapshot = subs.read_snapshot()?;
    let subscription = find_subscription(&snapshot, slug)?;
    let cached = cache.read(&subscription)?;
    let retained = cached.episodes.len();

    if index == 0 || index > retained {
        return Err(FeedError::IndexOutOfRange {
            slug: slug.to_string(),
            index,
            retained,
        });
    }
    // `index` is 1-based and already checked against `retained` above.
    let cached_episode = &cached.episodes[index - 1];
    warn_on_declared_type(slug, index, cached_episode);

    let episode = cached_episode.episode();
    match episode.source {
        Some(source) => Ok((episode.id, source)),
        None => Err(FeedError::NotPlayable {
            slug: slug.to_string(),
            index,
            title: episode.title.unwrap_or_else(|| "(untitled)".to_string()),
        }),
    }
}

/// Safe length/mime diagnostics at resolution (§2.1, §6.4): a declared type
/// is a claim, not evidence, so a present non-audio declaration is logged
/// and playback proceeds regardless — it never turns into `NotPlayable`.
/// Deliberately logs only the two enclosure fields that are already safe to
/// surface, never the full [`CachedEpisode`], which can carry an
/// enclosure URL and a title.
fn warn_on_declared_type(slug: &str, index: usize, cached_episode: &CachedEpisode) {
    tracing::debug!(
        slug,
        index,
        enclosure_length = cached_episode.enclosure_length,
        enclosure_mime = cached_episode.enclosure_mime.as_deref(),
        "resolved episode enclosure diagnostics"
    );
    if let Some(mime) = cached_episode.enclosure_mime.as_deref() {
        let top_level = mime.split('/').next().unwrap_or(mime);
        if !top_level.eq_ignore_ascii_case("audio") {
            tracing::warn!(
                slug,
                index,
                mime,
                "declared enclosure type is not audio/*; a declared type is a claim, not evidence, so playback proceeds"
            );
        }
    }
}
