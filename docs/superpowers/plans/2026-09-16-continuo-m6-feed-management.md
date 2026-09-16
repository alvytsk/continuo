# M6 Feed Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Subscribe, refresh (one or all) and unsubscribe from the terminal player's Podcasts tab, reusing the CLI's library calls and result wording.

**Architecture:** The single browse worker thread (`src/application/browse.rs`) gains three mutation requests and a lazily built `HttpService`; it answers with `BrowseResult::Mutation { request, outcome }` whose text is the CLI formatter written into a buffer. `BrowserState` (`src/tui/browser.rs`) gains a URL prompt, a confirm mode and a `pending` request that both blocks a second mutation and correlates the answer; on a matching answer it shows a notice and re-requests the visible list. The renderer draws the notice as a block above the rows.

**Tech Stack:** Rust 2024, Ratatui 0.30 + Crossterm 0.29, crossbeam-channel, tokio (already present via `HttpService`), `tracing`. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-09-16-continuo-m6-feed-management-design.md`

## Global Constraints

- Runtime code forbids `unsafe`, `unwrap` and `expect` (Clippy denies them); tests may use them.
- No new dependencies.
- `library.rs` stays free of `block_on`; the only bridge is `service.handle().block_on(...)`.
- Browsing (`Directory`, `Feeds`, `Episodes`) issues no network request; `tests/m5_no_network.rs` must stay green.
- Every string read from a feed or the filesystem passes through `crate::commands::displayable` before drawing.
- Gates before merge: `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked`.
- Commit after each task; conventional prefixes (`feat:`, `test:`, `docs:`).

## File map

| File | Change |
| --- | --- |
| `src/commands.rs` | `finish_subscribe`, `finish_unsubscribe`, `finish_refresh_one`, `finish_refresh_batch` become `pub(crate)`; new `pub(crate) fn report`. |
| `src/application/browse.rs` | Three request variants, `Mutation` result, lazy `HttpService`, `mutate`. |
| `tests/m6_feed_management.rs` | New: worker-level tests against the loopback server. |
| `src/tui/browser.rs` | `Notice`, `NoticeKind`, `prompt`/`confirm`/`pending`/`notice` fields, new keys, `apply` returns a follow-up request, `back()` re-reads feeds. |
| `src/tui/render/browser.rs` | Notice block above the rows, prompt/confirm text, Podcasts hint, new empty-list text. |
| `src/tui/render.rs` | One more help line. |
| `src/tui/mod.rs` | `Browsing::poll` issues the follow-up request. |
| `tests/m5_browser.rs` | Key, apply and drawing tests for the new modes. |
| `README.md`, `docs/architecture.md`, M5 spec, `docs/m6-acceptance.md` | M6 recorded. |

---

### Task 1: Worker mutations with CLI wording

**Files:**
- Modify: `src/commands.rs:447-590` (visibility) and add `report` after `stdout_failure`
- Modify: `src/application/browse.rs`
- Create: `tests/m6_feed_management.rs`

**Interfaces:**
- Consumes: `crate::library::{subscribe, refresh, refresh_all, unsubscribe}`, `HttpService::spawn(Limits::default())`, `service.handle().block_on`.
- Produces:
  - `BrowseRequest::{Subscribe { url: String }, Refresh { slug: Option<String> }, Unsubscribe { slug: String }}`
  - `BrowseResult::Mutation { request: BrowseRequest, outcome: Result<String, String> }`
  - `pub(crate) fn commands::report<T>(finish: impl FnOnce(&mut dyn Write, T) -> Result<(), FeedError>, outcome: T) -> Result<String, String>`

- [ ] **Step 1: Write the failing worker test**

Create `tests/m6_feed_management.rs`:

```rust
//! Design doc M6 §3 and §8: the browse worker's three mutation requests,
//! answered with the CLI's own wording, against a loopback feed server.
//! Browsing alone still makes no request.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use continuo::application::browse::{BrowseRequest, BrowseResult, BrowseWorker};
use continuo::application::runtime::LibraryStores;
use continuo::clock::SystemClock;
use continuo::feed::cache::CacheStore;
use continuo::library::list_feeds;
use continuo::subscription::store::SubscriptionStore;
use support::server::{DocumentReply, Script, TestServer};

fn rss(title: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0"?><rss version="2.0"><channel><title>{title}</title><item><title>e1</title><guid>e1</guid><enclosure url="https://cdn.example.org/1.mp3" type="audio/mpeg"/></item><item><title>e2</title><guid>e2</guid><enclosure url="https://cdn.example.org/2.mp3" type="audio/mpeg"/></item></channel></rss>"#
    )
    .into_bytes()
}

fn reply(path: &str, status: u16, body: Vec<u8>) -> DocumentReply {
    DocumentReply {
        path: path.to_string(),
        status,
        headers: Vec::new(),
        body,
        conditional: false,
        header_delay: Duration::ZERO,
    }
}

/// Stores rooted at `root`; built twice so the test can read what the
/// worker wrote.
fn stores(root: &Path) -> LibraryStores {
    LibraryStores {
        subscriptions: SubscriptionStore::new(
            root.join("data/continuo/subscriptions.json"),
            Arc::new(SystemClock),
        ),
        cache: CacheStore::new(root.join("cache/continuo/feeds")),
    }
}

