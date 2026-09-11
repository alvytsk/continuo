//! §2.4: binding parsed feed items to subscription-scoped episode
//! identities.
//!
//! `bind_feed` is the join between the identity-free parser
//! (`tests/m4_feed_parse.rs`) and a real `FeedId`: it is what turns a
//! `ParsedItem` into a `MediaId::PodcastEpisode` and a `media::Episode`.

use continuo::feed::episode::bind_feed;
use continuo::feed::model::{Enclosure, ParsedFeed, ParsedItem};
use continuo::feed::parse::{ParseReport, WarningKind, parse_feed};
use continuo::media::id::{FeedId, MediaId};
use continuo::subscription::model::validate_feed_id;
use url::Url;

fn feed_id() -> Result<FeedId, Box<dyn std::error::Error>> {
    Ok(validate_feed_id("0123456789abcdef0123456789abcdef")?)
}

fn feed_url() -> Result<Url, Box<dyn std::error::Error>> {
    Ok("https://example.org/feed".parse()?)
}

#[test]
fn duplicate_guid_keeps_first_and_nonplayable_identity_survives()
-> Result<(), Box<dyn std::error::Error>> {
    let xml = br#"<rss><channel>
      <item><guid>same</guid><title>first</title></item>
      <item><guid>same</guid><title>second</title><enclosure url="https://example.org/b.mp3"/></item>
      <item><title>no identity</title></item>
    </channel></rss>"#;
    let feed_id = validate_feed_id("0123456789abcdef0123456789abcdef")?;
    let bound = bind_feed(
        &feed_id,
        parse_feed(xml, &"https://example.org/feed".parse()?)?,
    );
    assert_eq!(bound.items.len(), 1);
    assert_eq!(bound.skipped, 2);
    assert_eq!(bound.items[0].episode.title.as_deref(), Some("first"));
    assert!(bound.items[0].episode.source.is_none());
    assert_eq!(bound.items[0].episode.id.feed(), Some(&feed_id));
    Ok(())
}

/// Pins the actual, unmodified behavior of `EpisodeKey::resolve` for an
/// empty `<guid></guid>` (Task 7 stored `Some("")` faithfully, and
/// deliberately deferred whether that counts as identity).
///
/// `resolve` filters an empty guid out (`guid.filter(|v| !v.is_empty())`),
/// so `Some("")` is treated exactly like `None`: resolution falls through
/// to the enclosure, then the link, and only fails if neither is present
/// either. Two items that both carry an empty `<guid>` and no enclosure or
/// link therefore do *not* collide as duplicates — each independently has
/// no identity at all, and each is skipped as `MissingIdentity`.
#[test]
fn empty_guid_is_treated_as_absent_not_as_a_duplicate_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let xml = br#"<rss><channel>
      <item><guid></guid><title>no fallback either</title></item>
      <item><guid></guid><title>falls through to enclosure</title><enclosure url="https://example.org/a.mp3"/></item>
    </channel></rss>"#;
    let bound = bind_feed(&feed_id()?, parse_feed(xml, &feed_url()?)?);
    assert_eq!(bound.items.len(), 1);
    assert_eq!(bound.skipped, 1);
    assert!(
        bound
            .warnings
            .iter()
            .any(|warning| warning.kind == WarningKind::MissingIdentity)
    );
    let kept = &bound.items[0].episode;
    assert_eq!(kept.title.as_deref(), Some("falls through to enclosure"));
    // The identity came from the enclosure URL, not from the empty guid:
    // the same URL used directly as an enclosure resolves to the same key.
    let enclosure_url: Url = "https://example.org/a.mp3".parse()?;
    let expected = MediaId::PodcastEpisode {
        feed: feed_id()?,
        episode: continuo::media::id::EpisodeKey::resolve(None, Some(&enclosure_url), None)?,
    };
    assert_eq!(kept.id, expected);
    Ok(())
}

#[test]
fn guid_overrides_two_different_enclosures_across_refetches()
-> Result<(), Box<dyn std::error::Error>> {
    let first_fetch = br#"<rss><channel>
      <item><guid>ep-1</guid><title>v1</title><enclosure url="https://example.org/v1.mp3"/></item>
    </channel></rss>"#;
    let second_fetch = br#"<rss><channel>
      <item><guid>ep-1</guid><title>v2 renamed</title><enclosure url="https://example.org/v2-different-host.mp3"/><pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate></item>
    </channel></rss>"#;
    let id = feed_id()?;
    let bound_first = bind_feed(&id, parse_feed(first_fetch, &feed_url()?)?);
    let bound_second = bind_feed(&id, parse_feed(second_fetch, &feed_url()?)?);
    assert_eq!(bound_first.items.len(), 1);
    assert_eq!(bound_second.items.len(), 1);
    // Same GUID, different title/date/enclosure: identity is stable.
    assert_eq!(
        bound_first.items[0].episode.id,
        bound_second.items[0].episode.id
    );
    assert_ne!(
        bound_first.items[0].episode.title,
        bound_second.items[0].episode.title
    );
    Ok(())
}

