# Continuo M4 — feeds, subscriptions, episode listing and progress

Approved design for Milestone 4. It builds on the [foundation spec](2026-09-07-continuo-foundation-design.md),
[local playback](2026-09-08-continuo-local-playback-design.md), [durable state](2026-09-08-continuo-durable-state-design.md),
[finite HTTP](2026-09-09-continuo-finite-http-design.md) and [estimated seeking](2026-09-10-continuo-estimated-seek-design.md).

M4 turns the podcast identity types M0 shipped and never used into a working feed subsystem: fetch and parse
RSS 2.0 and Atom 1.0, keep durable subscriptions, list episodes with the progress M2 already records, and play a
listed episode through the unchanged M1–M3 playback path.

## 1. Scope

### 1.1 Commands

Five new commands, plus a second form for the existing `play`.

| Command | Behavior |
|---|---|
| `continuo subscribe <url> [--as <slug>]` | Fetch once, parse, assign an immutable `FeedId`, derive or accept a slug, write the cache and then the subscription |
| `continuo unsubscribe <slug>` | Remove the subscription, then delete its cache. Checkpoints are **not** deleted |
| `continuo feeds` | List subscriptions: slug, retained episode count, when last refreshed, title |
| `continuo refresh [<slug>]` | Conditional GET one feed, or every feed when no slug is given. One outcome per feed |
| `continuo episodes <slug> [-n N]` | Read the cache only, never the network. Numbered listing with progress |
| `continuo play <slug> <index> [--probe-only]` | Resolve to a `MediaId::PodcastEpisode` plus its enclosure, then run the existing playback path |

`play` distinguishes its two forms by arity: one positional is a path or an HTTP(S) URL — the M1–M3 behavior,
unchanged — and two positionals are a slug and an index. A second positional that is not a positive integer is
an error naming both accepted forms.

### 1.2 Ordering and numbering

The canonical order is **feed document order**, preserved exactly as parsed. Items are *not* re-sorted by
publication date: §4.6 establishes that those dates are frequently absent or wrong, and sorting by them would
make indices depend on data this design has already declined to trust.

Numbering is contiguous over the **retained** list — after unusable items are dropped (§4.5) and duplicate
episode keys deduplicated (§4.7). Index 1 is the first retained item, with **no chronological guarantee**.
Feeds are conventionally newest-first, so it is usually the newest episode, but nothing here promises that.
`-n N` truncates the displayed list without changing any index (§6.3).

### 1.3 Targeted changes to existing code

This is the complete list of existing code M4 touches. Each entry is required by the work; nothing else is
modified.

**Behavior-changing:**

1. **`src/persistence/atomic.rs`** — the atomic-replace routine inside `StateStore::write` is extracted so the
   subscription and cache stores call it rather than copy it. Semantics are preserved exactly, including
   parent-directory `fsync` as best-effort *after* the rename, non-fatal and logged at debug
   (`src/persistence/store.rs:224-231`).
2. **`StateStore::read_snapshot`** — a non-mutating read path (§5.5). `StateStore::load` can quarantine a
   malformed file, so calling it from a listing would rewrite `state.json` while merely displaying progress.
   `load` keeps its current behavior, wrapping the same extracted decode step.
3. **`app::run` dispatch** — `run` currently destructures a single-variant `CliCommand` irrefutably
   (`src/app.rs:47`) and resolves the source internally. §6.5 describes the minimal dispatch and handoff
   wiring, including its return type changing to `Result<(), AppError>`.

**Additive only** — new items beside existing ones, with no existing behavior altered:

4. **`src/cli.rs`** — five new `CliCommand` variants and `play`'s second positional (§6.3).
5. **`src/media/id.rs`** — `MediaId::feed` and `MediaId::episode_key` (§2.2).
6. **`src/media/mod.rs`** — three descriptive fields on the existing `Episode` (§2.2).
7. **`src/http/service.rs`** — `HttpService::handle` and `HttpService::fetch_document`, with their request and
   outcome types in a new `src/http/document.rs` (§3.1).
8. **`src/http/limits.rs`** — `Limits::document_bytes`, default 8 MiB (§3.4).
9. **`src/http/error.rs`** — `RemoteFailure::DocumentTooLarge` and `RemoteFailure::UnsolicitedNotModified`
   (§3.3, §3.4).
10. **`src/error.rs`** — `AppError` (§6.5).

`src/app.rs` is **not** split; it is already 2,195 lines and splitting it is unrelated work.
`EpisodeKey::resolve`, `resolve_source`, `accept_redirect` and the whole playback path are read and reused,
never modified.

### 1.4 New dependencies

Two, both direct:

- `quick-xml = { version = "0.42", default-features = false, features = ["encoding"] }` — §4.1.
- `getrandom` — §2.3. Declared directly; a transitive edge through rustls is not a dependency this crate
  may call.

`feed-rs`, `rss` and `atom_syndication` were all considered and rejected (§9).

### 1.5 Non-goals

No queue or autoplay-next — M4 adds no field to `PersistedState`. No background or automatic refresh, no
episode download or offline audio, no OPML import or export, no search or filtering beyond `-n`, no
mark-played, no rename command, no unreferenced-cache sweep, no locking against concurrent processes, and
no TUI.

### 1.6 Two consequences accepted rather than solved

