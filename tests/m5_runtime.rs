mod support;

#[path = "support/feeds.rs"]
mod feeds;

#[path = "support/runtime.rs"]
mod runtime;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use continuo::application::runtime::{
    AppCommand, EnqueueItem, FlushReport, LibraryStores, PlayerRuntime,
};
use continuo::application::transport::PlaybackPhase;
use continuo::application::view::{NowPlaying, PlayerView};
use continuo::clock::SystemClock;
use continuo::feed::cache::CacheStore;
use continuo::media::id::{AbsolutePath, EpisodeKey, FeedId, MediaId};
use continuo::persistence::model::PersistedState;
use continuo::persistence::writer::WriterHandle;
use continuo::queue::{NewQueueEntry, QueueEntryId, QueueSource};
use continuo::session::Session;
use continuo::subscription::store::SubscriptionStore;
use runtime::{null_engine, parts, pump_for, pump_until, rig_with, rig_with_parts, row_ids};
use support::server::{DocumentReply, Script, TestServer};

const SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");
const FIVE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");
const MISSING: &str = "/nonexistent/m5-missing.flac";

fn now_playing(view: &PlayerView) -> NowPlaying {
    view.now_playing
        .clone()
        .unwrap_or_else(|| panic!("something is active"))
}

fn local_media(path: &Path) -> MediaId {
    MediaId::LocalFile(
        AbsolutePath::new(path.to_path_buf()).unwrap_or_else(|error| panic!("absolute: {error}")),
    )
}

fn local_entry(path: &Path) -> NewQueueEntry {
    let absolute =
        AbsolutePath::new(path.to_path_buf()).unwrap_or_else(|error| panic!("absolute: {error}"));
    NewQueueEntry::new(
        MediaId::LocalFile(absolute.clone()),
        QueueSource::LocalFile(absolute),
        Default::default(),
    )
    .unwrap_or_else(|error| panic!("entry: {error}"))
}

fn seeded(entries: Vec<NewQueueEntry>) -> PersistedState {
    let mut session = Session::new(PersistedState::default());
    session
        .enqueue(entries)
        .unwrap_or_else(|error| panic!("fits: {error}"));
    session.state().clone()
}

/// A media directory holding a copy of the 5-second fixture under each name.
fn media_dir(names: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let root = dir
        .path()
        .canonicalize()
        .unwrap_or_else(|error| panic!("canonical tempdir: {error}"));
    for name in names {
        std::fs::copy(FIVE, root.join(name))
            .unwrap_or_else(|error| panic!("copy fixture: {error}"));
    }
    (dir, root)
}

/// The furthest position history holds for `media`, established or estimated.
fn saved_at(runtime: &PlayerRuntime, media: &MediaId) -> Duration {
    runtime
        .session()
        .state()
        .entry_for(media)
        .map(|entry| {
            entry
                .position
                .unwrap_or_default()
                .max(entry.estimated.unwrap_or_default())
        })
        .unwrap_or_default()
}

fn is_playing(view: &PlayerView, id: QueueEntryId) -> bool {
    view.phase == PlaybackPhase::Playing
        && view
            .now_playing
            .as_ref()
            .is_some_and(|now| now.loaded && now.entry == Some(id))
}

#[test]
fn enter_plays_the_selected_entry_and_adopts_only_it() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::Enqueue(vec![
        EnqueueItem::Path(SHORT.into()),
        EnqueueItem::Path(SHORT.into()),
    ]));
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    // `Loaded` adopts the row while the engine is still paused; the start
    // follows on a later pass.
    pump_until(&mut rig.runtime, "second row active and playing", |view| {
        view.active == Some(ids[1])
            && matches!(view.phase, PlaybackPhase::Playing | PlaybackPhase::Ended)
    });
}

#[test]
fn completion_advances_once_and_the_last_entry_stays_ended() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::Enqueue(vec![
        EnqueueItem::Path(SHORT.into()),
        EnqueueItem::Path(SHORT.into()),
    ]));
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    pump_until(&mut rig.runtime, "advanced to the second row", |view| {
        view.active == Some(ids[1])
    });
    pump_until(&mut rig.runtime, "queue ended", |view| {
        view.phase == PlaybackPhase::Ended
    });
    assert_eq!(
        rig.runtime.view().active,
        Some(ids[1]),
        "no wrap back to the first row"
    );
}

