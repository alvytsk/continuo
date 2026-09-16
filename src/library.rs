//! The application seam a future TUI (M5) reuses (design doc §6.6): it
//! returns values, prints nothing, and contains no `block_on`. Read-only
//! functions take no [`crate::http::service::HttpService`] at all — only
//! `subscribe`, `unsubscribe` and the two `refresh` functions (Tasks 12 and
//! 13, added to this same file) ever touch the network.
//!
//! Task 11 built the three read-only functions: [`list_feeds`],
//! [`list_episodes`] and [`resolve_episode`]. Each calls
//! [`SubscriptionStore::read_snapshot`], never [`SubscriptionStore::load`]
//! — that split is what keeps §8.4's "listings change no files" true, since
//! `load` is free to quarantine a malformed `subscriptions.json` and a
//! listing must never do that merely by being asked to display something.
//!
//! Task 12 adds the two mutating functions, [`subscribe`] and
//! [`unsubscribe`]. Both call [`SubscriptionStore::load`] through the
//! private [`load_mutating`] helper, which narrows `load`'s outcomes down to
//! the two safe ones — `Loaded` and `Missing`, both writable — and turns
//! every other [`crate::persistence::store::LoadReason`] into a visible
//! [`FeedError::SubscriptionsUnreadable`] rather than mutating on top of a
//! file that was just quarantined, preserved unreadable, or an unsupported
//! version (§5.1, §5.6). §5.3's commit order is the other half of this
//! task: `subscribe` writes the cache before the subscription, and
//! `unsubscribe` writes the subscription before deleting the cache, so a
//! crash between the two steps is always recoverable and always reported
//! (never a bare bool — [`FollowupFailure`] carries the cause).
//!
//! Task 13 adds [`refresh`] and [`refresh_all`], completing the application
//! layer. `refresh` fetches unconditionally when the cache is missing,
//! corrupt or `parser_version`-mismatched, and conditionally otherwise
//! (§5.4); a 304 preserves the cached episodes, `fetched_from`,
//! `last_fetched_at` and skipped count, merging only the validators and
//! advancing `last_refreshed_at` (§3.3, §5.2). The cache commits before any
//! subscription update, and `subscriptions.json` is rewritten only when the
//! reconciled title or `fetch_url` actually differs from what is already
//! stored (§5.1, §5.3) — the private `refresh_work` does the per-feed work
//! and returns `Err` only for a failure that precedes any commit;
//! `refresh_one` turns that `Err` into `RefreshOutcome::Failed` with the
//! slug already known, so a per-feed failure is never swallowed and an
//! enumeration failure never has to invent one. `refresh_all` walks its
//! loaded snapshot strictly sequentially, threading the same
//! `&mut SubscriptionSnapshot` through every feed so that a later feed can
//! never silently commit an earlier feed's failed subscription update.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::time::Duration;

use time::OffsetDateTime;
use url::Url;

use crate::feed::cache::{CacheStore, CachedEpisode, CachedFeed};
use crate::feed::episode::bind_feed;
use crate::feed::error::FeedError;
use crate::feed::parse::{ParseWarning, parse_feed};
use crate::http::document::{DocumentOutcome, DocumentRequest};
use crate::http::error::{RemoteFailure, redact_url};
use crate::http::service::HttpService;
use crate::media::id::{FeedId, MediaId, NormalizedUrl};
use crate::media::source::SourceLocation;
use crate::persistence::model::PersistedCheckpoint;
use crate::persistence::store::{LoadReason, StateSnapshot};
use crate::subscription::model::{Subscription, choose_slug, new_feed_id, validate_slug};
use crate::subscription::store::{SubscriptionLoad, SubscriptionSnapshot, SubscriptionStore};

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

/// One candidate for the later on-demand episode browser (§6.6, plan
/// decision 14): unlike [`EpisodeRow`], carries the enclosure URL itself,
/// since choosing what to queue next needs it and a row built only for
/// display deliberately does not.
#[derive(Clone, Debug, PartialEq)]
pub struct EpisodeCandidate {
    pub media: MediaId,
    pub enclosure: Option<Url>,
    pub title: Option<String>,
    pub declared_duration: Option<Duration>,
    pub published: Option<OffsetDateTime>,
}

