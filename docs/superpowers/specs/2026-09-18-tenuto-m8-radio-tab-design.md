# Tenuto M8: a Radio tab for saved stations

Status: approved in conversation on 2026-09-18; ready for an implementation plan. The behavior below is the proposed contract, not an assertion that it already exists. ICY metadata framing stays out; see M7 §12 and §11 below.

Branch: `feat/m7-live-radio`, continuing from M7 rather than branching — M8's HTTP widening (§5) edits the `Accepted::Live` arm M7 introduced, and R2 is a rule about M7's own requests, so the two share a review.

## 1. Product decision

M7 made a station playable from `tenuto play <url>` and from the queue. It did not make one *keepable*: the URL has to be pasted every time, and nothing in the player knows a station's name until it is already playing.

M8 closes that. The on-demand browser gains a third tab, Radio, over a durable list of stations in a new `stations.json`. Adding a station probes it once; the name, genre and bitrate that come back are shown in the tab from that moment on and are cached, so the list draws on a cold start without a request. A station's logo, when it has one and it can be decoded, renders in the player pane when the station plays.

This is M6's shape applied to a new entity: three actions (add, remove, re-probe) on an existing tab, reusing the browser machinery M6 built rather than a new screen.

## 2. Existing foundations, and what they do not give us

- `http::response::accept` classifies a live response (M7 §4). `HttpMediaSource::open` keeps exactly one ICY field from it — `icy-name`, at `src/http/source.rs:257` — and `station_name()` feeds `prepare()`'s title (M7 L1). The other ICY headers are read and dropped.
- `subscription::store::SubscriptionStore` is the template for a user-authored list: plain DTOs, atomic write through `persistence::atomic`, quarantine to `subscriptions.json.rejected-<stamp>` on a malformed file, and — unlike `state.json` — no entry cap and no eviction (`src/subscription/store.rs` §5.1 note).
- `application::browse::BrowseWorker` is one background thread answering `BrowseRequest`s in order. M6 added `BrowseResult::Mutation { request, outcome }` for correlation, the `pending` single-mutation lock, and the `Working`/`Ok`/`Err` notice line; `BrowserState::apply` drops an answer for a list it has already left.
- `artwork::worker::CoverSource::Remote { url, http }` already fetches a cover over HTTP through `http.fetch_document`'s bounded-document path, for podcast episodes. `artwork::decode` bounds it at 10 MiB encoded and 16 million pixels and runs inside `lifecycle::panic::run_contained`.
- `queue::EnqueueItem::Url` already enqueues a bare URL, and M7 settled what a live entry does once queued: no `EndOfTrack`, no `Advance`, no completion (M7 L6).
- `tui::images` prepares exactly one cover per frame, outside `terminal.draw`, rebuilt only when media, artwork mode or cover area changes.

What they do not give us:

- No ICY field beyond the name survives `open`, so genre, bitrate and logo are unavailable to anything above the HTTP layer.
- No station persistence of any kind. A station is a queue entry or nothing.
- `artwork::decode` is JPEG and PNG only, and `Cargo.toml` pins `image` to exactly those two features. The reference station's logo is an SVG.
- `BrowserTab` has two variants and every `match` over it is exhaustive at two.
- Nothing records that Tenuto must not ask for metadata framing. See R2 — this is the sharpest edge in the milestone.

## 3. Rules

- **R1.** A station is a URL whose response `accept()` classifies as `Accepted::Live`. The rule governs what a *classification* permits, not whether one was obtained, and the two must not be conflated:
    - **Positively classified as not a station** — a finite file, an HLS playlist, an `icy-metaint` response, or any non-retryable failure such as `404`: never enters `stations.json`. These are "not a station", not "a station that failed".
    - **No classification obtained** — a retryable failure such as `429`, `503` or a reset connection: enters `stations.json` as an unverified candidate (`identity: None`), because the server said nothing about what the URL is. An unverified record is a candidate, not a claim; it is never treated as a verified station, and only a later probe that returns `Accepted::Live` promotes it.
    - A station may leave the unverified state only by classifying as `Accepted::Live`. No amount of retrying, and no user action short of a successful probe, promotes a candidate.