#[test]
fn a_failed_load_keeps_the_queue_and_does_not_skip() {
    let state = seeded(vec![local_entry(Path::new(MISSING))]);
    let mut rig = rig_with(state);
    rig.runtime
        .handle(AppCommand::Enqueue(vec![EnqueueItem::Path(SHORT.into())]));
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    pump_until(&mut rig.runtime, "load failed", |view| {
        view.phase == PlaybackPhase::LoadFailed
    });
    let view = rig.runtime.view();
    assert_eq!(view.rows.len(), 2);
    assert_eq!(view.active, None);
    assert!(view.status.as_deref().is_some_and(|s| !s.is_empty()));
}

#[test]
fn space_before_loading_loads_the_restored_active_entry() {
    let path = std::fs::canonicalize(SHORT).expect("fixture");
    let key = serde_json::to_value(MediaId::LocalFile(
        AbsolutePath::new(path.clone()).expect("absolute"),
    ))
    .expect("key");
    let file = serde_json::json!({ "schema_version": 3, "current_media": key, "volume": 1.0, "checkpoints": {},
        "queue": [{ "id": 1, "media": "local:/music/other.flac", "source": { "kind": "local", "path": "/music/other.flac" } },
                  { "id": 2, "media": key, "source": { "kind": "local", "path": path }}],
        "active_entry": 2 });
    let mut rig = rig_with(serde_json::from_value(file).expect("valid"));
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayPause {
        selected: Some(ids[0]),
    });
    pump_until(&mut rig.runtime, "active entry loaded", |view| {
        view.now_playing
            .as_ref()
            .is_some_and(|now| now.loaded && now.entry == Some(ids[1]))
    });
}

#[test]
fn seeking_before_any_load_is_a_notice_and_opens_nothing() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime
        .handle(AppCommand::Enqueue(vec![EnqueueItem::Path(SHORT.into())]));
    rig.runtime.handle(AppCommand::SeekBy(10));
    assert_eq!(
        rig.runtime.view().status.as_deref(),
        Some("Play a track before seeking")
    );
    assert_eq!(rig.runtime.view().phase, PlaybackPhase::Unloaded);
}

#[test]
fn volume_without_an_engine_is_persisted_at_shutdown() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::AdjustVolume(-0.25));
    let state_path = rig.state_path.clone();
    assert!(matches!(rig.runtime.shutdown(), FlushReport::Written));
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state_path).expect("written")).expect("json");
    assert_eq!(written["volume"], 0.75);
}

#[test]
fn an_oversized_enqueue_is_rejected_whole_with_a_visible_message() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::Enqueue(
        (0..257).map(|_| EnqueueItem::Path(SHORT.into())).collect(),
    ));
    assert!(rig.runtime.view().rows.is_empty());
    assert_eq!(
        rig.runtime.view().status.as_deref(),
        Some("Queue is full (256 entries)")
    );
}

#[test]
fn two_loads_of_one_media_submitted_together_end_on_the_later_row() {
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::Enqueue(vec![
        EnqueueItem::Path(SHORT.into()),
        EnqueueItem::Path(SHORT.into()),
    ]));
    let ids = row_ids(&rig.runtime);
    // Both are submitted before a single event is drained.
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    assert_eq!(rig.runtime.session().pending_load_count(), 2);
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while rig.runtime.view().active != Some(ids[1]) {
        rig.runtime.pump();
        if let Some(active) = rig.runtime.view().active
            && seen.last() != Some(&active)
        {
            seen.push(active);
        }
        assert!(Instant::now() < deadline, "never adopted the later row");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(rig.runtime.session().pending_load_count(), 0);
    assert!(
        !seen.windows(2).any(|pair| pair == [ids[1], ids[0]]),
        "never regressed to the earlier row: {seen:?}"
    );
}

struct FailingSink;
impl continuo::persistence::writer::StateSink for FailingSink {
    fn write(&self, _: &PersistedState) -> Result<(), continuo::persistence::PersistenceError> {
        Err(continuo::persistence::PersistenceError::NoStateDirectory)
    }
}