**The same audio reached two ways has two identities.** `continuo play <url>` yields `MediaId::RemoteUrl`;
`continuo play radio-t 3` yields `MediaId::PodcastEpisode`. They are separate checkpoints. This is the
foundation spec's deliberate design — identity follows how the media was reached — and M4 documents it in the
README rather than unifying the two.

**Unsubscribe then resubscribe does not reconnect progress.** Unsubscribe keeps checkpoints, but resubscribing
mints a fresh `FeedId`, so every `podcast:<old-feed>/<key>` entry is orphaned and the feed reads as never
played. Orphans remain in `state.json`, count toward `MAX_ENTRIES` (512), and are evicted lowest-`touch_seq`
first. `unsubscribe --forget` and a reattach flag are deferred, not shipped.

## 2. Domain model and identity

### 2.1 Parse-layer values

`src/feed/model.rs` holds what the XML said, and nothing about subscriptions:

```rust
pub struct ParsedFeed {
    pub title: Option<String>,
    pub site_link: Option<Url>,
    pub items: Vec<ParsedItem>,
}

pub struct ParsedItem {
    pub guid: Option<String>,
    pub link: Option<Url>,
    pub enclosure: Option<Enclosure>,
    pub title: Option<String>,
    pub published: Option<OffsetDateTime>,
    pub declared_duration: Option<Duration>,
}

pub struct Enclosure {
    pub url: Url,
    pub length: Option<u64>,
    pub mime_type: Option<String>,
}
```

`ParsedItem` is deliberately not an `Episode`: binding an item to identity needs a `FeedId`, which only the
subscription layer holds. That keeps parsing a pure bytes-to-values function testable against fixtures alone.

`Enclosure` models the XML element faithfully. Only `url` reaches `Episode`. `length` and `mime_type` are
cached and used for exactly two things: a debug log at play time, and a warning when `mime_type` is present
and is not `audio/*`. The item still plays — a declared type is a claim, not evidence, which is the line this
project already takes on transport not determining media semantics.

### 2.2 Episode

`src/media/mod.rs`'s existing `Episode` is extended rather than replaced:

```rust
pub struct Episode {
    pub id: MediaId,                        // always MediaId::PodcastEpisode
    pub source: Option<SourceLocation>,     // None when no usable enclosure
    pub title: Option<String>,
    pub published: Option<OffsetDateTime>,
    pub declared_duration: Option<Duration>,
}
```

It carries **no** `key` field. To reach the feed and episode key without storing them twice, `MediaId` gains
two accessors in `src/media/id.rs`:

```rust
impl MediaId {
    pub fn feed(&self) -> Option<&FeedId>;
    pub fn episode_key(&self) -> Option<&EpisodeKey>;
}
```

The two-fields-must-agree invariant disappears instead of being enforced.

`declared_duration`, from `itunes:duration`, is advisory. Checkpoints store no duration, so it is the only
denominator a listing can offer — and it is always displayed as the feed's claim (§6.2), never fed to seek,
resume or completion logic.

### 2.3 Feed identity

`FeedId` is a **random opaque 128-bit identifier**, lowercase hex, 32 characters. It is not a UUID: no version
or variant bits, no dashes, and nothing in the code or the docs calls it one. `getrandom` is declared directly
in `Cargo.toml`; a transitive edge through rustls is not a dependency this crate may call.

It is assigned once at `subscribe` and never changes. The reason it is random rather than derived from the
fetch URL is **not** redirect survival — a URL-derived id could have retained its original value across
redirects. The reason is that subscription identity must be independent of URLs altogether: one feed is often
reachable at several URLs, and a URL is a mutable locator, not a name.

### 2.4 Episode identity

`EpisodeKey::resolve(guid, enclosure, link)` (`src/media/id.rs:103`) is used exactly as M0 wrote it — GUID
first, then enclosure URL, then item link — and is **not modified by this milestone**.

**GUID preservation.** A GUID preserves the *decoded XML character value*: standard references (`&amp;`,
`&lt;`, `&gt;`, `&quot;`, `&apos;`, and numeric `&#…;`) are decoded, everything else is kept byte-for-byte
including leading and trailing whitespace, never trimmed, and never URL-normalized — **including when
`isPermaLink="true"`**. `isPermaLink` is a hint about what the string means, not a licence to normalize it.

**Pre-filtering is the caller's job.** `resolve` computes `enclosure.or(link)` and only then validates, so an
enclosure with an unsupported scheme poisons resolution instead of falling through to the link. M4 therefore
passes `Some(enclosure)` only when the enclosure URL parsed and is `http` or `https`, and `None` otherwise
(§4.5). The milestone does not quietly redefine an M0 identity function.

**No synthesis.** An item with no GUID, no usable enclosure and no link has no identity and is skipped with a
warning. Nothing is invented.

**Identity without playability is kept.** An item with identity but no usable enclosure is retained with
`source: None`, appears in listings with its progress intact, and fails only if played (§6.4).

### 2.5 Slugs

A slug is a presentation alias, never identity. It lives in `subscriptions.json` beside the `FeedId`.

Derivation from the feed title: lowercase, non-alphanumerics collapsed to a single `-`, trimmed of leading and
trailing `-`, truncated to 32 characters. An empty result falls back to the same transform over the fetch
URL's host. A derived slug that collides takes the first free `-2`, `-3`, … suffix, with the base truncated so
the **total stays within 32 ASCII characters**.

