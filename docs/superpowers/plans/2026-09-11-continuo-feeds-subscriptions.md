# Continuo M4 Feeds and Subscriptions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Subscribe to RSS 2.0 and Atom podcasts, explicitly refresh a disposable episode cache, list checkpoint-derived progress, and play a selected episode through the existing playback engine.

**Architecture:** Reuse the existing HTTP client and redirect policy through a bounded asynchronous document-fetch operation. Parse feeds into subscription-independent values, bind them to immutable subscription IDs, and atomically cache episodes with their validators and timestamps. A value-returning application layer serves both the CLI and a future TUI; subscription data and playback checkpoints keep their separate durable stores.

**Tech Stack:** Rust edition 2024, existing Tokio/reqwest, serde/serde_json, time, url, clap, directories, tracing; direct quick-xml 0.42 with `encoding` and direct getrandom 0.4. Existing tempfile, socket2 and virtual audio output provide tests.

**Spec:** [Continuo M4 design](../specs/2026-09-11-continuo-feeds-design.md), all nine sections, at commit `0a0793b`. The user's `:318` locator identifies a line in this document, not a request to implement only encoding. Read the complete spec and this plan.

## Global Constraints

- "The canonical order is **feed document order**, preserved exactly as parsed."
- "Index 1 is the first retained item, with **no chronological guarantee**."
- "Numbering is contiguous over the **retained** list"; `-n N` requires **N ≥ 1**.
- "`FeedId` is a **random opaque 128-bit identifier**, lowercase hex, 32 characters."
- Explicit slugs match `^[a-z0-9-]{1,32}$`; derivation is ASCII-only, with no transliteration.
- `quick-xml = { version = "0.42", default-features = false, features = ["encoding"] }`.
- "Two, both direct": quick-xml and getrandom. Do not add chrono, encoding_rs, uuid, an HTML parser, or a mocking dependency. getrandom 0.4 is already in Cargo.lock as 0.4.3.
- Keep Cargo.toml's `rust-version = "1.98.1"`, `edition = "2024"`, and `unsafe_code = "forbid"`.
- "`EpisodeKey::resolve`, `resolve_source`, `accept_redirect` and the whole playback path are read and reused, never modified." The spec explicitly permits moving the resolved-pair handoff within app.rs; preserve its downstream behavior.
- `Limits::document_bytes` defaults to **8 MiB**; `Limits::open` bounds the entire document operation. Existing default connect/header/stall/open deadlines are 10/15/15/30 seconds; redirect cap is 5.
- "Requests send `Accept-Encoding: identity` explicitly"; accept only terminal HTTP 200 or solicited 304.
- "Validators and the per-check timestamps live here", in the same atomic cache file as episodes.
- Subscribe/refresh commit cache before subscription; unsubscribe commits subscription before cache deletion. Every partial failure exits nonzero.
- "`episodes` and `feeds` **never touch the network and never delete a file.**" Their subscription/checkpoint reads never quarantine or create directories either.
- Durable subscriptions: `$XDG_DATA_HOME/continuo/subscriptions.json`; cache: `$XDG_CACHE_HOME/continuo/feeds/<feed_id>.json`. Preserve StateStore's existing platform checkpoint path.
- Subscription/cache schema version and initial parser version are **1**. PersistedState remains at its existing schema **2**, with no new fields.
- "All displayed timestamps are **UTC** and labeled as such in the header."
- Progress precedence: completed, then estimated, then established, then unknown-position entry, then no entry. Feed duration is advisory and parenthesized; estimated position alone uses `~`.
- "No queue or autoplay-next"; no automatic refresh, downloads, OPML, mark commands, rename, sweep, locking, or TUI.
- Existing state-write semantics stay intact: private files, atomic rename, file fsync, best-effort parent fsync after rename. New subscription/cache directories are private too.
- "Redaction must hold under `Debug`, not only `Display`." Never log live fetch URLs, validators, raw XML, or raw deserialization errors containing identifiers.
- Interpret the spec's "No test touches the network" as **no external network**: its required TestServer tests use loopback sockets. No real podcast endpoints, audio device, or TTY is required.

---

## Execution context and implementation decisions

The inspected branch is `feat/m4-feeds-subscriptions`; the worktree was clean before writing this plan. This deliverable is planning only. At execution time inspect git status again, preserve unrelated changes, and use the selected execution skill's isolation rules. Do not make a second speculative design or replace the approved parser approach.

The feed fetch, cache and subscription pieces are one connected milestone, not independent products: playback selection requires all three. Tasks below produce independently testable boundaries within one plan. Do not spawn agents merely because a task is independent; the user chooses the execution mode at handoff.

Concrete details needed to turn the spec into code:

1. The existing `FeedId::new` accepts any nonempty identifier, including M0 test values such as `subscription-1`. Do not tighten that constructor or invalidate old checkpoints. Validate the 32-hex subscription constraint in `subscription::model::validate_feed_id` and at CacheStore's path boundary before constructing a path.
2. Inspect `BytesDecl::encoding()` to distinguish missing/malformed/present attributes. After `Some(Ok(_))`, use `BytesDecl::encoder()` to resolve the label without importing transitive `encoding_rs`. This implements §4.1 without a third direct dependency.
3. quick-xml 0.42 emits `Event::GeneralRef`. Process it explicitly; do not assume references remain inside `Text`. CDATA is already literal and must not be unescaped a second time.
4. For nested `xml:base`, resolve the new base against the already-resolved parent base. The HTTP response's final URL is the root base. Cached `fetched_from` is historical metadata only.
5. On each redirect hop rebuild the request. Conditional headers are attached if and only if the current request URL equals the cached validator URL. Keep the original validator record available so a later hop arriving at that URL can validate it; do not permanently erase it on the first redirect.
6. On 304, preserve episodes, `fetched_from`, `last_fetched_at`, and skipped count; merge present validator headers and update `last_refreshed_at`. Reconcile durable title and permanent URL from the resulting cache/outcome even on 304, allowing a previously failed subscription write to recover.
7. Only the application layer knows whether a cache is usable. It sends `validators: None` for missing/corrupt/incompatible cache; the HTTP layer rejects 304 whenever it actually sent no nonempty conditional header.
8. A successfully quarantined subscription file still ends that command with the visible error required by §5.6; the next invocation can see Missing. Do not continue subscription creation in the same failed load.
9. No valid cache and `feeds` means an error for corruption/version mismatch, but a missing cache yields a row with `episodes: None`. `episodes` and episode resolution return CacheMissing for a missing cache.
10. The async application functions contain synchronous local filesystem operations, as approved in §5.1. Document this for M5: run those calls off its UI event loop when integrating; async alone does not make filesystem work nonblocking. M4 adds no worker pool or persistence writer for feeds.
11. `time::Duration` and `std::time::Duration` differ: episode/checkpoint durations use **std::time::Duration**; publication/check timestamps use `time::OffsetDateTime`.
12. Keep storage error causes, but sanitize text that can contain an untrusted URL. At new deserialization boundaries replace a verbose serde error with a category plus line/column, and do not attach the original unsafe error as a source.

## File and responsibility map

| Files | Responsibility |
|---|---|
| `src/persistence/atomic.rs`, existing `store.rs`, `mod.rs` | Shared atomic byte replacement; checkpoint decode shared by recovering load and non-mutating snapshot |
| `src/subscription/model.rs`, `store.rs`, `mod.rs` | Subscription values, hex IDs, ASCII aliases, durable store and read/recovery policy |
| `src/feed/model.rs`, `parse.rs`, `error.rs`, `mod.rs` | XML-derived values, pure parse report, safe typed errors |
| `src/feed/episode.rs` | Identity binding, first-occurrence deduplication, playable source selection |
| `src/feed/cache.rs` | Versioned cache DTOs, semantic validation, atomic save, non-mutating reads |
| `src/http/document.rs`, existing `service.rs`, `limits.rs`, `error.rs`, `mod.rs` | Bounded async GET and scoped validators; original media transport remains unchanged |
| `src/library.rs` | Application operations, per-feed outcomes, progress join, selection |
| `src/commands.rs`, existing `cli.rs`, `app.rs`, `error.rs`, `lib.rs` | CLI parsing, platform store construction, formatting, synchronous bridge, common playback handoff |
| `tests/support/server.rs` | Additive feed-response scripting using the existing loopback server |
| `tests/support/feeds.rs` | New feed fixture helpers and disposable store setup; import with `#[path = "support/feeds.rs"] mod feeds;` |
| `tests/fixtures/feeds/*.xml`, `tests/fixtures/feeds/README.md` | Text fixtures and reproducible encoded variants |
| `tests/m4_*.rs` | New regression suites, leaving existing M1–M3 tests unchanged |
| `Cargo.toml`, `Cargo.lock` | The two approved direct dependencies; no blanket dependency upgrades |
| `README.md`, `docs/architecture.md`, `tests/fixtures/README.md` | User command contract, new module/storage boundaries, fixture inventory |

New `mod.rs` declarations and `src/lib.rs` exports belong to the task introducing each module. Do not create empty modules or stub methods for later tasks. Use the concrete functions below when their producing task is reached.

## Task sequence and gates

Tasks 1–4 establish storage primitives and subscription behavior; 5–6 the HTTP document boundary; 7–8 the parser; 9–10 identity binding and cache; 11 the read model; 12–13 mutation orchestration; 14 the CLI; and 15 end-to-end acceptance and documentation. Execute in this order. The task-level Interfaces blocks are binding names; private helper names may vary only where no later task consumes them.

All tests use `Result<(), Box<dyn std::error::Error>>` with `?`, or a narrowly scoped test-only lint exemption. Do not add production unwrap/expect calls. For each behavior group: add its test, run the focused test and observe the intended failure, implement that group, rerun it, then continue. A task's listed groups are not permission to write all production code before testing.

### Task 1: Extract atomic byte replacement without changing checkpoints

**Files:** Create `src/persistence/atomic.rs`, `tests/m4_atomic.rs`; modify `src/persistence/store.rs`, `src/persistence/mod.rs`.

**Interfaces:** Consumes existing `PersistenceError`. Produces `pub fn replace_bytes(path: &Path, bytes: &[u8]) -> Result<(), PersistenceError>` in `persistence::atomic`. StateStore's public API stays unchanged.

- [ ] **Step 1: Add a failing regression for two destination names.**

```rust
use continuo::persistence::atomic::replace_bytes;

#[test]
fn independent_destinations_replace_whole_snapshots() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let state = dir.path().join("state.json");
    let subs = dir.path().join("subscriptions.json");
    replace_bytes(&state, br#"{"old":"a much longer value"}"#)?;
    replace_bytes(&subs, br#"{"subscriptions":[]}"#)?;
    replace_bytes(&state, b"{}")?;
    assert_eq!(std::fs::read(&state)?, b"{}");
    assert_eq!(std::fs::read(&subs)?, br#"{"subscriptions":[]}"#);
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 2);
    Ok(())
}
```

- [ ] **Step 2: Run `cargo test --locked --test m4_atomic`.** Expect failure because `persistence::atomic` is not exported yet, not an environment/build-dependency failure.
- [ ] **Step 3: Extract the existing implementation, preserving each error boundary.** Move directory preparation, `private_file`, and `set_private` into atomic.rs. `replace_bytes` owns temporary creation, writing, fsync, rename and cleanup. Use the destination filename, PID and a process-wide AtomicU64 for temporary names so multiple store instances cannot collide. Copy the existing platform permission branches and best-effort parent-sync policy; do not make parent-sync failure fatal after rename.

```rust
static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
// Within replace_bytes, after preparing the parent directory:
let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
let mut name = path.file_name().unwrap_or_default().to_os_string();
name.push(format!(".tmp-{}-{seq}", std::process::id()));
let temp = path.parent().unwrap_or_else(|| std::path::Path::new(".")).join(name);
```

`StateStore::write` keeps its schema assertion and serialization error, then delegates:

```rust
let bytes = serde_json::to_vec_pretty(state).map_err(|source| PersistenceError::Serialize {
    path: self.path.clone(), source,
})?;
super::atomic::replace_bytes(&self.path, &bytes)
```

Remove StateStore's now-unused `temp_seq` field and constructor initialization; the sequence belongs to the shared atomic helper. Keep quarantine timestamping and its existing filename collision policy in StateStore.

- [ ] **Step 4: Add failure and privacy cases, then implement only any missing extraction behavior.** Use a nonempty directory as the destination to force rename failure; verify the directory's sentinel remains and no temp is left. Under `#[cfg(unix)]`, inspect permissions and assert file `mode & 0o777 == 0o600` and new directory `== 0o700`. Reuse the exact assertions already in `tests/persistence_store.rs` rather than changing their expectations.
- [ ] **Step 5: Run `cargo test --locked --test m4_atomic --test persistence_store --test persistence_writer`.** All pass; existing state temporary names still begin `state.json.tmp-`.
- [ ] **Step 6: Commit this deliverable.**

```bash
git add src/persistence/atomic.rs src/persistence/mod.rs src/persistence/store.rs tests/m4_atomic.rs
git commit -m "refactor: share atomic snapshot replacement"
```

### Task 2: Read checkpoint snapshots without recovery side effects

**Files:** Modify `src/persistence/store.rs`; create `tests/m4_state_snapshot.rs`.