#[test]
fn a_failed_final_flush_is_reported_not_claimed_as_saved() {
    let clock: Arc<dyn continuo::clock::Clock> = Arc::new(SystemClock);
    let writer = WriterHandle::spawn(Box::new(FailingSink), clock.clone());
    let mut runtime = PlayerRuntime::new(parts(
        PersistedState::default(),
        writer,
        clock,
        None,
        null_engine(),
    ));
    runtime.handle(AppCommand::AdjustVolume(-0.1));
    assert!(matches!(runtime.shutdown(), FlushReport::Failed(_)));
}

// ------------------------------------------------------------- regressions

#[derive(Clone, Copy, Debug)]
enum Superseding {
    Load,
    /// A load of a remote B whose response is delayed past the burst's quiet
    /// window, so the burst comes due while B is still loading.
    StalledLoad,
    Stop,
    RemoveActive,
    Clear,
    SeekTo,
}

/// A position a leaked burst would have reached. The burst is `SeekBy(3)`
/// from just after A starts, and every legitimate position these checks
/// read stays well below it.
const LEAKED: Duration = Duration::from_millis(2500);

/// Starts A playing with a forward burst standing, supersedes it with `how`
/// before the burst's quiet window closes, then pumps well past that window.
fn supersede_a_burst(how: Superseding) {
    let (_media, root) = media_dir(&["a.flac", "b.flac"]);
    let (a_path, b_path) = (root.join("a.flac"), root.join("b.flac"));
    // Answers only after the burst's quiet window has long passed.
    let server = TestServer::start(Script::documents(vec![DocumentReply {
        path: "/b.flac".into(),
        status: 200,
        headers: vec![("Content-Type".into(), "audio/flac".into())],
        body: std::fs::read(FIVE).unwrap_or_else(|error| panic!("fixture: {error}")),
        conditional: false,
        header_delay: Duration::from_millis(600),
    }]));
    let b_item = match how {
        Superseding::StalledLoad => EnqueueItem::Url(server.url("/b.flac")),
        _ => EnqueueItem::Path(b_path.clone()),
    };
    let mut rig = rig_with(PersistedState::default());
    rig.runtime.handle(AppCommand::Enqueue(vec![
        EnqueueItem::Path(a_path.clone()),
        b_item,
    ]));
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    pump_until(&mut rig.runtime, "A playing with a duration", |view| {
        is_playing(view, ids[0])
            && view
                .now_playing
                .as_ref()
                .is_some_and(|n| n.duration.is_some())
    });

    rig.runtime.handle(AppCommand::SeekBy(3));
    match how {
        Superseding::Load | Superseding::StalledLoad => {
            rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
        }
        Superseding::Stop => rig.runtime.handle(AppCommand::Stop),
        Superseding::RemoveActive => rig.runtime.handle(AppCommand::Remove(ids[0])),
        Superseding::Clear => rig.runtime.handle(AppCommand::ClearQueue),
        Superseding::SeekTo => rig
            .runtime
            .handle(AppCommand::SeekTo(Duration::from_secs(1))),
    }

    match how {
        Superseding::Load | Superseding::StalledLoad => {
            if let Superseding::StalledLoad = how {
                pump_for(&mut rig.runtime, Duration::from_millis(400));
                assert_eq!(rig.runtime.view().phase, PlaybackPhase::Loading);
            }
            pump_until(&mut rig.runtime, "B playing", |view| {
                is_playing(view, ids[1])
            });
            pump_for(&mut rig.runtime, Duration::from_millis(300));
            let view = rig.runtime.view();
            assert!(
                is_playing(&view, ids[1]),
                "{how:?}: B stays playing: {view:?}"
            );
            assert!(
                now_playing(&view).position < LEAKED,
                "{how:?}: B near zero: {view:?}"
            );
            let b_media = rig
                .runtime
                .session()
                .state()
                .current_media()
                .cloned()
                .unwrap_or_else(|| panic!("B is current"));
            assert!(saved_at(&rig.runtime, &b_media) < LEAKED, "{how:?}");
        }
        Superseding::Stop => {
            pump_for(&mut rig.runtime, Duration::from_millis(400));
            let view = rig.runtime.view();
            assert_eq!(view.phase, PlaybackPhase::Stopped, "{how:?}: {view:?}");
            assert!(now_playing(&view).position < LEAKED, "{how:?}: {view:?}");
        }
        Superseding::RemoveActive => {
            pump_for(&mut rig.runtime, Duration::from_millis(400));
            let view = rig.runtime.view();
            assert_eq!(view.phase, PlaybackPhase::Unloaded, "{how:?}: {view:?}");
            assert_eq!(view.rows.len(), 1);
        }
        Superseding::Clear => {
            pump_for(&mut rig.runtime, Duration::from_millis(400));
            let view = rig.runtime.view();
            assert!(view.rows.is_empty());
            assert_eq!(view.phase, PlaybackPhase::Unloaded, "{how:?}: {view:?}");
        }
        Superseding::SeekTo => {
            pump_until(&mut rig.runtime, "landed at the absolute target", |view| {
                view.now_playing
                    .as_ref()
                    .is_some_and(|now| now.position >= Duration::from_secs(1))
            });
            pump_for(&mut rig.runtime, Duration::from_millis(300));
            let view = rig.runtime.view();
            assert!(
                is_playing(&view, ids[0]),
                "{how:?}: A still playing: {view:?}"
            );
            assert!(now_playing(&view).position < LEAKED, "{how:?}: {view:?}");
        }
    }
    assert!(
        saved_at(&rig.runtime, &local_media(&a_path)) < LEAKED,
        "{how:?}: A's history never advanced to the discarded target"
    );
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

#[test]
fn seek_then_load_discards_the_old_target() {
    for how in [
        Superseding::Load,
        Superseding::StalledLoad,
        Superseding::Stop,
        Superseding::RemoveActive,
        Superseding::Clear,
        Superseding::SeekTo,
    ] {
        supersede_a_burst(how);
    }
}

/// Plays A, requests the missing B (or two occurrences of it), waits for the
/// failure, creates B, then presses `retry` with the selection still on A.
fn retry_a_failed_switch(retry: impl Fn(QueueEntryId) -> AppCommand, duplicates: bool) {
    let (_media, root) = media_dir(&["a.flac"]);
    let (a_path, b_path) = (root.join("a.flac"), root.join("b.flac"));
    let mut entries = vec![local_entry(&a_path), local_entry(&b_path)];
    if duplicates {
        entries.push(local_entry(&b_path));
    }
    let mut rig = rig_with(seeded(entries));
    let ids = row_ids(&rig.runtime);
    let a = ids[0];
    let b = *ids.last().unwrap_or_else(|| panic!("B"));

    rig.runtime.handle(AppCommand::PlayEntry(a));
    pump_until(&mut rig.runtime, "A playing", |view| is_playing(view, a));
    let a_token = now_playing(&rig.runtime.view())
        .load
        .unwrap_or_else(|| panic!("A's token"));

    if duplicates {
        rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    }
    rig.runtime.handle(AppCommand::PlayEntry(b));
    pump_until(&mut rig.runtime, "B failed", |view| {
        view.phase == PlaybackPhase::LoadFailed
    });
    assert_eq!(rig.runtime.session().pending_load_count(), 0);
    assert_eq!(rig.runtime.view().active, Some(a), "active stays A");
    assert_eq!(rig.runtime.view().last_requested, Some(b));

    std::fs::copy(FIVE, &b_path).unwrap_or_else(|error| panic!("create B: {error}"));
    rig.runtime.handle(retry(a));
    // The retry is a load of B itself, not of the selected or active A (A
    // ending and advancing into B would otherwise reach B too).
    assert_eq!(rig.runtime.view().last_requested, Some(b));
    assert_eq!(rig.runtime.session().pending_load_count(), 1);
    pump_until(&mut rig.runtime, "B adopted", |view| {
        view.now_playing
            .as_ref()
            .is_some_and(|now| now.loaded && now.entry == Some(b))
    });
    let failed_attempts = if duplicates { 2 } else { 1 };
    let b_token = now_playing(&rig.runtime.view())
        .load
        .unwrap_or_else(|| panic!("B's token"));
    assert!(
        b_token.get() > a_token.get() + failed_attempts,
        "a fresh token, not a reuse of a failed one: {a_token:?} -> {b_token:?}"
    );
    let _ = rig.runtime.shutdown();
}

#[test]
fn space_retries_a_failed_switch_with_a_fresh_token() {
    retry_a_failed_switch(
        |selected| AppCommand::PlayPause {
            selected: Some(selected),
        },
        false,
    );
    retry_a_failed_switch(
        |selected| AppCommand::Play {
            selected: Some(selected),
        },
        false,
    );
    retry_a_failed_switch(
        |selected| AppCommand::PlayPause {
            selected: Some(selected),
        },
        true,
    );
}

const FEED_URL: &str = "https://feeds.example/radio-t.xml";

fn podcast_rss(enclosure: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Radio-T</title><item><title>e1</title><guid>e1</guid><enclosure url="{enclosure}" type="audio/flac"/></item></channel></rss>"#
    )
    .into_bytes()
}