An explicit `--as` is validated against `^[a-z0-9-]{1,32}$` and is **rejected** on collision rather than
silently suffixed: explicit intent deserves an explicit error.

M4 ships no rename command. The slug lives in `subscriptions.json` and editing it there is safe by
construction, because it is not identity.

## 3. Feed fetching

### 3.1 Interface

```rust
impl HttpService {
    pub fn handle(&self) -> tokio::runtime::Handle;
    pub async fn fetch_document(&self, request: DocumentRequest)
        -> Result<DocumentOutcome, RemoteFailure>;
}

pub struct DocumentRequest {
    pub origin: Url,
    pub validators: Option<CacheValidators>,
}

pub struct CacheValidators {
    pub url: Url,                    // the URL whose response supplied these
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub enum DocumentOutcome {
    Unchanged { final_url: Url, permanent_url: Option<Url>, validators: CacheValidators },
    Fetched   { bytes: Vec<u8>, final_url: Url, permanent_url: Option<Url>,
                validators: CacheValidators, content_type: Option<String> },
}
```

`fetch_document` contains no `block_on`. The CLI bridge calls
`service.handle().block_on(service.fetch_document(req))` (§6.6); M5 awaits it on the same runtime and never
blocks its event loop.

### 3.2 Redirects

The loop reuses `accept_redirect(from, location, hops)` from `src/http/response.rs` unchanged, so M4 inherits
M3's hop cap (`Limits::max_redirects`, 5), loop detection, scheme check and downgrade refusal rather than
forking that policy.

`permanent_url` advances only through the **initial uninterrupted run** of 301/308 hops and freezes at the
first non-permanent hop:

| Chain | `permanent_url` | `final_url` | Stored `fetch_url` becomes |
|---|---|---|---|
| A —301→ B —302→ C | `Some(B)` | C | B |
| A —302→ B —301→ C | `None` | C | unchanged (A) |
| A —301→ B —308→ C | `Some(C)` | C | C |

Relative URLs always resolve against `final_url` (§4.4). The stored `fetch_url` is rewritten **only after the
fetch, the parse and the cache write have all succeeded**: a permanent redirect to a body that would not parse
must not move the subscription.

### 3.3 Conditional requests

Conditional headers are sent **only** when the request URL equals `validators.url`. Crossing a redirect drops
them, because a validator identifies a representation of one resource, and forwarding it can validate
something unrelated.

A 304 preserves the cached body and **merges** any validators the 304 itself carries, then advances
`last_refreshed_at`. A 304 arriving when no conditional header was sent, or when the cache is missing or
corrupt, is `RemoteFailure::UnsolicitedNotModified` — not a success. A missing, corrupt or
parser-incompatible cache forces an **unconditional** fetch (§5.4).

### 3.4 Deadlines and bounds

`Limits::open` bounds the **entire** operation — every redirect hop, headers, and the whole body — measured
once from the first connect. Inside it, `Limits::connect` bounds each hop's connect, `Limits::headers` each
hop's header wait, and `Limits::stall` the gap between body chunks. Exceeding `open` is
`RemoteFailure::Timeout { phase: Phase::Open }`.

A new `Limits::document_bytes` (default 8 MiB) bounds the body, enforced **while streaming** and not trusted
to `Content-Length`: chunks accumulate, and the moment the running total would exceed the cap the request is
dropped with `RemoteFailure::DocumentTooLarge { limit }`. A declared `Content-Length` already above the cap
short-circuits before the body is read, but only as a cheap early exit — the streaming check is the authority,
because that header can be absent or false.

### 3.5 Headers and status

Requests send `Accept-Encoding: identity` explicitly rather than relying on reqwest's feature set to imply it,
and `Accept: application/rss+xml, application/atom+xml, application/xml;q=0.9, text/xml;q=0.9, */*;q=0.1`.

Status handling is small and explicit rather than routed through `accept()`, which is built around ranges:
200 → `Fetched`, 304 → `Unchanged`, anything else → `RemoteFailure::Status`. A non-identity `Content-Encoding`
is refused with the existing `RemoteFailure::NonIdentityEncoding`.

Every URL reaching an error passes through `redact_url` (§7.2).

## 4. Parsing

### 4.1 Dependency and decoding

`quick-xml = { version = "0.42", default-features = false, features = ["encoding"] }`. The crate's
`default = []`, so this is exactly the surface taken. `escape-html` is deliberately **not** enabled (§4.8).

Decoding is an explicit layer, never `NsReader` over raw bytes:

```text
bytes -> quick_xml::encoding::DecodingReader -> NsReader<DecodingReader<&[u8]>>
```

`DecodingReader::new` detects the encoding from a BOM or the XML declaration's byte pattern. The first event
is then read, and if it is `Event::Decl` the declared label is resolved with `e.encoder()`:

- `Some(enc)` → `reader.get_mut().set_encoding(enc)`, which must happen **before the prefix buffer drains**
  (the reader asserts this).
- `None` → `FeedError::UnsupportedEncoding { label }`, refused by name rather than guessed at.

`DecodingReader` transcodes with `decode_to_utf8_without_replacement`, so `DecoderResult::Malformed` surfaces
as `io::ErrorKind::InvalidData` and becomes `FeedError::Encoding`. Strict rejection of malformed bytes is the
reader's own default; nothing is replaced with U+FFFD, because a silently mangled title is worse than a
refusal that names its cause.