**Interfaces:** Consumes Task 1 and existing `PersistedState`, `PersistedCheckpoint`, `LoadReason`. Produces `StateSnapshot` in `persistence::store`, with `entry_for(&self, &MediaId) -> Option<&PersistedCheckpoint>`, `completed_for(&self, &MediaId) -> bool`; `StateStore::read_snapshot(&self) -> Result<StateSnapshot, PersistenceError>`.

- [ ] **Step 1: Add the missing/malformed read test.**

```rust
use std::sync::Arc;
use continuo::clock::FakeClock;
use continuo::persistence::store::StateStore;

#[test]
fn snapshot_read_does_not_quarantine() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("state.json");
    let store = StateStore::new(path.clone(), Arc::new(FakeClock::new()));
    store.read_snapshot()?;
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
    std::fs::write(&path, b"broken")?;
    assert!(store.read_snapshot().is_err());
    assert_eq!(std::fs::read(&path)?, b"broken");
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
    Ok(())
}
```

- [ ] **Step 2: Run `cargo test --locked --test m4_state_snapshot`; observe the missing method failure.**
- [ ] **Step 3: Extract pure version-first decoding.** Add private `decode_state(path: &Path, bytes: &[u8]) -> Result<PersistedState, PersistenceError>` in store.rs. Read VersionEnvelope first, reject unsupported versions before deserializing the full model, and preserve the existing in-memory v1-to-v2 migration. `load` delegates to this helper but retains each existing quarantine/writability outcome. The new method distinguishes NotFound from every other I/O failure and never calls directory creation or quarantine.

```rust
pub struct StateSnapshot(PersistedState);

impl StateSnapshot {
    pub fn entry_for(&self, media: &MediaId) -> Option<&super::model::PersistedCheckpoint> {
        self.0.entry_for(media)
    }
    pub fn completed_for(&self, media: &MediaId) -> bool {
        self.0.completed_for(media)
    }
}
```

At the snapshot error boundary, sanitize serde errors using their line/column rather than their potentially URL-bearing message. Preserve source categories and file paths. Do not change the successful snapshot's values or drop estimated positions.

- [ ] **Step 4: Add one test each for v1 migration, v2 estimated-only, unsupported version and unreadable path.** Write explicit JSON in each test. A directory at `state.json` is a deterministic unreadable-file test, including when tests run as root. For v1 assert established position survived in memory and bytes are unchanged on disk; for v2 assert `position: null` and `estimated` both survive. Compare bytes, directory names, and modified timestamps before/after.
- [ ] **Step 5: Run `cargo test --locked --test m4_state_snapshot --test persistence_store --test persistence_model --test estimated_resume`.** Expect all existing migration/recovery tests unchanged.
- [ ] **Step 6: Commit.**

```bash
git add src/persistence/store.rs tests/m4_state_snapshot.rs
git commit -m "feat: expose read-only checkpoint snapshots"
```

### Task 3: Define safe feed errors, subscription IDs and ASCII aliases

**Files:** Create `src/feed/mod.rs`, `src/feed/error.rs`, `src/subscription/mod.rs`, `src/subscription/model.rs`, `tests/m4_subscription_model.rs`; modify `src/lib.rs`, `Cargo.toml`, `Cargo.lock`.

**Interfaces:** Produces `FeedError`, `Subscription`, `validate_feed_id(&str) -> Result<FeedId, FeedError>`, `new_feed_id() -> Result<FeedId, FeedError>`, `validate_slug(&str) -> Result<(), FeedError>`, `choose_slug(title: Option<&str>, url: &Url, explicit: Option<&str>, occupied: &BTreeSet<String>) -> Result<String, FeedError>`. `Subscription` fields are `feed_id: FeedId`, `slug: String`, `title: Option<String>`, `fetch_url: Url`, `added_at: OffsetDateTime`; storage DTOs perform serialization in Task 4.

- [ ] **Step 1: Write model tests with exact expected aliases.**

```rust
use std::collections::BTreeSet;
use continuo::subscription::model::{choose_slug, validate_feed_id, validate_slug};

#[test]
fn cyrillic_title_uses_host_and_suffix_stays_bounded() -> Result<(), Box<dyn std::error::Error>> {
    let url = url::Url::parse("https://www.radio-t.com/rss/")?;
    assert_eq!(choose_slug(Some("Радио-Т"), &url, None, &BTreeSet::new())?, "radio-t-com");
    let full = "a".repeat(32);
    let used = BTreeSet::from([full.clone()]);
    let suffixed = choose_slug(Some(&full), &url, None, &used)?;
    assert_eq!(suffixed, format!("{}-2", "a".repeat(30)));
    validate_slug(&suffixed)?;
    assert!(validate_feed_id("../../state").is_err());
    Ok(())
}
```

Add explicit collision, leading/trailing punctuation, lowercase conversion, all-non-ASCII title, missing title, valid 32-hex ID, uppercase/31/33-character invalid ID cases.

- [ ] **Step 2: Run `cargo test --locked --test m4_subscription_model`; observe missing module failure.**
- [ ] **Step 3: Add only getrandom and define error types.** Add `getrandom = "0.4"` to Cargo.toml and regenerate the lock through the focused test command without `--locked` once. Keep existing versions; inspect `git diff Cargo.lock`. Define FeedError using thiserror with the exact variants below. All contextual strings supplied by constructors must already be safe for Debug.

```rust
#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    #[error("feed encoding is invalid")] Encoding,
    #[error("unsupported feed encoding: {label}")] UnsupportedEncoding { label: String },
    #[error("unsupported feed format")] UnsupportedFormat,
    #[error("malformed feed: {detail}")] Malformed { detail: String },
    #[error("episode {index} {title:?} has no audio enclosure; nothing to play")]
    NotPlayable { slug: String, index: usize, title: String },
    #[error("unknown feed: {slug}")] UnknownSlug { slug: String },
    #[error("episode index {index} is outside 1..={retained} for {slug}")]
    IndexOutOfRange { slug: String, index: usize, retained: usize },
    #[error("no cached episodes for {slug}; run continuo refresh {slug}")] CacheMissing { slug: String },
    #[error("corrupt cache for {slug}: {detail}; run continuo refresh {slug}")]
    CacheCorrupt { slug: String, detail: String },
    #[error("cache parser {found} differs from {expected} for {slug}; run continuo refresh {slug}")]
    CacheParserMismatch { slug: String, found: u32, expected: u32 },
    #[error("cannot use subscriptions: {reason}")] SubscriptionsUnreadable { reason: String },
    #[error("invalid slug {slug:?}; expected 1-32 ASCII lowercase letters, digits or hyphens")]
    InvalidSlug { slug: String },
    #[error("slug already taken: {slug}")] SlugTaken { slug: String },
    #[error("already subscribed as {slug}")] AlreadySubscribed { slug: String },
    #[error("{failed} of {total} feeds did not complete successfully")]
    BatchIncomplete { failed: usize, total: usize },
    #[error(transparent)] Remote(#[from] crate::http::error::RemoteFailure),
    #[error(transparent)] Persistence(#[from] crate::persistence::PersistenceError),
}
```

Use `SubscriptionsUnreadable { reason: "cannot generate subscription identifier".into() }` if OS randomness fails; this adds no undocumented error variant. `NotPlayable.title` uses `"(untitled)"` when the domain title is absent. Presentation later prints the spec's exact quoted-title form.

- [ ] **Step 4: Implement hex generation, validation and alias selection.**

```rust
pub fn new_feed_id() -> Result<FeedId, FeedError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| FeedError::SubscriptionsUnreadable {
        reason: "cannot generate subscription identifier".into(),
    })?;
    let value: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    validate_feed_id(&value)
}
```

Implement `ascii_base(&str) -> String` as a private helper: ASCII alphanumerics lowercase and append; every other run contributes one `-`; trim and take at most 32 ASCII bytes. Empty title base uses `url.host_str()` with leading `www.` removed. Explicit alias validates before collision checking. Automatic collisions iterate integer suffixes from 2 and shorten the base by the suffix length. Validate every returned alias; if an unusual host also produces an empty base, return InvalidSlug and name `--as` in the command diagnostic instead of emitting an invalid subscription. This defensive fallback preserves the validator even where the spec's host assumption does not hold. No URL-derived IDs and no changes to `FeedId::new`.

- [ ] **Step 5: Run model tests and existing identity tests.** `cargo test --locked --test m4_subscription_model --test media_id --test identity_values`. Test two generated IDs differ and both validate; do not assert specific random bytes.
- [ ] **Step 6: Commit.**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/feed src/subscription tests/m4_subscription_model.rs
git commit -m "feat: define subscriptions and stable feed identifiers"
```

### Task 4: Implement durable subscription snapshots and recovering loads

**Files:** Create `src/subscription/store.rs`, `tests/m4_subscription_store.rs`; modify `src/subscription/mod.rs`, `Cargo.toml`, `Cargo.lock`.

**Interfaces:** Consumes `replace_bytes`, `Subscription`, `FeedError`, `Clock`, existing `LoadReason`. Produces:

```rust
#[derive(Clone, Debug)]
pub struct SubscriptionSnapshot { pub subscriptions: Vec<Subscription> }
pub struct SubscriptionLoad {
    pub snapshot: SubscriptionSnapshot,
    pub writable: bool,
    pub reason: crate::persistence::store::LoadReason,
}
// SubscriptionStore owns path: PathBuf and clock: Arc<dyn Clock>.
impl SubscriptionStore {
    pub fn new(path: PathBuf, clock: Arc<dyn Clock>) -> Self;
    pub fn path(&self) -> &Path;
    pub fn now(&self) -> OffsetDateTime;
    pub fn read_snapshot(&self) -> Result<SubscriptionSnapshot, FeedError>;
    pub fn load(&self) -> SubscriptionLoad;
    pub fn save(&self, snapshot: &SubscriptionSnapshot) -> Result<(), FeedError>;
}
```

- [ ] **Step 1: Add the two-entry-point corruption test.**

```rust
#[test]
fn only_mutating_load_quarantines() -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::Arc;
    use continuo::clock::FakeClock;
    use continuo::subscription::store::SubscriptionStore;
    use continuo::persistence::store::LoadReason;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    std::fs::write(&path, b"broken")?;
    let store = SubscriptionStore::new(path.clone(), Arc::new(FakeClock::new()));
    assert!(store.read_snapshot().is_err());
    assert_eq!(std::fs::read(&path)?, b"broken");
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
    let result = store.load();
    let LoadReason::Quarantined { moved_to } = result.reason else { panic!("expected quarantine") };
    assert_eq!(std::fs::read(moved_to)?, b"broken");
    assert!(!path.exists());
    Ok(())
}
```

- [ ] **Step 2: Run `cargo test --locked --test m4_subscription_store`; observe the missing store failure.**
- [ ] **Step 3: Define DTOs and pure version-first decoding.**

```rust
#[derive(serde::Serialize, serde::Deserialize)]
struct SubscriptionFile { schema_version: u32, subscriptions: Vec<SubscriptionRecord> }
#[derive(serde::Serialize, serde::Deserialize)]
struct SubscriptionRecord {
    feed_id: String,
    slug: String,
    title: Option<String>,
    fetch_url: url::Url,
    #[serde(with = "time::serde::rfc3339")]
    added_at: time::OffsetDateTime,
}
```

Use `Url` consistently in the DTOs and enable its serde feature explicitly: `url = { version = "2", features = ["serde"] }` in Cargo.toml. This is an existing dependency feature, not an additional dependency. Regenerate Cargo.lock through the focused test command without `--locked` once, inspect the lock diff, then restore locked commands. Validate ID/slug uniqueness with BTreeSets and HTTP(S) URLs with `NormalizedUrl::parse`. Return sanitized context without embedding offending JSON values. Do not cap the vector.

- [ ] **Step 4: Implement read, recovery and save policies.** `read_snapshot`: NotFound → empty; otherwise decode or Err, without mkdir. `load`: share decoding, preserve unreadable/unsupported files, quarantine malformed files using timestamp plus collision suffix with the existing 100-candidate bound. Quarantine failure disables writes. `save`: validate the complete snapshot, serialize schema 1, call `replace_bytes`. `now()` uses the injected clock; no ambient clock in library.rs.
- [ ] **Step 5: Add the complete rejection matrix.** Start from one valid explicit JSON record. Mutate feed ID to traversal text/uppercase/invalid length, duplicate ID, duplicate slug, invalid slug, non-HTTP URL, bad RFC3339 timestamp, unsupported schema; assert the documented read and recovery results. Save 513 distinct subscriptions and assert all survive to prove there is no checkpoint-style eviction. Test quarantine collision and all 100 names occupied. Test a directory at the subscription path as Unreadable.
- [ ] **Step 6: Run `cargo test --locked --test m4_subscription_store --test m4_subscription_model --test persistence_store`.** Assert a missing read creates no parent directory.
- [ ] **Step 7: Commit.**

```bash
git add Cargo.toml Cargo.lock src/subscription tests/m4_subscription_store.rs
git commit -m "feat: persist subscriptions with non-mutating reads"
```

### Task 5: Add bounded asynchronous document GETs

**Files:** Create `src/http/document.rs`, `tests/m4_document_fetch.rs`; modify `src/http/service.rs`, `src/http/limits.rs`, `src/http/error.rs`, `src/http/mod.rs`.

**Interfaces:** Consumes existing reqwest client, Limits, `RemoteFailure`, `Operation::Open`, `Phase`. Produces the exact `DocumentRequest`, `CacheValidators`, `DocumentOutcome` types in spec §3.1; `HttpService::handle(&self) -> tokio::runtime::Handle`; `HttpService::fetch_document(&self, DocumentRequest) -> impl Future<Output = Result<DocumentOutcome, RemoteFailure>>`. CacheValidators derives Clone, Debug, Serialize, Deserialize and Eq/PartialEq, using the explicit URL serde support introduced in Task 4. Its request/outcome URL fields remain Url values, not redacted strings; never log the complete request/outcome/validator Debug representation.

- [ ] **Step 1: Test full GET without media Range headers.**

```rust
mod support;
use continuo::http::{document::{DocumentRequest, DocumentOutcome}, limits::Limits, service::HttpService};
use support::server::{Script, TestServer};