/// Candidates for `slug`, in the same stored (never re-sorted) order as
/// [`list_episodes`], from which this mirrors every step but the enclosure
/// column.
pub fn episode_candidates(
    subs: &SubscriptionStore,
    cache: &CacheStore,
    slug: &str,
) -> Result<Vec<EpisodeCandidate>, FeedError> {
    let snapshot = subs.read_snapshot()?;
    let subscription = find_subscription(&snapshot, slug)?;
    let cached = cache.read(&subscription)?;

    Ok(cached
        .episodes
        .iter()
        .map(|cached_episode| {
            let episode = cached_episode.episode();
            EpisodeCandidate {
                media: episode.id,
                enclosure: cached_episode.enclosure_url.clone(),
                title: episode.title,
                declared_duration: episode.declared_duration,
                published: episode.published,
            }
        })
        .collect())
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

/// Work that committed, followed by a step that did not. Carries the cause,
/// never a bare bool — §5.3's message must say exactly what did and did not
/// happen, and a bool cannot carry that (§6.6).
#[derive(Debug)]
pub struct FollowupFailure {
    pub step: FollowupStep,
    pub error: FeedError,
}

/// Which side of a two-step commit failed after the first side already
/// landed (§5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowupStep {
    SaveSubscription,
    RemoveCache,
}

/// `continuo subscribe <url> [--as slug]`'s result (§6.1, §6.6).
#[derive(Debug)]
pub struct SubscribeOutcome {
    pub slug: String,
    pub feed_id: FeedId,
    pub title: Option<String>,
    pub retained: usize,
    pub skipped: usize,
    pub followup: Option<FollowupFailure>,
}

/// `continuo unsubscribe <slug>`'s result (§6.1, §6.6).
#[derive(Debug)]
pub struct UnsubscribeOutcome {
    pub slug: String,
    pub followup: Option<FollowupFailure>,
}

/// The only entry point [`subscribe`] and [`unsubscribe`] use to read
/// `subscriptions.json` (§5.1, §5.6). Unlike [`list_feeds`] and
/// [`list_episodes`], a mutating command must never build a new snapshot on
/// top of a file [`SubscriptionStore::load`] just quarantined, preserved
/// unreadable, or refused as an unsupported version — every
/// [`crate::persistence::store::LoadReason`] but `Loaded` and `Missing`
/// (both `writable`) becomes a visible [`FeedError::SubscriptionsUnreadable`]
/// instead, naming the quarantine path where there is one.
fn load_mutating(subs: &SubscriptionStore) -> Result<SubscriptionSnapshot, FeedError> {
    let SubscriptionLoad {
        snapshot,
        writable,
        reason,
    } = subs.load();

    if writable && matches!(reason, LoadReason::Loaded | LoadReason::Missing) {
        return Ok(snapshot);
    }

    let reason = match reason {
        LoadReason::Quarantined { moved_to } => format!(
            "subscriptions file was malformed and has been moved aside to {}; \
             subscribe, unsubscribe and refresh are unavailable until this is resolved",
            moved_to.display()
        ),
        LoadReason::QuarantineFailed => "subscriptions file is malformed and could not be \
             moved aside; subscribe, unsubscribe and refresh are unavailable"
            .to_string(),
        LoadReason::Unreadable => "subscriptions file could not be read; subscribe, \
             unsubscribe and refresh are unavailable"
            .to_string(),
        LoadReason::UnsupportedVersion { found } => format!(
            "subscriptions file is schema version {found}, which this build does not \
             support; subscribe, unsubscribe and refresh are unavailable"
        ),
        // Unreachable given the `writable` guard above: kept so this match
        // stays exhaustive over every `LoadReason` rather than relying on a
        // wildcard arm to paper over a future variant.
        LoadReason::Loaded | LoadReason::Missing => {
            "subscriptions file could not be prepared for writing".to_string()
        }
    };
    Err(FeedError::SubscriptionsUnreadable { reason })
}

/// Parses `url` and validates it against the same public-source bar
/// [`crate::http::document`] holds a feed's `origin` to (§6.7): HTTP(S)
/// with a host, and no embedded userinfo. Re-expressed here, rather than
/// reused from that module, because this check must run — and be able to
/// reject a bad URL — *before* any [`HttpService`] call, including the ones
/// `AlreadySubscribed` and slug derivation need first.
fn validate_public_url(url: &str) -> Result<Url, FeedError> {
    let invalid = |reason: &'static str| {
        FeedError::from(RemoteFailure::InvalidSource {
            input: redact_url(url),
            reason,
        })
    };
    let parsed = Url::parse(url).map_err(|_| invalid("not a valid URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(invalid("expected http(s) with a host"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid("URLs with embedded credentials are not supported"));
    }
    Ok(parsed)
}