#[test]
fn a_resolution_failure_is_retryable_without_losing_the_previous_adoption() {
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let enclosure = server.url("/ep.flac");
    let library = feeds::Rig::new().expect("feeds rig");
    let subscription = library
        .seed(&podcast_rss(&enclosure), FEED_URL)
        .expect("seed");
    let cache_path = library
        .cache
        .path_for(&subscription.feed_id)
        .expect("cache path");
    std::fs::write(&cache_path, b"{").expect("corrupt the cache");

    let (_media, root) = media_dir(&["a.flac"]);
    let episode = MediaId::PodcastEpisode {
        feed: FeedId::new(feeds::FEED_ID.into()).expect("feed id"),
        episode: EpisodeKey::resolve(Some("e1"), None, None).expect("episode key"),
    };
    let podcast = NewQueueEntry::new(
        episode,
        QueueSource::Podcast {
            fallback: enclosure.parse().expect("url"),
        },
        Default::default(),
    )
    .expect("podcast entry");
    let clock: Arc<dyn continuo::clock::Clock> = Arc::new(SystemClock);
    let stores = LibraryStores {
        subscriptions: SubscriptionStore::new(
            library.root.path().join("data/continuo/subscriptions.json"),
            clock,
        ),
        cache: CacheStore::new(library.root.path().join("cache/continuo/feeds")),
    };
    let mut rig = rig_with_parts(
        seeded(vec![
            local_entry(&root.join("a.flac")),
            podcast,
            local_entry(&root.join("missing.flac")),
        ]),
        Some(stores),
        null_engine(),
    );
    let ids = row_ids(&rig.runtime);
    let (a, b, missing) = (ids[0], ids[1], ids[2]);

    rig.runtime.handle(AppCommand::PlayEntry(a));
    pump_until(&mut rig.runtime, "A playing", |view| is_playing(view, a));

    // Two older loads are still pending when B's resolution fails: one of A
    // that will succeed, and one of a missing file that will fail.
    rig.runtime.handle(AppCommand::PlayEntry(a));
    rig.runtime.handle(AppCommand::PlayEntry(missing));
    rig.runtime.handle(AppCommand::PlayEntry(b));
    let view = rig.runtime.view();
    assert_eq!(view.last_requested, Some(b));
    let resolution_error = view.status.expect("the resolution error is shown");
    assert!(!resolution_error.is_empty());
    let deadline = Instant::now() + Duration::from_secs(20);
    while rig.runtime.session().pending_load_count() > 0 {
        rig.runtime.pump();
        assert!(Instant::now() < deadline, "older loads never resolved");
        std::thread::sleep(Duration::from_millis(10));
    }
    pump_for(&mut rig.runtime, Duration::from_millis(100));
    let view = rig.runtime.view();
    assert_eq!(
        view.phase,
        PlaybackPhase::LoadFailed,
        "an older outcome cannot clear the later failure: {view:?}"
    );
    assert_eq!(
        view.status.as_deref(),
        Some(resolution_error.as_str()),
        "an older failure does not replace the later one's report"
    );
    assert_eq!(view.active, Some(a), "the previous adoption is kept");

    library
        .seed(&podcast_rss(&enclosure), FEED_URL)
        .expect("repair the cache");
    rig.runtime.handle(AppCommand::Play { selected: Some(a) });
    pump_until(&mut rig.runtime, "B loaded", |view| {
        view.active == Some(b) && view.now_playing.as_ref().is_some_and(|now| now.loaded)
    });
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

/// A and B submitted before a single event is drained: each `Loaded` adopts
/// its own occurrence, in submission order, and B ends up playing.
fn both_loads_adopt_their_own_occurrence_in_order() {
    let (_media, root) = media_dir(&["a.flac", "b.flac"]);
    let mut rig = rig_with(seeded(vec![
        local_entry(&root.join("a.flac")),
        local_entry(&root.join("b.flac")),
    ]));
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    assert_eq!(rig.runtime.session().pending_load_count(), 2);
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut settled = 0;
    // Keeps recording for a while after B plays, so a late regression to A
    // would show up too.
    while settled < 20 {
        rig.runtime.pump();
        let view = rig.runtime.view();
        if let Some(active) = view.active
            && seen.last() != Some(&active)
        {
            seen.push(active);
        }
        if is_playing(&view, ids[1]) {
            settled += 1;
        }
        assert!(Instant::now() < deadline, "B never played: {view:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        seen == vec![ids[1]] || seen == vec![ids[0], ids[1]],
        "adopted in submission order and B stays adopted: {seen:?}"
    );
    assert_eq!(rig.runtime.session().pending_load_count(), 0);
    let b_token = now_playing(&rig.runtime.view())
        .load
        .unwrap_or_else(|| panic!("B's token"));
    assert_eq!(b_token.get(), 2, "B's own token, the second one issued");
    let _ = rig.runtime.shutdown();
}

fn a_failed_second_load_is_not_retried_implicitly() {
    // A failed remote load keeps its source for one explicit reopen, so an
    // unrestricted start queued behind the load would retry it; every such
    // retry would reach the server again.
    let server = TestServer::start(Script::from_fixture("sine-5s.flac").status(404));
    let (_media, root) = media_dir(&["a.flac"]);
    let mut rig = rig_with(seeded(vec![local_entry(&root.join("a.flac"))]));
    rig.runtime
        .handle(AppCommand::Enqueue(vec![EnqueueItem::Url(
            server.url("/b.flac"),
        )]));
    let ids = row_ids(&rig.runtime);
    rig.runtime.handle(AppCommand::PlayEntry(ids[0]));
    rig.runtime.handle(AppCommand::PlayEntry(ids[1]));
    pump_until(&mut rig.runtime, "B failed", |view| {
        view.phase == PlaybackPhase::LoadFailed
    });
    let adopted = rig
        .runtime
        .session()
        .adopted()
        .unwrap_or_else(|| panic!("A adopted"))
        .request;
    assert_eq!(rig.runtime.view().active, Some(ids[0]));
    // The start command is dispatched right behind the failed load, so an
    // implicit reopen would already have reached the server by now.
    assert_eq!(server.requests().len(), 1, "B was fetched once");
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        rig.runtime.pump();
        let view = rig.runtime.view();
        assert_eq!(
            rig.runtime.session().pending_load_count(),
            0,
            "no retry registered"
        );
        assert_eq!(view.phase, PlaybackPhase::LoadFailed, "{view:?}");
        assert!(!now_playing(&view).loaded, "nothing reopened: {view:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(server.requests().len(), 1, "B was never fetched again");
    assert_eq!(
        rig.runtime.session().adopted().map(|load| load.request),
        Some(adopted)
    );
    let _ = rig.runtime.shutdown();
    server.shutdown();
}

#[test]
fn automatic_start_handles_intervening_outcomes() {
    // A start command delivered late for A (tests/m5_engine_load_outcomes.rs)
    // and a refused start command (the runtime's own unit tests) cannot be
    // staged through a real engine here.
    both_loads_adopt_their_own_occurrence_in_order();
    a_failed_second_load_is_not_retried_implicitly();
}
