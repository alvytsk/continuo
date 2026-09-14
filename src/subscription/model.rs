//! `FeedId` validation and generation, slug derivation and the
//! `Subscription` record (design spec §2.3, §2.5).

use std::collections::BTreeSet;

use time::OffsetDateTime;
use url::Url;

use crate::feed::error::FeedError;
use crate::media::id::FeedId;

/// A subscribed feed: an immutable identity, a presentation slug, the
/// last-known title, the URL it is fetched from, and when it was added.
///
/// Storage DTOs (serialization) are a later task's job.
#[derive(Clone, Debug, PartialEq)]
pub struct Subscription {
    pub feed_id: FeedId,
    pub slug: String,
    pub title: Option<String>,
    pub fetch_url: Url,
    pub added_at: OffsetDateTime,
}

/// Validates `value` as a `FeedId`: exactly 32 lowercase hex characters
/// (§2.3). This is the boundary every id read from `subscriptions.json` or a
/// cache file path must cross before it is trusted, so a traversal-shaped or
/// otherwise malformed string can never reach a filesystem path.
///
/// **On failure this returns `FeedError::SubscriptionsUnreadable` only as a
/// placeholder.** §7.2 fixes `FeedError`'s variant list and it has no
/// dedicated invalid-feed-id variant, so this function borrows the nearest
/// generic one purely to satisfy its signature. The returned error carries
/// no meaning beyond "this string failed feed-id validation" — it is
/// **not** a claim that anything is unreadable, and it is not a stand-in for
/// a corrupt-file or quarantine outcome either. Every caller MUST catch this
/// `Err` and re-map it to its own domain-appropriate error (a caller
/// validating `subscriptions.json` records maps it into that store's own
/// malformed/quarantine category per §5.6; a caller validating a cache path
/// maps it into `CacheCorrupt`, and so on). **Do not propagate this error
/// with `?`**, and never pattern-match on `SubscriptionsUnreadable` to tell
/// an unreadable file apart from a malformed one — this function's failures
/// and genuine subscription-file-unreadable failures share the variant by
/// necessity, not by relatedness, and are otherwise indistinguishable.
pub fn validate_feed_id(value: &str) -> Result<FeedId, FeedError> {
    let invalid = || FeedError::SubscriptionsUnreadable {
        reason: format!("feed id {value:?} is not 32 lowercase hex characters"),
    };
    let is_lower_hex = |byte: u8| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte);
    if value.len() != 32 || !value.bytes().all(is_lower_hex) {
        return Err(invalid());
    }
    FeedId::new(value.to_string()).map_err(|_| invalid())
}

/// Mints a fresh, random `FeedId` from OS randomness (§2.3). Never derived
/// from a URL: subscription identity is independent of the feed's mutable
/// locator.
pub fn new_feed_id() -> Result<FeedId, FeedError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| FeedError::SubscriptionsUnreadable {
        reason: "cannot generate subscription identifier".into(),
    })?;
    let value: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    validate_feed_id(&value)
}

/// Validates a slug against `^[a-z0-9-]{1,32}$` (§2.5, §5.6).
pub fn validate_slug(slug: &str) -> Result<(), FeedError> {
    let is_slug_byte =
        |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-';
    if !slug.is_empty() && slug.len() <= 32 && slug.bytes().all(is_slug_byte) {
        Ok(())
    } else {
        Err(FeedError::InvalidSlug {
            slug: slug.to_string(),
        })
    }
}

/// The ASCII-only slug-base transform (§2.5): ASCII letters are lowercased
/// and kept, ASCII digits are kept, and every other run of characters —
/// punctuation, whitespace, and non-ASCII letters — collapses to a single
/// `-`. Leading and trailing runs collapse away entirely rather than
/// leaving an edge `-`, and the result is at most 32 ASCII bytes. No
/// transliteration is attempted: a non-ASCII-only input yields `""`.
fn ascii_base(input: &str) -> String {
    let mut base = String::new();
    let mut pending_dash = false;
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !base.is_empty() {
                base.push('-');
            }
            pending_dash = false;
            base.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    base.truncate(32);
    base
}

/// Chooses a slug for a subscription (§2.5).
///
/// An explicit `--as` is validated against the slug pattern and rejected
/// outright on collision — never silently suffixed. Otherwise a slug is
/// derived from `title`; a title that yields no ASCII base (missing, or
/// written entirely in a non-ASCII script) falls back to the same transform
/// over `url`'s host with a leading `www.` stripped. A derived slug that
/// collides takes the first free `-2`, `-3`, … suffix, with the base
/// shortened so the total stays within 32 ASCII characters.
pub fn choose_slug(
    title: Option<&str>,
    url: &Url,
    explicit: Option<&str>,
    occupied: &BTreeSet<String>,
) -> Result<String, FeedError> {
    if let Some(explicit) = explicit {
        validate_slug(explicit)?;
        return if occupied.contains(explicit) {
            Err(FeedError::SlugTaken {
                slug: explicit.to_string(),
            })
        } else {
            Ok(explicit.to_string())
        };
    }

    let title_base = title.map(ascii_base).unwrap_or_default();
    let base = if title_base.is_empty() {
        let host = url.host_str().unwrap_or_default();
        let host = host.strip_prefix("www.").unwrap_or(host);
        ascii_base(host)
    } else {
        title_base
    };
    // Validated even for the fully-derived base: a host that somehow still
    // yields an empty base (the spec says this cannot happen, since `url`
    // stores IDN hosts as ASCII punycode) surfaces as `InvalidSlug` rather
    // than producing an invalid subscription.
    validate_slug(&base)?;

    if !occupied.contains(&base) {
        return Ok(base);
    }

    let mut suffix = 2_usize;
    loop {
        let suffix_str = format!("-{suffix}");
        let max_base_len = 32_usize.saturating_sub(suffix_str.len());
        let mut truncated = base.clone();
        truncated.truncate(max_base_len);
        let candidate = format!("{truncated}{suffix_str}");
        validate_slug(&candidate)?;
        if !occupied.contains(&candidate) {
            return Ok(candidate);
        }
        suffix += 1;
    }
}
