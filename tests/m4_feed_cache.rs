//! `feed::cache::CacheStore`: the atomic, disposable per-feed episode cache
//! (design doc §5.2, §5.4, §5.6).
//!
//! `Rig` (`tests/support/feeds.rs`) is the fixture every later M4 test suite
//! imports: a temp root, a fake clock, and the subscription/cache/state
//! stores that share it.

#[path = "support/feeds.rs"]
mod feeds;

use continuo::feed::error::FeedError;
use continuo::media::id::{EpisodeKey, FeedId, MediaId};
use continuo::subscription::model::Subscription;
use serde_json::json;

const RSS_ONE_ITEM: &[u8] = br#"<rss><channel><item><guid>id</guid></item></channel></rss>"#;
const RSS_TWO_ITEMS: &[u8] = br#"<rss><channel>
    <item><guid>id-1</guid></item>
    <item><guid>id-2</guid></item>
</channel></rss>"#;
const RSS_WITH_ENCLOSURE: &[u8] = br#"<rss><channel><item><guid>id</guid>
    <enclosure url="https://cdn.example.org/a.mp3"/></item></channel></rss>"#;

/// Step 1: seeding one feed produces a cache entry whose identity round-trips
/// exactly, and whose validators describe the same URL the subscription was
/// fetched from.
#[test]
fn cached_identity_and_validators_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RSS_ONE_ITEM, "https://example.org/feed")?;

    let cached = rig.cache.read(&sub)?;

    assert_eq!(cached.episodes.len(), 1);
    assert_eq!(cached.episodes[0].media_id.feed(), Some(&sub.feed_id));
    assert!(cached.episodes[0].episode().source.is_none());
    assert_eq!(cached.validators.url, sub.fetch_url);
    Ok(())
}

/// A cache that was never written is reported as `CacheMissing`, distinct
/// from every corruption outcome below, and reading it touches nothing.
#[test]
fn a_never_written_cache_is_reported_missing_not_corrupt() -> Result<(), Box<dyn std::error::Error>>
{
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RSS_ONE_ITEM, "https://example.org/feed")?;
    let id = FeedId::new(feeds::FEED_ID.to_string())?;
    rig.cache.remove(&id)?;

    let error = match rig.cache.read(&sub) {
        Ok(cached) => return Err(format!("expected CacheMissing, got {cached:?}").into()),
        Err(error) => error,
    };
    assert!(
        matches!(&error, FeedError::CacheMissing { slug } if *slug == sub.slug),
        "expected CacheMissing, got {error:?}"
    );

    // `remove` on an already-missing cache is success, not an error, so a
    // caller never has to check existence first.
    rig.cache.remove(&id)?;
    Ok(())
}

/// Round-tripping through JSON preserves the exact escaping `MediaId`
/// specifies for a GUID that itself contains `/` and other reserved bytes:
/// `:` stays literal, `/` becomes `%2F`, and nothing is duplicated.
#[test]
fn serialized_media_id_keeps_colon_literal_and_escapes_slash()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let xml =
        br#"<rss><channel><item><guid>https://example.org/p/987/</guid></item></channel></rss>"#;
    let sub = rig.seed(xml, "https://example.org/feed")?;

    let cached = rig.cache.read(&sub)?;
    let id = &cached.episodes[0].media_id;
    let rendered = id.to_string();

    assert!(rendered.starts_with("podcast:"));
    assert!(rendered.contains("guid:https:%2F%2Fexample.org%2Fp%2F987%2F"));
    assert_eq!(
        *id,
        MediaId::PodcastEpisode {
            feed: sub.feed_id.clone(),
            episode: EpisodeKey::resolve(Some("https://example.org/p/987/"), None, None)?,
        }
    );
    Ok(())
}