#[test]
fn fetches_a_document_without_a_media_range() -> Result<(), Box<dyn std::error::Error>> {
    let xml = b"<rss><channel/></rss>".to_vec();
    let server = TestServer::start(Script::serving(xml.clone()).without_ranges());
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service.handle().block_on(service.fetch_document(DocumentRequest {
        origin: url::Url::parse(&server.url("/feed"))?, validators: None,
    }))?;
    let DocumentOutcome::Fetched { bytes, .. } = result else { panic!("expected a body") };
    assert_eq!(bytes, xml);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header("range"), None);
    assert_eq!(requests[0].header("accept-encoding"), Some("identity"));
    server.shutdown();
    Ok(())
}
```

- [ ] **Step 2: Run `cargo test --locked --test m4_document_fetch`; observe missing document interface.**
- [ ] **Step 3: Add types, limit and errors, then the service wrapper.**

```rust
// Add to Limits; Default uses 8 << 20 and brisk inherits it.
pub document_bytes: usize,

// Add to RemoteFailure:
#[error("feed document exceeds {limit} bytes")]
DocumentTooLarge { limit: usize },
#[error("received HTTP 304 without a usable conditional request")]
UnsolicitedNotModified,

// In HttpService's impl, keeping its fields private:
pub fn handle(&self) -> tokio::runtime::Handle { self.runtime.handle().clone() }
pub async fn fetch_document(&self, request: super::document::DocumentRequest)
    -> Result<super::document::DocumentOutcome, RemoteFailure>
{
    super::document::fetch_document(self.client.clone(), self.limits, request).await
}
```

`document::fetch_document` is `pub(super) async fn fetch_document(client: reqwest::Client, limits: Limits, request: DocumentRequest) -> Result<DocumentOutcome, RemoteFailure>`. Its private `fetch_inner` has the same arguments and return type. Wrap the complete `fetch_inner` future once in `tokio::time::timeout(limits.open, ...)`; timeout maps to Phase::Open. Do not spawn an extra runtime or call block_on inside either function.

- [ ] **Step 4: Implement the basic terminal response/body path.** Construct a plain GET with the exact Accept and Accept-Encoding headers in §3.5. Apply per-request header timeout; map connect timeout to Connect where `error.is_connect() && error.is_timeout()`, other reqwest failures to Transport with the existing URL-safe conversion. Make `transport_detail` visible `pub(super)` within http if sharing it, without changing its body. For terminal 200 reject non-identity Content-Encoding before reading. Treat terminal statuses other than 200/304 as Status. For now 304 without request conditions is UnsolicitedNotModified; Task 6 adds the conditional success path.

```rust
// The terminal 200 accumulation loop within fetch_inner:
if response.content_length().is_some_and(|n| n > limits.document_bytes as u64) {
    return Err(RemoteFailure::DocumentTooLarge { limit: limits.document_bytes });
}
let mut bytes = Vec::new();
loop {
    let chunk = tokio::time::timeout(limits.stall, response.chunk()).await
        .map_err(|_| RemoteFailure::Timeout { phase: Phase::Stall })?
        .map_err(|error| RemoteFailure::Transport {
            operation: Operation::Open, detail: super::service::transport_detail(error),
        })?;
    let Some(chunk) = chunk else { break };
    if chunk.len() > limits.document_bytes.saturating_sub(bytes.len()) {
        return Err(RemoteFailure::DocumentTooLarge { limit: limits.document_bytes });
    }
    bytes.extend_from_slice(&chunk);
}
```

Capture final URL, content type and ETag/Last-Modified before consuming the response; Fetched returns the accumulated bytes. No preallocation from an untrusted Content-Length. Initial and redirect URLs must obey the existing source policy: HTTP(S), host, no userinfo; diagnostics use redacted input. Reuse the checks in `resolve_source` as a reference without modifying it or requiring feed code to depend on app.rs.

- [ ] **Step 5: Add and pass body-limit/timeout tests.** Use an injected `document_bytes: 16` and Script::serving(vec![b'x'; 17]) for early Content-Length rejection; add `.chunked()` for authoritative streaming rejection without Content-Length. A 16-byte body must succeed. Use `.gzip_encoded()` to reject encoding, `.status(206)` to reject unsolicited media ranges, and `.trickle(1, Duration::from_millis(20))` with `open: 80 ms, stall: 100 ms` to establish whole-body deadline rather than stall. Always release/shutdown the server after failure.
- [ ] **Step 6: Run `cargo test --locked --test m4_document_fetch --test http_fetch --test http_response --test http_errors`.** Check existing media tests compile with the new additive enum/limit field and retain their assertions.
- [ ] **Step 7: Commit.**

```bash
git add src/http tests/m4_document_fetch.rs
git commit -m "feat: fetch bounded feed documents asynchronously"
```

### Task 6: Reuse redirects and add resource-scoped conditional GET

**Files:** Modify `src/http/document.rs`, `tests/support/server.rs`; create `tests/m4_document_protocol.rs`.

**Interfaces:** Consumes Task 5 plus the actual existing `accept_redirect(from: &Url, location: &str, hops: u8, seen: &[Url], limits: &Limits) -> Result<Url, RemoteFailure>`. Produces complete §3 behavior with no API change. Test-only additions to server.rs:

```rust
#[derive(Clone, Debug)]
pub struct DocumentReply {
    pub path: String,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub conditional: bool,
    pub header_delay: Duration,
}
impl Script {
    pub fn documents(replies: Vec<DocumentReply>) -> Self;
}
```

Matching uses request path. Route lookup failure returns 404. `conditional: true` returns 304 if If-None-Match exactly matches the route ETag; otherwise, if If-None-Match is absent, compare If-Modified-Since with Last-Modified. Preserve supplied ETag/Last-Modified on the 304. Route headers include a Location for redirects; body framing always uses the actual length. Existing Script fields/default behavior remain unchanged when document routes are absent. Respect existing Script::stall_headers/release before document responses so later tests can fail a store write after loading subscriptions.

- [ ] **Step 1: Extend the loopback harness as part of the first failing protocol test.** In `handle_connection`, after recording request and before media range handling, route document requests through `write_document_reply(stream, reply, request) -> std::io::Result<()>`. This helper chooses conditional status, applies delay/stall, writes headers plus Content-Length, then body only for a body-bearing response. Add reason phrases for 301/302/303/304/307/308. No behavior change for existing tests.
- [ ] **Step 2: Write the mixed-redirect test.**

```rust
#[test]
fn permanent_prefix_stops_before_temporary_redirect() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;
    use continuo::http::{document::{DocumentRequest, DocumentOutcome}, limits::Limits, service::HttpService};
    use support::server::{DocumentReply, Script, TestServer};
    let replies = vec![
        DocumentReply { path: "/a".into(), status: 301, headers: vec![("Location".into(), "/b".into())],
            body: vec![], conditional: false, header_delay: Duration::ZERO },
        DocumentReply { path: "/b".into(), status: 302, headers: vec![("Location".into(), "/c".into())],
            body: vec![], conditional: false, header_delay: Duration::ZERO },
        DocumentReply { path: "/c".into(), status: 200, headers: vec![],
            body: b"<rss><channel/></rss>".to_vec(), conditional: false, header_delay: Duration::ZERO },
    ];
    let server = TestServer::start(Script::documents(replies));
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service.handle().block_on(service.fetch_document(DocumentRequest {
        origin: server.url("/a").parse()?, validators: None,
    }))?;
    let DocumentOutcome::Fetched { final_url, permanent_url, .. } = result else { panic!("expected 200") };
    assert_eq!(final_url.as_str(), server.url("/c"));
    assert_eq!(permanent_url.as_ref().map(url::Url::as_str), Some(server.url("/b").as_str()));
    server.shutdown();
    Ok(())
}
```

- [ ] **Step 3: Run `cargo test --locked --test m4_document_protocol`; observe a 301 Status failure before implementing redirect handling.**
- [ ] **Step 4: Add the manual redirect loop around request/header acquisition.** Recognize only 301/302/303/307/308 as redirects. Missing Location becomes Status; invalid Location/loop/downgrade/hops use accept_redirect's own failures. Seed seen with the current URL before following a hop; do not reset deadlines. The prefix update is:

```rust
let target = accept_redirect(&current, location, hops + 1, &seen, &limits)?;
if permanent_prefix && matches!(status, 301 | 308) {
    permanent_url = Some(target.clone());
} else {
    permanent_prefix = false;
}
current = target;
hops += 1;
```

Initialize `permanent_prefix = true`, `permanent_url = None`, `hops = 0`, and preserve a seen list covering all previous requests. Drop the redirect response without reading its body. Do not send Range/If-Range on any hop.

- [ ] **Step 5: Add red tests for matching, mismatched and absent validators, then implement conditions and 304 merging.**

```rust
// Within each hop, with request.validators retained unchanged across hops:
let matching = request.validators.as_ref().filter(|v| v.url == current);
let mut sent_conditional = false;
if let Some(v) = matching {
    if let Some(etag) = &v.etag {
        builder = builder.header(reqwest::header::IF_NONE_MATCH, etag);
        sent_conditional = true;
    }
    if let Some(modified) = &v.last_modified {
        builder = builder.header(reqwest::header::IF_MODIFIED_SINCE, modified);
        sent_conditional = true;
    }
}
```

On 304 require `sent_conditional`; clone the matching record, replace only supplied ETag/Last-Modified, and return Unchanged with final/permanent URLs. Do not run 200 body-length or body-encoding checks on the bodyless 304. For 200, missing validator headers reset old values to None. Invalid cached header values must fail safely during request building, not panic or leak through reqwest Debug.

- [ ] **Step 6: Complete the protocol matrix.** Reverse the mixed chain; test all-permanent chain, loop, hop overflow, 303/307, absent Location and unsupported redirect scheme. For validator scope test A→B with validators for B: A gets neither conditional header, B gets both. Also test validators for A redirected to B: B receives neither and B's unsolicited 304 fails. Test empty validators record, weak ETag byte preservation, Last-Modified-only, 304 validator merge, and permanent redirect ending in 304. Delay each of two hops 60 ms with `headers: 100 ms, open: 90 ms`; assert Phase::Open. Reuse existing pure downgrade test for HTTPS→HTTP rather than requiring a new TLS server.
- [ ] **Step 7: Run `cargo test --locked --test m4_document_protocol --test m4_document_fetch --test http_server_selftest --test http_protocol --test http_fetch`.**
- [ ] **Step 8: Commit.**

```bash
git add src/http/document.rs tests/support/server.rs tests/m4_document_protocol.rs
git commit -m "feat: revalidate feeds with scoped HTTP validators"
```

### Task 7: Parse RSS through strict decoding and namespace-aware events

**Files:** Create `src/feed/model.rs`, `src/feed/parse.rs`, `tests/m4_feed_parse.rs`, `tests/fixtures/feeds/rss2-minimal.xml`, `tests/fixtures/feeds/decl-without-encoding.xml`, `tests/fixtures/feeds/README.md`; modify `src/feed/mod.rs`, `Cargo.toml`, `Cargo.lock`.

**Interfaces:** Produces spec §2.1 `ParsedFeed`, `ParsedItem`, `Enclosure`, all Clone/Debug/PartialEq; `ParseReport { pub feed: ParsedFeed, pub skipped: usize, pub warnings: Vec<ParseWarning> }`; `ParseWarning { pub item: Option<usize>, pub kind: WarningKind }`; `WarningKind` variants `UnknownIdentityEntity`, `InvalidDate`, `InvalidEnclosure`, `ExtraEnclosures { ignored: usize }`. Warning payloads never contain raw GUIDs or URLs. Public parser: `parse_feed(bytes: &[u8], retrieval_url: &Url) -> Result<ParseReport, FeedError>`.

Identityless/duplicate skipping is Task 9's binding responsibility. `ParseReport.skipped` counts only parse-stage item rejection, e.g. invalid identity references; these counts are added exactly once when binding. Dates that fail do not increment skipped.

- [ ] **Step 1: Add the first real fixture and a failing parse assertion.**

```xml
<?xml version="1.0"?>
<rss version="2.0" xmlns:i="http://www.itunes.com/dtds/podcast-1.0.dtd">
  <channel><title>Radio &amp; Friends</title><link>https://example.org/</link>
    <item><guid isPermaLink="true">  https://EXAMPLE.org/p?a=1&amp;b=2  </guid>
      <title>Opening</title><enclosure url="audio/one.mp3" length="42" type="audio/mpeg"/>
      <pubDate>Sun, 06 Sep 2026 18:00:00 GMT</pubDate><i:duration>1:42:00</i:duration>
    </item>
  </channel>