### 4.2 Format dispatch

Dispatch is on the document element's **expanded** name:

- `{http://www.w3.org/2005/Atom}feed` → the Atom 1.0 mapping.
- unqualified `rss` containing `<channel>` → the RSS 2.0 mapping.
- anything else, RSS 1.0 / RDF included → `FeedError::UnsupportedFormat`.

RSS 1.0 is refused by name rather than half-parsed. It is rare for podcasts, and pretending to support it is
worse than saying no.

### 4.3 Element mappings

| | RSS 2.0 | Atom 1.0 |
|---|---|---|
| items | `channel/item` | `feed/entry` |
| identity | `guid` (+ `isPermaLink`), else enclosure URL, else `link` | `id`, else `link[rel=enclosure]@href`, else `link[rel=alternate]@href` |
| audio | `enclosure@url`, `@length`, `@type` | `link[rel=enclosure]@href`, `@length`, `@type` |
| title | `title` | `title`, per `@type` (§4.8) |
| date | `pubDate`, RFC 2822 | `published`, else `updated`, RFC 3339 |
| duration | `{http://www.itunes.com/dtds/podcast-1.0.dtd}duration` | same |
| feed title | `channel/title` | `feed/title` |
| site link | `channel/link` | `feed/link[rel=alternate]@href` |

An Atom `link` with **no `rel` attribute is `alternate`** (RFC 4287 §4.2.7.2), applied before any selection.

`itunes:duration` accepts `HH:MM:SS`, `MM:SS`, and bare seconds. Anything else yields `None`.

### 4.4 Relative URLs

Resolution follows RFC 3986 §5.1: against the innermost `xml:base` in scope — tracked on a small stack as the
reader descends — falling back to the **final URL after redirects**, which is the retrieval URI. Absolute URLs
pass through untouched. A relative URL that fails to resolve makes that link or enclosure absent rather than
failing the item.

### 4.5 Unusable enclosures

An enclosure URL that will not parse, or whose scheme is not `http` or `https`, is **discarded**. The item
survives if a GUID or link still yields identity, and is retained with `source: None`. Only `http(s)` is
accepted as a playback source.

### 4.6 Malformed input has two tiers

**Feed-level** faults fail the whole document: XML that will not parse, a decoding failure, an unknown or
unsupported document element, a missing `<channel>` or `<feed>`.

**Item-level** faults skip the item and keep the feed: no resolvable identity at all, or an unknown entity in
an identity field (§4.8).

**Neither** is a date that will not parse: it yields `published: None` plus a warning, and the item stays. A
bad timestamp is not a reason to hide an episode.

Skipped items are counted, logged at warn with the reason, and reported: `3 items skipped
(RUST_LOG=continuo=warn for detail)`.

### 4.7 Multiple enclosures and duplicate keys

**Multiple enclosures on one item:** the first in document order whose resolved URL is `http(s)` wins; the
rest are ignored with a warning naming the count.

**Duplicate resolved episode keys within one feed:** the first occurrence in document order wins; later ones
are skipped, counted and logged. Deterministic, and stable across refetches for as long as feed order is.

### 4.8 Entities and Atom titles

**Unknown-entity passthrough is confined to display fields.** Display fields (titles) decode via
`unescape_with` using a resolver that returns an unknown entity as literal text, so a title may show a literal
`&nbsp;`. Identity fields — `guid`, Atom `id`, every href — use plain `unescape`, and an unknown entity there
fails the item: an identity that cannot be decoded consistently is not an identity.

quick-xml's `escape-html` feature is deliberately not enabled. It would make identity depend on an HTML entity
table, and identity may depend only on standard XML references.

**Atom `title@type`:** absent means `text` (RFC 4287 §3.1.1).

- `text` → character content.
- `xhtml` → the concatenated descendant text nodes of the wrapper `<div>`, markup dropped. Well-defined,
  because that XML is already being parsed.
- `html` → entity-decoded and kept literally, markup included, because handling it properly needs an HTML
  parser this project is not adding.

Both the literal `&nbsp;` and the literal `html` markup are recorded as known limitations. Titles are
display-only.

## 5. Storage

### 5.1 subscriptions.json

At `ProjectDirs::data_dir()` — `$XDG_DATA_HOME/continuo/subscriptions.json` on Linux.

```json
{
  "schema_version": 1,
  "subscriptions": [
    {
      "feed_id": "9f3c1a7e42b58d0c6f19ab3e5d72c840",
      "slug": "radio-t",
      "title": "Радио-Т",
      "fetch_url": "https://feeds.example/radio-t",
      "added_at": "2026-09-11T09:14:22Z"
    }
  ]
}
```

`title` is duplicated here and in the cache deliberately: `feeds` must still name a subscription whose
cache is missing or corrupt, and the cache is by definition disposable.

It holds **only durable subscription data** and is written **only when one of its own fields changes** —
subscribe, unsubscribe, a committed permanent redirect, or a changed feed title. An ordinary refresh, 200 or
304, touches exactly one file: the cache.

Load policy is `StateStore::load`'s, deliberately: unreadable → keep the file and disable writing for the
session; an unknown `schema_version` → preserve and disable writing; malformed → move aside as
`subscriptions.json.rejected-<timestamp>`. Two differences, both because this is user-authored data rather
than derived history: **no entry cap and no eviction**. Silently dropping a subscription to respect a limit
would be data loss.