/// Normalizes a **requested** URL for the `AlreadySubscribed` comparison
/// (§6.7): scheme, host and default port normalized, fragment removed,
/// query serialization preserved. Called only after [`validate_public_url`]
/// has already accepted `url`, so failure here is not expected in practice,
/// but the type system does not know that.
fn normalize_requested(url: &str) -> Result<NormalizedUrl, FeedError> {
    NormalizedUrl::parse(url).map_err(|_| {
        FeedError::from(RemoteFailure::InvalidSource {
            input: redact_url(url),
            reason: "expected http(s) with a host",
        })
    })
}

/// Normalizes an already-stored `fetch_url` the same way, for the other
/// side of the `AlreadySubscribed` comparison. A stored `fetch_url` already
/// passed §5.6's validation when the snapshot was loaded, so failure here
/// would mean the snapshot is corrupt in a way `load` missed.
fn normalize_stored(url: &Url) -> Result<NormalizedUrl, FeedError> {
    NormalizedUrl::parse(url.as_str()).map_err(|_| FeedError::SubscriptionsUnreadable {
        reason: "a stored fetch_url could not be normalized".to_string(),
    })
}

/// Rebuilds a `Url` from a [`NormalizedUrl`], for the case where `subscribe`
/// has no `permanent_url` and must fall back to the normalized requested URL
/// as the new subscription's `fetch_url`. Infallible in practice: a
/// `NormalizedUrl` is always a serialized, previously-valid `Url`.
fn url_from_normalized(normalized: &NormalizedUrl) -> Result<Url, FeedError> {
    Url::parse(normalized.as_str()).map_err(|_| FeedError::SubscriptionsUnreadable {
        reason: "cannot rebuild a normalized fetch URL".to_string(),
    })
}

/// §6.7's `AlreadySubscribed` check: the **requested** URL against each
/// stored `fetch_url`, both normalized. Redirect targets never participate
/// — this is called before any fetch happens, so there is no `final_url` to
/// compare against yet, and there must not be one.
fn already_subscribed(
    snapshot: &SubscriptionSnapshot,
    requested: &NormalizedUrl,
) -> Result<Option<String>, FeedError> {
    for existing in &snapshot.subscriptions {
        let normalized = normalize_stored(&existing.fetch_url)?;
        if &normalized == requested {
            return Ok(Some(existing.slug.clone()));
        }
    }
    Ok(None)
}

/// Mints a [`FeedId`] not already present in `occupied` (§2.3). A collision
/// against 128 bits of OS randomness is not realistically reachable; this
/// loop exists so "unused" is an assertion the code makes, not an
/// assumption it relies on.
fn unused_feed_id(occupied: &BTreeSet<String>) -> Result<FeedId, FeedError> {
    loop {
        let candidate = new_feed_id()?;
        if !occupied.contains(candidate.as_str()) {
            return Ok(candidate);
        }
    }
}

/// Logs [`bind_feed`]'s warnings at the point a feed is first subscribed.
/// Every [`ParseWarning`] field is already redaction-safe by construction
/// (§7.2: no GUID, no URL, no title) — only the ordinal and the category are
/// logged, which is enough to find the item in the feed.
fn log_bound_warnings(slug: &str, warnings: &[ParseWarning]) {
    for warning in warnings {
        tracing::warn!(
            slug,
            item = warning.item,
            kind = ?warning.kind,
            "feed parse warning while subscribing"
        );
    }
}

