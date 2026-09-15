//! Task 10 of the M5 plan: [`resolve_podcast`] decides which URL to play
//! for a queued podcast episode using only the local subscription and
//! feed-cache files (never the network), and [`episode_candidates`] gives
//! the later on-demand browser the enclosure URL `EpisodeRow` deliberately
//! omits.
//!
//! `Rig` (`tests/support/feeds.rs`) is the fixture every M4/M5 library test
//! suite imports: a temp root, a fake clock, and the subscription/cache/
//! state stores that share it.

#[path = "support/feeds.rs"]
mod feeds;

use std::time::Duration;

use continuo::application::podcast::{PodcastResolution, PodcastResolveError, resolve_podcast};
use continuo::feed::error::FeedError;
use continuo::library::episode_candidates;
use continuo::media::id::{EpisodeKey, FeedId, MediaId};
use continuo::media::source::SourceLocation;
use continuo::subscription::store::SubscriptionSnapshot;
use url::Url;

const FEED_URL: &str = "https://feeds.example/radio-t.xml";

fn rss(items: &str) -> Vec<u8> {
    format!(r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Radio-T</title>{items}</channel></rss>"#).into_bytes()
}

/// Test 8 only: declares the itunes namespace the parser matches
/// `itunes:duration` against, which the plain [`rss`] helper omits since no
/// other test needs it.
fn rss_with_itunes(items: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0"?><rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd"><channel><title>Radio-T</title>{items}</channel></rss>"#
    )
    .into_bytes()
}