#[test]
fn absent_guid_uses_enclosure() -> Result<(), Box<dyn std::error::Error>> {
    let xml = br#"<rss><channel>
      <item><title>no guid</title><enclosure url="https://example.org/only.mp3"/></item>
    </channel></rss>"#;
    let bound = bind_feed(&feed_id()?, parse_feed(xml, &feed_url()?)?);
    assert_eq!(bound.items.len(), 1);
    let enclosure_url: Url = "https://example.org/only.mp3".parse()?;
    let expected = MediaId::PodcastEpisode {
        feed: feed_id()?,
        episode: continuo::media::id::EpisodeKey::resolve(None, Some(&enclosure_url), None)?,
    };
    assert_eq!(bound.items[0].episode.id, expected);
    Ok(())
}

/// A `ParsedItem` built directly (never through `parse_feed`) with an
/// enclosure whose scheme the parser would never let through. `bind_feed`
/// must re-filter the scheme itself and fall back to the link, because a
/// caller can construct `ParsedItem` without going through the parser.
#[test]
fn invalid_enclosure_scheme_built_directly_falls_back_to_link()
-> Result<(), Box<dyn std::error::Error>> {
    let link: Url = "https://example.org/item/42".parse()?;
    let bad_enclosure: Url = "ftp://example.org/audio.mp3".parse()?;
    let item = ParsedItem {
        guid: None,
        link: Some(link.clone()),
        enclosure: Some(Enclosure {
            url: bad_enclosure,
            length: None,
            mime_type: None,
        }),
        title: Some("built directly".into()),
        published: None,
        declared_duration: None,
    };
    let parsed = ParseReport {
        feed: ParsedFeed {
            title: None,
            site_link: None,
            items: vec![item],
        },
        skipped: 0,
        warnings: Vec::new(),
    };
    let bound = bind_feed(&feed_id()?, parsed);
    assert_eq!(bound.items.len(), 1);
    assert!(bound.items[0].episode.source.is_none());
    let expected = MediaId::PodcastEpisode {
        feed: feed_id()?,
        episode: continuo::media::id::EpisodeKey::resolve(None, None, Some(&link))?,
    };
    assert_eq!(bound.items[0].episode.id, expected);
    Ok(())
}

/// An item with a GUID and no enclosure at all keeps its identity and is
/// retained with `source: None` (§2.4 "identity without playability").
#[test]
fn no_enclosure_with_guid_stays_nonplayable() -> Result<(), Box<dyn std::error::Error>> {
    let xml = br#"<rss><channel>
      <item><guid>solo</guid><title>text only</title></item>
    </channel></rss>"#;
    let bound = bind_feed(&feed_id()?, parse_feed(xml, &feed_url()?)?);
    assert_eq!(bound.items.len(), 1);
    assert!(bound.items[0].episode.source.is_none());
    assert_eq!(bound.items[0].episode.title.as_deref(), Some("text only"));
    Ok(())
}

/// Unknown-entity items are already skipped at the *parse* stage
/// (`WarningKind::UnknownIdentityEntity`, counted in `ParseReport::skipped`).
/// `bind_feed` must carry that count and those warnings forward, adding its
/// own binding-stage rejections on top rather than losing or double-counting
/// either.
#[test]
fn parse_stage_and_binding_stage_skips_both_count_exactly_once()
-> Result<(), Box<dyn std::error::Error>> {
    let xml = br#"<rss><channel>
      <item><guid>&nbsp;bad</guid><title>unknown entity in guid</title></item>
      <item><title>no identity at all</title></item>
      <item><guid>fine</guid><title>kept</title></item>
    </channel></rss>"#;
    let parsed = parse_feed(xml, &feed_url()?)?;
    assert_eq!(parsed.skipped, 1);
    let bound = bind_feed(&feed_id()?, parsed);
    // 1 parse-stage skip (unknown entity) + 1 binding-stage skip (no
    // identity) = 2, and exactly one item survives.
    assert_eq!(bound.skipped, 2);
    assert_eq!(bound.items.len(), 1);
    assert_eq!(bound.items[0].episode.title.as_deref(), Some("kept"));
    assert!(
        bound
            .warnings
            .iter()
            .any(|warning| warning.kind == WarningKind::UnknownIdentityEntity)
    );
    assert!(
        bound
            .warnings
            .iter()
            .any(|warning| warning.kind == WarningKind::MissingIdentity)
    );
    Ok(())
}

#[test]
fn local_file_and_remote_url_accessors_return_none() -> Result<(), Box<dyn std::error::Error>> {
    use continuo::media::id::{AbsolutePath, NormalizedUrl};
    use std::path::PathBuf;

    let path = if cfg!(windows) {
        "C:/audio/a.mp3"
    } else {
        "/audio/a.mp3"
    };
    let local = MediaId::LocalFile(AbsolutePath::new(PathBuf::from(path))?);
    assert_eq!(local.feed(), None);
    assert_eq!(local.episode_key(), None);

    let remote = MediaId::RemoteUrl(NormalizedUrl::parse("https://example.com/audio.mp3")?);
    assert_eq!(remote.feed(), None);
    assert_eq!(remote.episode_key(), None);
    Ok(())
}