</rss>
```

```rust
#[test]
fn rss_keeps_decoded_guid_whitespace() -> Result<(), Box<dyn std::error::Error>> {
    use continuo::feed::parse::parse_feed;
    let report = parse_feed(include_bytes!("fixtures/feeds/rss2-minimal.xml"),
        &url::Url::parse("https://example.org/feed.xml")?)?;
    assert_eq!(report.feed.title.as_deref(), Some("Radio & Friends"));
    assert_eq!(report.feed.items[0].guid.as_deref(), Some("  https://EXAMPLE.org/p?a=1&b=2  "));
    assert_eq!(report.feed.items[0].enclosure.as_ref().map(|e| e.url.as_str()),
        Some("https://example.org/audio/one.mp3"));
    assert_eq!(report.feed.items[0].declared_duration, Some(std::time::Duration::from_secs(6120)));
    Ok(())
}
```

- [ ] **Step 2: Run `cargo test --locked --test m4_feed_parse`; observe missing parser.**
- [ ] **Step 3: Add the exact quick-xml dependency and the value types.** Update Cargo.lock using the focused test without `--locked` once, then use locked commands. Copy the complete §2.1 structs; use std::time::Duration. Inspect the resolved quick-xml 0.42 source, particularly BytesDecl, DecodingReader, NsReader and Event::GeneralRef. Read the primary [declaration API](https://docs.rs/quick-xml/latest/quick_xml/events/struct.BytesDecl.html#method.encoding) and [decoding API](https://docs.rs/quick-xml/latest/quick_xml/encoding/struct.DecodingReader.html) if local source is unavailable. Do not import encoding_rs directly.
- [ ] **Step 4: Implement declaration handling and strict decode error mapping.**

```rust
// In the Event::Decl branch, after taking an owned declaration if needed:
match declaration.encoding() {
    None => {},
    Some(Err(_)) => return Err(FeedError::Malformed { detail: "invalid XML declaration".into() }),
    Some(Ok(label)) => {
        let label = label.into_owned();
        let encoding = declaration.encoder().ok_or_else(|| FeedError::UnsupportedEncoding {
            label: label.clone(),
        })?;
        reader.get_mut().set_encoding(encoding);
    }
}
```

The reader is `NsReader::from_reader(DecodingReader::new(bytes))`. Keep whitespace trimming disabled globally. Map XML I/O InvalidData to Encoding, other parse failures to Malformed with safe byte-position/context, not raw document text. Check declaration version/attribute errors; absent declaration defaults appropriately. Do not assume a known label alone guarantees safe switching: add a long-declaration characterization test before depending on the crate's fixed prefix. Never let feed-controlled input trigger `set_encoding`'s assertion. If the resolved crate cannot safely switch after that declaration, return a typed malformed/encoding error before that call and record the supported declaration bound in the fixture README; do not catch panics as normal control flow or add a decoder dependency.

- [ ] **Step 5: Implement the RSS pull walker with explicit contexts.** Use namespace-expanded element names and a stack of owned frames. Each frame holds namespace URI, local name, resolved base URL, and accumulated field text. Own the namespace result before advancing the reader. Distinguish root/channel/item fields by path and depth; extension elements with matching local names never substitute for RSS fields. Treat Empty as enter+exit. Require one supported root and channel, and finish at EOF only with all opened elements closed. Unknown extensions are ignored; malformed XML in them still fails the document. Do not fetch DTDs or expand custom DTD entities.

Private accumulator interfaces inside parse.rs:

```rust
enum Format { Rss, Atom }
struct ItemBuilder { item: ParsedItem, rejected: bool, enclosure_count: usize }
struct Frame { namespace: Option<String>, local: String, base: Option<Url>, text: String }
fn parse_duration(value: &str) -> Option<std::time::Duration>;
fn resolved_url(base: Option<&Url>, value: &str) -> Option<Url>;
```

Initialize ParsedItem fields to None. RSS selects direct channel/item children. On guid close store exact assembled decoded text. Title/link/date/duration values may trim surrounding formatting whitespace; GUID/id never do. Resolve enclosure attributes at element entry using its effective xml:base, require HTTP(S), and select first usable enclosure. Bad length yields None, not item rejection. Use `time::format_description::well_known::Rfc2822` for RSS dates. Implement `parse_duration` with checked integer arithmetic: seconds and minutes after a colon must be below 60; minutes in MM:SS may exceed 59; reject negatives, decimals, empty parts, overflow and more than three parts.

- [ ] **Step 6: Add encoding tests with actual encoded bytes.** Keep readable UTF-8 source fixtures under feeds/; construct encoded variants in test code, documented in fixtures/feeds/README.md. This avoids a runtime fixture-generation dependency and avoids pretending a UTF-8 file is Latin-1.

```rust
#[test]
fn utf16_bom_is_decoded_strictly() -> Result<(), Box<dyn std::error::Error>> {
    use continuo::feed::parse::parse_feed;
    let xml = "<?xml version=\"1.0\"?><rss><channel><title>Радио</title></channel></rss>";
    let mut bytes = vec![0xff, 0xfe];
    for unit in xml.encode_utf16() { bytes.extend_from_slice(&unit.to_le_bytes()); }
    let report = parse_feed(&bytes, &"https://example.org/feed".parse()?)?;
    assert_eq!(report.feed.title.as_deref(), Some("Радио"));
    Ok(())
}
```

Add UTF-16BE BOM, UTF-16LE declared without BOM, UTF-8 BOM, declared Latin-1 (`b"\xe9"` in the title), invalid UTF-8 (`b"\xff"`), unknown label, declaration without encoding, malformed encoding attribute, and a long declaration with a non-UTF-8 body. Verify no U+FFFD substitution, no panic and the documented error class. Test malformed feed suffix after a valid item to prove parsing consumes the complete document before success.
- [ ] **Step 7: Run `cargo test --locked --test m4_feed_parse --test fixtures_and_features`.** Update only additive dependency-feature assertions if that suite explicitly enumerates dependencies; do not relax codec requirements.
- [ ] **Step 8: Commit.**

```bash
git add Cargo.toml Cargo.lock src/feed tests/m4_feed_parse.rs tests/fixtures/feeds
git commit -m "feat: parse RSS feeds with strict XML decoding"
```

### Task 8: Complete Atom, namespace/base handling and entity policies

**Files:** Modify `src/feed/parse.rs`, `tests/m4_feed_parse.rs`, `tests/fixtures/feeds/README.md`; create the remaining textual fixtures named in spec §8.1 under `tests/fixtures/feeds/`.

**Interfaces:** Consumes Task 7's parse types and `parse_feed`; produces complete RSS/Atom behavior without signature changes. Any pure parse helper remains private and is tested through parse_feed or unit tests in parse.rs.

- [ ] **Step 1: Add Atom/base fixtures and failing assertions.**

```xml
<a:feed xmlns:a="http://www.w3.org/2005/Atom" xml:base="../podcasts/">
  <a:title>Example</a:title><a:link href="./"/>
  <a:entry xml:base="season/">
    <a:id>  stable&amp;id  </a:id>
    <a:title type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml">Hello <b>world</b></div></a:title>
    <a:link rel="enclosure" href="one.mp3" length="42" type="audio/mpeg"/>
    <a:published>2026-09-06T18:00:00Z</a:published>
  </a:entry>