Writing is synchronous on the calling thread, not through M2's `WriterHandle`: that thread exists to keep
filesystem work off the playback path, and no playback path is running during `subscribe` or `refresh`.

### 5.2 The cache

At `ProjectDirs::cache_dir()/feeds/<feed_id>.json` — one file per feed, keyed by `FeedId`, never by slug, so a
rename cannot orphan it.

It stores the **parsed** result rather than raw XML: `episodes` must not re-parse on every listing, the parse
is deterministic so nothing is lost, and a malformed feed then fails once at refresh instead of on every
listing. The cost is that a later parser fix would not reach already-cached data, so the file carries a
`parser_version` (§5.4).

**Validators and the per-check timestamps live here**, replaced in the same atomic write as the episodes, so a
validator can never describe a snapshot other than the one on display.

```json
{
  "schema_version": 1,
  "parser_version": 1,
  "feed_id": "9f3c1a7e42b58d0c6f19ab3e5d72c840",
  "fetched_from": "https://cdn.example/radio-t.xml",
  "last_refreshed_at": "2026-09-11T09:14:22Z",
  "last_fetched_at": "2026-09-11T09:14:22Z",
  "validators": { "url": "https://cdn.example/radio-t.xml",
                  "etag": "\"a1b2c3\"", "last_modified": "Sun, 06 Sep 2026 18:00:00 GMT" },
  "title": "Радио-Т",
  "site_link": "https://example.org/",
  "skipped_items": 3,
  "episodes": [
    { "media_id": "podcast:9f3c1a7e42b58d0c6f19ab3e5d72c840/guid:https:%2F%2Fexample.org%2Fp%2F987%2F",
      "enclosure_url": "https://cdn.example/rt_podcast987.mp3",
      "enclosure_length": 74187213,
      "enclosure_mime": "audio/mpeg",
      "title": "Радио-Т 987",
      "published": "2026-09-06T18:00:00Z",
      "declared_duration_secs": 6120 }
  ]
}
```

`fetched_from` is the `final_url`, and it is the base for relative URLs on the next parse. `media_id` is stored
in canonical string form — `MediaId` already round-trips through `String` via serde — and the §2.2 accessors
recover the feed and key, so neither is stored twice. The escaping is `MediaId`'s own and is not URL encoding:
`:` stays literal while `/` becomes `%2F` (`tests/media_id.rs:73`), and a `guid%3A…` spelling is rejected as
noncanonical (`tests/media_id.rs:86`).

`last_refreshed_at` advances on any successful check, 304 included. `last_fetched_at` advances on a 200 that
was successfully parsed and cached.

**There is no `last_changed_at`.** A parsed 200 does not prove the body changed, and this design keeps neither
body nor fingerprint. Real change detection — a SHA-256 over the decoded document, compared before replacement
— is separately specifiable and not shipped.

### 5.3 Commit order and partial failure

| Command | Order | A failure between steps leaves | Reported as |
|---|---|---|---|
| `subscribe` | cache, then subscription | an unreferenced cache file | `the feed was fetched but the subscription could not be saved` |
| `refresh` | cache, then subscription (only if its fields changed) | a newer cache against the old fetch URL | `refreshed, but the redirect could not be recorded` — self-heals on the next successful refresh |
| `unsubscribe` | subscription, then cache delete | an inert cache file | `unsubscribed; the cached copy could not be removed` |

Neither sequence needs a transaction across the data and cache directories. Unreferenced cache files are
ignored by every read path. Every one of these partial failures exits nonzero (§6.4).

### 5.4 Cache recovery preserves explicit refresh

`episodes` and `feeds` **never touch the network and never delete a file.** Missing, corrupt or
parser-incompatible cache data is reported: nonzero exit for corrupt or incompatible, while a missing cache is
the normal "never refreshed" state for `feeds` and exits zero.

`refresh` fetches **unconditionally** when the cache is missing, corrupt, or `parser_version`-mismatched —
otherwise a 304 against a stale-parser cache would leave it unusable indefinitely. The cache is replaced only
after the fetch **and** the parse both succeed.

### 5.5 Reading checkpoints without writing them

`StateStore::load` can quarantine a malformed file, so a listing that called it would rewrite `state.json`
while merely displaying progress. M4 extracts the pure decode step and adds:

```rust
impl StateStore {
    pub fn read_snapshot(&self) -> Result<StateSnapshot, PersistenceError>;
}
```

`StateSnapshot` is a read-only wrapper over the decoded `PersistedState`, exposing only the lookups a
listing needs — `entry_for` and `completed_for` — and no mutator at all, so a read path cannot record a
checkpoint even by mistake.

`load` keeps its current behavior by wrapping the same decode with its quarantine and writability policy.
`read_snapshot` never quarantines and never writes. A missing file yields an empty snapshot — no checkpoints.
Unreadable, malformed or unsupported state is an `Err` surfaced as a visible error, so a listing can never
label every episode unplayed because it silently failed to read state.

### 5.6 Semantic validation on load

Deserialization alone is not enough.