/// `continuo subscribe <url> [--as slug]` (§6.1, §6.6, §6.7). Fetches
/// unconditionally — a subscribe has no cache to revalidate against, so an
/// `Unchanged` outcome can never legitimately occur here and is reported as
/// [`RemoteFailure::UnsolicitedNotModified`] rather than fabricating a
/// cache entry from nothing.
///
/// Preflight (no I/O beyond the subscription read): the requested URL is
/// validated and normalized, checked against every stored `fetch_url` for
/// `AlreadySubscribed`, and an explicit `--as` alias is validated and
/// checked for collision — all before the one network call this function
/// makes. A derived slug collision is resolved only after the fetch, once a
/// title is known, and takes the first free `-2`, `-3`, … suffix.
///
/// Commit order (§5.3): the cache is saved before the subscription. A
/// failure saving the subscription after the cache already landed is
/// reported as `followup`, never silently dropped — the cache file is left
/// behind, unreferenced, recoverable by a future successful subscribe.
pub async fn subscribe(
    http: &HttpService,
    subs: &SubscriptionStore,
    cache: &CacheStore,
    url: &str,
    slug: Option<&str>,
) -> Result<SubscribeOutcome, FeedError> {
    let mut snapshot = load_mutating(subs)?;

    let origin = validate_public_url(url)?;
    let requested = normalize_requested(url)?;

    if let Some(existing_slug) = already_subscribed(&snapshot, &requested)? {
        return Err(FeedError::AlreadySubscribed {
            slug: existing_slug,
        });
    }

    let occupied_slugs: BTreeSet<String> = snapshot
        .subscriptions
        .iter()
        .map(|subscription| subscription.slug.clone())
        .collect();
    if let Some(explicit) = slug {
        validate_slug(explicit)?;
        if occupied_slugs.contains(explicit) {
            return Err(FeedError::SlugTaken {
                slug: explicit.to_string(),
            });
        }
    }

    let fetched = http
        .fetch_document(DocumentRequest {
            origin: origin.clone(),
            validators: None,
        })
        .await?;
    let (bytes, final_url, permanent_url, validators) = match fetched {
        DocumentOutcome::Unchanged { .. } => {
            return Err(RemoteFailure::UnsolicitedNotModified.into());
        }
        DocumentOutcome::Fetched {
            bytes,
            final_url,
            permanent_url,
            validators,
            ..
        } => (bytes, final_url, permanent_url, validators),
    };

    let parsed = parse_feed(&bytes, &final_url)?;
    let now = subs.now();

    let occupied_ids: BTreeSet<String> = snapshot
        .subscriptions
        .iter()
        .map(|subscription| subscription.feed_id.as_str().to_string())
        .collect();
    let chosen_slug = choose_slug(parsed.feed.title.as_deref(), &origin, slug, &occupied_slugs)?;
    let feed_id = unused_feed_id(&occupied_ids)?;

    let bound = bind_feed(&feed_id, parsed);
    log_bound_warnings(&chosen_slug, &bound.warnings);
    let title = bound.title.clone();
    let cached = CachedFeed::from_bound(&feed_id, bound, final_url, validators, now);

    let fetch_url = match permanent_url {
        Some(permanent_url) => permanent_url,
        None => url_from_normalized(&requested)?,
    };

    let subscription = Subscription {
        feed_id,
        slug: chosen_slug,
        title,
        fetch_url,
        added_at: now,
    };

    // Commit seam (design doc §5.3, brief Step 3): the cache lands first,
    // so a failure saving the subscription leaves only an unreferenced
    // cache file — never a subscription pointing at nothing.
    cache.save(&subscription, &cached)?;
    let retained = cached.episodes.len();
    let skipped = cached.skipped_items;
    let slug = subscription.slug.clone();
    let feed_id = subscription.feed_id.clone();
    let title = subscription.title.clone();
    snapshot.subscriptions.push(subscription);
    let followup = subs.save(&snapshot).err().map(|error| FollowupFailure {
        step: FollowupStep::SaveSubscription,
        error,
    });
    Ok(SubscribeOutcome {
        slug,
        feed_id,
        title,
        retained,
        skipped,
        followup,
    })
}

/// `continuo unsubscribe <slug>` (§6.1, §6.6). Keeps checkpoints: there is
/// no [`crate::persistence::store::StateStore`] argument and this function
/// never deletes one. Resubscribing later mints a fresh `FeedId` (§1.6), so
/// any checkpoint keyed to the old one is simply orphaned, never reattached.
///
/// Commit order (§5.3): the subscription is removed before the cache is
/// deleted. A subscription-save failure returns before cache deletion is
/// even attempted; an already-missing cache counts as successful cleanup.
pub fn unsubscribe(
    subs: &SubscriptionStore,
    cache: &CacheStore,
    slug: &str,
) -> Result<UnsubscribeOutcome, FeedError> {
    let mut snapshot = load_mutating(subs)?;
    let subscription = find_subscription(&snapshot, slug)?;
    snapshot
        .subscriptions
        .retain(|entry| entry.feed_id != subscription.feed_id);
    subs.save(&snapshot)?;
    let followup = cache
        .remove(&subscription.feed_id)
        .err()
        .map(|error| FollowupFailure {
            step: FollowupStep::RemoveCache,
            error,
        });
    Ok(UnsubscribeOutcome {
        slug: slug.to_owned(),
        followup,
    })
}