</a:feed>
```

```rust
#[test]
fn atom_uses_nested_base_and_exact_id() -> Result<(), Box<dyn std::error::Error>> {
    use continuo::feed::parse::parse_feed;
    let report = parse_feed(include_bytes!("fixtures/feeds/atom-minimal.xml"),
        &"https://example.org/feeds/show.xml".parse()?)?;
    let item = &report.feed.items[0];
    assert_eq!(item.guid.as_deref(), Some("  stable&id  "));
    assert_eq!(item.title.as_deref(), Some("Hello world"));
    assert_eq!(item.enclosure.as_ref().map(|e| e.url.as_str()),
        Some("https://example.org/podcasts/season/one.mp3"));
    Ok(())
}
```

- [ ] **Step 2: Run `cargo test --locked --test m4_feed_parse atom_uses_nested_base_and_exact_id`; expect UnsupportedFormat before Atom mapping is added.**
- [ ] **Step 3: Implement Atom mappings in the same walker.** Atom uses only the Atom namespace, entry/id for guid, first alternate link including absent rel, and first usable enclosure link. Parse published with Rfc3339; if published is absent use updated; malformed present published yields None and warning as §4.6 specifies. Do not accidentally use feed/updated as entry time. Titles implement text/html/xhtml exactly as §4.8, preserving whitespace at markup boundaries. Site link is first usable feed alternate link. Keep xmlns aliases immaterial by matching URI/local pairs.
- [ ] **Step 4: Implement distinct character/reference handling with regression tests.** Text appends character content, CData appends literal content, GeneralRef decodes one standard/numeric reference or retains it literally only for title targets. Attribute values use strict XML unescape. An unknown entity in item identity attributes/text marks that item rejected and continues parsing to its close; document-level structural errors still fail the feed. Feed-level invalid identity fields have no enclosing item and therefore return Malformed. Warnings contain item ordinal and category only.

```rust
// A complete helper for a single reference, not an HTML entity table:
fn reference_text(name: &str, display: bool) -> Result<String, quick_xml::escape::EscapeError> {
    let spelling = format!("&{name};");
    quick_xml::escape::unescape_with(&spelling, |entity| {
        if display && !entity.starts_with('#') && quick_xml::escape::resolve_xml_entity(entity).is_none() {
            Some(spelling.as_str())
        } else {
            None // Let quick-xml decode standard and numeric references.
        }
    }).map(std::borrow::Cow::into_owned)
}
```

The resolver returns borrowed storage that lives across unescape_with; do not return a temporary `format!` string from the resolver or leak an allocation to satisfy its lifetime. Do not unescape the fully assembled field after CDATA/reference processing: that would double-decode `&amp;lt;`.

- [ ] **Step 5: Complete fixture matrix as small test cycles.** Each named fixture has an explicit assertion, not merely `parse_feed(...).is_ok()`:

| Fixture/test case | Assertion |
|---|---|
| `xml-base-relative` | Nested base includes both parent and child directories; sibling base restores on pop |
| `cdata-title` | `<![CDATA[A &amp; B]]>` displays `A &amp; B` literally |
| `atom-title-types` | text literal text; html retains markup; xhtml concatenates descendant text |
| `atom-link-no-rel` | Acts as alternate, used for identity if id/enclosure absent |
| `unknown-entity-in-title` | Literal `&nbsp;`; standard `&amp;` still decodes |
| `unknown-entity-in-guid` | Item skipped, following valid item retained, skipped count increments once |
| `itunes-duration-forms` | 1:02:03→3723, 62:03→3723, 3723→3723; 1:60, negative and overflow→None |
| `enclosure-malformed-with-guid` | Valid GUID retained, enclosure None |
| `enclosure-scheme-unsupported` | file/data/ftp never become playback sources |
| `multiple-enclosures` | First valid HTTP(S) enclosure wins, ignored-count warning matches |
| `rss1-rdf` | UnsupportedFormat |
| `truncated-xml` | Whole-document failure even after a complete earlier item |
| namespace-shadowing case | A foreign `title`/`id` cannot replace RSS/Atom fields |
| invalid date case | Item stays, date None, warning, skipped count unchanged |
| `cyrillic-title` | Descriptive Unicode title preserved; alias policy remains a different layer |

Add missing `<channel>`, empty valid feed, multiple document roots and out-of-context item elements. Keep a plain stack rather than recursive descent so deeply nested extensions cannot overflow the Rust call stack.

- [ ] **Step 6: Run `cargo test --locked --test m4_feed_parse` and `cargo clippy --locked --lib -- -D warnings`.** All fixtures pass using only the approved dependency features.
- [ ] **Step 7: Commit.**

```bash
git add src/feed/parse.rs tests/m4_feed_parse.rs tests/fixtures/feeds
git commit -m "feat: interpret Atom feeds and podcast XML extensions"
```

### Task 9: Bind parsed items to existing episode identities

**Files:** Create `src/feed/episode.rs`, `tests/m4_episode_binding.rs`; modify `src/feed/mod.rs`, `src/feed/model.rs`, `src/media/mod.rs`, `src/media/id.rs`.

**Interfaces:** Consumes `ParseReport`, existing `EpisodeKey::resolve`, `MediaId`, `SourceLocation`. Produces `bind_feed(feed_id: &FeedId, parsed: ParseReport) -> BoundFeed`; `BoundFeed { title: Option<String>, site_link: Option<Url>, items: Vec<BoundItem>, skipped: usize, warnings: Vec<ParseWarning> }`; `BoundItem { episode: Episode, enclosure_length: Option<u64>, enclosure_mime: Option<String> }`. Produces the spec's three added descriptive Episode fields and both MediaId accessors. Extend WarningKind with `MissingIdentity` and `DuplicateIdentity`.

- [ ] **Step 1: Add the identity/deduplication test.**

```rust
#[test]
fn duplicate_guid_keeps_first_and_nonplayable_identity_survives()
    -> Result<(), Box<dyn std::error::Error>>
{
    use continuo::feed::{parse::parse_feed, episode::bind_feed};
    use continuo::subscription::model::validate_feed_id;
    let xml = br#"<rss><channel>
      <item><guid>same</guid><title>first</title></item>
      <item><guid>same</guid><title>second</title><enclosure url="https://example.org/b.mp3"/></item>
      <item><title>no identity</title></item>
    </channel></rss>"#;
    let feed_id = validate_feed_id("0123456789abcdef0123456789abcdef")?;
    let bound = bind_feed(&feed_id, parse_feed(xml, &"https://example.org/feed".parse()?)?);
    assert_eq!(bound.items.len(), 1);
    assert_eq!(bound.skipped, 2);
    assert_eq!(bound.items[0].episode.title.as_deref(), Some("first"));
    assert!(bound.items[0].episode.source.is_none());
    assert_eq!(bound.items[0].episode.id.feed(), Some(&feed_id));
    Ok(())
}
```

- [ ] **Step 2: Run `cargo test --locked --test m4_episode_binding`; observe missing binding/accessors.**
- [ ] **Step 3: Extend the existing types, preserving every identity encoding rule.**

```rust
impl MediaId {
    pub fn feed(&self) -> Option<&FeedId> {
        match self { Self::PodcastEpisode { feed, .. } => Some(feed), _ => None }
    }
    pub fn episode_key(&self) -> Option<&EpisodeKey> {
        match self { Self::PodcastEpisode { episode, .. } => Some(episode), _ => None }
    }
}
```

Add only `title`, `published`, `declared_duration` to media::Episode, preserving its derives and source/id fields. Do not introduce a duplicate key field or a new domain Episode type.

- [ ] **Step 4: Implement binding and warning/count ownership.** Iterate parsed items in order. Pre-filter enclosure scheme/host again at the binding boundary because callers can construct ParsedItem without the parser. Resolve identity from the original GUID, usable enclosure, then link. On resolution failure append MissingIdentity and skip. Use a BTreeSet of EpisodeKey to detect duplicates, keeping the first. Build SourceLocation::Http only from the selected usable enclosure. Carry length/mime beside Episode in BoundItem and carry parsed warnings forward.

```rust
let key = match EpisodeKey::resolve(item.guid.as_deref(), enclosure.as_ref().map(|e| &e.url), item.link.as_ref()) {
    Ok(key) => key,
    Err(_) => {
        skipped += 1;
        warnings.push(ParseWarning { item: Some(ordinal), kind: WarningKind::MissingIdentity });
        continue;
    }
};
if !seen.insert(key.clone()) {
    skipped += 1;
    warnings.push(ParseWarning { item: Some(ordinal), kind: WarningKind::DuplicateIdentity });
    continue;
}
let id = MediaId::PodcastEpisode { feed: feed_id.clone(), episode: key };
```

Do not log `item` or identity error Debug: those may contain signed enclosure URLs. Warnings are values now; the application layer emits typed warning categories once after parsing/binding.

- [ ] **Step 5: Add fallback and identity-stability tests.** GUID overrides two different enclosures; absent GUID uses enclosure; invalid enclosure with valid link uses link; unknown entity was already skipped by parser; no enclosure with GUID stays. Compare IDs across two fetches with changed title/date/enclosure but the same GUID. Assert LocalFile and RemoteUrl accessors return None. Add fixtures `guid-absent-uses-enclosure`, `item-without-enclosure`, `item-without-identity`, `duplicate-identity` if not yet added.
- [ ] **Step 6: Run `cargo test --locked --test m4_episode_binding --test media_id --test identity_values --test domain_values --test m4_feed_parse`.** Existing canonical identity tests pass unchanged.
- [ ] **Step 7: Commit.**

```bash
git add src/media src/feed tests/m4_episode_binding.rs tests/fixtures/feeds
git commit -m "feat: bind feed items to podcast episode identities"
```

### Task 10: Atomically cache episodes with representation metadata

**Files:** Create `src/feed/cache.rs`, `tests/m4_feed_cache.rs`, `tests/support/feeds.rs`; modify `src/feed/mod.rs`.

**Interfaces:** Consumes `BoundFeed`, `BoundItem`, CacheValidators, `replace_bytes`, `validate_feed_id`, `Subscription`. Produces:

```rust
pub const CACHE_SCHEMA_VERSION: u32 = 1;
pub const PARSER_VERSION: u32 = 1;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CachedEpisode {
    pub media_id: MediaId,
    pub enclosure_url: Option<Url>,
    pub enclosure_length: Option<u64>,
    pub enclosure_mime: Option<String>,
    pub title: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub published: Option<OffsetDateTime>,
    pub declared_duration_secs: Option<u64>,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CachedFeed {
    pub schema_version: u32,
    pub parser_version: u32,
    pub feed_id: String,
    pub fetched_from: Url,
    #[serde(with = "time::serde::rfc3339")]
    pub last_refreshed_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_fetched_at: OffsetDateTime,
    pub validators: CacheValidators,
    pub title: Option<String>,
    pub site_link: Option<Url>,
    pub skipped_items: usize,
    pub episodes: Vec<CachedEpisode>,
}
impl CachedEpisode { pub fn episode(&self) -> Episode; }
impl CachedFeed {
    pub fn from_bound(id: &FeedId, feed: BoundFeed, final_url: Url,
                      validators: CacheValidators, now: OffsetDateTime) -> Self;
}
impl CacheStore {
    pub fn new(feeds_dir: PathBuf) -> Self;
    pub fn path_for(&self, id: &FeedId) -> Result<PathBuf, FeedError>;
    pub fn read(&self, subscription: &Subscription) -> Result<CachedFeed, FeedError>;
    pub fn save(&self, subscription: &Subscription, feed: &CachedFeed) -> Result<(), FeedError>;
    pub fn remove(&self, id: &FeedId) -> Result<(), FeedError>;
}
```

Validate both reads and writes, so a public DTO cannot bypass invariants. New cache serde errors return CacheCorrupt with safe category/line/column; cache filesystem failures are mapped to PersistenceError::Io with an accurate operation string.

- [ ] **Step 1: Define a reusable test fixture and add a failing cache roundtrip.** In `tests/support/feeds.rs`, define the following helpers in full. This helper module is imported directly from new tests, without changing all existing support consumers.

```rust
use std::sync::Arc;
use continuo::{clock::FakeClock, feed::{cache::CacheStore, episode::bind_feed, parse::parse_feed},
    persistence::store::StateStore, subscription::{model::{Subscription, validate_feed_id},
    store::{SubscriptionStore, SubscriptionSnapshot}}};

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
        let subs = SubscriptionStore::new(root.path().join("data/continuo/subscriptions.json"), clock.clone());
        let cache = CacheStore::new(root.path().join("cache/continuo/feeds"));
        let state = StateStore::new(root.path().join("state/continuo/state.json"), clock.clone());
        Ok(Self { root, clock, subs, cache, state })
    }
    pub fn seed(&self, xml: &[u8], url: &str) -> Result<Subscription, Box<dyn std::error::Error>> {
        use continuo::{clock::Clock, feed::cache::CachedFeed, http::document::CacheValidators};
        let id = validate_feed_id(FEED_ID)?;
        let url: url::Url = url.parse()?;
        let bound = bind_feed(&id, parse_feed(xml, &url)?);
        let subscription = Subscription { feed_id: id.clone(), slug: "radio-t".into(),
            title: bound.title.clone(), fetch_url: url.clone(), added_at: self.clock.sample().wall };
        let feed = CachedFeed::from_bound(&id, bound, url.clone(),
            CacheValidators { url, etag: None, last_modified: None }, self.clock.sample().wall);
        self.cache.save(&subscription, &feed)?;
        self.subs.save(&SubscriptionSnapshot { subscriptions: vec![subscription.clone()] })?;
        Ok(subscription)
    }
}
```

```rust
#[path = "support/feeds.rs"] mod feeds;
#[test]
fn cached_identity_and_validators_roundtrip() -> Result<(), Box<dyn std::error::Error>> {
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(br#"<rss><channel><item><guid>id</guid></item></channel></rss>"#,
        "https://example.org/feed")?;
    let cached = rig.cache.read(&sub)?;
    assert_eq!(cached.episodes.len(), 1);
    assert_eq!(cached.episodes[0].media_id.feed(), Some(&sub.feed_id));
    assert!(cached.episodes[0].episode().source.is_none());
    assert_eq!(cached.validators.url, sub.fetch_url);
    Ok(())
}
```

- [ ] **Step 2: Run `cargo test --locked --test m4_feed_cache`; observe missing cache API.**
- [ ] **Step 3: Implement DTO conversion and version-first cache validation.** `CachedFeed::from_bound` flattens each BoundItem into the exact §5.2 JSON shape, serializing durations as seconds. `CachedEpisode::episode` reconstructs the existing domain value without re-resolving identity from enclosure URLs. Read schema first; unsupported cache schema is CacheCorrupt with a static version diagnostic. Parser version mismatch is CacheParserMismatch. Check envelope/feed ID equality, every media ID's podcast variant/feed equality, unique keys, and usable HTTP(S) enclosure URLs. Validate fetched_from and validator URL as HTTP(S); in a fresh snapshot validator URL equals fetched_from. A valid identity with no enclosure remains legal.
- [ ] **Step 4: Implement filesystem operations with no read-side mutation.** `path_for` validates ID before `feeds_dir.join(format!("{}.json", id.as_str()))`. `read` maps NotFound to CacheMissing; never mkdir/rename/delete. `save` validates, serializes and calls replace_bytes. `remove` treats NotFound as success and reports every other removal failure. The caller never loads an unreferenced cache: it first finds a subscription.
- [ ] **Step 5: Add semantic-corruption and failed-save tests.** After seeding, deserialize the cache into `serde_json::Value`, modify one field, write it back and assert rejection without deletion. Cover foreign envelope ID, RemoteUrl media ID, foreign podcast ID, duplicate key, noncanonical ID spelling, unsupported schema, parser mismatch, FTP enclosure, and malformed URL. CacheMissing remains distinct. To prove failed replacement preserves old data, inject invalid DTO data so validation fails before replacement and compare old bytes. Task 1 already exercises rename failure cleanup; Task 13 tests orchestration failure after a real cache commit.
- [ ] **Step 6: Run `cargo test --locked --test m4_feed_cache --test m4_episode_binding --test m4_atomic`.** Inspect one serialized fixture to ensure `guid:` stays literal, `/` is `%2F`, and no duplicate episode-key field was added.
- [ ] **Step 7: Commit.**

```bash
git add src/feed/cache.rs src/feed/mod.rs tests/m4_feed_cache.rs tests/support/feeds.rs
git commit -m "feat: atomically cache parsed podcast episodes"
```

### Task 11: Join checkpoints into read-only listings and selection

**Files:** Create `src/library.rs`, `tests/m4_library_reads.rs`; modify `src/lib.rs`.

**Interfaces:** Consumes SubscriptionStore::read_snapshot, CacheStore::read, StateSnapshot. Produces spec §6.6 `FeedSummary`, `EpisodeRow`, `Progress`, and these functions (exact spec signatures):

```rust
pub fn list_feeds(subs: &SubscriptionStore, cache: &CacheStore) -> Result<Vec<FeedSummary>, FeedError>;
pub fn list_episodes(subs: &SubscriptionStore, cache: &CacheStore, state: &StateSnapshot,
    slug: &str, limit: Option<NonZeroUsize>) -> Result<Vec<EpisodeRow>, FeedError>;
pub fn resolve_episode(subs: &SubscriptionStore, cache: &CacheStore, slug: &str, index: usize)
    -> Result<(MediaId, SourceLocation), FeedError>;