**subscriptions.json:** every `feed_id` matches `^[0-9a-f]{32}$` and is unique; every `slug` matches
`^[a-z0-9-]{1,32}$` and is unique; `fetch_url` parses as `http(s)`. Any violation makes the **whole file**
malformed and takes the quarantine path, because there is no way to tell which record was corrupted. An id is
validated into a `FeedId` newtype **before** any path is built from it, so a traversal-shaped id cannot reach
the filesystem.

**Cache file:** its `feed_id` equals the feed it was loaded for; every `media_id` parses as
`MediaId::PodcastEpisode` whose `feed()` equals that id; no duplicate episode keys. Violation → corrupt,
handled per §5.4.

**`LoadOutcome.reason` is inspected, not ignored.** `StateStore::load` returns default state with a `reason`
(`src/persistence/store.rs:52`), so a caller that reads only `state` cannot tell an empty file from an
unreadable one. For subscriptions, anything but `Loaded` or `Missing` is a visible command error with a
nonzero exit; `Missing` alone means "no subscriptions yet", and an empty listing is the honest answer.

### 5.7 Concurrency

Atomic replacement prevents a torn file, not a lost update. Two concurrent `continuo refresh` processes can
overwrite each other's subscription changes. The project is single-user and single-process by design; M4 adds
no locking and states this limitation in the README.

## 6. CLI surface and the progress join

### 6.1 Output

```text
$ continuo feeds
SLUG        EPISODES  REFRESHED (UTC)   TITLE
radio-t          412  2026-09-11 09:14  Радио-Т
sysdesign          —  never             System Design

$ continuo episodes radio-t -n 5
  #  PROGRESS            AUDIO  PUBLISHED (UTC)  TITLE
  1  —                   -      2026-09-06       Радио-Т 987
  2  23:14 / (1:42:00)   -      2026-08-30       Радио-Т 986
  3  played              -      2026-08-23       Радио-Т 985
  4  ~18:02 / (1:39:00)  -      2026-08-16       Радио-Т 984
  5  23:14               none   2026-08-09       Bonus: outtakes
```

All displayed timestamps are **UTC** and labeled as such in the header. No local-time conversion is attempted:
`time` without `local-offset` cannot reliably determine the offset in a multithreaded process, and mislabeled
local time is worse than honest UTC.

### 6.2 The progress cell

A read-only join against `state.json` through `read_snapshot` (§5.5). Precedence matches
`restart_preference` (`src/resume.rs:145`), which treats an estimate as the listener's most recent intent and
keeps the established position only as a fallback:

| Checkpoint | Cell |
|---|---|
| `completed: true` | `played` |
| `estimated` present | `~MM:SS` |
| else `position` present | `MM:SS` |
| entry with neither | `position unknown` |
| no entry | `—` |

"Entry with neither" is distinct from "no entry" and must not be collapsed into it.

Two marks, each meaning one thing. **Parentheses** mark a duration the *feed* declared — never
decoder-confirmed, since checkpoints store no duration. A **tilde** marks an estimated position, because M3's
provenance rule forbids presenting an estimate as a confirmed position, and a listing is exactly where that
would otherwise happen silently.

**Playability is a separate column.** `AUDIO` shows `-` for a playable item and `none` for one with no usable
enclosure, so a removed enclosure never hides existing progress. A column of `-` for every row in a healthy
feed is accepted redundancy; overloading the progress cell would make the marks ambiguous.

### 6.3 Argument contract

- `-n N` requires **N ≥ 1**; `-n 0` is rejected with `expected a positive count`. It truncates the displayed
  list without changing any index (§1.2).
- `--probe-only` is supported with the episode form and applies **after** resolution: the episode is resolved,
  then the existing `run_probe_only` runs over its enclosure URL. An item with no audio fails with
  `NotPlayable` before any probe.
- A missing `subscriptions.json` during episode resolution reports `UnknownSlug { slug }`. A missing file means
  zero subscriptions, so a slug that cannot exist is unknown, not a storage fault.

### 6.4 Exit statuses

Any failed step exits nonzero, and the message says exactly what did and did not happen.

- `refresh` with no slug continues through **all** feeds, returns one outcome per feed, prints each, and exits
  nonzero if any refresh or any required persistence step failed.
- Every §5.3 partial failure exits nonzero, cleanup failures included.
- `episodes` exits nonzero on corrupt or parser-incompatible cache data; `feeds` exits zero for a merely
  missing cache.
- `play` on an item whose `source` is `None` fails with `FeedError::NotPlayable { slug, index, title }` —
  `episode 5 "Bonus: outtakes" has no audio enclosure; nothing to play` — exiting nonzero. No `EngineHandle`, no
  `AudioOutput` and no `HttpService` are constructed on that path.

### 6.5 Dispatch and handoff

`app::run` currently destructures a single-variant `CliCommand` irrefutably (`src/app.rs:47`) and resolves the
source internally, so it cannot take a resolved pair as it stands. The minimal wiring:

1. `run` matches on `cli.command` instead of destructuring it, dispatching the five new commands to
   `commands::*` and both `play` forms onward.
2. Both `play` forms resolve **before** entering the shared playback body: one positional through the existing
   `resolve_source`, two positionals through `library::resolve_episode`. Each yields the same
   `(MediaId, SourceLocation)` pair.
3. The shared body — persistence open, engine assembly, resume, session, key loop — takes that pair and is
   otherwise unchanged. `resolve_source` itself is not modified; it gains a sibling.