/// Reads the cache file's raw bytes as a `serde_json::Value`, applies
/// `mutate`, and writes the result back — bypassing `CacheStore::save`'s own
/// validation entirely, exactly as Step 5 prescribes: a corruption test must
/// inject the bad shape directly onto disk, since `save` would otherwise
/// reject it before it ever landed there.
fn corrupt(
    rig: &feeds::Rig,
    sub: &Subscription,
    mutate: impl FnOnce(&mut serde_json::Value),
) -> Result<(), Box<dyn std::error::Error>> {
    let path = rig.cache.path_for(&sub.feed_id)?;
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    mutate(&mut value);
    std::fs::write(&path, serde_json::to_vec_pretty(&value)?)?;
    Ok(())
}

/// Every §5.6 semantic-validation rejection is `CacheCorrupt`, never a panic
/// or a silent pass-through, and the corrupt file is left exactly as
/// written: `read` never deletes on a rejection.
fn assert_rejected_as_corrupt(
    rig: &feeds::Rig,
    sub: &Subscription,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = rig.cache.path_for(&sub.feed_id)?;
    let before = std::fs::read(&path)?;

    let error = match rig.cache.read(sub) {
        Ok(cached) => return Err(format!("expected CacheCorrupt, got {cached:?}").into()),
        Err(error) => error,
    };
    assert!(
        matches!(&error, FeedError::CacheCorrupt { slug, .. } if *slug == sub.slug),
        "expected CacheCorrupt, got {error:?}"
    );

    assert_eq!(
        std::fs::read(&path)?,
        before,
        "a rejected read must not modify the cache file"
    );
    Ok(())
}

#[test]
fn a_foreign_envelope_feed_id_is_corrupt() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RSS_ONE_ITEM, "https://example.org/feed")?;
    corrupt(&rig, &sub, |value| {
        value["feed_id"] = json!("ffffffffffffffffffffffffffffffff");
    })?;
    assert_rejected_as_corrupt(&rig, &sub)
}

#[test]
fn a_remote_url_media_id_is_corrupt() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RSS_ONE_ITEM, "https://example.org/feed")?;
    corrupt(&rig, &sub, |value| {
        value["episodes"][0]["media_id"] = json!("remote:https://example.org/x");
    })?;
    assert_rejected_as_corrupt(&rig, &sub)
}

#[test]
fn a_foreign_podcast_feed_id_on_an_episode_is_corrupt() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RSS_ONE_ITEM, "https://example.org/feed")?;
    corrupt(&rig, &sub, |value| {
        value["episodes"][0]["media_id"] =
            json!("podcast:ffffffffffffffffffffffffffffffff/guid:id");
    })?;
    assert_rejected_as_corrupt(&rig, &sub)
}

#[test]
fn a_duplicate_episode_key_is_corrupt() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RSS_TWO_ITEMS, "https://example.org/feed")?;
    corrupt(&rig, &sub, |value| {
        let first = value["episodes"][0]["media_id"].clone();
        value["episodes"][1]["media_id"] = first;
    })?;
    assert_rejected_as_corrupt(&rig, &sub)
}

#[test]
fn a_noncanonical_media_id_spelling_is_corrupt() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RSS_ONE_ITEM, "https://example.org/feed")?;
    corrupt(&rig, &sub, |value| {
        value["episodes"][0]["media_id"] =
            json!("podcast:0123456789abcdef0123456789abcdef/guid%3Aid");
    })?;
    assert_rejected_as_corrupt(&rig, &sub)
}

#[test]
fn an_unsupported_schema_version_is_corrupt() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RSS_ONE_ITEM, "https://example.org/feed")?;
    corrupt(&rig, &sub, |value| {
        value["schema_version"] = json!(99);
    })?;
    assert_rejected_as_corrupt(&rig, &sub)
}

#[test]
fn a_parser_version_mismatch_is_reported_distinctly() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RSS_ONE_ITEM, "https://example.org/feed")?;
    corrupt(&rig, &sub, |value| {
        value["parser_version"] = json!(99);
    })?;

    let path = rig.cache.path_for(&sub.feed_id)?;
    let before = std::fs::read(&path)?;

    let error = match rig.cache.read(&sub) {
        Ok(cached) => return Err(format!("expected CacheParserMismatch, got {cached:?}").into()),
        Err(error) => error,
    };
    assert!(
        matches!(
            &error,
            FeedError::CacheParserMismatch { slug, found: 99, expected: 1 } if *slug == sub.slug
        ),
        "expected CacheParserMismatch, got {error:?}"
    );
    assert_eq!(std::fs::read(&path)?, before);
    Ok(())
}