```

Derive Debug/Clone/PartialEq for row and Progress types. Keep private `progress_for(entry: Option<&PersistedCheckpoint>) -> Progress` in library.rs; it is not the existing playback::event::Progress. Private `find_subscription(snapshot: &SubscriptionSnapshot, slug: &str) -> Result<Subscription, FeedError>` returns an owned clone or UnknownSlug and is also consumed inside Tasks 12–13.

- [ ] **Step 1: Write the estimate-wins/no-audio test using real snapshot JSON.**

```rust
#[path = "support/feeds.rs"] mod feeds;
#[test]
fn latest_estimate_and_missing_audio_are_independent() -> Result<(), Box<dyn std::error::Error>> {
    use continuo::{library::{list_episodes, Progress}, persistence::model::PersistedState};
    let rig = feeds::Rig::new()?;
    let sub = rig.seed(br#"<rss><channel><item><guid>id</guid><title>Outtakes</title></item></channel></rss>"#,
        "https://example.org/feed")?;
    let cached = rig.cache.read(&sub)?;
    let id = cached.episodes[0].media_id.to_string();
    let json = serde_json::json!({"schema_version": 2, "checkpoints": {
        (id): {"position": {"secs": 100, "nanos": 0}, "estimated": {"secs": 1082, "nanos": 0},
             "completed": false, "touch_seq": 1, "updated_at": "2026-09-11T00:00:00Z"}
    }});
    let state: PersistedState = serde_json::from_value(json)?;
    rig.state.write(&state)?;
    let rows = list_episodes(&rig.subs, &rig.cache, &rig.state.read_snapshot()?, "radio-t", None)?;
    assert_eq!(rows[0].progress, Progress::Estimated(std::time::Duration::from_secs(1082)));
    assert!(!rows[0].playable);
    assert_eq!(rows[0].index, 1);
    Ok(())
}
```

The parenthesized `(id)` is the computed serde_json::json! object key; never replace it with the literal key `"id"`. Verify the stored map contains the actual canonical media ID before asserting progress.

- [ ] **Step 2: Run `cargo test --locked --test m4_library_reads`; observe missing library API.**
- [ ] **Step 3: Implement precedence and listing composition.**

```rust
fn progress_for(entry: Option<&PersistedCheckpoint>) -> Progress {
    let Some(c) = entry else { return Progress::None };
    if c.completed { return Progress::Played; }
    if let Some(value) = c.estimated { return Progress::Estimated(value); }
    if let Some(value) = c.position { return Progress::Established(value); }
    Progress::Unknown
}
```

Build rows by enumerating `cached.episodes` in stored order and assigning `index + 1`, then apply `.take(limit.map_or(usize::MAX, NonZeroUsize::get))`. Do not sort by date or title. `list_feeds` iterates subscription order, uses durable title, reports missing cache as None count/timestamp, and propagates corrupt/incompatible errors. Do not pass HttpService to either function.

- [ ] **Step 4: Implement episode resolution before all playback resources.** Read subscriptions through read_snapshot, find slug, read/validate cache, reject zero/out-of-range index with retained count. Obtain CachedEpisode::episode; missing source returns NotPlayable without initializing runtime, engine, or audio. Return the cached podcast MediaId, never call resolve_source on the enclosure to generate identity. Emit safe length/mime diagnostics at resolution: compare MIME top-level type case-insensitively, warning for a present non-audio declaration without refusing playback. Never log the full CachedEpisode.
- [ ] **Step 5: Add the remaining read-contract tests.** Table-test completed-with-estimate, established-only, estimated-only, neither and no entry. Use unsorted/missing publication dates and verify stored order. Test `-n 2` and selection 2 return matching IDs. Unplayable index returns typed NotPlayable. Missing subscriptions yield UnknownSlug; missing cache produces CacheMissing for episode operations and a zero-success missing row for feeds. Corrupt state is reported by the snapshot-read caller; neither listing mutates any file. Snapshot the temporary tree's sorted relative filenames, bytes and modified times before/after valid and failing reads. Do not include access time in this comparison.
- [ ] **Step 6: Run `cargo test --locked --test m4_library_reads --test m4_state_snapshot --test m4_feed_cache --test estimated_resume`.**
- [ ] **Step 7: Commit.**

```bash
git add src/lib.rs src/library.rs tests/m4_library_reads.rs
git commit -m "feat: list cached episodes with checkpoint progress"
```

### Task 12: Subscribe and unsubscribe with explicit partial-success outcomes

**Files:** Modify `src/library.rs`; create `tests/m4_library_mutations.rs`.

**Interfaces:** Consumes SubscriptionStore::load/save, CacheStore::save/remove, `new_feed_id`, `choose_slug`, `parse_feed`, `bind_feed`, `CachedFeed::from_bound`, HttpService::fetch_document. Produces these exact spec types and functions:

```rust
pub struct FollowupFailure { pub step: FollowupStep, pub error: FeedError }
pub enum FollowupStep { SaveSubscription, RemoveCache }
pub struct SubscribeOutcome {
    pub slug: String, pub feed_id: FeedId, pub title: Option<String>,
    pub retained: usize, pub skipped: usize, pub followup: Option<FollowupFailure>,
}
pub struct UnsubscribeOutcome { pub slug: String, pub followup: Option<FollowupFailure> }
pub async fn subscribe(http: &HttpService, subs: &SubscriptionStore, cache: &CacheStore,
    url: &str, slug: Option<&str>) -> Result<SubscribeOutcome, FeedError>;
pub fn unsubscribe(subs: &SubscriptionStore, cache: &CacheStore, slug: &str)
    -> Result<UnsubscribeOutcome, FeedError>;
```

Private helper consumed by Task 13: `load_mutating(subs: &SubscriptionStore) -> Result<SubscriptionSnapshot, FeedError>`. It accepts only Loaded/Missing with writable true. All other LoadReason values produce SubscriptionsUnreadable, with quarantine path where relevant. Derive Debug for outcome types with already-safe URL diagnostics.

- [ ] **Step 1: Add a subscribe/offline-read/unsubscribe test.**

```rust
mod support;
#[path = "support/feeds.rs"] mod feeds;

#[test]
fn subscription_roundtrip_does_not_touch_checkpoints() -> Result<(), Box<dyn std::error::Error>> {
    use continuo::{http::{limits::Limits, service::HttpService}, library};
    use support::server::{Script, TestServer};
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::serving(
        br#"<rss><channel><title>Radio T</title><item><guid>id</guid></item></channel></rss>"#.to_vec()));
    let service = HttpService::spawn(Limits::brisk())?;
    let result = service.handle().block_on(library::subscribe(&service, &rig.subs, &rig.cache,
        &server.url("/feed"), None))?;
    assert_eq!(result.slug, "radio-t");
    assert!(result.followup.is_none());
    assert_eq!(library::list_feeds(&rig.subs, &rig.cache)?.len(), 1);
    server.shutdown();
    let outcome = library::unsubscribe(&rig.subs, &rig.cache, "radio-t")?;
    assert!(outcome.followup.is_none());
    assert!(library::list_feeds(&rig.subs, &rig.cache)?.is_empty());
    assert!(!rig.state.path().exists());
    Ok(())
}
```

- [ ] **Step 2: Run `cargo test --locked --test m4_library_mutations`; observe missing mutation functions.**
- [ ] **Step 3: Implement preflight and subscribe orchestration in small groups.** Load the subscription snapshot before network I/O; reject invalid explicit alias/collision and AlreadySubscribed using normalized **requested** URL versus stored fetch URLs. Validate HTTP(S)/host/userinfo with safe diagnostics. Fetch unconditionally. Unchanged cannot initialize a cache and is UnsolicitedNotModified. Parse using the returned final_url, choose the slug using the original requested fetch host, generate an unused random ID, bind, emit categorized warnings, then create the new cache.

```rust
// At the commit seam after all validation, fetching, parsing and binding:
cache.save(&subscription, &cached)?;
let retained = cached.episodes.len();
let skipped = cached.skipped_items;
let slug = subscription.slug.clone();
let feed_id = subscription.feed_id.clone();
let title = subscription.title.clone();
snapshot.subscriptions.push(subscription);
let followup = subs.save(&snapshot).err().map(|error| FollowupFailure {
    step: FollowupStep::SaveSubscription, error,
});
Ok(SubscribeOutcome { slug, feed_id, title, retained, skipped, followup })
```

The new subscription's fetch_url is permanent_url if supplied, otherwise the normalized requested URL; a temporary final_url remains only in cache metadata. All timestamps for the successful subscribe are sampled from `subs.now()` after parsing. Do not write an initial empty subscription file before the cache succeeds.

- [ ] **Step 4: Implement unsubscribe's opposite commit order.**

```rust
let mut snapshot = load_mutating(subs)?;
let subscription = find_subscription(&snapshot, slug)?;
snapshot.subscriptions.retain(|entry| entry.feed_id != subscription.feed_id);
subs.save(&snapshot)?;
let followup = cache.remove(&subscription.feed_id).err().map(|error| FollowupFailure {
    step: FollowupStep::RemoveCache, error,
});
Ok(UnsubscribeOutcome { slug: slug.to_owned(), followup })
```

There is no StateStore argument and no checkpoint deletion. An already-missing cache counts as successful cleanup. A subscription-save error returns before attempting cache deletion.

- [ ] **Step 5: Add preflight and identity tests.** Duplicate requested URL with a fragment/default-port variation returns AlreadySubscribed and adds no recorded request. Query differences remain distinct. Two requested URLs redirecting to one route are allowed as two subscriptions. Explicit collision fails before fetching; derived collision gets `-2`; same title later changes no existing slug. Subscribe→unsubscribe→subscribe yields a fresh ID while seeded old checkpoint bytes remain identical. Feed parse failure writes neither store. A permanently redirected malformed response does not create a subscription.
- [ ] **Step 6: Add deterministic commit-boundary failure injection without production hooks.** For subscribe, run the async call on a test thread with `Script::stall_headers()`. Wait on TestServer::wait_until_stalled to prove preflight completed; create a nonempty directory at the still-missing subscription **file** path, then release the response. Cache save succeeds, subscription rename fails, SubscribeOutcome carries SaveSubscription and the cache remains unreferenced. For unsubscribe, seed normally, replace the cache file with a directory containing a sentinel, then call unsubscribe; assert subscription removed, RemoveCache failure retained, sentinel untouched. A nonempty directory at the subscription file path before unsubscribe must fail before deleting cache. Keep all manipulated paths inside the Rig tempdir.

```rust
// Deterministic cache-deletion failure preparation, after reading any expected values:
let path = rig.cache.path_for(&sub.feed_id)?;
std::fs::remove_file(&path)?;
std::fs::create_dir(&path)?;
std::fs::write(path.join("sentinel"), b"keep")?;
let result = continuo::library::unsubscribe(&rig.subs, &rig.cache, &sub.slug)?;
assert!(matches!(result.followup.as_ref().map(|f| &f.step),
    Some(continuo::library::FollowupStep::RemoveCache)));
assert!(rig.subs.read_snapshot()?.subscriptions.is_empty());
assert_eq!(std::fs::read(path.join("sentinel"))?, b"keep");
```

- [ ] **Step 7: Run `cargo test --locked --test m4_library_mutations --test m4_library_reads --test m4_subscription_store`.** Every failed-load reason aborts the mutating command visibly; reads of orphan cache files are never attempted.
- [ ] **Step 8: Commit.**

```bash
git add src/library.rs tests/m4_library_mutations.rs
git commit -m "feat: manage podcast subscriptions with recoverable commits"
```

### Task 13: Refresh single feeds and batches without losing partial failures

**Files:** Modify `src/library.rs`; create `tests/m4_library_refresh.rs`.

**Interfaces:** Consumes all previous tasks. Produces spec §6.6 `RefreshOutcome` and exact public functions:

```rust
pub enum RefreshOutcome {
    Unchanged { slug: String, url_moved: Option<String>, followup: Option<FollowupFailure> },
    Updated { slug: String, retained: usize, skipped: usize,
        url_moved: Option<String>, followup: Option<FollowupFailure> },
    Failed { slug: String, error: FeedError },
}
pub async fn refresh(http: &HttpService, subs: &SubscriptionStore, cache: &CacheStore,
    slug: &str) -> Result<RefreshOutcome, FeedError>;
pub async fn refresh_all(http: &HttpService, subs: &SubscriptionStore, cache: &CacheStore)
    -> Result<Vec<RefreshOutcome>, FeedError>;
```

Private worker: `refresh_one(http: &HttpService, subs: &SubscriptionStore, cache: &CacheStore, snapshot: &mut SubscriptionSnapshot, index: usize) -> impl Future<Output = RefreshOutcome>`. A private async `refresh_work` with the same arguments returns `Result<RefreshOutcome, FeedError>`; refresh_one converts its precommit Err into Failed with the known slug. This avoids swallowing a per-feed failure or inventing a slug for an enumeration failure.

- [ ] **Step 1: Write a 304 timestamp/representation test.**

```rust
mod support;
#[path = "support/feeds.rs"] mod feeds;
#[test]
fn unchanged_preserves_representation_and_advances_check_time() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;
    use continuo::{http::{limits::Limits, service::HttpService}, library::{self, RefreshOutcome}};
    use support::server::{DocumentReply, Script, TestServer};
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::documents(vec![DocumentReply {
        path: "/feed".into(), status: 200, headers: vec![("ETag".into(), "\"v1\"".into())],
        body: br#"<rss><channel><title>Radio T</title><item><guid>id</guid></item></channel></rss>"#.to_vec(),
        conditional: true, header_delay: Duration::ZERO,
    }]));
    let service = HttpService::spawn(Limits::brisk())?;
    service.handle().block_on(library::subscribe(&service, &rig.subs, &rig.cache,
        &server.url("/feed"), Some("radio-t")))?;
    let sub = rig.subs.read_snapshot()?.subscriptions.remove(0);
    let before = rig.cache.read(&sub)?;
    let subs_bytes = std::fs::read(rig.subs.path())?;
    let subs_mtime = std::fs::metadata(rig.subs.path())?.modified()?;
    rig.clock.advance(Duration::from_secs(60));
    let outcome = service.handle().block_on(library::refresh(&service, &rig.subs, &rig.cache, "radio-t"))?;
    assert!(matches!(outcome, RefreshOutcome::Unchanged { followup: None, .. }));
    let after = rig.cache.read(&sub)?;
    assert_eq!(after.last_fetched_at, before.last_fetched_at);
    assert!(after.last_refreshed_at > before.last_refreshed_at);
    assert_eq!(serde_json::to_value(&after.episodes)?, serde_json::to_value(&before.episodes)?);
    assert_eq!(std::fs::read(rig.subs.path())?, subs_bytes);
    assert_eq!(std::fs::metadata(rig.subs.path())?.modified()?, subs_mtime);
    server.shutdown();
    Ok(())
}
```

- [ ] **Step 2: Run `cargo test --locked --test m4_library_refresh`; observe missing refresh API.**
- [ ] **Step 3: Implement cache-state selection and 200/304 processing.**

```rust
let old = match cache.read(&subscription) {
    Ok(feed) => Some(feed),
    Err(FeedError::CacheMissing { .. } | FeedError::CacheCorrupt { .. }
        | FeedError::CacheParserMismatch { .. }) => None,
    Err(error) => return Err(error),
};
let outcome = http.fetch_document(DocumentRequest {
    origin: subscription.fetch_url.clone(),
    validators: old.as_ref().map(|feed| feed.validators.clone()),
}).await?;
```

For 200, parse only with that response's final_url, bind using the **existing** FeedId, build fresh CachedFeed with both timestamps `subs.now()`. For 304, require old cache, preserve its episode/body-derived fields and last_fetched_at, set merged validators and last_refreshed_at. The document layer already rejects unsolicited 304, but the application must also reject one without a cache rather than fabricate CachedFeed. Cache save happens before any subscription update, and its failure returns Failed without touching durable subscription fields.

- [ ] **Step 4: Implement durable reconciliation, including 304 and batch consistency.** Prepare an updated Subscription with the cached title and permanent_url if any; leave slug, ID and added_at unchanged. If none of its fields changed, skip saving subscriptions entirely. If changed, clone the in-memory snapshot, replace only this record and attempt save. Update the shared batch snapshot only after save succeeds, so a later feed cannot silently commit an earlier feed's failed subscription update. `url_moved` is the redacted proposed permanent target; followup indicates whether recording it failed. Emit Unchanged or Updated only after the cache committed.

```rust
let followup = if changed {
    let mut candidate = snapshot.clone();
    candidate.subscriptions[index] = updated_subscription;
    match subs.save(&candidate) {
        Ok(()) => { *snapshot = candidate; None }
        Err(error) => Some(FollowupFailure { step: FollowupStep::SaveSubscription, error }),
    }
} else { None };
```

Derive Clone on SubscriptionSnapshot/Subscription. Name `changed` by comparing title and fetch_url only, not clock values. No lock, background task or retries on generic network failures.

- [ ] **Step 5: Implement outer versus per-feed errors.**

```rust
pub async fn refresh_all(http: &HttpService, subs: &SubscriptionStore, cache: &CacheStore)
    -> Result<Vec<RefreshOutcome>, FeedError>
{
    let mut snapshot = load_mutating(subs)?;
    let mut results = Vec::with_capacity(snapshot.subscriptions.len());
    for index in 0..snapshot.subscriptions.len() {
        results.push(refresh_one(http, subs, cache, &mut snapshot, index).await);
    }
    Ok(results)
}
```

Single refresh loads once, finds index or UnknownSlug, then returns its worker outcome. Batch enumeration failure returns outer Err; an empty valid snapshot returns an empty successful batch. Execute sequentially for deterministic outcomes and bounded resource use.

- [ ] **Step 6: Add recovery and failure matrices as separate red/green groups.** Seed cache without a matching parser version, corrupt bytes, and remove file; each refresh sends no conditionals and accepts a full 200. A server always answering 304 in those cases yields Failed and does not replace cache. For network/HTTP/parse/body-limit failures compare prior cache and subscription bytes exactly. A valid 200 increments both timestamps; an identical body still reports Updated because M4 promises fetch status, not content hashing. Title updates preserve slug. Permanent redirects preserve IDs and existing checkpoints; temporary redirects preserve fetch_url. Failed cache save does not move subscription URL.
- [ ] **Step 7: Test 304 permanent redirect followed by failed subscription save.** Seed a cached representation whose validators.url is B and whose stored fetch_url is A. Script A→301→B with B returning 304. Stall headers after the subscription snapshot is loaded; while stalled, rename subscriptions.json to a backup **inside the tempdir**, place a nonempty directory at its path, then release. Assert Unchanged contains `url_moved: Some(_)` and SaveSubscription followup, cache revalidation committed, and backup subscription still has A. Restore the test path and refresh again; assert the URL moves to B and title reconciliation succeeds. Add the same failure seam for a 200. Test batches with one valid feed, one HTTP failure, and one followup failure; all three outcomes exist in subscription order.
- [ ] **Step 8: Run `cargo test --locked --test m4_library_refresh --test m4_library_mutations --test m4_document_protocol`.**
- [ ] **Step 9: Commit.**

```bash
git add src/library.rs tests/m4_library_refresh.rs
git commit -m "feat: refresh subscribed feeds with explicit partial failures"
```

### Task 14: Wire commands, output and the existing playback handoff

**Files:** Create `src/commands.rs`, `tests/m4_cli.rs`; modify `src/cli.rs`, `src/app.rs`, `src/error.rs`, `src/lib.rs`. Add unit tests inside commands.rs for injected stores/writers and inside app.rs for handoff. No split of app.rs and no changes to resolve_source/run_probe_only/engine/session policy.

**Interfaces:** Consumes Task 11–13 library functions/outcomes. Produces the spec's five new CliCommand variants, Play's optional `index: Option<NonZeroUsize>`, and AppError. Command entry point: `pub fn run(command: CliCommand) -> Result<(), FeedError>` for the five feed commands; reject Play as unreachable-by-dispatch with a safe Malformed context if defensively called directly. Platform helpers: `pub(crate) fn platform_subscription_stores() -> Result<(SubscriptionStore, CacheStore), FeedError>` and `pub(crate) fn platform_state_store() -> Result<StateStore, FeedError>`. The first helper never opens checkpoints; no helper creates directories until a write.

Inside commands.rs define private formatting functions `duration_text(Duration) -> String`, `progress_text(&Progress, Option<Duration>) -> String`, `write_feeds(&mut dyn Write, &[FeedSummary]) -> Result<(), FeedError>`, `write_episodes(&mut dyn Write, &[EpisodeRow]) -> Result<(), FeedError>`, `write_refresh(&mut dyn Write, &RefreshOutcome) -> Result<(), FeedError>`, `finish_refresh_batch(&mut dyn Write, Vec<RefreshOutcome>) -> Result<(), FeedError>`, `finish_refresh_one(&mut dyn Write, RefreshOutcome) -> Result<(), FeedError>`, `finish_subscribe(&mut dyn Write, SubscribeOutcome) -> Result<(), FeedError>`, `finish_unsubscribe(&mut dyn Write, UnsubscribeOutcome) -> Result<(), FeedError>`. Errors writing stdout use PersistenceError::Io with path `<stdout>` and op `write command output to`; they never falsely report command success.

- [ ] **Step 1: Add failing CLI parse/arity tests.**

```rust
use clap::Parser;
use continuo::cli::{Cli, CliCommand};