4. `run` returns `Result<(), AppError>`, where `AppError` is a new enum in `src/error.rs` with
   `#[error(transparent)]` arms for `PlaybackError` and `FeedError`. `main.rs` needs no shape change: both
   `{error}` and `?error` still apply.

M1–M3 playback code is not otherwise touched.

### 6.6 Application functions

`src/library.rs` is the seam M5 reuses: it returns values, prints nothing, and contains no `block_on`.
Network-using functions are `async` and take `&HttpService`; read-only functions do not take one at all. No
function receives an `Option<&HttpService>`.

```rust
pub async fn subscribe(http: &HttpService, subs: &SubscriptionStore, cache: &CacheStore,
                       url: &str, slug: Option<&str>) -> Result<SubscribeOutcome, FeedError>;
pub async fn refresh(http: &HttpService, subs: &SubscriptionStore, cache: &CacheStore,
                     slug: &str) -> Result<RefreshOutcome, FeedError>;
pub async fn refresh_all(http: &HttpService, subs: &SubscriptionStore,
                         cache: &CacheStore) -> Vec<RefreshOutcome>;

pub fn unsubscribe(subs: &SubscriptionStore, cache: &CacheStore, slug: &str)
    -> Result<UnsubscribeOutcome, FeedError>;
pub fn list_feeds(subs: &SubscriptionStore, cache: &CacheStore)
    -> Result<Vec<FeedSummary>, FeedError>;
pub fn list_episodes(subs: &SubscriptionStore, cache: &CacheStore, state: &StateSnapshot,
                     slug: &str, limit: Option<NonZeroUsize>)
    -> Result<Vec<EpisodeRow>, FeedError>;
pub fn resolve_episode(subs: &SubscriptionStore, cache: &CacheStore, slug: &str, index: usize)
    -> Result<(MediaId, SourceLocation), FeedError>;
```

Outcomes are structured values; every user-facing message is formatted by `src/commands.rs` from these fields,
never assembled as a string inside the library:

```rust
pub struct SubscribeOutcome { pub slug: String, pub feed_id: FeedId, pub title: Option<String>,
                              pub retained: usize, pub skipped: usize, pub subscription_saved: bool }

pub enum RefreshOutcome {
    Unchanged { slug: String },
    Updated   { slug: String, retained: usize, skipped: usize,
                url_moved: Option<String>, subscription_saved: bool },
    Failed    { slug: String, error: FeedError },
}

pub struct UnsubscribeOutcome { pub slug: String, pub cache_removed: bool }
```

The two listing row types:

```rust
pub struct FeedSummary { pub slug: String, pub title: Option<String>,
                         pub episodes: Option<usize>,          // None when no usable cache
                         pub last_refreshed_at: Option<OffsetDateTime> }

pub struct EpisodeRow { pub index: usize, pub media: MediaId, pub title: Option<String>,
                        pub published: Option<OffsetDateTime>,
                        pub declared_duration: Option<Duration>,
                        pub playable: bool, pub progress: Progress }

pub enum Progress { None, Played, Estimated(Duration), Established(Duration), Unknown }
```

`Progress` is computed in the library, not the formatter, so §6.2's precedence is tested once as a value
rather than re-derived from rendered text.

`src/commands.rs` owns the synchronous CLI bridge — the one `handle().block_on(...)` — and all formatting and
exit-code computation.

### 6.7 AlreadySubscribed

`subscribe` compares the **requested** URL, before fetching, against each stored `fetch_url`. Both sides are
compared through `NormalizedUrl::parse`: scheme, host and default port normalized, fragment removed, query
serialization preserved. A match is `AlreadySubscribed { slug }`, exits nonzero, and performs no fetch.

**Redirect targets do not participate.** Two distinct feed URLs may legitimately redirect to one aggregator
endpoint, and treating those as the same subscription would be wrong. A duplicate that becomes apparent only
after a redirect is allowed to exist as a second subscription, with the documented consequence: two `FeedId`s
and two independent sets of checkpoints.

Titles and site links never establish equivalence.

## 7. Errors, modules and diagnostics

### 7.1 Modules

```text
src/feed/
  mod.rs
  model.rs      ParsedFeed, ParsedItem, Enclosure
  parse.rs      DecodingReader + NsReader; RSS and Atom mappings
  episode.rs    ParsedItem + FeedId -> media::Episode: identity, filtering, dedup
  cache.rs      cache file model, load, save, validation
  error.rs      FeedError
src/subscription/
  mod.rs
  model.rs      Subscription, slug derivation and validation
  store.rs      load policy, semantic validation, save
src/library.rs                application functions (§6.6)
src/commands.rs               presentation for the five new commands; the one block_on
src/persistence/atomic.rs     the extracted atomic replace
src/http/document.rs          DocumentRequest, DocumentOutcome, fetch_document
```

### 7.2 FeedError and redaction

`FeedError` carries typed context: `Encoding`, `UnsupportedEncoding { label }`, `UnsupportedFormat`,
`Malformed { detail }`, `NotPlayable { slug, index, title }`, `UnknownSlug { slug }`,
`IndexOutOfRange { slug, index, retained }`, `CacheMissing { slug }`, `CacheCorrupt { slug, detail }`,
`CacheParserMismatch { slug, found, expected }`, `SubscriptionsUnreadable { reason }`, `InvalidSlug { slug }`,
`SlugTaken { slug }`, `AlreadySubscribed { slug }`, plus `#[from]` arms for `RemoteFailure` and
`PersistenceError`.