#[test]
fn an_ftp_enclosure_is_corrupt() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RSS_WITH_ENCLOSURE, "https://example.org/feed")?;
    corrupt(&rig, &sub, |value| {
        value["episodes"][0]["enclosure_url"] = json!("ftp://example.org/a.mp3");
    })?;
    assert_rejected_as_corrupt(&rig, &sub)
}

#[test]
fn a_malformed_fetched_from_url_is_corrupt() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RSS_ONE_ITEM, "https://example.org/feed")?;
    corrupt(&rig, &sub, |value| {
        value["fetched_from"] = json!("not a url");
    })?;
    assert_rejected_as_corrupt(&rig, &sub)
}

/// A valid identity with no enclosure remains legal — the semantic
/// validator must not reject an episode merely for lacking a playable
/// source.
#[test]
fn an_identity_with_no_enclosure_is_not_corrupt() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RSS_ONE_ITEM, "https://example.org/feed")?;
    let cached = rig.cache.read(&sub)?;
    assert_eq!(cached.episodes.len(), 1);
    assert!(cached.episodes[0].enclosure_url.is_none());
    Ok(())
}

/// `save` validates the value it is given exactly as `read` validates what
/// it decodes: an in-memory `CachedFeed` that fails validation is rejected
/// before `replace_bytes` ever runs, and the previous, valid cache entry is
/// left byte-for-byte untouched.
#[test]
fn a_failed_save_leaves_the_previous_cache_untouched() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(RSS_ONE_ITEM, "https://example.org/feed")?;
    let path = rig.cache.path_for(&sub.feed_id)?;
    let before = std::fs::read(&path)?;

    let mut invalid = rig.cache.read(&sub)?;
    invalid.feed_id = "ffffffffffffffffffffffffffffffff".to_string();

    match rig.cache.save(&sub, &invalid) {
        Ok(()) => return Err("an invalid CachedFeed must be rejected".into()),
        Err(error) => assert!(
            matches!(error, FeedError::CacheCorrupt { .. }),
            "expected CacheCorrupt"
        ),
    }

    assert_eq!(
        std::fs::read(&path)?,
        before,
        "a failed save must not replace the previous cache file"
    );
    Ok(())
}

/// `path_for`'s R4 fallback for a `FeedId` that fails re-validation must
/// never echo the offending id back: `slug` is the one field every
/// `FeedError` `Display` treats as a safe, bare identifier, and
/// `CacheCorrupt` prints it straight into a suggested command
/// (`run continuo refresh {slug}`). A traversal-shaped id is exactly the
/// case where that would be most dangerous, so neither the traversal text
/// nor any path separator may appear in either rendering — under `Debug` as
/// much as `Display` (§7.2).
#[test]
fn path_for_never_echoes_a_traversal_shaped_id() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    // `FeedId::new` performs no format check (only `validate_feed_id` does),
    // exactly the gap R4 exists to guard: this is how a traversal-shaped id
    // could reach `CacheStore` without ever having passed subscription-load
    // validation.
    let traversal = FeedId::new("../../etc/passwd".to_string())?;

    let error = match rig.cache.path_for(&traversal) {
        Ok(path) => return Err(format!("expected CacheCorrupt, got path {path:?}").into()),
        Err(error) => error,
    };
    assert!(
        matches!(&error, FeedError::CacheCorrupt { .. }),
        "expected CacheCorrupt, got {error:?}"
    );

    let display = error.to_string();
    let debug = format!("{error:?}");
    for rendering in [&display, &debug] {
        assert!(
            !rendering.contains("..") && !rendering.contains('/'),
            "rendering must not echo the traversal-shaped id, got {rendering:?}"
        );
    }
    Ok(())
}