#[test]
fn play_selectors_are_positive_and_single_source_still_parses() -> Result<(), Box<dyn std::error::Error>> {
    assert!(Cli::try_parse_from(["continuo", "play", "file.mp3"]).is_ok());
    assert!(Cli::try_parse_from(["continuo", "play", "radio-t", "3", "--probe-only"]).is_ok());
    assert!(Cli::try_parse_from(["continuo", "play", "radio-t", "0"]).is_err());
    assert!(Cli::try_parse_from(["continuo", "play", "radio-t", "newest"]).is_err());
    assert!(Cli::try_parse_from(["continuo", "episodes", "radio-t", "-n", "0"]).is_err());
    let parsed = Cli::try_parse_from(["continuo", "refresh"])?;
    assert!(matches!(parsed.command, CliCommand::Refresh { slug: None }));
    Ok(())
}
```

Add parse cases for subscribe --as, unsubscribe, feeds, episodes -n, refresh slug and three Play positionals.

- [ ] **Step 2: Run `cargo test --locked --test m4_cli`; observe the new-variant/missing-argument failures.**
- [ ] **Step 3: Define CLI variants and exact positive parsers.**

```rust
// Add variants to CliCommand; keep Play.source and probe_only names:
Play {
    source: String,
    #[arg(value_parser = positive_index)]
    index: Option<std::num::NonZeroUsize>,
    #[arg(long)]
    probe_only: bool,
},
Subscribe { url: String, #[arg(long = "as")] slug: Option<String> },
Unsubscribe { slug: String },
Feeds,
Refresh { slug: Option<String> },
Episodes { slug: String, #[arg(short = 'n', value_parser = positive_count)] limit: Option<std::num::NonZeroUsize> },

// Complete parser bodies in cli.rs:
fn positive_index(value: &str) -> Result<std::num::NonZeroUsize, String> {
    value.parse::<usize>().ok().and_then(std::num::NonZeroUsize::new)
        .ok_or_else(|| "expected a positive episode index: play <path-or-url> or play <slug> <index>".into())
}
fn positive_count(value: &str) -> Result<std::num::NonZeroUsize, String> {
    value.parse::<usize>().ok().and_then(std::num::NonZeroUsize::new)
        .ok_or_else(|| "expected a positive count".into())
}
```

New enum variants make the old irrefutable app destructuring stop compiling, so implement the minimal dispatch in the same red/green group; do not weaken tests or introduce temporary panicking arms.

- [ ] **Step 4: Implement AppError and preserve the playback body.**

```rust
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)] Playback(#[from] crate::playback::error::PlaybackError),
    #[error(transparent)] Feed(#[from] crate::feed::error::FeedError),
}
```

Move the existing app::run body from the comment "Persistence opens before the engine" through its final finish call into private `run_resolved(media: MediaId, location: SourceLocation) -> Result<(), PlaybackError>` in the same file, byte-for-byte apart from the function boundary. app::run becomes this dispatch:

```rust
pub fn run(cli: cli::Cli) -> Result<(), crate::error::AppError> {
    match cli.command {
        CliCommand::Play { source, index: None, probe_only } => {
            if probe_only { return run_probe_only(&source).map_err(Into::into); }
            let (media, location) = resolve_source(&source)?;
            run_resolved(media, location).map_err(Into::into)
        }
        CliCommand::Play { source: slug, index: Some(index), probe_only } => {
            let (subs, cache) = crate::commands::platform_subscription_stores()?;
            let (media, location) = crate::library::resolve_episode(&subs, &cache, &slug, index.get())?;
            if probe_only {
                if let SourceLocation::Http(url) = &location {
                    return run_probe_only(url.as_str()).map_err(Into::into);
                }
                return Err(crate::feed::error::FeedError::Malformed {
                    detail: "podcast cache contained a non-HTTP source".into(),
                }.into());
            }
            run_resolved(media, location).map_err(Into::into)
        }
        command => crate::commands::run(command).map_err(Into::into),
    }
}
```

Single-source probe takes exactly its existing path, including no state store. Episode probe resolves first and only then probes its enclosure; the probe's internal RemoteUrl is never persisted. Normal episode playback receives the podcast media ID unchanged. Verify `resume_commands`, `open_persistence`, both loops and `finish` stayed behavior-identical.

- [ ] **Step 5: Implement platform resources and synchronous bridge.** Construct ProjectDirs only inside commands.rs. Data dir supplies subscriptions.json, cache_dir supplies feeds, existing StateStore::platform_path supplies checkpoints. Constructors create no files. Create `Arc<SystemClock>` for stores. Only subscribe/refresh create HttpService; listings/unsubscribe do not. For Episodes call StateStore::read_snapshot before list_episodes, surfacing any error. Use one generic bridge helper rather than duplicated runtime entry logic:

```rust
fn wait_http<F: std::future::Future>(service: &HttpService, future: F) -> F::Output {
    service.handle().block_on(future)
}
```

Each command invokes its library function, then its formatter/finalizer. Network operations pass `&service` into the async function before the bridge; do not call `block_on` from library.rs or enter a runtime in run_resolved's decoder path.

- [ ] **Step 6: Add pure formatter tests, then implement output.**

```rust
fn duration_text(value: std::time::Duration) -> String {
    let secs = value.as_secs();
    if secs >= 3600 { format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60) }
    else { format!("{}:{:02}", secs / 60, secs % 60) }
}
fn progress_text(progress: &Progress, declared: Option<std::time::Duration>) -> String {
    let position = match progress {
        Progress::None => return "—".into(),
        Progress::Played => return "played".into(),
        Progress::Unknown => return "position unknown".into(),
        Progress::Established(value) => duration_text(*value),
        Progress::Estimated(value) => format!("~{}", duration_text(*value)),
    };
    match declared {
        Some(value) => format!("{position} / ({})", duration_text(value)),
        None => position,
    }
}
```

Assert `23:14 / (1:42:00)`, `~18:02 / (1:39:00)`, `played`, `position unknown`, `—`, and positions over one hour. Print the exact §6.1 column labels and UTC dates; absent published date is `—`, absent cache count is `—` with `never`. Format times from OffsetDateTime converted to UtcOffset::UTC, using numeric year/month/day/hour/minute so no new time feature is needed. A missing title prints `(untitled)`. Replace line breaks/control characters in displayed titles with visible escapes or spaces at the formatting boundary without changing cached titles/identity. Do not transliterate titles.

- [ ] **Step 7: Implement partial-failure printing and nonzero propagation.**

```rust
fn finish_refresh_batch(out: &mut dyn std::io::Write, outcomes: Vec<RefreshOutcome>)
    -> Result<(), FeedError>
{
    let total = outcomes.len();
    let mut failed = 0;
    for outcome in outcomes {
        let bad = match &outcome {
            RefreshOutcome::Failed { .. } => true,
            RefreshOutcome::Updated { followup, .. } | RefreshOutcome::Unchanged { followup, .. } => followup.is_some(),
        };
        write_refresh(out, &outcome)?;
        failed += usize::from(bad);
    }
    if failed == 0 { Ok(()) } else { Err(FeedError::BatchIncomplete { failed, total }) }
}
```

`write_refresh` prints slug and updated/unchanged/failure, retained/skipped counts for Updated, and the committed-work message plus followup cause where present. It redacts url_moved before presentation even though the library already supplied safe text. A followup SaveSubscription message mentions a redirect when url_moved exists, otherwise changed subscription metadata/title. `finish_refresh_one`, `finish_subscribe` and `finish_unsubscribe` print committed work first, then consume and return the concrete followup/Failed error. Successful subscribe prints slug and counts. Add injected Vec<u8> writer tests for all outcomes, including 304+followup and mixed batch. main.rs's existing Result-to-ExitCode conversion then makes every failure nonzero.

- [ ] **Step 8: Add process tests with temporary XDG directories.**

```rust
fn run_cli(root: &std::path::Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new(env!("CARGO_BIN_EXE_continuo"))
        .args(args)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("RUST_LOG", "continuo=warn")
        .output()
}
```

On Linux assert empty feeds succeeds without creating directories; subscribe to loopback then shut the server down and run episodes/feeds successfully; missing/corrupt/parser-mismatched cache follows exact exit policy without mutation. Bad state must not print unplayed rows. Test index 0, noninteger, out of range, `-n 0`, missing slug and unplayable selection/probe. For cross-platform tests prefer injecting platform-independent stores into formatter tests; gate Linux XDG process assertions with cfg(target_os = "linux").

- [ ] **Step 9: Run `cargo test --locked --test m4_cli --test cli --test cli_playback --test http_cli` and `cargo test --locked --lib`.** Existing one-positional tests must continue unchanged. Verify new errors stay readable even when no audio device or TTY exists.
- [ ] **Step 10: Commit.**

```bash
git add src/cli.rs src/app.rs src/error.rs src/lib.rs src/commands.rs tests/m4_cli.rs
git commit -m "feat: expose feed commands and podcast episode playback"
```

### Task 15: Prove playback identity, diagnostic safety and user contracts

**Files:** Create `tests/m4_playback_identity.rs`, `tests/m4_diagnostics.rs`; extend `tests/m4_cli.rs` and command/app unit tests; modify `README.md`, `docs/architecture.md`, `tests/fixtures/README.md`, `tests/fixtures/feeds/README.md`.

**Interfaces:** Consumes Task 14's completed command interface, existing TestEngine, PlaybackCommand, Session::observe/reconcile_shutdown and StateStore::write. Produces acceptance evidence and documentation; no new public production API or playback policy.

- [ ] **Step 1: Add a resolved-podcast-to-checkpoint integration test.** Drive the existing virtual audio output; do not require a CPAL device or make an engine change to enable this test.

```rust
mod support;
#[path = "support/feeds.rs"] mod feeds;