**Redaction must hold under `Debug`, not only `Display`.** `main.rs:38-39` prints `{error}` *and* logs
`?error`, and derived `Debug` prints field values, so a redaction that applied only to a `Display`
implementation would leak through the log line. Every URL-bearing field in `FeedError` therefore holds an
**already-redacted `String`**, never a live `Url` — the same discipline `RemoteFailure` follows, and it is
asserted by test, not by convention (§8.5).

## 8. Testing

No test touches the network. `tests/support/server.rs`'s `Script`/`TestServer` gains conditional-GET and
redirect-chain scripting.

### 8.1 Parsing fixtures

Under `tests/fixtures/feeds/`: `rss2-minimal`, `atom-minimal`, `latin1-declared`, `utf16le-bom`,
`utf16be-bom`, `utf16le-declared-no-bom`, `bom-utf8`, `utf8-invalid-bytes`, `encoding-unknown-label`,
`xml-base-relative`, `guid-absent-uses-enclosure`, `item-without-enclosure`, `enclosure-scheme-unsupported`,
`enclosure-malformed-with-guid`, `multiple-enclosures`, `duplicate-identity`, `cdata-title`,
`atom-title-types`, `atom-link-no-rel`, `unknown-entity-in-title`, `unknown-entity-in-guid`,
`itunes-duration-forms`, `item-without-identity`, `truncated-xml`, `rss1-rdf`.

### 8.2 Protocol

Both mixed redirect orders from §3.2; a validator offered for the wrong URL across a redirect; a 304 with no
cache; an unsolicited 304; a body exceeding `document_bytes` with no `Content-Length`; a declared
`Content-Length` above the cap; the `open` deadline spanning multiple hops.

### 8.3 Storage

Every `LoadReason` for the subscription store; each §5.6 semantic-validation rejection; atomic replacement;
cache validation including a foreign `feed_id` and a duplicate key; parser-mismatch recovery through an
unconditional refetch; failure injection **between** the cache and subscription commits for all three commands
in §5.3.

### 8.4 Application boundaries

- A checkpoint with **both** `position` and `estimated` renders the estimate (§6.2).
- `completed: true` **with** an estimate renders `played`.
- An entry with neither position renders `position unknown`, distinct from no entry.
- Progress on an **unplayable** item is still displayed, with `AUDIO` reading `none`.
- Unreadable `state.json` produces a visible error from `episodes`, not a listing of unplayed rows.
- `episodes` and `feeds` **change no files** — asserted by comparing directory contents and mtimes before and
  after.
- Batch `refresh` with mixed outcomes returns one outcome per feed and exits nonzero.
- `-n 0` is rejected; `-n N` truncates without renumbering.

### 8.5 Playback integration and diagnostics

- Podcast playback retains `MediaId::PodcastEpisode` all the way through checkpoint writing — asserted against
  the written `state.json`, not only at resolution.
- One-positional `play` behavior is unchanged, asserted by the existing M1–M3 CLI tests continuing to pass
  untouched.
- Every `FeedError` variant is audited through **both** `Display` and `Debug` for unredacted URLs, credentials
  and query strings.

## 9. Alternatives considered and rejected

**`feed-rs` for all formats.** One parser covering RSS, Atom and JSON Feed with a normalized model. Rejected
on dependency weight: it pulls chrono, regex, uuid, mediatype, siphasher and serde_json, adding a second date
library beside the `time` crate already in the tree. Its default synthesis of missing entry ids is **not** part
of the rationale — `feed-rs` exposes a configurable `id_generator`, so that behavior is an integration concern,
not an unavoidable override. The cost accepted in exchange is explicit: this project now maintains its own
feed interpretation logic.

**`rss` + `atom_syndication`.** `rss` with default features off is only quick-xml, but `atom_syndication`
forces chrono and `diligent-date-parser`, and the pairing yields two differently shaped models to map into one
domain type while still handing back dates as strings.

**quick-xml's `escape-html` feature.** It would decode HTML named entities such as `&nbsp;` everywhere,
including in a GUID, making identity depend on an HTML entity table. Identity may depend only on standard XML
references (§4.8).

**`FeedId` derived from the fetch URL.** It would let resubscribing reconnect existing checkpoints (§1.6).
Rejected because subscription identity must be independent of URLs: one feed is often reachable at several
URLs, and a URL is a mutable locator, not a name. Redirect survival is *not* the argument — a derived id could
have retained its original value across redirects.

**Re-sorting episodes by publication date.** Rejected because §4.6 already establishes those dates as
frequently absent or wrong; sorting by them would make indices depend on data this design declines to trust.

**`last_changed_at`.** Rejected as an unsupportable claim: a successfully parsed 200 does not prove the body
changed, and neither the body nor a fingerprint is retained for comparison (§5.2).

**Validators in `subscriptions.json`.** Rejected because a crash between the two file replacements could leave
a validator describing a snapshot other than the one displayed. They live with the episodes they describe and
are replaced in the same atomic write (§5.2).

**Splitting `src/app.rs`.** It is 2,195 lines and would benefit, but the split is unrelated to this milestone.
New presentation goes to `src/commands.rs` instead, and `app.rs` gains only a dispatch arm (§6.5).
