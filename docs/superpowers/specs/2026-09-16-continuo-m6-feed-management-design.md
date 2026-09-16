# Continuo M6: feed management from the terminal player

Status: approved in conversation on 2026-09-16; ready for an implementation plan. The behavior below is the proposed contract, not an assertion that it already exists.

Branch: `feat/m6-feed-management`.

## 1. Product decision

After two days of daily use the M5 player is fine except for one gap: adding a podcast means leaving the TUI for `continuo subscribe`. M6 closes it. The Podcasts tab of the on-demand browser gains the three subscription commands the CLI already has — subscribe, refresh (one or all), unsubscribe — and nothing else. The M5 design (§8) deliberately excluded "full subscription management screens"; this milestone adds exactly those three actions to the existing tab rather than a new screen.

The CLI commands remain, unchanged. The TUI reuses their library functions and their result wording.

## 2. Existing foundations

- `library::subscribe`, `library::refresh`, `library::refresh_all`, `library::unsubscribe` (`src/library.rs`) already implement the three actions with their commit-order guarantees (M4 §5.3). Subscribe and refresh are `async` and take an `HttpService`; unsubscribe is synchronous and needs no network.
- `commands.rs` drives the async ones with `service.handle().block_on(...)` and formats each outcome into a `dyn Write`. Those formatters (`finish_subscribe`, `write_refresh`, `finish_unsubscribe` and their helpers) are the wording M6 reuses; they become `pub(crate)` where needed and gain no new behavior.
- `application::browse::BrowseWorker` is one background thread answering `BrowseRequest`s in order with `BrowseResult`s; `tui::browser::BrowserState::apply` accepts a result only for the list still being viewed, so late answers are harmless. The worker owns `LibraryStores` and, today, never builds an `HttpService`.
- `BrowserState` already has `loading` and `error` for the visible list and a notice line that draws them (`src/tui/render/browser.rs`).
- Unsubscribing never touches the queue or checkpoints: podcast queue entries carry an enclosure fallback (`queue.rs`), and `unsubscribe` keeps checkpoints by design (M4 §5.3). M6 adds no queue handling.

## 3. Worker: requests, results, network

`BrowseRequest` gains three variants:

| Variant | Library call | Network |
| --- | --- | --- |
| `Subscribe { url: String }` | `subscribe(&http, &subs, &cache, &url, None)` | yes |
| `Refresh { slug: Option<String> }` | `refresh(...)` for `Some`, `refresh_all(...)` for `None` | yes |
| `Unsubscribe { slug: String }` | `unsubscribe(&subs, &cache, &slug)` | no |

`BrowseResult` gains one variant, `Mutation(Result<String, String>)`: `Ok` carries the CLI's success wording for that outcome, `Err` carries the `FeedError` display (or the batch error for refresh-all) after whatever partial success wording the CLI would have printed first. The text is produced by writing the CLI formatter into a `Vec<u8>`; the TUI never composes outcome sentences of its own.

The worker keeps its single thread. It builds an `HttpService` with the default `Limits` lazily, on the first request that needs one, and keeps it for the thread's lifetime. A service that fails to start is reported as that request's `Err` and retried on the next network request. Async calls run through `service.handle().block_on`, the same pattern `commands.rs` uses, so `library.rs` stays free of `block_on`.

The slug is always derived, as `continuo subscribe <url>` without `--as` does. A slug override is out of scope.

Requests stay serial: a directory read issued while a refresh-all is in flight waits behind it. This mirrors the CLI's serial refresh and is marked in code with a `ponytail:` note naming the upgrade path (a second worker for mutations). The module's doc comment changes from "nothing here touches the network" to "only an explicit `Subscribe` or `Refresh` request touches the network"; the M5 no-network invariant — browsing, enqueueing and restoring make no requests — is unchanged and its test (`tests/m5_no_network.rs`) must keep passing.

## 4. Browser: keys and modes

All of this is scoped to `BrowserTab::Podcasts`. The Files tab is untouched.

`BrowserState` gains two fields:

- `prompt: Option<String>` — the feed URL being typed.
- `confirm: Option<String>` — the slug whose removal awaits `y`.

