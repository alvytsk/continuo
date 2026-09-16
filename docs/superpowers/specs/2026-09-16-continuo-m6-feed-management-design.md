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

`BrowseResult` gains one variant, `Mutation { request: BrowseRequest, outcome: Result<String, String> }`. `request` echoes the request it answers, so the browser can tell whose answer it is (§5). `Ok` carries the CLI's success wording for that outcome, `Err` carries the `FeedError` display (or the batch error for refresh-all) after whatever partial success wording the CLI would have printed first; either may span several lines, exactly as the CLI output does. The text is produced by writing the CLI formatter into a `Vec<u8>`; the TUI never composes outcome sentences of its own. `BrowseRequest` derives `PartialEq` for the comparison.

The worker keeps its single thread. It builds an `HttpService` with the default `Limits` lazily, on the first request that needs one, and keeps it for the thread's lifetime. A service that fails to start is reported as that request's `Err` and retried on the next network request. Async calls run through `service.handle().block_on`, the same pattern `commands.rs` uses, so `library.rs` stays free of `block_on`.

The slug is always derived, as `continuo subscribe <url>` without `--as` does. A slug override is out of scope.

Requests stay serial: a directory read issued while a refresh-all is in flight waits behind it. This mirrors the CLI's serial refresh and is marked in code with a `ponytail:` note naming the upgrade path (a second worker for mutations). The module's doc comment changes from "nothing here touches the network" to "only an explicit `Subscribe` or `Refresh` request touches the network"; the M5 no-network invariant — browsing, enqueueing and restoring make no requests — is unchanged and its test (`tests/m5_no_network.rs`) must keep passing.

## 4. Browser: keys and modes

All of this is scoped to `BrowserTab::Podcasts`. The Files tab is untouched.

`BrowserState` gains three fields:

- `prompt: Option<String>` — the feed URL being typed.
- `confirm: Option<String>` — the slug whose removal awaits `y`.
- `pending: Option<BrowseRequest>` — the mutation in flight, if any.

While `prompt` is `Some`, `handle_key` consumes every key for it: printable characters append, Backspace pops, Enter submits `Subscribe` with the trimmed text (an empty submission just closes the prompt), Esc cancels. Player shortcuts and browser shortcuts cannot fire while typing, matching M5 §7's rule for the queue's URL input. Ctrl-C and Ctrl-L are handled before the browser sees the key, as today.

While `confirm` is `Some`, `y` sends `Unsubscribe`; any other key press cancels.

Otherwise, on the Podcasts tab:

| Key | Action |
| --- | --- |
| `a` | Open the URL prompt |
| `r` | Refresh the feed under the cursor on the feed list, or the feed whose episodes are open |
| `R` | Refresh every subscription |
| `d` | Ask to confirm removing the feed under the cursor on the feed list, or the feed whose episodes are open |

`r`, `R` and `d` do nothing when there is no feed to act on (empty list); all four do nothing while a listing is loading or a mutation is `pending`. Navigation keys, tab switching and closing keep working during a mutation. The keys are added to the help overlay and the browser footer.

`back()` from an episode view changes in one way: it still restores the cached feed rows and cursor at once, but also requests `Feeds`, so every return to the list re-reads the store rather than trusting rows cached before a mutation. The rows stay visible while the answer is pending (`loading` is not set); the answer replaces them and clamps the cursor.

## 5. Feedback, correlation and list refresh

Submitting a mutation sets `pending` to the request and shows a working notice (`Subscribing…`, `Refreshing…`, `Removing…`). It does not set `loading`: the current rows remain valid and stay visible.

**Correlation.** `apply(Mutation { request, .. })` acts only when `request == pending`; any other mutation answer is dropped. The worker outlives close and reopen (`Browsing` keeps it), so without this a result from a session the user already closed would land on the next `BrowserState`. A reopened browser starts with `pending = None` and therefore ignores late answers. Switching tabs keeps `pending`; the answer is shown on whichever tab is visible. Ceiling, marked with a `ponytail:` note: an identical mutation resubmitted after a close and reopen adopts the earlier answer and drops its own — harmless, since both committed in the store and the list is re-read either way.

**On a matching answer,** in this order:

1. `pending` clears.
2. If the request was `Unsubscribe { slug }` and an episode view for that slug is open, leave it the way `back()` changes the view (episodes cleared, cursor on the feed list, marks cleared) but without its `Feeds` request, which step 4 issues — whether the outcome is `Ok` or `Err`, because the subscription is removed before cache cleanup (M4 §5.3), so a follow-up failure still means the feed is gone, and an `UnknownSlug` error means it was already gone.
3. The notice becomes the outcome text with its kind (`Ok` or `Err`).
4. The visible list is re-requested when the Podcasts tab is showing: `Feeds` on the feed list, `Episodes { slug }` in an episode view. On the Files tab nothing is requested; switching back to Podcasts already requests `Feeds`.