fn item(guid: &str, enclosure: Option<&str>) -> String {
    let enclosure = enclosure
        .map(|url| format!(r#"<enclosure url="{url}" type="audio/mpeg"/>"#))
        .unwrap_or_default();
    format!("<item><title>{guid}</title><guid>{guid}</guid>{enclosure}</item>")
}

fn item_with_duration(guid: &str, enclosure: &str, duration_secs: u64) -> String {
    format!(
        r#"<item><title>{guid}</title><guid>{guid}</guid><enclosure url="{enclosure}" type="audio/mpeg"/><itunes:duration>{duration_secs}</itunes:duration></item>"#
    )
}

fn episode_media(guid: &str) -> Result<MediaId, Box<dyn std::error::Error>> {
    Ok(MediaId::PodcastEpisode {
        feed: FeedId::new(feeds::FEED_ID.into())?,
        episode: EpisodeKey::resolve(Some(guid), None, None)?,
    })
}

#[test]
fn a_rotated_enclosure_under_the_same_key_is_current_and_refreshes_the_fallback()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    rig.seed(
        &rss(&item("e1", Some("https://cdn.example/v2.mp3"))),
        FEED_URL,
    )?;
    let media = episode_media("e1")?;
    let fallback: Url = "https://cdn.example/v1.mp3".parse()?;

    let resolution = resolve_podcast(&rig.subs, &rig.cache, &media, &fallback)?;

    let v2: Url = "https://cdn.example/v2.mp3".parse()?;
    match resolution {
        PodcastResolution::Current {
            location,
            refreshed_fallback,
        } => {
            assert_eq!(location, SourceLocation::Http(v2.clone()));
            assert_eq!(refreshed_fallback, Some(v2));
        }
        other => return Err(format!("expected Current, got {other:?}").into()),
    }
    assert_eq!(media, episode_media("e1")?, "identity is unchanged");
    Ok(())
}

#[test]
fn a_reordered_feed_still_matches_by_identity() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let items = format!(
        "{}{}",
        item("e2", Some("https://cdn.example/e2.mp3")),
        item("e1", Some("https://cdn.example/e1.mp3"))
    );
    rig.seed(&rss(&items), FEED_URL)?;
    let media = episode_media("e1")?;
    let fallback: Url = "https://cdn.example/e1.mp3".parse()?;

    let resolution = resolve_podcast(&rig.subs, &rig.cache, &media, &fallback)?;

    match resolution {
        PodcastResolution::Current {
            location,
            refreshed_fallback,
        } => {
            assert_eq!(location, SourceLocation::Http(fallback));
            assert_eq!(refreshed_fallback, None);
        }
        other => return Err(format!("expected Current, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn an_absent_episode_uses_the_saved_source() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    rig.seed(
        &rss(&item("e2", Some("https://cdn.example/e2.mp3"))),
        FEED_URL,
    )?;
    let media = episode_media("e1")?;
    let fallback: Url = "https://cdn.example/saved.mp3".parse()?;

    let resolution = resolve_podcast(&rig.subs, &rig.cache, &media, &fallback)?;

    match resolution {
        PodcastResolution::Saved { location } => {
            assert_eq!(location, SourceLocation::Http(fallback));
        }
        other => return Err(format!("expected Saved, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn an_absent_subscription_uses_the_saved_source() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    rig.subs.save(&SubscriptionSnapshot {
        subscriptions: Vec::new(),
    })?;
    let media = episode_media("e1")?;
    let fallback: Url = "https://cdn.example/saved.mp3".parse()?;

    let resolution = resolve_podcast(&rig.subs, &rig.cache, &media, &fallback)?;

    match resolution {
        PodcastResolution::Saved { location } => {
            assert_eq!(location, SourceLocation::Http(fallback));
        }
        other => return Err(format!("expected Saved, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn a_missing_cache_file_uses_the_saved_source() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let subscription = rig.seed(
        &rss(&item("e1", Some("https://cdn.example/e1.mp3"))),
        FEED_URL,
    )?;
    rig.cache.remove(&subscription.feed_id)?;
    let media = episode_media("e1")?;
    let fallback: Url = "https://cdn.example/saved.mp3".parse()?;

    let resolution = resolve_podcast(&rig.subs, &rig.cache, &media, &fallback)?;

    match resolution {
        PodcastResolution::Saved { location } => {
            assert_eq!(location, SourceLocation::Http(fallback));
        }
        other => return Err(format!("expected Saved, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn a_present_episode_without_an_enclosure_is_not_playable() -> Result<(), Box<dyn std::error::Error>>
{
    let rig = feeds::Rig::new()?;
    rig.seed(&rss(&item("e1", None)), FEED_URL)?;
    let media = episode_media("e1")?;
    let fallback: Url = "https://cdn.example/saved.mp3".parse()?;

    match resolve_podcast(&rig.subs, &rig.cache, &media, &fallback) {
        Err(PodcastResolveError::NotPlayable) => {}
        other => return Err(format!("expected NotPlayable, got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn a_corrupt_cache_is_a_resolution_error_not_absence() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let subscription = rig.seed(
        &rss(&item("e1", Some("https://cdn.example/e1.mp3"))),
        FEED_URL,
    )?;
    let cache_path = rig.cache.path_for(&subscription.feed_id)?;
    std::fs::write(&cache_path, b"{")?;
    let media = episode_media("e1")?;
    let fallback: Url = "https://cdn.example/saved.mp3".parse()?;

    match resolve_podcast(&rig.subs, &rig.cache, &media, &fallback) {
        Err(PodcastResolveError::Library(FeedError::CacheCorrupt { .. })) => {}
        other => return Err(format!("expected Library(CacheCorrupt), got {other:?}").into()),
    }
    Ok(())
}

#[test]
fn episode_candidates_carry_the_enclosure_and_declared_duration()
-> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    rig.seed(
        &rss_with_itunes(&item_with_duration("e1", "https://cdn.example/e1.mp3", 61)),
        FEED_URL,
    )?;

    let candidates = episode_candidates(&rig.subs, &rig.cache, "radio-t")?;

    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].enclosure,
        Some("https://cdn.example/e1.mp3".parse()?)
    );
    assert_eq!(
        candidates[0].declared_duration,
        Some(Duration::from_secs(61))
    );
    Ok(())
}