/// `continuo refresh [<slug>]`'s result (§6.1, §6.6).
#[derive(Debug)]
pub enum RefreshOutcome {
    /// A 304. The cache was revalidated; a permanent redirect may still
    /// need committing.
    Unchanged {
        slug: String,
        url_moved: Option<String>,
        followup: Option<FollowupFailure>,
    },
    /// A 200 that parsed and was cached. An identical body still reports
    /// this: §5.2 keeps no fingerprint, so M4 promises fetch status, never
    /// content hashing.
    Updated {
        slug: String,
        retained: usize,
        skipped: usize,
        url_moved: Option<String>,
        followup: Option<FollowupFailure>,
    },
    /// Nothing committed: the fetch, the parse, or the cache write failed.
    Failed { slug: String, error: FeedError },
}

/// The per-feed work behind both [`refresh`] and [`refresh_all`] (§5.3,
/// §5.4, §6.6). Returns `Err` only for a failure that precedes any commit —
/// reading a cache that failed for a reason other than "no usable
/// representation yet", or the fetch itself — so [`refresh_one`] can turn it
/// into a [`RefreshOutcome::Failed`] without inventing anything.
///
/// Cache-state selection (§5.4): a missing, corrupt or parser-mismatched
/// cache is treated as "nothing to revalidate against" and forces an
/// unconditional fetch, rather than propagating that error — the whole
/// point of an unconditional refresh is to recover from exactly those
/// states. Any other cache read failure (a filesystem fault) is not
/// something a refetch can fix and is propagated as-is.
///
/// A 200 parses with *that* response's `final_url`, binds using the
/// subscription's existing `feed_id` (never a new one) and stamps both
/// timestamps from `subs.now()`. A 304 requires the old cache — the
/// transport layer already rejects an unsolicited 304 when no conditional
/// header was sent at all, but a 304 answered despite the cache being
/// unusable here reaches this function with `old: None`, and is rejected
/// the same way rather than fabricating a `CachedFeed` from nothing.
///
/// The cache save happens before any subscription reconciliation, and its
/// failure returns `Err` here without touching `snapshot` at all (§5.3).
/// The reconciled title and `fetch_url` — "the cached title and
/// `permanent_url`, if any" — are compared against the subscription's
/// current fields; if neither differs, `subscriptions.json` is not
/// rewritten at all. If either does, the batch's `snapshot` is updated only
/// after `subs.save` itself succeeds, so a later feed in the same batch can
/// never build on top of a subscription update that did not actually land.
async fn refresh_work(
    http: &HttpService,
    subs: &SubscriptionStore,
    cache: &CacheStore,
    snapshot: &mut SubscriptionSnapshot,
    index: usize,
) -> Result<RefreshOutcome, FeedError> {
    let subscription = snapshot.subscriptions[index].clone();

    let old = match cache.read(&subscription) {
        Ok(feed) => Some(feed),
        Err(
            FeedError::CacheMissing { .. }
            | FeedError::CacheCorrupt { .. }
            | FeedError::CacheParserMismatch { .. },
        ) => None,
        Err(error) => return Err(error),
    };

    let fetched = http
        .fetch_document(DocumentRequest {
            origin: subscription.fetch_url.clone(),
            validators: old.as_ref().map(|feed| feed.validators.clone()),
        })
        .await?;

    let now = subs.now();
    let (cached, permanent_url, freshly_parsed) = match fetched {
        DocumentOutcome::Fetched {
            bytes,
            final_url,
            permanent_url,
            validators,
            ..
        } => {
            let parsed = parse_feed(&bytes, &final_url)?;
            let bound = bind_feed(&subscription.feed_id, parsed);
            log_bound_warnings(&subscription.slug, &bound.warnings);
            let cached =
                CachedFeed::from_bound(&subscription.feed_id, bound, final_url, validators, now);
            (cached, permanent_url, true)
        }
        DocumentOutcome::Unchanged {
            permanent_url,
            validators,
            ..
        } => {
            // §3.3/§5.4: the transport layer already rejects a 304 sent
            // without a matching conditional header; this also rejects one
            // that arrives when this layer had no usable cache to
            // revalidate against, rather than fabricating a `CachedFeed`.
            let Some(mut cached) = old else {
                return Err(RemoteFailure::UnsolicitedNotModified.into());
            };
            cached.validators = validators;
            cached.last_refreshed_at = now;
            (cached, permanent_url, false)
        }
    };

    // Commit seam (§5.3): the cache lands first. Its failure returns here
    // without touching `snapshot`, leaving the durable subscription exactly
    // as it was.
    cache.save(&subscription, &cached)?;

    let mut updated_subscription = subscription.clone();
    updated_subscription.title = cached.title.clone();
    if let Some(permanent) = &permanent_url {
        updated_subscription.fetch_url = permanent.clone();
    }
    let url_moved = permanent_url.as_ref().map(|url| redact_url(url.as_str()));
    let changed = updated_subscription.title != subscription.title
        || updated_subscription.fetch_url != subscription.fetch_url;

    let followup = if changed {
        let mut candidate = snapshot.clone();
        candidate.subscriptions[index] = updated_subscription;
        match subs.save(&candidate) {
            Ok(()) => {
                *snapshot = candidate;
                None
            }
            Err(error) => Some(FollowupFailure {
                step: FollowupStep::SaveSubscription,
                error,
            }),
        }
    } else {
        None
    };

    let slug = subscription.slug;
    Ok(if freshly_parsed {
        RefreshOutcome::Updated {
            slug,
            retained: cached.episodes.len(),
            skipped: cached.skipped_items,
            url_moved,
            followup,
        }
    } else {
        RefreshOutcome::Unchanged {
            slug,
            url_moved,
            followup,
        }
    })
}