While `prompt` is `Some`, `handle_key` consumes every key for it: printable characters append, Backspace pops, Enter submits `Subscribe` with the trimmed text (an empty submission just closes the prompt), Esc cancels. Player shortcuts and browser shortcuts cannot fire while typing, matching M5 §7's rule for the queue's URL input. Ctrl-C and Ctrl-L are handled before the browser sees the key, as today.

While `confirm` is `Some`, `y` sends `Unsubscribe`; any other key press cancels.

Otherwise, on the Podcasts tab:

| Key | Action |
| --- | --- |
| `a` | Open the URL prompt |
| `r` | Refresh the feed under the cursor on the feed list, or the feed whose episodes are open |
| `R` | Refresh every subscription |
| `d` | Ask to confirm removing the feed under the cursor on the feed list, or the feed whose episodes are open |

`r`, `R` and `d` do nothing when there is no feed to act on (empty list) or while a request is loading; `a` works whenever the tab is visible and nothing is loading. The keys are added to the help overlay and the browser footer.

## 5. Feedback and list refresh

Submitting any of the three sets `loading`, so the existing notice shows the loading text. `apply(Mutation(..))` clears `loading` and stores the text on the notice slot — `error: Option<String>` becomes `notice: Option<(String, NoticeKind)>` with `Ok`/`Err` so success draws in the normal color and failure in amber, as errors do today — then re-requests the visible list (`Feeds`, or `Episodes { slug }` when a feed is open) so counts and episode rows update. The listing answer settles `loading` again but must not erase a notice that arrived with the mutation; the notice clears on the next key that changes what is shown (tab switch, enter/back, a new request) or on close.

The prompt and the confirmation question draw on that same notice line: `Feed URL: <text>` with a cursor, and `Remove <slug>? y/N`.

If the browser is closed before a result arrives, the result is dropped, exactly as a late listing is dropped today. The mutation itself has already committed in the worker; the next open shows the updated list.

## 6. Failure handling

Everything is a value. `FeedError` displays become the notice; a `FollowupFailure` appends its text as the CLI prints it; a refresh-all with failures shows the per-feed lines the CLI prints followed by the `BatchIncomplete` error. There are no retries and no partial states beyond what the library already commits atomically (M4 §5.3). A panic in the worker thread is already reported as the "reader is not running" error value; M6 keeps that.

## 7. Out of scope

A slug override, cancelling an in-flight request, a footer fallback for results that arrive after the browser closed, editing a subscription's URL, and any of the v0.1 exclusions in the foundation spec. Add each when it is actually needed.

## 8. Validation

Automated:

- `tests/m5_browser.rs`, with the existing `press` and `screen` helpers: `a` opens the prompt and the prompt swallows `q`, `b`, space and `d`; Enter with text sends `Subscribe`, Enter empty sends nothing, Esc cancels; `r`/`R`/`d` on an empty list send nothing; `d` then a non-`y` key sends nothing, `d` then `y` sends `Unsubscribe` for the cursor feed; `r` inside an open feed's episodes refreshes that slug; `apply(Mutation(Ok))` clears loading, shows the notice in the normal color, and re-requests the visible list; `apply(Mutation(Err))` shows amber; the follow-up listing answer keeps the notice; the drawn overlay contains the prompt and the confirmation question.
- New `tests/m6_feed_management.rs`, using `support/server.rs`'s `TestServer` scripts and the worker harness from `tests/m5_no_network.rs`: subscribe, then refresh one, then refresh all, then unsubscribe through `BrowseWorker`, asserting the summaries match what `continuo subscribe`/`refresh`/`unsubscribe` print for the same fixture; a URL that fails validation and a server returning 500 each yield `Err` and leave the store as it was; browsing alone (`Feeds`, `Episodes`) keeps the server's request count at zero; an `Unsubscribe` request builds no `HttpService` (the server sees no connection).
- `tests/m5_no_network.rs` unchanged and green.
- Gates: `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked`.

Manual, by the user in Ghostty against Radio-T: add the feed with `a`, see the episode count appear, open it, `r` to refresh, `d`/`y` to remove, resubscribe. Record the result in `docs/m5-acceptance.md`'s style in a new `docs/m6-acceptance.md`.

## 9. Documentation

`README.md`'s terminal player section and `docs/architecture.md` §8's milestone table gain M6. The M5 spec's §8 sentence excluding subscription management is annotated as superseded by this document.