- **R2.** Tenuto never sends `Icy-MetaData: 1`, on any request, including the probe. The reference endpoint returns `Icy-Metaint: 8192` the moment a client asks for it, and `response.rs:191` refuses any response carrying that header. Tenuto is safe today only by omission. M8 makes that explicit and tests it. The rule is lifted by M7 §12, which deletes `IcyFramingUnsupported`, and not before.
- **R3.** Drawing the Radio tab makes no request. Every field in a row comes from the store. This extends M5's no-network invariant (`tests/m5_no_network.rs`) to the new tab.
- **R4.** Stored identity is a cache of the last successful probe. For the **title**, it is never authority over a live open: `prepare()` keeps taking the title from the response in hand, so a station that renames itself is right on screen the moment it plays, whatever the store says. For **artwork**, the store is authoritative and refreshes only on add or re-probe — the single exception, with its reasoning in §8.1.
- **R5.** Enter in the Radio tab enqueues, and Enter on a queued row removes it, exactly as in Files and Podcasts. The Radio tab introduces no transport verb.

## 4. Station model and store

`src/station/{mod,model,store}.rs`, mirroring `src/subscription/` in structure, atomicity and quarantine behavior.

```rust
pub struct Station {
    pub slug: String,                      // subscription::model::validate_slug, reused
    pub url: Url,
    pub identity: Option<StationIdentity>, // None: added but not yet reached
    pub added_at: OffsetDateTime,
    pub probed_at: Option<OffsetDateTime>,
}

pub struct StationIdentity {
    pub name: Option<String>,
    pub genre: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub logo: Option<Url>,
}
```

`identity: Option<_>` is the unverified state of §10 in one field, rather than a parallel boolean that can disagree with the data it describes. Every field inside it is itself optional because ICY guarantees none of them: `is_icy` accepts a response carrying `icy-br` and no `icy-name` (`src/http/response.rs:181`), and such a station is legitimately verified-but-unnamed. A verified station with no name displays its URL, the same as an unverified one, and is told apart by the absence of the unreached marker.

**Deviation from the approved sketch:** the conversation's illustration carried a `station_id` of 32 hex characters, mirroring `FeedId`. This spec drops it. `FeedId` exists because a feed owns a cache directory named by its id and needs a handle that survives a slug change; a station owns no files, and editing a station's URL or slug is out of scope (§11). Adding an identifier with no reader in this milestone is the kind of speculative field that later has to be migrated whether or not it was ever used. A station's identity is its URL, and its handle is its slug. If §11's URL editing arrives, it brings an id with it.

Slugs live in their own namespace. A station slug and a feed slug may collide without either store noticing, which is correct because no command takes a slug without also naming which list it means. Slug derivation mirrors `subscription::model::choose_slug` over the station list; the `--as` override is out of scope, as it was in M6.

`StationStore` exposes `load`, `read_snapshot` and `save` with `SubscriptionStore`'s signatures and failure taxonomy, over `<state dir>/stations.json`. A malformed file is quarantined to `stations.json.rejected-<stamp>` and the list starts empty; a station list is user-authored data, so this is a loud, recoverable failure, never a silent truncation.

## 5. Probe: ICY identity from the open path

The probe opens the real source through `HttpMediaSource::open`, reads the headers, takes the identity and retires it. It does not use a bespoke header-only request. A second classification path would be free to disagree with `accept()`, and a probe that says "fine" where playback says `IcyFramingUnsupported` is worse than no probe: M7 spent twenty L-numbers fixing what counts as live, and M8 must inherit that answer rather than re-derive it. One connection opened and immediately closed is the price.

The single widening this needs is at `src/http/source.rs:257`, where the `Accepted::Live` arm keeps only the name:

