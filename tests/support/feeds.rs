//! Shared fixture for the M4 cache and (later) subscription/refresh test
//! suites: a `Rig` bundling a temp root, a fake clock, and the three stores
//! that share it, plus `seed`, which parses, binds and caches one feed in a
//! single call so a test can start from "one feed is already subscribed and
//! cached" without repeating that setup by hand.

#![allow(dead_code)]

use std::sync::Arc;

use tenuto::{
    clock::FakeClock,
    feed::{cache::CacheStore, episode::bind_feed, parse::parse_feed},
    persistence::store::StateStore,
    subscription::{
        model::{Subscription, validate_feed_id},
        store::{SubscriptionSnapshot, SubscriptionStore},
    },
};

pub const FEED_ID: &str = "0123456789abcdef0123456789abcdef";

pub struct Rig {
    pub root: tempfile::TempDir,
    pub clock: Arc<FakeClock>,
    pub subs: SubscriptionStore,
    pub cache: CacheStore,
    pub state: StateStore,
}

impl Rig {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let clock = Arc::new(FakeClock::new());
        let subs = SubscriptionStore::new(
            root.path().join("data/tenuto/subscriptions.json"),
            clock.clone(),
        );
        let cache = CacheStore::new(root.path().join("cache/tenuto/feeds"));
        let state = StateStore::new(root.path().join("state/tenuto/state.json"), clock.clone());
        Ok(Self {
            root,
            clock,
            subs,
            cache,
            state,
        })
    }

    pub fn seed(&self, xml: &[u8], url: &str) -> Result<Subscription, Box<dyn std::error::Error>> {
        use tenuto::{clock::Clock, feed::cache::CachedFeed, http::document::CacheValidators};

        let id = validate_feed_id(FEED_ID)?;
        let url: url::Url = url.parse()?;
        let bound = bind_feed(&id, parse_feed(xml, &url)?);
        let subscription = Subscription {
            feed_id: id.clone(),
            slug: "radio-t".into(),
            title: bound.title.clone(),
            fetch_url: url.clone(),
            added_at: self.clock.sample().wall,
        };
        let feed = CachedFeed::from_bound(
            &id,
            bound,
            url.clone(),
            CacheValidators {
                url,
                etag: None,
                last_modified: None,
            },
            self.clock.sample().wall,
        );
        self.cache.save(&subscription, &feed)?;
        self.subs.save(&SubscriptionSnapshot {
            subscriptions: vec![subscription.clone()],
        })?;
        Ok(subscription)
    }
}