fn answer(worker: &BrowseWorker, request: BrowseRequest) -> (BrowseRequest, Result<String, String>) {
    worker.request(request);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(result) = worker.try_result() {
            match result {
                BrowseResult::Mutation { request, outcome } => return (request, outcome),
                other => panic!("expected a mutation answer, got {other:?}"),
            }
        }
        assert!(Instant::now() < deadline, "the browse worker never answered");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn slugs(root: &Path) -> Vec<String> {
    let stores = stores(root);
    list_feeds(&stores.subscriptions, &stores.cache)
        .unwrap_or_else(|error| panic!("list: {error}"))
        .into_iter()
        .map(|feed| feed.slug)
        .collect()
}

#[test]
fn subscribe_refresh_and_unsubscribe_answer_with_the_cli_wording() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::documents(vec![
        reply("/a", 200, rss("Radio T")),
        reply("/bad", 500, Vec::new()),
    ]));
    let worker = BrowseWorker::spawn(Some(stores(root.path())));

    let request = BrowseRequest::Subscribe { url: server.url("/a") };
    let (echoed, outcome) = answer(&worker, request.clone());
    assert_eq!(echoed, request);
    assert_eq!(
        outcome.as_deref(),
        Ok("radio-t: subscribed, 2 episodes retained, 0 skipped"),
        "{outcome:?}"
    );
    assert_eq!(slugs(root.path()), ["radio-t"]);

    let (_, again) = answer(&worker, BrowseRequest::Subscribe { url: server.url("/a") });
    assert_eq!(again.as_deref(), Err("already subscribed as radio-t"), "{again:?}");

    let (_, invalid) = answer(&worker, BrowseRequest::Subscribe { url: "not a url".into() });
    assert!(invalid.is_err(), "{invalid:?}");
    let (_, failing) = answer(&worker, BrowseRequest::Subscribe { url: server.url("/bad") });
    assert!(failing.is_err(), "{failing:?}");
    assert_eq!(slugs(root.path()), ["radio-t"], "a failed subscribe changes nothing");

    let (_, refreshed) = answer(&worker, BrowseRequest::Refresh { slug: Some("radio-t".into()) });
    assert_eq!(
        refreshed.as_deref(),
        Ok("radio-t: updated, 2 episodes retained, 0 skipped"),
        "{refreshed:?}"
    );

    let (_, removed) = answer(&worker, BrowseRequest::Unsubscribe { slug: "radio-t".into() });
    assert_eq!(removed.as_deref(), Ok("radio-t: unsubscribed"), "{removed:?}");
    assert!(slugs(root.path()).is_empty());
    let (_, missing) = answer(&worker, BrowseRequest::Unsubscribe { slug: "radio-t".into() });
    assert_eq!(missing.as_deref(), Err("unknown feed: radio-t"), "{missing:?}");
    server.shutdown();
}

/// §8: a refresh-all whose first feed succeeds and second fails carries the
/// first feed's success line, the second's failure and the batch line.
#[test]
fn refresh_all_reports_every_feed_then_the_batch_error() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let alive = TestServer::start(Script::documents(vec![reply("/a", 200, rss("Radio T"))]));
    let doomed = TestServer::start(Script::documents(vec![reply("/b", 200, rss("Other Show"))]));
    let worker = BrowseWorker::spawn(Some(stores(root.path())));
    answer(&worker, BrowseRequest::Subscribe { url: alive.url("/a") });
    answer(&worker, BrowseRequest::Subscribe { url: doomed.url("/b") });
    assert_eq!(slugs(root.path()), ["radio-t", "other-show"]);
    doomed.shutdown();

    let (_, outcome) = answer(&worker, BrowseRequest::Refresh { slug: None });
    let text = outcome.expect_err("one feed failed");
    assert!(text.contains("radio-t: updated, 2 episodes retained, 0 skipped"), "{text}");
    assert!(text.contains("other-show: failed:"), "{text}");
    assert!(text.ends_with("1 of 2 feeds did not complete successfully"), "{text}");
    alive.shutdown();
}