/// Converts [`refresh_work`]'s precommit `Err` into
/// [`RefreshOutcome::Failed`] with the slug already known — never an
/// invented one — so a per-feed failure is reported rather than aborting
/// the whole batch (§6.6).
async fn refresh_one(
    http: &HttpService,
    subs: &SubscriptionStore,
    cache: &CacheStore,
    snapshot: &mut SubscriptionSnapshot,
    index: usize,
) -> RefreshOutcome {
    let slug = snapshot.subscriptions[index].slug.clone();
    match refresh_work(http, subs, cache, snapshot, index).await {
        Ok(outcome) => outcome,
        Err(error) => RefreshOutcome::Failed { slug, error },
    }
}

/// `continuo refresh <slug>` (§6.1, §6.6). Loads the subscription snapshot
/// once, resolves `slug` to its index — reusing [`find_subscription`] for
/// the same `UnknownSlug` presentation every other single-feed lookup in
/// this file uses — and returns [`refresh_one`]'s outcome for it.
pub async fn refresh(
    http: &HttpService,
    subs: &SubscriptionStore,
    cache: &CacheStore,
    slug: &str,
) -> Result<RefreshOutcome, FeedError> {
    let mut snapshot = load_mutating(subs)?;
    let subscription = find_subscription(&snapshot, slug)?;
    let index = snapshot
        .subscriptions
        .iter()
        .position(|entry| entry.feed_id == subscription.feed_id)
        .ok_or_else(|| FeedError::UnknownSlug {
            slug: slug.to_string(),
        })?;
    Ok(refresh_one(http, subs, cache, &mut snapshot, index).await)
}

/// `continuo refresh` with no slug (§6.1, §6.6). Executes strictly
/// sequentially — no lock, no background task, no retries on a generic
/// network failure — for deterministic outcomes and bounded resource use.
///
/// The outer `Result` is reserved for a failure that prevents enumeration
/// itself: an unreadable `subscriptions.json` yields no slug to attach a
/// per-feed outcome to. Once enumeration succeeds, every per-feed result —
/// success or failure — travels in the returned vector; an empty valid
/// snapshot returns an empty, successful batch.
pub async fn refresh_all(
    http: &HttpService,
    subs: &SubscriptionStore,
    cache: &CacheStore,
) -> Result<Vec<RefreshOutcome>, FeedError> {
    let mut snapshot = load_mutating(subs)?;
    let mut results = Vec::with_capacity(snapshot.subscriptions.len());
    for index in 0..snapshot.subscriptions.len() {
        results.push(refresh_one(http, subs, cache, &mut snapshot, index).await);
    }
    Ok(results)
}