#[test]
fn playback_persists_the_podcast_id_not_the_enclosure_url() -> Result<(), Box<dyn std::error::Error>> {
    use continuo::{clock::Clock, http::{limits::Limits, service::HttpService}, library,
        media::id::{MediaId, NormalizedUrl}, persistence::model::PersistedState,
        playback::{command::{PlaybackCommand, ResumeIntent}, state::PlaybackState}, session::Session};
    use support::server::{Script, TestServer};
    let rig = feeds::Rig::new()?;
    let server = TestServer::start(Script::from_fixture("sine-5s.flac"));
    let enclosure = server.url("/audio.flac");
    let xml = format!("<rss><channel><item><guid>episode-1</guid><enclosure url=\"{enclosure}\"/></item></channel></rss>");
    rig.seed(xml.as_bytes(), "https://example.org/feed")?;
    let (media, source) = library::resolve_episode(&rig.subs, &rig.cache, "radio-t", 1)?;
    let mut engine = support::TestEngine::start_idle();
    engine.handle().set_http(Some(HttpService::spawn(Limits::brisk())?));
    engine.send(PlaybackCommand::Load { media: media.clone(), source,
        resume: ResumeIntent::StartAt(std::time::Duration::ZERO) });
    engine.await_state(PlaybackState::Paused);
    let mut session = Session::new(PersistedState::default());
    while let Some(event) = engine.try_event() { let _ = session.observe(&event, rig.clock.sample()); }
    engine.send(PlaybackCommand::Play);
    engine.await_state(PlaybackState::Playing);
    engine.play_for(std::time::Duration::from_secs(1));
    while let Some(event) = engine.try_event() { let _ = session.observe(&event, rig.clock.sample()); }
    let _ = session.tick(&engine.progress(), rig.clock.sample());
    let report = engine.shutdown_report().ok_or("engine already joined")?;
    rig.state.write(&session.reconcile_shutdown(&report, rig.clock.sample()))?;
    let snapshot = rig.state.read_snapshot()?;
    assert!(snapshot.entry_for(&media).is_some());
    let remote = MediaId::RemoteUrl(NormalizedUrl::parse(&enclosure)?);
    assert!(snapshot.entry_for(&remote).is_none());
    server.shutdown();
    Ok(())
}
```

The existing TestEngine's event inbox retains Loaded for observation; if an await helper consumes it, explicitly pass that returned event to Session rather than fabricating one. Add an app.rs unit assertion around `resume_commands(media.clone(), location, ...)` using a podcast pair to prove the actual handoff preserves that ID. No test-only branch in the production engine.

- [ ] **Step 2: Run the new identity test before making any required wiring correction.** `cargo test --locked --test m4_playback_identity`. A failing assertion must lead to a handoff/binding fix, never a change in checkpoint identity policy or an amended expected RemoteUrl key. If it already passes, retain it as the acceptance test without changing working code to manufacture a red phase.
- [ ] **Step 3: Add CLI episode-probe and partial-exit acceptance tests.** Seed a cache pointing to the loopback FLAC server, invoke `play radio-t 1 --probe-only`, assert the existing sample-rate/capability output and no state file. For absent enclosure assert NotPlayable, no recorded media request, and no state. For unsubscribe cleanup failure run the process against a directory at the cache-file path: assert committed unsubscribe text and nonzero status. For subscribe/refresh-save failure use the header stall/path replacement technique from Tasks 12–13 around a spawned child process. Bound child wait and always release the server; do not use permission bits as the only failure mechanism. Assert batch output includes each slug and BatchIncomplete count.
- [ ] **Step 4: Audit diagnostics through realistic construction paths and both renderers.** Use signed URL `https://user:secret@example.org/feed?token=SECRETVALUE`, a malformed URL containing the same marker, and cache JSON with a failing canonical media ID containing the marker. Exercise input rejection, redirect Location failure, transport failure on loopback, cache/state deserialization and wrapped persistence errors. Test the exact errors returned by those operations, not hand-built errors containing already-clean dummy strings.

```rust
fn assert_no_transport_secret(error: &(impl std::fmt::Display + std::fmt::Debug)) {
    for text in [format!("{error}"), format!("{error:?}")] {
        assert!(!text.contains("SECRETVALUE"), "query leaked: {text}");
        assert!(!text.contains("user:secret"), "credentials leaked: {text}");
    }
}
```

For each FeedError variant, document which constructor supplies safe context. Test nested Remote/Persistence sources as well as top-level Display; do not blindly preserve a raw serde DomainError message. Titles and explicit aliases are user-visible content, so keep their formatting policy distinct from transport URL secrecy. Check tracing output at `RUST_LOG=continuo=debug`; never log a full ParsedItem, CachedFeed, request or validator record.

- [ ] **Step 5: Add README command examples with the actual implemented contract.**

```text
continuo subscribe https://radio-t.com/rss/ --as radio-t
continuo feeds
continuo episodes radio-t -n 5
continuo play radio-t 3
continuo play radio-t 3 --probe-only
continuo refresh radio-t
continuo refresh
continuo unsubscribe radio-t
```

Document explicit refresh/offline listings, feed-order indices and renumbering only after cache replacement, one-positional compatibility, `--as`/ASCII host fallback and suffixes, unknown/unplayed/played/estimated progress and advisory duration, no-audio column, UTC, cache recovery messages, partial-success nonzero exits, single-process limitation, orphan checkpoints after resubscribe, distinct direct-URL/podcast identities, XML/Atom/HTML-title limitations, RSS1 refusal, data/cache paths and no offline audio. Do not recommend editing state.json to mark progress or deleting a shared directory to reset feeds.

- [ ] **Step 6: Update architecture/fixture documentation in the same acceptance deliverable.** Add the feed application seam and document fetch alongside streaming media; state that the domain resolver/playback policy are reused. Explain atomic cache-plus-validator consistency and why cross-file commit order still permits reported partial success. Clarify that library async operations perform synchronous filesystem work, which M5 must schedule away from its event loop. List every fixture name, byte construction for encodings, expected parsing result and the accepted declaration-handling bounds established in Task 7. Keep README examples consistent with schema-validated IDs/slugs.
- [ ] **Step 7: Run focused acceptance tests and inspect the final implementation diff.**

```bash
cargo test --locked --test m4_playback_identity --test m4_diagnostics --test m4_cli
git diff --check
git diff --stat
```

Verify `EpisodeKey::resolve`, `resolve_source`, `accept_redirect`, engine/session/resume/checkpoint code and original M1–M3 tests were not changed to make new tests pass. Wiring moves in app.rs are the only playback-adjacent implementation edits allowed by this plan.

- [ ] **Step 8: Commit.**

```bash
git add tests/m4_playback_identity.rs tests/m4_diagnostics.rs tests/m4_cli.rs src/commands.rs src/app.rs README.md docs/architecture.md tests/fixtures/README.md tests/fixtures/feeds/README.md
git commit -m "test: verify and document the podcast command workflow"
```

## Final verification gate

- [ ] Run `cargo fmt --check`.
- [ ] Run `cargo clippy --locked --all-targets --all-features -- -D warnings`.
- [ ] Run `cargo test --locked` once after all changes. The device smoke test remains optional/ignored according to its existing configuration; do not silently exclude a failing required suite.
- [ ] Inspect `cargo tree -i quick-xml` and `cargo tree -i getrandom@0.4.3` (or the exact 0.4 version resolved in the lock) and verify only the two approved new direct dependencies were introduced. Existing URL serde feature activation is recorded explicitly.
- [ ] Run `git diff --check`; inspect `git status --short` and the commit series. Do not commit unrelated workspace changes.
- [ ] Report exact validation results, any environment-limited checks and remaining debt. Do not report implementation complete solely because the plan's checkboxes were checked. No merge/push is part of this plan.

## Spec coverage map

| Spec requirements | Tasks and evidence |
|---|---|
| §1.1 commands and arity; §1.2 ordering | 11, 14, 15: value ordering, CLI parse and process assertions |
| §1.3 limited changes; §1.4 dependencies; §1.5 scope | Global Constraints; 1–3, 5, 7, 14; final diff/dependency gate |
| §1.6 dual identities and resubscribe orphaning | 12, 15: old checkpoint bytes survive; podcast identity persisted; README |
| §2.1–2.2 parse/domain split and advisory duration | 7–11: ParsedItem/BoundItem/Episode/CachedEpisode; progress formatter in 14 |
| §2.3 random IDs; §2.4 resolution and no synthesis | 3, 9: validation, unchanged resolver and fallback/dedup tests |
| §2.5 aliases | 3, 12, 14: ASCII/Cyrillic/collision tests and `--as` |
| §3.1 async interface; §3.2 redirects | 5–6: service wrapper, mixed chains, scoped request recording |
| §3.3 conditionals, 304 metadata, unusable cache | 6, 10, 13: conditions, merging, unconditional recovery and timestamps |
| §3.4 bounds and deadlines; §3.5 headers/status | 5–6: limit with/without length, phase timing, encoding/status tests |
| §4.1 strict decoding and declarations | 7: known/unknown/missing/malformed encodings, UTF-16/BOM/Latin-1 |
| §4.2–4.4 format/mapping/base | 7–8: RSS/Atom URI-aware walker, nested-base fixtures |
| §4.5–4.8 rejection, duplicates, entities, titles | 8–9: exact field and retained/skipped assertions |
| §5.1 recovering vs read-only subscriptions | 4, 11–13: LoadReason matrix and non-mutating read tests |
| §5.2 cache contents and §5.3 commit order | 10, 12–13: versioned DTOs, actual failed filesystem steps |
| §5.4 recovery; §5.5 checkpoint reads | 2, 10–11, 13: no mutations and unconditional parser-version refresh |
| §5.6 validation; §5.7 concurrency | 3–4, 10, 15: foreign/traversal/duplicate IDs and documented single-process scope |
| §6.1–6.4 output/progress/arguments/exits | 11, 14–15: pure precedence and formatting; process statuses |
| §6.5 playback handoff; §6.6 application interface | 11–15: exact library contracts, shared body and real checkpoint evidence |
| §6.7 AlreadySubscribed | 12: normalized requested URL preflight and allowed redirected duplicates |
| §7 modules/errors/redaction | File map; 3, 14–15: AppError and Display/Debug audit |
| §8.1 fixtures; §8.2 protocol; §8.3 stores | 4–10, 12–13: explicit fixture/protocol/commit-boundary matrices |
| §8.4 application and §8.5 playback/diagnostics | 11–15: reads/partial results/progress/checkpoint/secret tests |
| §9 rejected alternatives | Global Constraints; 15 docs keep the accepted limitations visible |

## Planning self-review and handoff

Before handing this document to an executor, check that every named producer precedes its consumers, every public signature agrees with §6.6, every new test fixture has an expected result, and all required spec rows map to a task above. Planning checks validate this document, not the future implementation. The source and tests have not been implemented by writing this file.

Execution choices after user selection: **Subagent-Driven** uses a fresh implementer per task with review between tasks; **Inline Execution** uses executing-plans with checkpoints. Both start by reading the corrected spec and this plan, inspecting the current workspace, and applying the appropriate isolation workflow. Neither choice authorizes a merge or unrelated refactoring.