- `HttpMediaSource` stores `identity: Option<StationIdentity>` in place of `station_name: Option<String>`, populated from `icy-name`, `icy-genre`, `icy-br` and `icy-logo` on that same arm.
- `station_name()` becomes `station_identity() -> Option<&StationIdentity>`; `prepare()`'s title continues to read `.name`, so M7 L1's contract is unchanged in behavior and changes only in the expression that reaches it.
- `icy-br` parses as a decimal `u32`; anything else is `None`, never an error. `icy-logo` parses as an absolute `Url`; a relative or malformed value is `None`. A station is never rejected over a decorative field.
- Header values are display text and the caller escapes them, exactly as `station_name`'s existing doc comment already says.

## 6. Worker: requests, results, network

`BrowseRequest` gains three variants, and `BrowseResult` gains one list variant. Mutations continue to answer through M6's `Mutation { request, outcome }`.

| Variant | Action | Network |
| --- | --- | --- |
| `Stations` | list from the store | no |
| `AddStation { url: String }` | validate, probe, save per §10 | yes |
| `RemoveStation { slug: String }` | drop the record | no |
| `ReprobeStation { slug: String }` | probe, refresh the cached identity | yes |

`BrowseResult::Stations(Result<Vec<StationRow>, String>)` carries what the tab draws. `StationRow` is the presentation projection of a `Station` — slug, display text, and the `MediaId` the row enqueues — built in `library.rs` beside `list_feeds`, so `tui::browser` never reaches into the store.

That `MediaId` must be the one `EnqueueItem::Url(url)` itself resolves to, from the same `url` by the same `resolve_source` call (`application::runtime::new_entry`). The tab enqueues `EnqueueItem::Url`, and `sync_queue` matches a row to a queue entry by `MediaId` alone: if a station row derived its identity any other way, the row would enqueue successfully and then fail to draw its own tick, and Enter would add a second copy instead of removing the first. The identity is derived once, in `library.rs`, and never recomputed in the browser.

The worker owns a `StationStore` alongside its `LibraryStores` and reuses the `HttpService` M6 already builds lazily on the first request that needs one. `AddStation` and `ReprobeStation` are the only new requests that touch the network; `Stations` and `RemoveStation` must not, and R3's test asserts it at the wire.

Requests stay serial, with M6's `ponytail:` note about a second worker for mutations unchanged. A probe is bounded by the existing opening deadline and limits, so a station that accepts a connection and then says nothing cannot wedge the worker past that budget.

`ReprobeStation` on a station that now classifies as not-live leaves the record alone and reports the failure. Removing it silently would destroy user-authored data over one bad answer; R1 governs what may *enter* the list, not what is evicted from it.

## 7. Browser: the Radio tab

`BrowserTab` gains `Radio` and `Tab` cycles all three. The exhaustive `match`es over the tab — `queued_at`, the hints line, the title line, the empty-list text and the row renderer — each gain an arm.

A row draws the slug, then the identity: `Lofi · 128 kbps`, with genre and bitrate omitted when absent. An unverified station draws its URL and an unreached marker instead.

Keys follow M6's Podcasts tab so the two are learnable as one thing: `a` opens the URL prompt, `d` then `y` removes the station under the cursor, `r` re-probes it, and every one of them is refused while `pending` holds. Enter enqueues, Enter on a queued row removes the queue entry, and space marks — R5, with no special case in `queued_at` beyond its new arm. The hints line becomes `tab files/podcasts/radio`.

`queued_at`'s `match (self.tab, &self.episodes)` gains `(Radio, _)`, returning the row's `MediaId` so a queued station draws the same tick every other queued row draws.

## 8. Cover art

### 8.1 How a station's logo reaches the player

`CoverSource::Remote` is unreachable for a station today. `active_cover()` (`src/application/runtime.rs:420`) matches `QueueSource::RemoteUrl(_) => embedded()?`, so a plain remote URL only ever offers the front cover embedded in the decoded stream. A station is a `RemoteUrl`. Naming the podcast path was not enough; this subsection defines the path.

**The logo comes from the store, not from the response in hand.** `LibraryStores` gains `stations: StationStore` beside `subscriptions` and `cache`, and `active_cover()`'s `RemoteUrl` arm becomes a lookup mirroring the `Podcast` arm exactly:

```rust
QueueSource::RemoteUrl(url) => {
    let station_art = || {
        let http = Arc::clone(self.http.as_ref()?);
        let logo = self.library.as_ref()?.stations.logo_for(url)?;
        Some(CoverSource::Remote { url: logo, http })
    };
    station_art().or_else(embedded)?
}
```

Three properties follow, and each is the reason for this shape rather than the alternative:

- **Nothing is fetched for a listed, enqueued or restored station.** `self.http.as_ref()?` is the same gate the podcast arm already uses, and `active_cover`'s own doc comment already states the rule it enforces: the HTTP service exists only once playback has opened it. R3 therefore extends to artwork for free, with no new mechanism to get wrong.
- **A non-station remote URL behaves exactly as it does today.** `logo_for` returns `None` for a URL no station claims, and `.or_else(embedded)` is the current arm. The change is additive; no existing remote-URL behaviour moves.
- **`prepare()` and the HTTP source are untouched.** The alternative — carrying the live response's `icy-logo` out through `prepare()` and the Mirror alongside the title — would put M7's contract surface in play for a decorative field, and would deliver the logo only after load. §5's widening stays confined to the probe, and §14's claim that `prepare` changes only at a rename holds.

**Refresh.** A station's logo changes when its record changes: on add, and on re-probe (`r`). It does not refresh mid-playback. This is a deliberate narrowing of R4, which says stored identity is never authority over a live open: **R4 governs the title, which `prepare()` continues to take from the response in hand. Artwork is the exception, and takes the store's cached value.** A station that changes its logo shows the old one until re-probed. Re-probing is one keystroke, and the alternative costs M7 surface for a picture.

### 8.2 Decoding SVG

The logo renders in the player pane while the station plays. It is not drawn in the browser overlay: `tui::images` prepares one cover per frame, and a second image surface keyed on the browser cursor is a larger change than the milestone earns.

Decoding needs **`resvg` 0.48 with `default-features = false`, and `usvg` must not be added as a second direct dependency.** resvg already pins `usvg` at `default-features = false` (`resvg-0.48.1/Cargo.toml:106`); declaring usvg directly would union its own defaults back on through feature unification. Use `resvg::usvg` for `Options`.

Turning resvg's defaults off drops `text`, `system-fonts`, `memmap-fonts`, `raster-images` and `svgz`. Two consequences, stated rather than hidden:

- **Text inside an SVG does not render** (no `text`, so no `fontdb`). Station logos are overwhelmingly geometry, and a font stack is a poor trade for the exceptions.
- **`.svgz` is unsupported.** `svgz` is a resvg *default* that pulls `flate2`; a gzip inflater fed an attacker-supplied logo is a decompression-bomb surface, and the outer 10 MiB cap bounds the compressed bytes, not the inflated ones. A gzipped logo is rejected as an unsupported format rather than silently inflated.

The existing 10 MiB / 16 Mpx pair does not describe SVG risk, so decoding adds these bounds:

- **No external references, and no nested SVG.** **Both** halves of `usvg::Options::image_href_resolver` are overridden to return `None` — `resolve_string` *and* `resolve_data` — and `resources_dir` is `None`.

    Overriding only `resolve_string` is insufficient, and the reason is worth recording because it is not obvious from the feature list. usvg's `default_data_resolver` (`usvg-0.48.1/src/parser/image.rs:58`) routes `image/svg+xml` into `load_sub_svg`, which parses an entire nested tree — and its `text/plain` arm falls through to `load_sub_svg` for *any* payload whose magic bytes are not JPEG, PNG, GIF or WebP. Disabling resvg's `raster-images` does not close this: that feature governs rendering, in resvg, while `load_sub_svg` is parsing, in usvg. usvg's own doc comment on that resolver is explicit — "The actual images would not be decoded. It's up to the renderer." An earlier revision of this spec claimed `raster-images` covered embedded `data:` images; it does not, and the two crates' feature sets must not be reasoned about as one.

