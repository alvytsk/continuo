//! Design doc M5 §8: enqueueing, restoring or browsing a URL/podcast entry
//! makes no network request — only an explicit play prepares a remote
//! source. A loopback server counts every request it sees.

mod support;

#[path = "support/feeds.rs"]
mod feeds;

#[path = "support/runtime.rs"]
mod runtime;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use continuo::application::browse::{BrowseRequest, BrowseResult, BrowseWorker};
use continuo::application::enrich::TagProbe;
use continuo::application::runtime::{AppCommand, EnqueueItem, LibraryStores};
use continuo::clock::{Clock, SystemClock};
use continuo::feed::cache::CacheStore;
use continuo::library::EpisodeCandidate;
use continuo::media::id::{EpisodeKey, FeedId, MediaId, NormalizedUrl};
use continuo::media::tags::probe_local_tags;
use continuo::persistence::model::PersistedState;
use continuo::queue::{NewQueueEntry, QueueSource};
use continuo::session::Session;
use continuo::subscription::store::SubscriptionStore;
use runtime::{pump_for, rig_with, rig_with_probe, row_ids};
use support::server::{Script, TestServer};

const FEED_URL: &str = "https://feeds.example/radio-t.xml";
const LOCAL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");

fn remote_entry(url: &str) -> NewQueueEntry {
    let normalized = NormalizedUrl::parse(url).unwrap_or_else(|error| panic!("url: {error}"));
    NewQueueEntry::new(
        MediaId::RemoteUrl(normalized.clone()),
        QueueSource::RemoteUrl(normalized),
        Default::default(),
    )
    .unwrap_or_else(|error| panic!("remote entry: {error}"))
}

fn podcast_entry(fallback: &str) -> NewQueueEntry {
    NewQueueEntry::new(
        MediaId::PodcastEpisode {
            feed: FeedId::new(feeds::FEED_ID.into())
                .unwrap_or_else(|error| panic!("feed: {error}")),
            episode: EpisodeKey::resolve(Some("e1"), None, None)
                .unwrap_or_else(|error| panic!("episode key: {error}")),
        },
        QueueSource::Podcast {
            fallback: fallback
                .parse()
                .unwrap_or_else(|error| panic!("url: {error}")),
        },
        Default::default(),
    )
    .unwrap_or_else(|error| panic!("podcast entry: {error}"))
}

fn seeded(entries: Vec<NewQueueEntry>) -> PersistedState {
    let mut session = Session::new(PersistedState::default());
    session
        .enqueue(entries)
        .unwrap_or_else(|error| panic!("fits: {error}"));
    session.state().clone()
}

fn podcast_rss(enclosure: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Radio-T</title><item><title>e1</title><guid>e1</guid><enclosure url="{enclosure}" type="audio/mpeg"/></item></channel></rss>"#
    )
    .into_bytes()
}

fn wait_for_result(worker: &BrowseWorker) -> BrowseResult {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(result) = worker.try_result() {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "the browse worker never answered"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn restoring_enqueueing_and_browsing_remote_entries_make_no_requests() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));

    // Enrichment is on, with a probe that records every path it is handed:
    // only the one local file below may ever reach it.
    let probed = Arc::new(Mutex::new(Vec::new()));
    let probe: TagProbe = Arc::new({
        let probed = Arc::clone(&probed);
        move |path| {
            probed
                .lock()
                .unwrap_or_else(|error| panic!("probe log: {error}"))
                .push(path.clone());
            probe_local_tags(path)
        }
    });
    let mut rig = rig_with_probe(
        seeded(vec![
            remote_entry(&server.url("/a.mp3")),
            podcast_entry(&server.url("/ep.mp3")),
        ]),
        probe,
    );
    let episode = EpisodeCandidate {
        media: MediaId::PodcastEpisode {
            feed: FeedId::new(feeds::FEED_ID.into())
                .unwrap_or_else(|error| panic!("feed: {error}")),
            episode: EpisodeKey::resolve(Some("e2"), None, None)
                .unwrap_or_else(|error| panic!("episode key: {error}")),
        },
        enclosure: Some(
            server
                .url("/ep2.mp3")
                .parse()
                .unwrap_or_else(|error| panic!("url: {error}")),
        ),
        title: None,
        declared_duration: None,
    };
    let local = std::fs::canonicalize(LOCAL).unwrap_or_else(|error| panic!("fixture: {error}"));
    rig.runtime.handle(AppCommand::Enqueue(vec![
        EnqueueItem::Url(server.url("/b.mp3")),
        EnqueueItem::Episode(episode),
        // The positive control: enrichment is running in this rig.
        EnqueueItem::Path(local.clone()),
    ]));
    let view = rig.runtime.view();
    assert_eq!(view.rows.len(), 5, "{view:?}");
    pump_for(&mut rig.runtime, Duration::from_millis(300));
    let probed: Vec<_> = probed
        .lock()
        .unwrap_or_else(|error| panic!("probe log: {error}"))
        .iter()
        .map(|path| path.as_path().to_path_buf())
        .collect();
    assert_eq!(probed, vec![local], "enrichment probes local files only");

    let library = feeds::Rig::new().unwrap_or_else(|error| panic!("feeds rig: {error}"));
    library
        .seed(&podcast_rss(&server.url("/ep.mp3")), FEED_URL)
        .unwrap_or_else(|error| panic!("seed: {error}"));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let worker = BrowseWorker::spawn(Some(LibraryStores {
        subscriptions: SubscriptionStore::new(
            library.root.path().join("data/continuo/subscriptions.json"),
            clock,
        ),
        cache: CacheStore::new(library.root.path().join("cache/continuo/feeds")),
    }));
    worker.request(BrowseRequest::Feeds);
    assert!(
        matches!(wait_for_result(&worker), BrowseResult::Feeds(Ok(feeds)) if feeds.len() == 1),
        "the seeded feed is listed"
    );
    worker.request(BrowseRequest::Episodes {
        slug: "radio-t".to_owned(),
    });
    let expected = server.url("/ep.mp3");
    match wait_for_result(&worker) {
        BrowseResult::Episodes {
            slug,
            episodes: Ok(episodes),
        } => {
            assert_eq!(slug, "radio-t");
            assert!(
                matches!(
                    &episodes[..],
                    [only] if only.enclosure.as_ref().map(url::Url::as_str) == Some(expected.as_str())
                ),
                "{episodes:?}"
            );
        }
        other => panic!("expected an episode listing, got {other:?}"),
    }

    assert!(
        server.requests().is_empty(),
        "no request before an explicit play: {:?}",
        server.requests()
    );
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

#[test]
fn only_an_explicit_play_prepares_the_remote_source() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let mut rig = rig_with(seeded(vec![remote_entry(&server.url("/a.mp3"))]));
    pump_for(&mut rig.runtime, Duration::from_millis(100));
    assert!(server.requests().is_empty());

    let remote = row_ids(&rig.runtime)[0];
    rig.runtime.handle(AppCommand::PlayEntry(remote));
    let deadline = Instant::now() + Duration::from_secs(5);
    while server.requests().is_empty() {
        assert!(
            Instant::now() < deadline,
            "an explicit play never reached the server: {:?}",
            rig.runtime.view()
        );
        rig.runtime.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = rig.runtime.shutdown();
    server.shutdown();
}