/// Listing requests still touch nothing but the disk.
#[test]
fn browsing_makes_no_request() {
    let root = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let server = TestServer::start(Script::documents(vec![reply("/a", 200, rss("Radio T"))]));
    let worker = BrowseWorker::spawn(Some(stores(root.path())));
    answer(&worker, BrowseRequest::Subscribe { url: server.url("/a") });
    let before = server.requests().len();

    worker.request(BrowseRequest::Feeds);
    worker.request(BrowseRequest::Episodes { slug: "radio-t".into() });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = 0;
    while seen < 2 {
        assert!(Instant::now() < deadline, "listings never answered");
        match worker.try_result() {
            Some(BrowseResult::Feeds(Ok(feeds))) => {
                assert_eq!(feeds.len(), 1);
                seen += 1;
            }
            Some(BrowseResult::Episodes { episodes: Ok(episodes), .. }) => {
                assert_eq!(episodes.len(), 2);
                seen += 1;
            }
            Some(other) => panic!("unexpected {other:?}"),
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    assert_eq!(server.requests().len(), before, "browsing made a request");
    server.shutdown();
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --locked --test m6_feed_management`
Expected: compile error, `no variant named Subscribe` / `Mutation`.

- [ ] **Step 3: Expose the CLI formatters and add `report`**

In `src/commands.rs` change the four signatures to `pub(crate)`:

```rust
pub(crate) fn finish_refresh_batch(
pub(crate) fn finish_refresh_one(out: &mut dyn Write, outcome: RefreshOutcome) -> Result<(), FeedError> {
pub(crate) fn finish_subscribe(out: &mut dyn Write, outcome: SubscribeOutcome) -> Result<(), FeedError> {
pub(crate) fn finish_unsubscribe(out: &mut dyn Write, outcome: UnsubscribeOutcome) -> Result<(), FeedError> {
```

Add after `stdout_failure`:

```rust
/// The CLI's report of `outcome` as text, for a front end that shows it
/// instead of printing it: `Ok` is what stdout would have carried, `Err`
/// that text (when any) followed by the error the exit status would have
/// named. Trailing newlines are dropped; the TUI splits on the rest.
pub(crate) fn report<T>(
    finish: impl FnOnce(&mut dyn Write, T) -> Result<(), FeedError>,
    outcome: T,
) -> Result<String, String> {
    let mut out = Vec::new();
    let status = finish(&mut out, outcome);
    let text = String::from_utf8_lossy(&out).trim_end().to_owned();
    match status {
        Ok(()) => Ok(text),
        Err(error) if text.is_empty() => Err(error.to_string()),
        Err(error) => Err(format!("{text}\n{error}")),
    }
}
```

- [ ] **Step 4: Extend the worker**

In `src/application/browse.rs` replace the module doc's second paragraph:

```rust
//! Only an explicit `Subscribe` or `Refresh` request touches the network.
//! The worker owns its own [`LibraryStores`] and builds an `HttpService`
//! lazily, on the first request that needs one; the feed listings are the
//! same read-only snapshot reads `continuo feeds` uses, so opening the
//! browser or listing episodes never refreshes a feed. A listing is a
//! directory read and a `stat` per entry — no recursion and no media
//! metadata probing. Mutations run one at a time, in order with the
//! listings.
//!
//! ponytail: one thread for listings and mutations, so a directory read
//! issued during a refresh-all waits behind it; a second worker for
//! mutations is the upgrade if that wait ever matters.
```

Add imports:

```rust
use std::sync::Arc;

use crate::commands::{
    finish_refresh_batch, finish_refresh_one, finish_subscribe, finish_unsubscribe, report,
};
use crate::http::limits::Limits;
use crate::http::service::HttpService;
use crate::library::{
    EpisodeCandidate, FeedSummary, episode_candidates, list_feeds, refresh, refresh_all,
    subscribe, unsubscribe,
};
```

Extend the enums:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrowseRequest {
    Directory(PathBuf),
    Feeds,
    Episodes { slug: String },
    /// `continuo subscribe <url>`, slug derived.
    Subscribe { url: String },
    /// `continuo refresh [slug]`: one feed, or every feed for `None`.
    Refresh { slug: Option<String> },
    /// `continuo unsubscribe <slug>`.
    Unsubscribe { slug: String },
}

#[derive(Clone, Debug)]
pub enum BrowseResult {
    Directory { path: PathBuf, entries: Result<Vec<DirEntry>, String> },
    Feeds(Result<Vec<FeedSummary>, String>),
    Episodes { slug: String, episodes: Result<Vec<EpisodeCandidate>, String> },
    /// A mutation's answer, echoing the request so the browser can tell
    /// whose answer it is. The text is the CLI's own wording (§3).
    Mutation {
        request: BrowseRequest,
        outcome: Result<String, String>,
    },
}
```

Change `serve` to hold the service:

```rust
fn serve(
    library: Option<&LibraryStores>,
    requests: &Receiver<BrowseRequest>,
    results: &Sender<BrowseResult>,
) {
    let mut http = None;
    for request in requests {
        if results.send(answer(library, &mut http, request)).is_err() {
            return;
        }
    }
}
```

Extend `answer_with`:

```rust
        request @ (BrowseRequest::Subscribe { .. }
        | BrowseRequest::Refresh { .. }
        | BrowseRequest::Unsubscribe { .. }) => BrowseResult::Mutation {
            request,
            outcome: Err(message.to_owned()),
        },
```

Change `answer` to take `http: &mut Option<Arc<HttpService>>` and add the arm before the closing brace:

```rust
        request @ (BrowseRequest::Subscribe { .. }
        | BrowseRequest::Refresh { .. }
        | BrowseRequest::Unsubscribe { .. }) => {
            let outcome = match library {
                Some(stores) => mutate(stores, http, &request),
                None => Err(NO_LIBRARY.to_owned()),
            };
            match &outcome {
                Ok(text) => tracing::info!(?request, "{text}"),
                Err(text) => tracing::info!(?request, "failed: {text}"),
            }
            BrowseResult::Mutation { request, outcome }
        }
```

Add the two helpers:

```rust
/// Runs one mutation the way its CLI command does, and reports it the way
/// the CLI prints it (§3). Only `Subscribe` and `Refresh` need the service.
fn mutate(
    stores: &LibraryStores,
    http: &mut Option<Arc<HttpService>>,
    request: &BrowseRequest,
) -> Result<String, String> {
    let subs = &stores.subscriptions;
    let cache = &stores.cache;
    match request {
        BrowseRequest::Subscribe { url } => {
            let service = http_service(http)?;
            let outcome = service
                .handle()
                .block_on(subscribe(&service, subs, cache, url, None))
                .map_err(|error| error.to_string())?;
            report(finish_subscribe, outcome)
        }
        BrowseRequest::Refresh { slug: Some(slug) } => {
            let service = http_service(http)?;
            let outcome = service
                .handle()
                .block_on(refresh(&service, subs, cache, slug))
                .map_err(|error| error.to_string())?;
            report(finish_refresh_one, outcome)
        }
        BrowseRequest::Refresh { slug: None } => {
            let service = http_service(http)?;
            let outcomes = service
                .handle()
                .block_on(refresh_all(&service, subs, cache))
                .map_err(|error| error.to_string())?;
            report(finish_refresh_batch, outcomes)
        }
        BrowseRequest::Unsubscribe { slug } => {
            let outcome = unsubscribe(subs, cache, slug).map_err(|error| error.to_string())?;
            report(finish_unsubscribe, outcome)
        }
        BrowseRequest::Directory(_) | BrowseRequest::Feeds | BrowseRequest::Episodes { .. } => {
            Err("not a mutation".to_owned())
        }
    }
}

/// The worker's HTTP service, built on the first request that needs one
/// and kept for the thread's lifetime. A failure to start is this request's
/// error; the next request tries again.
fn http_service(slot: &mut Option<Arc<HttpService>>) -> Result<Arc<HttpService>, String> {
    if let Some(service) = slot {
        return Ok(Arc::clone(service));
    }
    let service = HttpService::spawn(Limits::default()).map_err(|error| error.to_string())?;
    *slot = Some(Arc::clone(&service));
    Ok(service)
}
```

- [ ] **Step 5: Run the new suite and the no-network suite**

Run: `cargo test --locked --test m6_feed_management --test m5_no_network --test m5_browser`
Expected: all pass. If `refresh_all_reports_every_feed_then_the_batch_error` fails on the exact `other-show: failed:` text, print `text` and match on the `FeedError` display actually produced by a refused connection; the assertion on the batch line must hold as written.

- [ ] **Step 6: Commit**

```bash
git add src/commands.rs src/application/browse.rs tests/m6_feed_management.rs
git commit -m "feat(browse): subscribe, refresh and unsubscribe requests on the browse worker"
```

---

### Task 2: Browser state — prompt, confirm, pending, notice

**Files:**
- Modify: `src/tui/browser.rs`
- Test: `tests/m5_browser.rs`

**Interfaces:**
- Consumes: Task 1's `BrowseRequest` variants and `BrowseResult::Mutation`.
- Produces:
  - `pub enum NoticeKind { Working, Ok, Err }`, `pub struct Notice { pub text: String, pub kind: NoticeKind }`
  - `BrowserState { pub prompt: Option<String>, pub confirm: Option<String>, pub pending: Option<BrowseRequest>, pub notice: Option<Notice>, .. }`
  - `pub fn apply(&mut self, result: BrowseResult) -> Option<BrowseRequest>` — a follow-up request the caller must issue.

- [ ] **Step 1: Write the failing tests**

Append to `tests/m5_browser.rs` (the imports at the top gain `use continuo::tui::browser::{BrowserEffect, BrowserState, BrowserTab, NoticeKind};`):

```rust
/// A Podcasts tab showing `feeds`.
fn podcasts(feeds: Vec<FeedSummary>) -> BrowserState {
    let mut state = BrowserState::new(PathBuf::from("/music"));
    press(&mut state, &[KeyCode::Tab]);
    state.apply(BrowseResult::Feeds(Ok(feeds)));
    state
}

fn requests(effects: &[BrowserEffect]) -> Vec<BrowseRequest> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            BrowserEffect::Request(request) => Some(request.clone()),
            _ => None,
        })
        .collect()
}

fn mutation(request: BrowseRequest, outcome: Result<&str, &str>) -> BrowseResult {
    BrowseResult::Mutation {
        request,
        outcome: outcome.map(str::to_owned).map_err(str::to_owned),
    }
}

#[test]
fn the_prompt_swallows_shortcuts_and_enter_subscribes() {
    let mut state = podcasts(vec![feed("one")]);
    assert!(press(&mut state, &[KeyCode::Char('a')]).is_empty());
    assert_eq!(state.prompt.as_deref(), Some(""));

    // `q`, `b`, space and `d` are text now, not shortcuts.
    let typed = press(
        &mut state,
        &[KeyCode::Char('q'), KeyCode::Char('b'), KeyCode::Char(' '), KeyCode::Char('d')],
    );
    assert!(typed.is_empty(), "{typed:?}");
    assert_eq!(state.prompt.as_deref(), Some("qb d"));
    assert!(state.confirm.is_none());

    press(&mut state, &[KeyCode::Backspace, KeyCode::Backspace, KeyCode::Backspace, KeyCode::Backspace]);
    assert!(press(&mut state, &[KeyCode::Enter]).is_empty(), "empty submits nothing");
    assert!(state.prompt.is_none());

    press(&mut state, &[KeyCode::Char('a'), KeyCode::Char('x')]);
    assert!(press(&mut state, &[KeyCode::Esc]).is_empty(), "Esc cancels, never closes");
    assert!(state.prompt.is_none());

    press(&mut state, &[KeyCode::Char('a')]);
    for c in "https://x.example/f ".chars() {
        press(&mut state, &[KeyCode::Char(c)]);
    }
    let effects = press(&mut state, &[KeyCode::Enter]);
    let expected = BrowseRequest::Subscribe {
        url: "https://x.example/f".to_owned(),
    };
    assert_eq!(requests(&effects), [expected.clone()]);
    assert_eq!(state.pending, Some(expected));
    assert_eq!(state.notice.as_ref().map(|n| n.kind), Some(NoticeKind::Working));
    assert!(state.prompt.is_none());
}

#[test]
fn refresh_and_remove_need_a_feed_and_no_pending_mutation() {
    let mut empty = podcasts(Vec::new());
    for code in [KeyCode::Char('r'), KeyCode::Char('R'), KeyCode::Char('d')] {
        assert!(press(&mut empty, &[code]).is_empty(), "{code:?}");
    }
    assert!(empty.confirm.is_none());

    let mut state = podcasts(vec![feed("one"), feed("two")]);
    press(&mut state, &[KeyCode::Down]);
    let effects = press(&mut state, &[KeyCode::Char('r')]);
    let expected = BrowseRequest::Refresh {
        slug: Some("two".to_owned()),
    };
    assert_eq!(requests(&effects), [expected.clone()]);
    assert_eq!(state.pending, Some(expected));

    // Everything management-related waits while a mutation is pending.
    for code in [KeyCode::Char('a'), KeyCode::Char('r'), KeyCode::Char('R'), KeyCode::Char('d')] {
        assert!(press(&mut state, &[code]).is_empty(), "{code:?}");
    }
    assert!(state.prompt.is_none() && state.confirm.is_none());

    let mut all = podcasts(vec![feed("one")]);
    assert_eq!(
        requests(&press(&mut all, &[KeyCode::Char('R')])),
        [BrowseRequest::Refresh { slug: None }]
    );
}

#[test]
fn remove_asks_first_and_only_y_confirms() {
    let mut state = podcasts(vec![feed("one"), feed("two")]);
    assert!(press(&mut state, &[KeyCode::Char('d')]).is_empty());
    assert_eq!(state.confirm.as_deref(), Some("one"));
    assert!(press(&mut state, &[KeyCode::Char('n')]).is_empty());
    assert!(state.confirm.is_none() && state.pending.is_none());

    press(&mut state, &[KeyCode::Char('d')]);
    let effects = press(&mut state, &[KeyCode::Char('y')]);
    let expected = BrowseRequest::Unsubscribe {
        slug: "one".to_owned(),
    };
    assert_eq!(requests(&effects), [expected.clone()]);
    assert_eq!(state.pending, Some(expected));
}

#[test]
fn r_and_d_inside_an_open_feed_act_on_that_feed() {
    let mut state = podcasts(vec![feed("one"), feed("two")]);
    press(&mut state, &[KeyCode::Down, KeyCode::Enter]);
    state.apply(BrowseResult::Episodes {
        slug: "two".to_owned(),
        episodes: Ok(vec![episode("g1", Some("https://cdn.example.org/1.mp3"))]),
    });
    assert_eq!(
        requests(&press(&mut state, &[KeyCode::Char('r')])),
        [BrowseRequest::Refresh {
            slug: Some("two".to_owned())
        }]
    );
    let follow_up = state.apply(mutation(
        BrowseRequest::Refresh {
            slug: Some("two".to_owned()),
        },
        Ok("two: updated"),
    ));
    assert_eq!(
        follow_up,
        Some(BrowseRequest::Episodes {
            slug: "two".to_owned()
        }),
        "an open feed re-reads its episodes"
    );
    // The re-read is loading until its answer lands; management keys wait.
    assert!(press(&mut state, &[KeyCode::Char('d')]).is_empty());
    assert!(state.confirm.is_none());
    state.apply(BrowseResult::Episodes {
        slug: "two".to_owned(),
        episodes: Ok(Vec::new()),
    });
    press(&mut state, &[KeyCode::Char('d')]);
    assert_eq!(state.confirm.as_deref(), Some("two"));
}

#[test]
fn back_re_reads_the_feed_list_while_keeping_the_cached_rows() {
    let mut state = podcasts(vec![feed("one"), feed("two")]);
    press(&mut state, &[KeyCode::Down, KeyCode::Enter]);
    state.apply(BrowseResult::Episodes {
        slug: "two".to_owned(),
        episodes: Ok(Vec::new()),
    });
    let effects = press(&mut state, &[KeyCode::Backspace]);
    assert_eq!(requests(&effects), [BrowseRequest::Feeds]);
    assert!(state.episodes.is_none());
    assert_eq!(state.feeds.len(), 2, "cached rows stay up");
    assert_eq!(state.cursor, 1);
    assert!(!state.loading);
}

#[test]
fn a_matching_answer_shows_the_notice_and_re_reads_the_list() {
    let mut state = podcasts(vec![feed("one")]);
    press(&mut state, &[KeyCode::Char('r')]);
    let request = BrowseRequest::Refresh {
        slug: Some("one".to_owned()),
    };

    // Someone else's answer, and a listing answer, leave `pending` alone.
    assert_eq!(
        state.apply(mutation(BrowseRequest::Refresh { slug: None }, Ok("x"))),
        None
    );
    assert!(state.pending.is_some());

    let follow_up = state.apply(mutation(request.clone(), Ok("one: updated")));
    assert_eq!(follow_up, Some(BrowseRequest::Feeds));
    assert!(state.pending.is_none());
    assert!(state.loading);
    let notice = state.notice.clone().expect("a notice");
    assert_eq!((notice.text.as_str(), notice.kind), ("one: updated", NoticeKind::Ok));

    // The listing answer settles loading but keeps the notice.
    state.apply(BrowseResult::Feeds(Ok(vec![feed("one")])));
    assert!(!state.loading);
    assert_eq!(state.notice.as_ref().map(|n| n.text.as_str()), Some("one: updated"));

    press(&mut state, &[KeyCode::Char('r')]);
    state.apply(mutation(request, Err("one: failed: boom")));
    assert_eq!(state.notice.as_ref().map(|n| n.kind), Some(NoticeKind::Err));

    // A fresh browser has nothing pending, so a late answer changes nothing.
    let mut fresh = podcasts(vec![feed("one")]);
    assert_eq!(
        fresh.apply(mutation(BrowseRequest::Refresh { slug: None }, Ok("late"))),
        None
    );
    assert!(fresh.notice.is_none());
}

#[test]
fn removing_the_open_feed_returns_to_the_list_whatever_the_outcome() {
    for outcome in [
        Ok("two: unsubscribed"),
        Err("two: the subscription was removed, but its cached episodes could not be deleted: x"),
        Err("unknown feed: two"),
    ] {
        let mut state = podcasts(vec![feed("one"), feed("two")]);
        press(&mut state, &[KeyCode::Down, KeyCode::Enter]);
        state.apply(BrowseResult::Episodes {
            slug: "two".to_owned(),
            episodes: Ok(Vec::new()),
        });
        press(&mut state, &[KeyCode::Char('d'), KeyCode::Char('y')]);
        let follow_up = state.apply(mutation(
            BrowseRequest::Unsubscribe {
                slug: "two".to_owned(),
            },
            outcome,
        ));
        assert!(state.episodes.is_none(), "{outcome:?}");
        assert_eq!(follow_up, Some(BrowseRequest::Feeds), "{outcome:?}");
        assert!(state.notice.is_some());
    }
}

#[test]
fn an_answer_on_the_files_tab_shows_the_notice_and_requests_nothing() {
    let mut state = podcasts(vec![feed("one")]);
    press(&mut state, &[KeyCode::Char('R')]);
    press(&mut state, &[KeyCode::Tab]);
    assert_eq!(state.tab, BrowserTab::Files);
    assert!(state.pending.is_some(), "a tab switch keeps the mutation");
    let follow_up = state.apply(mutation(BrowseRequest::Refresh { slug: None }, Ok("done")));
    assert_eq!(follow_up, None);
    assert_eq!(state.notice.as_ref().map(|n| n.text.as_str()), Some("done"));
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --locked --test m5_browser`
Expected: compile error, `no field prompt` / `NoticeKind` not found.

- [ ] **Step 3: Implement the state**

In `src/tui/browser.rs`, after `BrowserTab`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoticeKind {
    /// A mutation is in flight.
    Working,
    Ok,
    Err,
}

/// A mutation's progress or outcome (design doc M6 §5), drawn above the
/// rows and kept until the next key that changes what is shown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notice {
    pub text: String,
    pub kind: NoticeKind,
}
```

Add the fields to `BrowserState` after `error`:

```rust
    /// The feed URL being typed after `a`.
    pub prompt: Option<String>,
    /// The slug whose removal awaits `y`.
    pub confirm: Option<String>,
    /// The mutation in flight: blocks a second one and identifies its
    /// answer (M6 §5).
    pub pending: Option<BrowseRequest>,
    pub notice: Option<Notice>,
```

and initialize all four to `None` in `new`.

Replace `apply` and `settle`'s neighbors:

```rust
    /// Takes in a worker's answer when it is for the list on screen or for
    /// the mutation in flight; otherwise — a directory already left, a feed
    /// no longer viewed, the other tab, a mutation from a browser since
    /// closed — drops it. Returns the follow-up read a mutation calls for.
    pub fn apply(&mut self, result: BrowseResult) -> Option<BrowseRequest> {
        let mut follow_up = None;
        match result {
            BrowseResult::Directory { path, entries } => {
                if self.tab == BrowserTab::Files && path == self.cwd {
                    self.entries = self.settle(entries);
                }
            }
            BrowseResult::Feeds(feeds) => {
                if self.tab == BrowserTab::Podcasts && self.episodes.is_none() {
                    self.feeds = self.settle(feeds);
                }
            }
            BrowseResult::Episodes { slug, episodes } => {
                let viewing = matches!(&self.episodes, Some((current, _)) if *current == slug);
                if self.tab == BrowserTab::Podcasts && viewing {
                    let list = self.settle(episodes);
                    self.episodes = Some((slug, list));
                }
            }
            BrowseResult::Mutation { request, outcome } => {
                // ponytail: an identical mutation resubmitted after a close
                // and reopen adopts the earlier answer; both committed, and
                // the list is re-read either way.
                if self.pending.as_ref() != Some(&request) {
                    return None;
                }
                self.pending = None;
                if let BrowseRequest::Unsubscribe { slug } = &request
                    && matches!(&self.episodes, Some((open, _)) if open == slug)
                {
                    // Removed before any cache cleanup could fail, and an
                    // unknown slug was already gone: the view has to go.
                    self.leave_episodes();
                }
                self.notice = Some(match outcome {
                    Ok(text) => Notice { text, kind: NoticeKind::Ok },
                    Err(text) => Notice { text, kind: NoticeKind::Err },
                });
                if self.tab == BrowserTab::Podcasts {
                    self.loading = true;
                    follow_up = Some(match &self.episodes {
                        Some((slug, _)) => BrowseRequest::Episodes { slug: slug.clone() },
                        None => BrowseRequest::Feeds,
                    });
                }
            }
        }
        self.cursor = self.cursor.min(self.len().saturating_sub(1));
        follow_up
    }
```

Replace `handle_key`'s opening:

```rust
    /// Up/Down/`j`/`k` move, Tab switches tabs, Enter opens or enqueues,
    /// Space marks, Backspace/Left goes back, `b`/Esc closes. On the
    /// Podcasts tab `a` prompts for a feed URL, `r`/`R` refresh one/all and
    /// `d` asks before removing (M6 §4); the prompt and the question take
    /// every key while they are up. A Ctrl or Alt chord does nothing, as in
    /// the rest of the keyboard map.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<BrowserEffect> {
        if key.kind != KeyEventKind::Press {
            return Vec::new();
        }
        if self.prompt.is_some() {
            return self.prompt_key(key);
        }
        if let Some(slug) = self.confirm.take() {
            return if key.code == KeyCode::Char('y') && !blocks_ordinary_bindings(&key) {
                self.submit(BrowseRequest::Unsubscribe { slug })
            } else {
                Vec::new()
            };
        }
        if blocks_ordinary_bindings(&key) {
            return Vec::new();
        }
        match key.code {
```

and add these arms before `_ => Vec::new(),`:

```rust
            KeyCode::Char('a') if self.can_manage() => {
                self.notice = None;
                self.prompt = Some(String::new());
                Vec::new()
            }
            KeyCode::Char('r') if self.can_manage() => match self.target_slug() {
                Some(slug) => self.submit(BrowseRequest::Refresh { slug: Some(slug) }),
                None => Vec::new(),
            },
            KeyCode::Char('R') if self.can_manage() && !self.feeds.is_empty() => {
                self.submit(BrowseRequest::Refresh { slug: None })
            }
            KeyCode::Char('d') if self.can_manage() => {
                if let Some(slug) = self.target_slug() {
                    self.notice = None;
                    self.confirm = Some(slug);
                }
                Vec::new()
            }
```

Add the helpers (after `enqueueable`):

```rust
    /// Whether a management key may act: the Podcasts tab, nothing loading,
    /// no mutation in flight.
    fn can_manage(&self) -> bool {
        self.tab == BrowserTab::Podcasts && !self.loading && self.pending.is_none()
    }

    /// The feed `r` and `d` act on: the open feed, else the cursor's row.
    fn target_slug(&self) -> Option<String> {
        match &self.episodes {
            Some((slug, _)) => Some(slug.clone()),
            None => self.feeds.get(self.cursor).map(|feed| feed.slug.clone()),
        }
    }

    /// Sends a mutation and remembers it until its answer arrives.
    fn submit(&mut self, request: BrowseRequest) -> Vec<BrowserEffect> {
        let text = match &request {
            BrowseRequest::Subscribe { .. } => "Subscribing…",
            BrowseRequest::Refresh { .. } => "Refreshing…",
            BrowseRequest::Unsubscribe { .. } => "Removing…",
            BrowseRequest::Directory(_) | BrowseRequest::Feeds | BrowseRequest::Episodes { .. } => {
                "Loading…"
            }
        };
        self.notice = Some(Notice {
            text: text.to_owned(),
            kind: NoticeKind::Working,
        });
        self.pending = Some(request.clone());
        vec![BrowserEffect::Request(request)]
    }

    /// Printable characters append, Backspace pops, Enter submits the
    /// trimmed URL (nothing when empty), Esc cancels. Shortcuts never fire.
    fn prompt_key(&mut self, key: KeyEvent) -> Vec<BrowserEffect> {
        match key.code {
            KeyCode::Esc => {
                self.prompt = None;
                Vec::new()
            }
            KeyCode::Backspace => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.pop();
                }
                Vec::new()
            }
            KeyCode::Enter => {
                let url = self.prompt.take().unwrap_or_default().trim().to_owned();
                if url.is_empty() {
                    Vec::new()
                } else {
                    self.submit(BrowseRequest::Subscribe { url })
                }
            }
            KeyCode::Char(c) if !c.is_control() && !blocks_ordinary_bindings(&key) => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.push(c);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Returns from an episode view to the feed list, cursor on the feed
    /// just left, cached rows still showing.
    fn leave_episodes(&mut self) {
        if let Some((slug, _)) = self.episodes.take() {
            self.cursor = self
                .feeds
                .iter()
                .position(|feed| feed.slug == slug)
                .unwrap_or(0);
            self.marked.clear();
            self.loading = false;
            self.error = None;
        }
    }
```

Add `self.notice = None;` as the first line of `start_loading`. Replace `back`'s Podcasts arm:

```rust
            BrowserTab::Podcasts => {
                if self.episodes.is_none() {
                    return Vec::new();
                }
                self.leave_episodes();
                self.notice = None;
                // Re-read rather than trust rows cached before a mutation;
                // the cached rows stay up until the answer lands (M6 §4).
                vec![BrowserEffect::Request(BrowseRequest::Feeds)]
            }
```

- [ ] **Step 4: Run the suite**

Run: `cargo test --locked --test m5_browser`
Expected: all pass, including the ten pre-existing tests. `tui::run` does not compile yet against the new `apply` return type only if it uses the value; it ignores it today, so the crate builds. Task 4 wires it.

- [ ] **Step 5: Commit**

```bash
git add src/tui/browser.rs tests/m5_browser.rs
git commit -m "feat(tui): subscribe, refresh and remove keys on the Podcasts tab"
```

---

### Task 3: Rendering — notice block, prompt, confirmation, hints

**Files:**
- Modify: `src/tui/render/browser.rs`
- Modify: `src/tui/render.rs:86-104`
- Test: `tests/m5_browser.rs`

**Interfaces:**
- Consumes: `BrowserState::{prompt, confirm, notice}`, `Notice`, `NoticeKind` from Task 2.
- Produces: nothing new for later tasks.

- [ ] **Step 1: Write the failing drawing tests**

Append to `tests/m5_browser.rs`:

```rust
#[test]
fn the_overlay_draws_the_prompt_the_question_and_the_notice_above_rows() {
    let mut state = podcasts(vec![feed("one"), feed("two")]);
    press(&mut state, &[KeyCode::Char('a')]);
    for c in "https://x.example/f".chars() {
        press(&mut state, &[KeyCode::Char(c)]);
    }
    let (text, _) = screen(&state);
    assert!(text.contains("Feed URL: https://x.example/f"), "{text}");
    assert!(text.contains("a subscribe"), "podcasts hint: {text}");
    press(&mut state, &[KeyCode::Esc, KeyCode::Char('d')]);
    let (text, _) = screen(&state);
    assert!(text.contains("Remove one? y/N"), "{text}");
    press(&mut state, &[KeyCode::Char('y')]);
    let (text, _) = screen(&state);
    assert!(text.contains("Removing…"), "{text}");

    state.apply(mutation(
        BrowseRequest::Unsubscribe {
            slug: "one".to_owned(),
        },
        Ok("one: unsubscribed"),
    ));
    state.apply(BrowseResult::Feeds(Ok(vec![feed("two")])));
    let (text, _) = screen(&state);
    let notice_row = text
        .lines()
        .position(|line| line.contains("one: unsubscribed"))
        .unwrap_or_else(|| panic!("no notice: {text}"));
    let feed_row = text
        .lines()
        .position(|line| line.contains("two title"))
        .unwrap_or_else(|| panic!("no rows: {text}"));
    assert!(notice_row < feed_row, "notice above the rows: {text}");

    // An empty feed list points at the key, not the CLI.
    let empty = podcasts(Vec::new());
    let (text, _) = screen(&empty);
    assert!(text.contains("press a to add a feed URL"), "{text}");
}

#[test]
fn a_long_notice_is_cut_to_a_third_of_the_list_with_a_marker() {
    let mut state = podcasts(vec![feed("one")]);
    press(&mut state, &[KeyCode::Char('R')]);
    let lines: Vec<String> = (1..=8).map(|n| format!("line{n}")).collect();
    state.apply(mutation(
        BrowseRequest::Refresh { slug: None },
        Err(&lines.join("\n")),
    ));
    state.apply(BrowseResult::Feeds(Ok(vec![feed("one")])));
    let (text, buffer) = screen(&state);
    // The 90×24 screen gives the list 17 rows; a third is 5.
    assert!(text.contains("line5"), "{text}");
    assert!(!text.contains("line6"), "{text}");
    assert!(text.contains("+3 more lines, see log"), "{text}");
    assert!(text.contains("one title"), "rows still drawn: {text}");
    let marker_row = text
        .lines()
        .position(|line| line.contains("+3 more lines"))
        .unwrap_or_else(|| panic!("{text}"));
    let amber = buffer[(4, u16::try_from(marker_row).unwrap_or(0))].fg;
    let ok_state = {
        let mut s = podcasts(vec![feed("one")]);
        press(&mut s, &[KeyCode::Char('R')]);
        s.apply(mutation(BrowseRequest::Refresh { slug: None }, Ok("fine")));
        s
    };
    let (ok_text, ok_buffer) = screen(&ok_state);
    let ok_row = ok_text
        .lines()
        .position(|line| line.contains("fine"))
        .unwrap_or_else(|| panic!("{ok_text}"));
    let muted = ok_buffer[(4, u16::try_from(ok_row).unwrap_or(0))].fg;
    assert_ne!(amber, muted, "errors and successes differ in color");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --locked --test m5_browser the_overlay_draws_the_prompt a_long_notice`
Expected: FAIL on `Feed URL:` missing.

- [ ] **Step 3: Draw the block**

In `src/tui/render/browser.rs` change the imports and constants:

```rust
use ratatui::style::{Color, Modifier, Style};
use crate::tui::browser::{BrowserState, BrowserTab, NoticeKind};

const HINTS: &str = "enter open/add · space mark · tab files/podcasts · ⌫ back · b close";
const PODCAST_HINTS: &str =
    "enter open/add · space mark · a subscribe · r/R refresh · d remove · ⌫ back · b close";
const NO_FEEDS: &str = "No subscriptions — press a to add a feed URL";
const PROMPT: &str = "Feed URL: ";
```

In `draw_browser`, pick the hint by tab:

```rust
    let hints = match browser.tab {
        BrowserTab::Files => HINTS,
        BrowserTab::Podcasts => PODCAST_HINTS,
    };
    if body.height >= 4 {
        Line::styled(hints, Style::new().fg(theme.muted)).render(row(body, hint_y), buffer);
    }
```

Replace the start of `draw_list` so the notice block takes the top rows and the rest of the function draws into `rows`:

```rust
fn draw_list(buffer: &mut Buffer, area: Rect, browser: &BrowserState, theme: &Theme) {
    if area.is_empty() {
        return;
    }
    let rows = draw_notice_block(buffer, area, browser, theme);
    if rows.is_empty() {
        return;
    }
    let notice = if browser.loading {
```

then, in the same function, replace every later `area` with `rows` (the single-row notice render, `visible_rows(..., usize::from(rows.height))`, `let mut y = rows.y;`, `row(rows, y)`).

Add the block function:

```rust
/// The prompt, the confirmation question or the mutation notice, as a block
/// of up to a third of `area` above the rows (M6 §5); returns what is left
/// for the rows. A notice with more lines than fit ends in a marker; the
/// worker logged the whole text.
fn draw_notice_block(
    buffer: &mut Buffer,
    area: Rect,
    browser: &BrowserState,
    theme: &Theme,
) -> Rect {
    let Some((lines, color)) = notice_lines(browser, theme) else {
        return area;
    };
    let budget = usize::from(area.height / 3).max(1);
    let shown = lines.len().min(budget);
    let hidden = lines.len() - shown;
    let height = u16::try_from(shown + usize::from(hidden > 0)).unwrap_or(u16::MAX);
    let mut y = area.y;
    for line in lines.iter().take(shown) {
        Line::styled(line.clone(), Style::new().fg(color)).render(row(area, y), buffer);
        y = y.saturating_add(1);
    }
    if hidden > 0 {
        Line::styled(
            format!("+{hidden} more lines, see log"),
            Style::new().fg(color).add_modifier(Modifier::DIM),
        )
        .render(row(area, y), buffer);
    }
    Rect {
        y: area.y.saturating_add(height),
        height: area.height.saturating_sub(height),
        ..area
    }
    .intersection(area)
}

fn notice_lines(browser: &BrowserState, theme: &Theme) -> Option<(Vec<String>, Color)> {
    if let Some(prompt) = &browser.prompt {
        return Some((vec![format!("{PROMPT}{}▏", displayable(prompt))], theme.text));
    }
    if let Some(slug) = &browser.confirm {
        return Some((vec![format!("Remove {}? y/N", displayable(slug))], theme.amber));
    }
    let notice = browser.notice.as_ref()?;
    let color = match notice.kind {
        NoticeKind::Err => theme.amber,
        NoticeKind::Working | NoticeKind::Ok => theme.muted,
    };
    Some((notice.text.lines().map(displayable).collect(), color))
}
```

In `src/tui/render.rs` make `HELP_LINES` `[&str; 19]` and insert after the `"b               Open/close browser",` line:

```rust
    "  in Podcasts   a subscribe · r/R refresh one/all · d unsubscribe",
```

- [ ] **Step 4: Run the suite**

Run: `cargo test --locked --test m5_browser --test m5_tui_screens`
Expected: pass. If a help-overlay screen test counts lines, update its expected count by one.

- [ ] **Step 5: Commit**

```bash
git add src/tui/render/browser.rs src/tui/render.rs tests/m5_browser.rs
git commit -m "feat(tui): notice block, feed prompt and confirmation in the browser"
```

---

### Task 4: Wire the follow-up request, docs, gates

**Files:**
- Modify: `src/tui/mod.rs:614-626` (`Browsing::poll`)
- Modify: `README.md`, `docs/architecture.md:259-266`, `docs/superpowers/specs/2026-09-14-continuo-ratatui-design.md:172`
- Create: `docs/m6-acceptance.md`

**Interfaces:**
- Consumes: `BrowserState::apply -> Option<BrowseRequest>`, `BrowseWorker::request`.

- [ ] **Step 1: Issue the follow-up read**

Replace `Browsing::poll`:

```rust
    /// Hands every finished read to the open browser, and sends the read a
    /// mutation's answer asks for; an answer that finishes after the
    /// browser closed is dropped.
    fn poll(&mut self) {
        let Some(worker) = &self.worker else {
            return;
        };
        while let Some(result) = worker.try_result() {
            if let Some(state) = &mut self.state
                && let Some(follow_up) = state.apply(result)
            {
                worker.request(follow_up);
            }
        }
    }
```

- [ ] **Step 2: Build and run the whole suite**

Run: `cargo test --locked`
Expected: everything passes; note the totals.

- [ ] **Step 3: Documents**

`docs/architecture.md` §8 table, add after the M5 row:

```markdown
| **M6** | Feed management from the terminal player: subscribe, refresh one/all and unsubscribe on the browser's Podcasts tab, over the same library functions the CLI calls — implemented; manual check pending (`docs/m6-acceptance.md`) |
```

and in the same section's paragraph that begins "M0 explicitly defers", append: "M6 adds the three subscription commands to the browser (`docs/superpowers/specs/2026-09-16-continuo-m6-feed-management-design.md`)."

`README.md`: in the milestones paragraph (line 5) change "Milestones 0 through 5 are implemented" to "Milestones 0 through 6 are implemented" and append after the M5 clause: ", and subscribing, refreshing and unsubscribing from the player's Podcasts tab (M6)". In the terminal player section's key table (search for `b` / "Open/close browser"), add a row: `a` / `r` / `R` / `d` in the browser's Podcasts tab — subscribe by URL, refresh the highlighted feed, refresh all, remove with `y` to confirm.

M5 spec `docs/superpowers/specs/2026-09-14-continuo-ratatui-design.md`, end of §8's second paragraph, append:

```markdown
*Superseded for subscribe, refresh and unsubscribe by the M6 design (`2026-09-16-continuo-m6-feed-management-design.md`).*
```

Create `docs/m6-acceptance.md`:

```markdown
# M6 acceptance

`docs/superpowers/specs/2026-09-16-continuo-m6-feed-management-design.md` §8 lists the evidence: the automated suites, then one manual run against Radio-T.

## Automated

| Requirement | Tests |
| --- | --- |
| Prompt, confirm, refresh keys, pending, correlation, reconciliation, notice drawing | `tests/m5_browser.rs` (the tests added for M6) |
| Worker answers with the CLI's wording; failures are values; refresh-all reports every feed; browsing makes no request | `tests/m6_feed_management.rs` |
| Browsing, enqueueing and restoring make no request | `tests/m5_no_network.rs` (unchanged) |
| Gates | `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked` — record the run here |

## Manual, Ghostty, Radio-T

| Step | Result |
| --- | --- |
| `b`, Tab to Podcasts, `a`, paste the Radio-T feed URL, Enter: "Subscribing…" then "radio-t: subscribed, N episodes retained" and the feed appears with its count | not run — requires a human at the terminal |
| Enter on the feed, `r`: "Refreshing…" then "radio-t: updated/unchanged", episodes still listed | not run — requires a human at the terminal |
| `R` on the feed list with two feeds: both lines reported | not run — requires a human at the terminal |
| `d`, `n`: nothing happens; `d`, `y` inside the feed: back on the list, feed gone, "radio-t: unsubscribed" | not run — requires a human at the terminal |
| Resubscribe: the feed returns with a fresh count | not run — requires a human at the terminal |
| Typing `q`, `b`, space in the prompt inserts text and nothing else happens | not run — requires a human at the terminal |
```

- [ ] **Step 4: Gates**

Run:

```bash
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
```

Expected: all three exit 0. Fix anything they raise (a `#[must_use]` or an unused import is the likely kind), then paste the `cargo test` totals into `docs/m6-acceptance.md`'s Gates row.

- [ ] **Step 5: Commit**

```bash
git add src/tui/mod.rs README.md docs/architecture.md docs/superpowers/specs/2026-09-14-continuo-ratatui-design.md docs/m6-acceptance.md
git commit -m "feat(tui): feed management from the Podcasts tab (M6)"
```

Then open the PR against `main` with the branch `feat/m6-feed-management`; the user runs the manual rows in Ghostty and fills them in before merge.