The listing answer that follows settles `loading` as today but must not erase the notice. The notice clears on the next key that changes what is shown (tab switch, enter, back, a new mutation, a new prompt) and on close.

**Notice area.** `error: Option<String>` becomes `notice: Option<(String, NoticeKind)>` with kinds `Working`, `Ok`, `Err`. It is drawn as its own block at the top of the list area and the rows are drawn beneath it, instead of today's single row that replaces the list. Its height is the smaller of its line count and a third of the list area, at least one row when a notice exists. When the text has more lines than fit, the block takes a third of the list area (two rows at minimum) and its last row reads `+N more lines, see log`; the full text is logged once at info level through `tracing`, so a long refresh-all report can be read with `RUST_LOG`. Listing errors, the loading text and the empty-list texts still use the existing single-row-and-return behavior; only `notice` gets the coexisting block. `Err` draws in amber as errors do today, `Working` and `Ok` in the muted color.

The prompt and the confirmation question draw in the notice block: `Feed URL: <text>` with a cursor, and `Remove <slug>? y/N`.

## 6. Failure handling

Everything the library reports is a value. `FeedError` displays become the notice; a `FollowupFailure` appends its text as the CLI prints it; a refresh-all with failures shows the per-feed lines the CLI prints followed by the `BatchIncomplete` error, which is what the overflow rule in §5 exists for. There are no retries and no partial states beyond what the library already commits atomically (M4 §5.3).

A panic on the worker thread is fatal, as it is today: `BrowseWorker` has no panic containment, and the TUI's global panic hook records a worker panic and initiates shutdown (M5 §11). The worker's "reader is not running" answer covers only requests sent after the thread has died. M6 adds no containment; a recoverable mutation panic would need its own design.

## 7. Out of scope

A slug override, cancelling an in-flight request, a footer fallback for results that arrive after the browser closed, editing a subscription's URL, and any of the v0.1 exclusions in the foundation spec. Add each when it is actually needed.

## 8. Validation

Automated:

- `tests/m5_browser.rs`, with the existing `press` and `screen` helpers: `a` opens the prompt and the prompt swallows `q`, `b`, space and `d`; Enter with text sends `Subscribe` and sets `pending`, Enter empty sends nothing, Esc cancels; `r`/`R`/`d` on an empty list send nothing; `a`/`r`/`R`/`d` while `pending` send nothing; `d` then a non-`y` key sends nothing, `d` then `y` sends `Unsubscribe` for the cursor feed; `r` inside an open feed's episodes refreshes that slug; `back()` from an episode view requests `Feeds` while keeping the cached rows; a matching `Mutation` `Ok` clears `pending`, shows the notice in the muted color, and re-requests the visible list; `Err` shows amber; a `Mutation` whose request differs from `pending`, and one arriving in a fresh `BrowserState`, change nothing; an `Unsubscribe` answer for the open feed leaves the episode view and requests `Feeds`, for `Ok`, for a follow-up cache failure, and for `UnknownSlug`; the follow-up listing answer keeps the notice; a mutation answer on the Files tab shows the notice and requests nothing; the drawn overlay contains the prompt, the confirmation question, and a notice above visible rows; an eight-line notice in a seventeen-row list shows four lines and the `+4 more lines` marker.
- New `tests/m6_feed_management.rs`, using `support/server.rs`'s `TestServer` scripts and the worker harness from `tests/m5_no_network.rs`: subscribe, then refresh one, then refresh all, then unsubscribe through `BrowseWorker`, asserting the summaries match what `continuo subscribe`/`refresh`/`unsubscribe` print for the same fixture; a URL that fails validation and a server returning 500 each yield `Err` and leave the store as it was; a refresh-all whose first feed succeeds and second fails yields `Err` whose text contains the first feed's success line, the second feed's failure and the batch line, and the first feed's cache is updated; browsing alone (`Feeds`, `Episodes`) keeps the server's request count at zero; an `Unsubscribe` request builds no `HttpService` (the server sees no connection); every `Mutation` echoes the request it answers. Close–reopen: with a server script that delays the feed response, submit `Refresh`, drop the `BrowserState`, build a new one, then deliver the answer through `apply` and assert the new state shows no notice and requests nothing.
- `tests/m5_no_network.rs` unchanged and green.
- Gates: `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked`.

Manual, by the user in Ghostty against Radio-T: add the feed with `a`, see the episode count appear, open it, `r` to refresh, `d`/`y` to remove, resubscribe. Record the result in `docs/m5-acceptance.md`'s style in a new `docs/m6-acceptance.md`.

## 9. Documentation

`README.md`'s terminal player section and `docs/architecture.md` §8's milestone table gain M6. The M5 spec's §8 sentence excluding subscription management is annotated as superseded by this document.