- **Fixed raster size.** Rasterization targets a 512×512 bound derived from the viewBox's aspect ratio, never the SVG's declared dimensions. A `viewBox` claiming 50000×50000 costs exactly what a 48×48 one costs. The existing terminal-side resize then handles the cover area as it does for JPEG and PNG.
- **Entity expansion** is bounded by usvg's XML parser; the 10 MiB encoded cap from `read_limited` stays as the outer bound and applies to the fetched bytes before parsing.

Format detection is by content sniffing, not by the URL's suffix or the response's content type, both of which are attacker-controlled in the general case. Rasterization runs inside the existing `run_contained` guard, so a malformed logo is an `ArtworkError::Panicked` and not a crash. `ArtworkError::Unsupported`'s message changes from "artwork is not JPEG or PNG" to name SVG as well.

## 9. Queue, session and persistence

Nothing changes. A station enqueued from the Radio tab is the same queue entry `tenuto play <url>` already produces, and M7 already fixed its behavior: it is never checkpointed (L16), it never advances (L6), and it is restored and drawn without a request (L20). The queue parks on a station until the user skips, which is M7's designed behavior and not a defect M8 introduces.

`stations.json` is a new file, so no existing on-disk schema is migrated and `state.json`'s `schema_version` is untouched.

## 10. Failure taxonomy: what an add does

`RemoteFailure::is_retryable` already separates the two cases and is tested (M7 L4: `404` false, `429` and `503` true).

| Probe outcome | Stored | Shown |
| --- | --- | --- |
| `Accepted::Live` | yes, with identity | name · genre · bitrate |
| Failure, `is_retryable` true | yes, `identity: None` | the URL, plus an unreached marker; `r` re-probes |
| Failure, `is_retryable` false | **no** | error notice carrying the reason |
| Not live — finite, HLS, `IcyFramingUnsupported` | **no** | error notice: this URL is not a live stream |
| URL fails validation | **no** | error notice, no request made |

The split is the difference between "this station is wrong" and "this station is down right now". A typo and a maintenance window look identical to a store that accepts everything, and a station that keeps its URL through an outage is the common case a store that accepts nothing cannot serve.

A duplicate URL is not an error: the add resolves to the existing station, re-probes it and says so.

## 11. Out of scope

ICY metadata framing and now-playing titles (M7 §12 owns them, and R2 holds until it lands). A station directory, search or import. Editing a station's URL or slug, and the stable id that would require. A `--as` slug override. New CLI verbs: `BrowseRequest::Subscribe`'s doc comment pairs each browse mutation with a `tenuto` subcommand, and M8 deliberately breaks that pairing rather than doubling the surface for an entity with three actions — `library.rs` still exposes them as ordinary functions, so adding `tenuto station add` later is wiring, not design. Cover art in the browser overlay. Cancelling an in-flight probe. Per-station volume, favourites or ordering.

## 12. Validation

Automated:

- `tests/http_server_selftest.rs`: the existing ICY station fixture gains `Icy-Genre` and `Icy-Logo`, and keeps asserting it sends no `icy-metaint`.
- **R2, new and non-negotiable:** a test asserting that no request Tenuto issues carries an `Icy-MetaData` header — over an initial open, a reconnect, a Pause → Play rejoin and a station probe — read from the requests `TestServer` actually received. This is the one regression the milestone is most likely to introduce and the one nothing currently catches.
- `tests/m8_station_store.rs`: round-trip; a station with no identity; a malformed file quarantined to `stations.json.rejected-<stamp>` with the list starting empty; slug derivation and collision within the station namespace; a station slug colliding with a feed slug leaving both stores intact.
- `tests/m8_station_probe.rs`, against `TestServer` scripts: the five rows of §10, each asserted on both the store's contents and the notice text; identity parsed from all four ICY headers; `icy-br` that is not a decimal and `icy-logo` that is not an absolute URL each degrading to `None` without failing the add; a re-probe of a now-finite URL leaving the record intact and reporting the failure; a duplicate URL resolving to the existing station.
- A station row's `MediaId` equals the one `EnqueueItem::Url` produces for the same URL, asserted by enqueueing a station from the CLI path and from the Radio tab and finding one queue entry and a drawn tick, not two entries.
- `tests/m5_browser.rs`: `Tab` cycles three tabs; `a`/`d`/`r` on the Radio tab send the right requests and are refused while `pending`; Enter enqueues and re-Enter removes; a queued station draws a tick; an unverified row draws its URL and the unreached marker; a `Stations` answer arriving after the tab changed is dropped.
- `tests/m5_no_network.rs`: R3 — opening the Radio tab and listing stations from a populated store keeps the server's request count at zero, and `RemoveStation` builds no `HttpService`.
- **§8.1, the artwork path, as an integration test** — one `TestServer` serving both a station and its logo, counting requests per path: listing the Radio tab, enqueueing a station and restoring one across a process restart each leave the logo's request count at **zero**; playing it requests the logo **exactly once**; a remote URL that no station claims still resolves to its embedded cover and requests no logo; a station whose record carries no logo requests nothing and falls back to the embedded cover.
- `tests/m8_artwork_svg.rs`: an SVG logo decodes to the expected raster; a viewBox of 50000×50000 rasterizes within the fixed bound; a truncated and a malformed SVG each yield a typed `ArtworkError`; detection is by content, so an SVG served as `image/png` and a PNG served as `image/svg+xml` both decode correctly; a `.svgz` (gzipped) logo is rejected as unsupported rather than inflated.
- **Both resolver halves, separately** (§8.2): an external `href` and an external `resources_dir` reference each resolve to nothing without a filesystem read or a second request (`resolve_string`); and, because this is the half a feature flag does not cover, a nested `<image href="data:image/svg+xml,…">` **and** a `data:text/plain` payload whose magic bytes are not JPEG/PNG/GIF/WebP each resolve to nothing rather than parsing a sub-tree (`resolve_data`). The `text/plain` case is the one usvg's fallback arm routes into `load_sub_svg`, so it must be asserted in its own right and not inferred from the `image/svg+xml` case passing.
- `tests/m5_tui_render.rs`: the Radio tab's drawn overlay — tabs, rows, hints, empty-list text, prompt and confirmation.
- Gates: `cargo fmt --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps`, `cargo test --locked`.

Manual, by the user in Ghostty, against `https://radio.cliamp.stream/lofi/stream` — whose headers are confirmed to carry `Icy-Name: Lofi`, `Icy-Genre: Lofi`, `Icy-Br: 128` and `Icy-Logo: .../logo.svg`, and to send `Icy-Metaint` only when a client asks for it: add it with `a` and see `Lofi · 128 kbps` appear; Enter to enqueue and play, and see the logo in the cover pane; quit and relaunch and confirm the tab lists it with no connection opened; `r` to re-probe; `d`/`y` to remove. Record the result in a new `docs/m8-acceptance.md`, in `docs/m7-acceptance.md`'s style.

## 13. Documentation

`README.md`'s terminal player section and `docs/architecture.md` §8's milestone table gain M8. `docs/reference.md` gains `stations.json` beside `subscriptions.json` and the Radio tab's keys. M7 §12's ICY follow-up gains a note that R2 is now tested, so lifting the rule means deleting that test deliberately rather than discovering it.

## 14. Modules

New: `station/{mod,model,store}`, `artwork/svg`.

Expected to change: `http/source` (§5), `application/{browse,runtime,view}` — `runtime` for `LibraryStores::stations` and `active_cover`'s `RemoteUrl` arm (§8.1) — `library`, `tui/{browser,render/browser}`, `artwork/{decode,worker}`, `Cargo.toml`, docs.

Expected to stay as they are, without promise: `http/{response,service,error,channel,document}` — §5 reads ICY headers that `accept` already parsed and adds no classification — `playback/*`, `session`, `queue`, `persistence/*`, `feed/*`, `subscription/*`, `media/*`, `lifecycle`, `app.rs`, `cli.rs`, `commands.rs`. `prepare` changes only where `station_name()` is renamed at its call site — §8.1 takes the logo from the store precisely so that stays true.
