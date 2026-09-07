# Continuo Foundation (Milestone 0) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver the approved M0 foundation: validated media identities, independent source capabilities, checkpoint value types, tracing, documented architectural contracts, and CI.

**Architecture:** A library exposes small domain modules and a thin binary initializes tracing and reports startup errors. Validating newtypes make invalid identities unrepresentable; checkpoints have no dependency on source capabilities. Playback, networking, and storage remain documented contracts for later milestones.

**Tech Stack:** Rust 1.98.1, edition 2024; thiserror 2, serde 1, tracing 0.1, tracing-subscriber 0.3, url 2, percent-encoding 2, time 0.3; serde_json 1 for tests.

**Spec:** `docs/superpowers/specs/2026-09-07-continuo-foundation-design.md` (approved; read alongside this plan).

## Global Constraints

- `rust-toolchain.toml` pinning Rust **1.98.1** with `rustfmt` and `clippy`.
- `Cargo.lock` committed; Continuo is an application.
- `thiserror`, `serde` (derive), `tracing`, `tracing-subscriber` (env-filter), `url`, `percent-encoding`, `time` (serde + RFC 3339).
- Dev-dependencies: `serde_json`.
- No `clap` — argument parsing arrives with the M1 CLI. No `anyhow` — `thiserror` throughout, to keep error modeling honest rather than stringly-typed.
- `unsafe_code` is forbidden crate-wide. `clippy::unwrap_used` and `clippy::expect_used` are denied, with `clippy.toml` setting `allow-unwrap-in-tests` and `allow-expect-in-tests`.
- Clippy’s test exemptions cover `#[test]` functions and `#[cfg(test)]` modules, not bare integration-test helpers. Scope any necessary `#[allow(clippy::unwrap_used)]` to the fixture helper; never relax runtime lints.
- No `audio/`, `http/`, `podcast/`, `tui/`, or `persistence/` directories are created in M0.
- No filesystem I/O ships in M0. Here this means no application source-opening or persistence I/O; stderr tracing and build tooling remain necessary.
- No `libasound2-dev` yet — M0 has no audio dependency.
- No matrix, coverage service, or release automation.
- HTTP is a transport. It establishes neither continuity nor seekability on its own.
- GUIDs are opaque. They are never parsed, normalized, or interpreted — only escaped and compared byte-for-byte.
- Identity normalization is never applied to the fetch URL.
- A checkpoint is never discarded because `ResumeCapability` is `Unsupported` or `Undetermined`.
- No merge algorithm exists, in M0 or later.
- No near-end reset in M0.

---

## Starting point and implementation decisions

The repository currently has `Cargo.toml` (package `continuo`, edition 2024, no dependencies), a minimal committed `Cargo.lock`, `.gitignore`, and `src/main.rs` printing “Hello, world!”. There are no existing domain modules or tests. The local default compiler reports Rust 1.98.1, but the named pinned toolchain may still need installation at execution time. Do not silently change the required version if rustup cannot retrieve it.

This is one plan because the approved scope is one small foundation library. The independent runtime subsystems belong to M1–M5 and must receive their own plans then.

Resolve the spec's provisional shapes as follows:

- `FeedId` wraps a nonempty opaque string, supplied by the future subscription layer. M0 neither derives it from a mutable fetch URL nor generates subscription IDs. It has no mutator.
- `EpisodeKey` internally distinguishes `Guid(String)` from `Url(NormalizedUrl)` so a literal GUID equal to an enclosure URL cannot collide with URL fallback identity. Enclosure and item-link fallback use the same URL namespace. Empty GUIDs count as absent; nonempty GUIDs, including whitespace, are preserved exactly.
- Canonical identities are `local:<escaped-path>`, `remote:<escaped-url>`, or `podcast:<escaped-feed>/<escaped-episode-key>`. The episode key before outer escaping is `guid:<opaque-guid>` or `url:<normalized-url>`. Encoding leaves ordinary ASCII punctuation readable; non-ASCII UTF-8 bytes are percent-encoded by `percent-encoding` and round-trip without loss. All components escape controls, spaces, double quotes, backslashes, and `%`; podcast components additionally escape `/`, their internal separator. Local and remote bodies extend to the end of the string and need no slash escaping. Percent escaping prevents ambiguities with literal escape-looking text. Parsing rejects noncanonical encodings by parsing then requiring `parsed.to_string() == input`.
- URL identities use the `url` parser's scheme/host/default-port normalization, explicitly remove fragments, and do not rewrite queries. They require HTTP(S) with a host, matching this player's source scope. Fetch sources retain the separate parsed `Url`, including its fragment and untouched query serialization.
- Absolute paths are validated without I/O. Inspect the original UTF-8 spelling for `.` and `..` segments **before** relying on `Path::components()`, which can hide interior `.`. Reject repeated and trailing separators (except filesystem roots) rather than assigning distinct IDs to these spellings. Respect platform prefixes and separators; reject non-UTF-8 on every platform, with an executable Unix-specific test. Store the validated UTF-8 `String` as the single source of truth and derive `&Path` from it, so formatting has no fallible UTF-8 conversion.
- `Episode` is a small value in `media/mod.rs` containing `id` and optional `source`. No feed parser is introduced. Metadata is `title: Option<String>` and `duration: Option<Duration>`.
- Domain identity serialization is explicit string serde. A checkpoint derives serde with `time::serde::rfc3339` for its human-readable timestamp; no snapshot schema or storage API is introduced yet. `Duration` uses serde's standard structured representation; choosing a persisted snapshot format remains M2 work.

## File map and ownership

| File | Responsibility |
|---|---|
| `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `clippy.toml` | Dependency resolution, compiler pin, lint policy |
| `src/lib.rs` | Library module declarations |
| `src/error.rs` | Contextual validation errors and telemetry initialization errors |
| `src/media/id.rs` | Validating identity newtypes, canonical parsing/formatting, string serde |
| `src/media/capabilities.rs` | Capability enums and derived resume matrix |
| `src/media/source.rs` | Physical source locations, independent of semantics |
| `src/media/metadata.rs` | Title and optional duration |
| `src/media/mod.rs` | Media exports and episode value |
| `src/playback/checkpoint.rs`, `src/playback/mod.rs` | Checkpoint value and export |
| `src/telemetry.rs`, `src/main.rs` | Tracing initialization and application error boundary |
| `tests/capabilities.rs` | Complete capability matrix |
| `tests/identity_values.rs` | Path/URL validation and episode-key priority |
| `tests/media_id.rs` | Adversarial canonical encoding and JSON map keys |
| `tests/domain_values.rs` | Fetch URL preservation, unplayable episodes, checkpoint serde |
| `tests/telemetry.rs`, `tests/cli.rs` | Subscriber validation and process-level startup behavior |
| `docs/architecture.md`, `README.md` | Binding contracts, scope, and contributor instructions |
| `.github/workflows/ci.yml` | Required single-platform checks |

Each task ends in an independently testable deliverable. Add the named files only when their task is reached. Commands run at the repository root. Commit steps are local integration checkpoints during implementation, not part of writing this plan. Do not include unrelated work in commits.

### Task 1: Establish the library and implement the capability matrix

**Files:** Modify `Cargo.toml`, `Cargo.lock`; create `rust-toolchain.toml`, `clippy.toml`, `src/lib.rs`, `src/media/mod.rs`, `src/media/capabilities.rs`, `tests/capabilities.rs`.

**Interfaces:** Consumes no domain APIs. Produces `continuo::media::capabilities::{Continuity, SeekSupport, ResumeCapability, MediaCapabilities}` and `MediaCapabilities::resume_capability(&self) -> ResumeCapability`.

- [ ] **Step 1: Establish compiler and dependency configuration.** Preserve existing package identity and `.gitignore`. Set the following manifest and create the two configuration files; these are prerequisites for the domain test, not a separate scaffold task.

```toml
# Cargo.toml
[package]
name = "continuo"
version = "0.1.0"
edition = "2024"
rust-version = "1.98.1"

[dependencies]
thiserror = "2"
serde = { version = "1", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
url = "2"
percent-encoding = "2"
time = { version = "0.3", features = ["serde", "serde-well-known"] }

[dev-dependencies]
serde_json = "1"

[lints.rust]
unsafe_code = "forbid"

[lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
```

```toml
# rust-toolchain.toml
[toolchain]
channel = "1.98.1"
profile = "minimal"
components = ["rustfmt", "clippy"]
```

```toml
# clippy.toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
```

Run `cargo generate-lockfile`, then `cargo --version` and `rustc --version`. Expected: lockfile resolves only the stated direct dependencies and compiler version is 1.98.1. Use the environment's approval mechanism if downloads require network permission; never claim a check passed without its output.

- [ ] **Step 2: Write the matrix regression test.** Create `tests/capabilities.rs`:

```rust
use continuo::media::capabilities::{Continuity as C, MediaCapabilities, ResumeCapability as R, SeekSupport as S};

#[test]
fn resume_capability_covers_every_pair() {
    let seeks = [S::Unknown, S::Native, S::RestartAndDiscard, S::Unsupported];
    let rows = [
        (C::Unresolved, [R::Undetermined; 4]),
        (C::Indefinite, [R::Unsupported; 4]),
        (C::Finite, [R::Undetermined, R::Supported, R::Supported, R::Unsupported]),
    ];
    for (continuity, expected) in rows {
        for (seek, expected) in seeks.into_iter().zip(expected) {
            let capabilities = MediaCapabilities { continuity, seek };
            assert_eq!(capabilities.resume_capability(), expected, "{continuity:?}/{seek:?}");
        }
    }
}
```

- [ ] **Step 3: Run the failing test.** Run `cargo test --locked --test capabilities`. Expected: unresolved `continuo` library or capability module, rather than dependency/toolchain failure.

- [ ] **Step 4: Implement the library and derived capability method.** `src/lib.rs` initially contains `pub mod media;`; `src/media/mod.rs` initially contains `pub mod capabilities;`. Create `src/media/capabilities.rs`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Continuity { Unresolved, Finite, Indefinite }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekSupport { Unknown, Native, RestartAndDiscard, Unsupported }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeCapability { Supported, Unsupported, Undetermined }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaCapabilities {
    pub continuity: Continuity,
    pub seek: SeekSupport,
}
impl MediaCapabilities {
    pub fn resume_capability(&self) -> ResumeCapability {
        match (self.continuity, self.seek) {
            (Continuity::Indefinite, _) => ResumeCapability::Unsupported,
            (Continuity::Unresolved, _) | (Continuity::Finite, SeekSupport::Unknown) => ResumeCapability::Undetermined,
            (Continuity::Finite, SeekSupport::Unsupported) => ResumeCapability::Unsupported,
            (Continuity::Finite, SeekSupport::Native | SeekSupport::RestartAndDiscard) => ResumeCapability::Supported,
        }
    }
}
```

- [ ] **Step 5: Verify and commit.** Run `cargo fmt`, `cargo test --locked --test capabilities`, and `cargo clippy --locked --all-targets --all-features -- -D warnings`. Expected: all pass. Commit the task's files with `git commit -m "feat: establish foundation and source capability model"` after explicitly staging the listed files.

### Task 2: Validate identity components and resolve episode identity

**Files:** Create `src/error.rs`, `src/media/id.rs`, `tests/identity_values.rs`; modify `src/lib.rs`, `src/media/mod.rs`.

**Interfaces:** Produces `DomainError`; `AbsolutePath::new(PathBuf) -> Result<Self, DomainError>`, `as_path(&self) -> &Path`, `as_str(&self) -> &str`; `NormalizedUrl::parse(&str) -> Result<Self, DomainError>`, `as_str(&self) -> &str`; `FeedId::new(String) -> Result<Self, DomainError>`, `as_str(&self) -> &str`; `EpisodeKey::resolve(Option<&str>, Option<&Url>, Option<&Url>) -> Result<Self, DomainError>`. Newtypes derive `Clone, Debug, Eq, PartialEq, Hash`; their fields stay private.

- [ ] **Step 1: Write validation and priority tests.** Create `tests/identity_values.rs`:

```rust
use continuo::media::id::{AbsolutePath, EpisodeKey, FeedId, NormalizedUrl};
use std::path::PathBuf;
use url::Url;

#[test]
fn paths_validate_original_spelling_without_io() {
    let root = if cfg!(windows) { "C:/" } else { "/" };
    assert!(AbsolutePath::new(PathBuf::from(format!("{root}does-not-exist/episode.mp3"))).is_ok());
    for suffix in ["a/./episode.mp3", "a/../episode.mp3", "a/link/../episode.mp3", "a/.", "a/..", "a//episode.mp3", "a/episode.mp3/"] {
        assert!(AbsolutePath::new(PathBuf::from(format!("{root}{suffix}"))).is_err(), "{suffix}");
    }
    assert!(AbsolutePath::new(PathBuf::from(root)).is_ok());
    assert!(AbsolutePath::new(PathBuf::from(format!("{root}/episode.mp3"))).is_err());
    assert!(AbsolutePath::new(PathBuf::from("episode.mp3")).is_err());
}

#[cfg(unix)]
#[test]
fn rejects_non_utf8_path() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    assert!(AbsolutePath::new(PathBuf::from(OsString::from_vec(b"/audio/\xff.mp3".to_vec()))).is_err());
}

#[test]
fn identity_url_normalizes_only_identity_fields() {
    let url = NormalizedUrl::parse("HTTPS://EXAMPLE.COM:443/a?b=2&a=%2f&a=%2F#part").unwrap();
    assert_eq!(url.as_str(), "https://example.com/a?b=2&a=%2f&a=%2F");
    assert!(NormalizedUrl::parse("not a url").is_err());
    assert!(NormalizedUrl::parse("file:///audio.mp3").is_err());
    assert!(FeedId::new(String::new()).is_err());
}

#[test]
fn episode_priority_and_opaque_guids() {
    let enclosure = Url::parse("https://example.com/audio.mp3#part").unwrap();
    let link = Url::parse("https://example.com/item").unwrap();
    let guid = " #/:? opaque GUID ";
    assert_eq!(EpisodeKey::resolve(Some(guid), Some(&enclosure), Some(&link)).unwrap(), EpisodeKey::resolve(Some(guid), None, None).unwrap());
    assert_eq!(EpisodeKey::resolve(None, Some(&enclosure), Some(&link)).unwrap(), EpisodeKey::resolve(None, Some(&enclosure), None).unwrap());
    assert_eq!(EpisodeKey::resolve(Some(""), None, Some(&link)).unwrap(), EpisodeKey::resolve(None, Some(&link), None).unwrap());
    assert_ne!(EpisodeKey::resolve(Some(guid), None, None).unwrap(), EpisodeKey::resolve(Some(guid.trim()), None, None).unwrap());
    assert_ne!(EpisodeKey::resolve(Some(enclosure.as_str()), None, None).unwrap(), EpisodeKey::resolve(None, Some(&enclosure), None).unwrap());
    assert!(EpisodeKey::resolve(None, None, None).is_err());
}
```

- [ ] **Step 2: Verify the tests fail for missing APIs.** Run `cargo test --locked --test identity_values`.

- [ ] **Step 3: Add contextual errors.** Add `pub mod error;` to the library and `pub mod id;` to media. Implement `src/error.rs`:

```rust
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("cannot identify path {path:?}: {reason}")]
    InvalidPath { path: PathBuf, reason: &'static str },
    #[error("cannot normalize identity URL {input:?}: {source}")]
    InvalidUrl { input: String, #[source] source: url::ParseError },
    #[error("cannot normalize identity URL {input:?}: expected HTTP(S) with a host")]
    UnsupportedUrl { input: String },
    #[error("cannot construct feed identity: empty identifier")]
    EmptyFeedId,
    #[error("cannot resolve episode identity: no GUID, enclosure URL, or item link")]
    MissingEpisodeIdentity,
    #[error("cannot parse media identity {input:?}: {reason}")]
    InvalidMediaId { input: String, reason: &'static str },
}
```

The operation and offending path/URL/serialized media identity are present where available. Do not invent decoder/device variants for future operations.

- [ ] **Step 4: Implement path and URL validation.** Start `src/media/id.rs` with:

```rust
use crate::error::DomainError;
use std::path::{Component, Path, PathBuf};
use url::Url;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct AbsolutePath(String);
impl AbsolutePath {
    pub fn new(path: PathBuf) -> Result<Self, DomainError> {
        let invalid = |reason| DomainError::InvalidPath { path: path.clone(), reason };
        let raw = path.to_str().ok_or_else(|| invalid("non-UTF-8 paths are unsupported"))?;
        if !path.is_absolute() { return Err(invalid("path must be absolute")); }
        let prefix_len = match path.components().next() {
            Some(Component::Prefix(prefix)) => prefix.as_os_str().len(),
            _ => 0,
        };
        // Absolute paths have a root separator after any Windows prefix.
        let rooted = &raw[prefix_len..];
        let tail = rooted.strip_prefix(std::path::is_separator)
            .ok_or_else(|| invalid("path must include a root separator"))?;
        if !tail.is_empty() && tail.split(std::path::is_separator)
            .any(|part| matches!(part, "" | "." | "..")) {
            return Err(invalid("path contains empty, . or .. components"));
        }
        Ok(Self(raw.to_owned()))
    }
    pub fn as_path(&self) -> &Path { Path::new(&self.0) }
    pub fn as_str(&self) -> &str { &self.0 }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct NormalizedUrl(String);
impl NormalizedUrl {
    pub fn parse(input: &str) -> Result<Self, DomainError> {
        let mut url = Url::parse(input).map_err(|source| DomainError::InvalidUrl { input: input.into(), source })?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(DomainError::UnsupportedUrl { input: input.into() });
        }
        url.set_fragment(None);
        Ok(Self(url.into()))
    }
    pub fn as_str(&self) -> &str { &self.0 }
}
```

- [ ] **Step 5: Implement opaque feed IDs and episode-key selection.** Append:

```rust
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct FeedId(String);
impl FeedId {
    pub fn new(value: String) -> Result<Self, DomainError> {
        if value.is_empty() { return Err(DomainError::EmptyFeedId); }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str { &self.0 }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct EpisodeKey(EpisodeIdentity);
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum EpisodeIdentity { Guid(String), Url(NormalizedUrl) }
impl EpisodeKey {
    pub fn resolve(guid: Option<&str>, enclosure: Option<&Url>, link: Option<&Url>) -> Result<Self, DomainError> {
        if let Some(guid) = guid.filter(|value| !value.is_empty()) {
            return Ok(Self(EpisodeIdentity::Guid(guid.into())));
        }
        let url = enclosure.or(link).ok_or(DomainError::MissingEpisodeIdentity)?;
        Ok(Self(EpisodeIdentity::Url(NormalizedUrl::parse(url.as_str())?)))
    }
}
```

The private episode-key formatting method arrives with its consumer in Task 3.

- [ ] **Step 6: Verify and commit.** Run `cargo fmt`, `cargo test --locked --test identity_values`, and `cargo clippy --locked --all-targets --all-features -- -D warnings`. Expected: all pass. Stage the task's listed files and commit with `git commit -m "feat: validate media identity components"`.

### Task 3: Implement canonical MediaId strings and JSON map keys

**Files:** Modify `src/media/id.rs`; create `tests/media_id.rs`.

**Interfaces:** Consumes Task 2's newtypes and errors. Produces `MediaId::{LocalFile(AbsolutePath), PodcastEpisode { feed: FeedId, episode: EpisodeKey }, RemoteUrl(NormalizedUrl)}`, `Display`, `FromStr<Err = DomainError>`, `From<MediaId> for String`, `TryFrom<String>`, and serde string conversion. Derives `Clone, Debug, Eq, PartialEq, Hash`.

- [ ] **Step 1: Write adversarial identity tests.** Create `tests/media_id.rs`:

```rust
use continuo::media::id::{AbsolutePath, EpisodeKey, FeedId, MediaId, NormalizedUrl};
use std::{collections::HashMap, path::PathBuf};

#[allow(clippy::unwrap_used)] // Fallible construction of fixed test fixtures.
fn identities() -> Vec<MediaId> {
    let path = if cfg!(windows) { "C:/audio/a #?:%.mp3" } else { "/audio/a #?:%.mp3" };
    let mut ids = vec![
        MediaId::LocalFile(AbsolutePath::new(PathBuf::from(path)).unwrap()),
        MediaId::RemoteUrl(NormalizedUrl::parse("https://example.com:443/a?q=%2f&x=1#f").unwrap()),
    ];
    let enclosure = url::Url::parse("https://example.com/audio?sig=%2F#part").unwrap();
    ids.push(MediaId::PodcastEpisode {
        feed: FeedId::new("subscription-1".into()).unwrap(),
        episode: EpisodeKey::resolve(None, Some(&enclosure), None).unwrap(),
    });
    for value in ["#", "/", ":", "?", " ", "%2F", "тест/🎵", "url:https://example.com/x", "guid:x", "https://feed.example/a b?q=1#frag"] {
        ids.push(MediaId::PodcastEpisode {
            feed: FeedId::new(value.into()).unwrap(),
            episode: EpisodeKey::resolve(Some(value), None, None).unwrap(),
        });
    }
    ids
}

#[test]
fn canonical_strings_and_json_map_keys_round_trip() {
    assert_eq!(MediaId::RemoteUrl(NormalizedUrl::parse("https://cdn.radio-t.com/rt_podcast900.mp3").unwrap()).to_string(), "remote:https://cdn.radio-t.com/rt_podcast900.mp3");
    for id in identities() {
        let encoded = id.to_string();
        assert_eq!(encoded.parse::<MediaId>().unwrap(), id);
        let map = HashMap::from([(id.clone(), 42_u64)]);
        let json = serde_json::to_string(&map).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value[&encoded], 42);
        assert_eq!(serde_json::from_str::<HashMap<MediaId, u64>>(&json).unwrap(), map);
        assert_eq!(serde_json::to_value(&id).unwrap(), encoded);
    }
}

#[test]
fn delimiters_do_not_collide() {
    let make = |feed: &str, guid: &str| MediaId::PodcastEpisode {
        feed: FeedId::new(feed.into()).unwrap(),
        episode: EpisodeKey::resolve(Some(guid), None, None).unwrap(),
    };
    assert_ne!(make("a/b", "c").to_string(), make("a", "b/c").to_string());
    assert_eq!(make("a/b", "# :?%").to_string(), "podcast:a%2Fb/guid:#%20:?%25");
}

#[test]
fn parser_rejects_invalid_and_noncanonical_forms() {
    for input in ["", "unknown:a", "local:relative", "remote:%FF", "remote:%GG", "podcast:a", "podcast:/guid%3Ax", "podcast:a/guid%3A", "podcast:a/other%3Ax", "podcast:a/guid:x/extra", "podcast:%61/guid:x", "podcast:a/guid:%GG", "podcast:a/guid:%2f"] {
        assert!(input.parse::<MediaId>().is_err(), "{input}");
        assert!(serde_json::from_value::<MediaId>(serde_json::json!(input)).is_err());
    }
}
```

- [ ] **Step 2: Observe the missing MediaId failure.** Run `cargo test --locked --test media_id`.

- [ ] **Step 3: Implement formatting and explicit serde string conversion.** Consolidate imports at the top of `src/media/id.rs`, then add the definitions below:

```rust
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

impl EpisodeKey {
    fn canonical(&self) -> String {
        match &self.0 {
            EpisodeIdentity::Guid(guid) => format!("guid:{guid}"),
            EpisodeIdentity::Url(url) => format!("url:{}", url.as_str()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum MediaId {
    LocalFile(AbsolutePath),
    PodcastEpisode { feed: FeedId, episode: EpisodeKey },
    RemoteUrl(NormalizedUrl),
}
const ID_ESCAPE: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'\\').add(b'%');
const PODCAST_ESCAPE: &AsciiSet = &ID_ESCAPE.add(b'/');
fn escape(value: &str, set: &'static AsciiSet) -> String {
    utf8_percent_encode(value, set).to_string()
}
impl fmt::Display for MediaId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LocalFile(path) => write!(f, "local:{}", escape(path.as_str(), ID_ESCAPE)),
            Self::RemoteUrl(url) => write!(f, "remote:{}", escape(url.as_str(), ID_ESCAPE)),
            Self::PodcastEpisode { feed, episode } => write!(f, "podcast:{}/{}", escape(feed.as_str(), PODCAST_ESCAPE), escape(&episode.canonical(), PODCAST_ESCAPE)),
        }
    }
}
impl From<MediaId> for String {
    fn from(value: MediaId) -> Self { value.to_string() }
}
impl TryFrom<String> for MediaId {
    type Error = DomainError;
    fn try_from(value: String) -> Result<Self, Self::Error> { value.parse() }
}
```

- [ ] **Step 4: Implement strict parsing.** Append:

```rust
impl FromStr for MediaId {
    type Err = DomainError;
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let invalid = |reason| DomainError::InvalidMediaId { input: input.into(), reason };
        let decode = |part: &str| -> Result<String, DomainError> {
            percent_decode_str(part).decode_utf8()
                .map(|value| value.into_owned())
                .map_err(|_| invalid("component is not UTF-8"))
        };
        let (kind, body) = input.split_once(':').ok_or_else(|| invalid("missing kind separator"))?;
        let id = match kind {
            "local" => Self::LocalFile(AbsolutePath::new(PathBuf::from(decode(body)?))?),
            "remote" => Self::RemoteUrl(NormalizedUrl::parse(&decode(body)?)?),
            "podcast" => {
                let (feed, key) = body.split_once('/').ok_or_else(|| invalid("missing episode separator"))?;
                let key = decode(key)?;
                let episode = if let Some(guid) = key.strip_prefix("guid:") {
                    if guid.is_empty() { return Err(invalid("empty GUID")); }
                    EpisodeKey(EpisodeIdentity::Guid(guid.into()))
                } else if let Some(url) = key.strip_prefix("url:") {
                    EpisodeKey(EpisodeIdentity::Url(NormalizedUrl::parse(url)?))
                } else {
                    return Err(invalid("unknown episode key kind"));
                };
                Self::PodcastEpisode { feed: FeedId::new(decode(feed)?)?, episode }
            }
            _ => return Err(invalid("unknown media kind")),
        };
        if id.to_string() != input { return Err(invalid("noncanonical representation")); }
        Ok(id)
    }
}
```

The final equality check also rejects malformed `%` escapes, lowercase escapes, unnecessary escapes, unescaped characters from the selected set, extra podcast separators, and unnormalized URL payloads. No decoder unwrap or lossy path conversion is needed.

- [ ] **Step 5: Verify and commit.** Run `cargo fmt`, `cargo test --locked --test media_id --test identity_values`, and `cargo clippy --locked --all-targets --all-features -- -D warnings`. Expected: all pass. Stage the task's files and commit with `git commit -m "feat: serialize canonical media identities as strings"`.

### Task 4: Add source, metadata, episode, and checkpoint values

**Files:** Create `src/media/source.rs`, `src/media/metadata.rs`, `src/playback/mod.rs`, `src/playback/checkpoint.rs`, `tests/domain_values.rs`; modify `src/lib.rs`, `src/media/mod.rs`.

**Interfaces:** Consumes `MediaId`. Produces `SourceLocation::{LocalPath(PathBuf), Http(Url)}`, `MediaMetadata { title: Option<String>, duration: Option<Duration> }`, `Episode { id: MediaId, source: Option<SourceLocation> }`, and `PlaybackCheckpoint { media: MediaId, position: Duration, updated_at: OffsetDateTime }`. No new behavior changes playback position.

- [ ] **Step 1: Write behavioral boundary tests.** Create `tests/domain_values.rs`:

```rust
use continuo::media::{Episode, id::{EpisodeKey, FeedId, MediaId, NormalizedUrl}, metadata::MediaMetadata, source::SourceLocation};
use continuo::playback::checkpoint::PlaybackCheckpoint;
use std::time::Duration;
use time::OffsetDateTime;
use url::Url;

#[test]
fn fetch_url_stays_separate_from_identity_and_unplayable_items_exist() {
    let fetch = Url::parse("HTTPS://CDN.EXAMPLE.COM:443/audio?z=%2f&a=1&z=%2F#part").unwrap();
    let parsed_spelling = fetch.as_str().to_owned();
    let feed = FeedId::new("subscription-1".into()).unwrap();
    let id = MediaId::PodcastEpisode { feed: feed.clone(), episode: EpisodeKey::resolve(Some("guid"), Some(&fetch), None).unwrap() };
    let episode = Episode { id: id.clone(), source: Some(SourceLocation::Http(fetch.clone())) };
    let SourceLocation::Http(actual) = episode.source.unwrap() else { panic!("expected HTTP source") };
    assert_eq!(actual.as_str(), parsed_spelling);
    assert_eq!(actual.query(), Some("z=%2f&a=1&z=%2F"));
    assert_eq!(actual.fragment(), Some("part"));
    assert_eq!(NormalizedUrl::parse(fetch.as_str()).unwrap().as_str(), "https://cdn.example.com/audio?z=%2f&a=1&z=%2F");
    let redirected = Url::parse("https://new.example.com/changed?signature=new").unwrap();
    let same_id = MediaId::PodcastEpisode { feed, episode: EpisodeKey::resolve(Some("guid"), Some(&redirected), None).unwrap() };
    assert_eq!(same_id, id);
    assert!(Episode { id, source: None }.source.is_none());
    assert_eq!(MediaMetadata { title: None, duration: None }.duration, None);
}

#[test]
fn checkpoint_round_trip_preserves_position_and_rfc3339_timestamp() {
    let media = MediaId::RemoteUrl(NormalizedUrl::parse("https://example.com/audio").unwrap());
    let checkpoint = PlaybackCheckpoint {
        media,
        position: Duration::new(3_597, 123_000_000),
        updated_at: OffsetDateTime::UNIX_EPOCH,
    };
    let json = serde_json::to_value(&checkpoint).unwrap();
    assert_eq!(json["updated_at"], "1970-01-01T00:00:00Z");
    assert!(json["media"].is_string());
    let restored: PlaybackCheckpoint = serde_json::from_value(json).unwrap();
    assert_eq!(restored, checkpoint);
}
```

The ID test demonstrates independence from an enclosure URL change; real feed redirects and checkpoint storage retention require M4 and M2 respectively. Do not label these tests as runtime stop/resume or redirect integration coverage.

- [ ] **Step 2: Run the failing tests.** Run `cargo test --locked --test domain_values`. Expected: missing domain modules/types.

- [ ] **Step 3: Implement the value types and exports.** Add `pub mod playback;` to `src/lib.rs`; add `pub mod metadata;` and `pub mod source;` to `src/media/mod.rs` and define `Episode` there:

```rust
// src/media/mod.rs (retain the existing module declarations)
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Episode {
    pub id: id::MediaId,
    pub source: Option<source::SourceLocation>,
}
```

```rust
// src/media/source.rs
use std::path::PathBuf;
use url::Url;
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceLocation { LocalPath(PathBuf), Http(Url) }
```

```rust
// src/media/metadata.rs
use std::time::Duration;
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MediaMetadata {
    pub title: Option<String>,
    pub duration: Option<Duration>,
}
```

```rust
// src/playback/mod.rs
pub mod checkpoint;
```

```rust
// src/playback/checkpoint.rs
use crate::media::id::MediaId;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use time::OffsetDateTime;

/// Logical resume position, independent of current transport capabilities.
/// updated_at is for inspection, never ordering or merging updates.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlaybackCheckpoint {
    pub media: MediaId,
    pub position: Duration,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}
```

- [ ] **Step 4: Verify and commit.** Run `cargo fmt`, `cargo test --locked --test domain_values`, and `cargo clippy --locked --all-targets --all-features -- -D warnings`. Expected: pass. Inspect `PlaybackCheckpoint` to confirm there is no capability field, merge routine, completion policy, or automatic reset. Stage the task files and commit with `git commit -m "feat: add source metadata and checkpoint values"`.

### Task 5: Initialize tracing and expose a concise application error boundary

**Files:** Create `src/telemetry.rs`, `tests/telemetry.rs`, `tests/cli.rs`; modify `src/lib.rs`, `src/error.rs`, `src/main.rs`.

**Interfaces:** Produces `TelemetryError` with source-preserving variants; `telemetry::subscriber(filter: &str) -> Result<impl tracing::Subscriber + Send + Sync, TelemetryError>` for local tests and `telemetry::init() -> Result<(), TelemetryError>` for one-time application startup. `main` returns `ExitCode`; no CLI, async runtime, or playback is introduced.

- [ ] **Step 1: Write initialization and process boundary tests.** Create `tests/telemetry.rs`:

```rust
use continuo::telemetry;

#[test]
fn validates_filter_without_installing_global_state() {
    assert!(telemetry::subscriber("continuo=debug,warn").is_ok());
    let error = match telemetry::subscriber("continuo=not-a-level") {
        Ok(_) => panic!("invalid filter accepted"),
        Err(error) => error,
    };
    assert!(std::error::Error::source(&error).is_some());
}
```

Create `tests/cli.rs`:

```rust
use std::process::Command;

#[test]
fn startup_is_minimal_and_reports_filter_errors() {
    let success = Command::new(env!("CARGO_BIN_EXE_continuo")).env("RUST_LOG", "continuo=info").output().unwrap();
    assert!(success.status.success());
    assert!(success.stdout.is_empty());
    assert!(String::from_utf8_lossy(&success.stderr).contains("Continuo foundation initialized"));
    let failure = Command::new(env!("CARGO_BIN_EXE_continuo")).env("RUST_LOG", "continuo=not-a-level").output().unwrap();
    assert!(!failure.status.success());
    let stderr = String::from_utf8_lossy(&failure.stderr);
    assert!(stderr.contains("continuo: invalid tracing filter"));
    assert!(stderr.contains("application startup failed"));
}
```

Child process environment configuration avoids global environment mutation and unsafe code in edition 2024.

- [ ] **Step 2: Run the failing tests.** Run `cargo test --locked --test telemetry --test cli`. Expected: missing telemetry module; after that module exists, old greeting/process behavior fails.

- [ ] **Step 3: Add telemetry errors and subscriber creation.** Add to `src/error.rs`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("invalid tracing filter")]
    Filter(#[from] tracing_subscriber::filter::ParseError),
    #[error("RUST_LOG is not valid Unicode")]
    Environment(#[source] std::env::VarError),
    #[error("cannot initialize tracing")]
    Install(#[from] tracing::subscriber::SetGlobalDefaultError),
}
```

Add `pub mod telemetry;` to the library. Create `src/telemetry.rs`:

```rust
use crate::error::TelemetryError;
use tracing_subscriber::EnvFilter;

pub fn subscriber(filter: &str) -> Result<impl tracing::Subscriber + Send + Sync, TelemetryError> {
    Ok(tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(filter)?)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .finish())
}

pub fn init() -> Result<(), TelemetryError> {
    let filter = match std::env::var("RUST_LOG") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => "continuo=info".into(),
        Err(error) => return Err(TelemetryError::Environment(error)),
    };
    tracing::subscriber::set_global_default(subscriber(&filter)?)?;
    Ok(())
}
```

- [ ] **Step 4: Replace the greeting with the application boundary.** Use a local fallback subscriber when tracing itself fails, so the full startup error is still logged. Replace `src/main.rs`:

```rust
use continuo::{error::TelemetryError, telemetry};
use std::{error::Error, process::ExitCode};

fn run() -> Result<(), TelemetryError> {
    telemetry::init()?;
    tracing::info!("Continuo foundation initialized");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("continuo: {error}");
            let fallback = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .finish();
            tracing::subscriber::with_default(fallback, || {
                let mut chain = vec![error.to_string()];
                let mut source = error.source();
                while let Some(cause) = source {
                    chain.push(cause.to_string());
                    source = cause.source();
                }
                tracing::error!(error = %error, causes = ?chain, "application startup failed");
            });
            ExitCode::FAILURE
        }
    }
}
```

- [ ] **Step 5: Verify and commit.** Run `cargo fmt`, `cargo test --locked --test telemetry --test cli`, and `cargo clippy --locked --all-targets --all-features -- -D warnings`. Expected: pass. Stage the task files and commit with `git commit -m "feat: initialize tracing and report startup failures"`.

### Task 6: Publish the architectural contracts and enforce M0 checks in CI

**Files:** Create `docs/architecture.md`, `README.md`, `.github/workflows/ci.yml`. No runtime source changes are expected.

**Interfaces:** Consumes the completed library and pinned toolchain. Produces contributor commands and documented contracts for M1–M5, plus one Ubuntu push/PR CI job. This task's deliverable is independently verifiable through the documented commands and a spec coverage review; do not write tests that merely assert documentation text.

- [ ] **Step 1: Write `docs/architecture.md` from the approved contracts.** Use the following section order and content requirements. Each numbered item becomes a section with prose and the spec's tables where specified. State clearly which rules are implemented in M0 and which bind future implementation.

  1. **Scope and invariant:** quote the central invariant and canonical position contract from spec §§1 and 3. M0 ships values and documentation only. Stop/recreate preserves position; restoration, selection, explicit restart, and successful seek establish position.
  2. **Execution contexts:** reproduce the ownership table from §2. Decode worker owns the entire CPAL lifecycle; Tokio never touches decoder/device state; callback drains bounded SPSC, emits underrun silence, publishes counters, and never locks, allocates, waits, or performs I/O. Writer filesystem work stays off Tokio.
  3. **Cancellation and channel backpressure:** source reads and PCM backpressure permit cancellable synchronous waits, directly woken by stop/seek/shutdown. Commands queued behind a blocking read are insufficient. Bounded event publication never blocks indefinitely; progress is keep-latest; lifecycle/error events remain ordered and lossless; a disconnected receiver means shutdown. No speculative bridge task, channel implementation, or protocol enums.
  4. **Position accounting:** coherent `(media_timestamp, output_frame_count, generation)` anchor plus callback-submitted media frames at output rate, adjusted for resampler delay/padding and available device latency. Underrun/pause silence does not advance; recorded media silence does. Successful seeks anchor actual results; failed seeks preserve position and reopen there or error. Buffer invalidation, callback handoff, and publication are coordinated; generation tags alone are insufficient. Occupancy is read directly; decoded position stays diagnostic. EOF is not output drain.
  5. **Identity and capabilities:** explain orthogonal transport/continuity/seekability, unknown versus unsupported and finite duration unknown; include the resume matrix. Describe all validating types and canonical grammar from this plan, GUID priority/opacity, URL query preservation, separate fetch URLs, parsed serialization caveat, missing enclosure, and immutable subscription-assigned `FeedId` across mutable feed fetch URLs. `AbsolutePath` rejects raw dot, repeated, and trailing segments (except roots), never canonicalizes; M1 passes `fs::canonicalize` output. Mention non-UTF-8 limitation.
  6. **Durable state (M2):** reproduce the four XDG paths in §5, honoring variables/defaults. Current media references one per-identity checkpoint within one atomic playback snapshot; subscriptions remain separate durable data. One concrete typed persistence module, no repository trait. Single writer's accepted sequence orders updates after generation validation, never timestamp/maximum position; capture periodically and on pause/stop/change/successful seek, flush shutdown. Capture and maximum coalescing intervals are each bounded to single-digit seconds, bounding end-to-end loss. Write same-directory temporary file, fsync file, rename, fsync parent where supported. `schema_version` from first write; preserve/report malformed or unsupported files. Unsupported/undetermined resume never deletes progress; backward seeks supersede earlier larger positions. Completed status follows output drain and is separate from position; replay reset requires explicit completed policy in M2. No near-end reset.
  7. **Diagnostics and errors:** list source opened, redirects, ranges, decoder, duration, requested/actual seek, state transition, checkpoint write, end of track, output failure. No per-frame logging. Contextual typed errors, concise user text, full structured chains. Document `RUST_LOG` and estimated position limits.
  8. **Milestones and backend:** reproduce M0–M5 table and explicit M0 deferrals. Symphonia + CPAL is M1 choice for control; do not claim Rodio cannot seek; range support belongs to sources. No speculative backend trait; Rodio is contingency. Record all v0.1 exclusions, including deferred MPRIS/media keys. M1 requires ALSA development headers on Linux; M0 does not.
  9. **Future acceptance:** copy all eleven Radio-T manual scenario steps from §8, noting the full feed-driven case becomes executable with M4 while M3 exercises its HTTP playback portion. Specify local-server automated cases: ranges, no ranges, redirects, invalid range responses, reconnect-after-stop. No public-network dependency for automated tests.

Link back to `superpowers/specs/2026-09-07-continuo-foundation-design.md` from `docs/architecture.md`. Do not replace the approved spec or imply deferred behavior works today.

- [ ] **Step 2: Write `README.md`.** Use this concise starting content, with the existing identity grammar explained in architecture rather than duplicated:

```markdown
# Continuo

A keyboard-first terminal audio player being built for local audio, finite HTTP media, and podcasts.

Milestone 0 provides domain types, validated media identities, checkpoint values, tracing, and CI. The binary initializes tracing and exits; audio playback and a terminal UI are not implemented yet.

## Development

Install Rust through rustup. The repository pins Rust 1.98.1 and the rustfmt and clippy components. Dependency versions are recorded in the committed Cargo.lock.

Run from the repository root:

    cargo run --locked
    RUST_LOG=continuo=debug cargo run --locked
    cargo fmt --check
    cargo clippy --locked --all-targets --all-features -- -D warnings
    cargo test --locked

Logging goes to stderr. Runtime code forbids unsafe code and denies unwrap/expect; tests may use unwrap/expect for assertions and fixtures.

M0 has no audio system dependency. M1 will require libasound2-dev on Linux when CPAL is introduced; the runtime libasound.so.2 alone is insufficient.

## Design and roadmap

Read the [architecture](docs/architecture.md) and [approved foundation spec](docs/superpowers/specs/2026-09-07-continuo-foundation-design.md).

M1 adds local playback; M2 adds durable resume; M3 adds finite HTTP playback; M4 adds feeds and subscriptions; M5 adds the TUI.

Non-UTF-8 local paths are unsupported. Position will be an estimate when device latency is unavailable, and seek support may remain unknown until probed. HTTP transport never implies live radio.
```

- [ ] **Step 3: Add the required CI workflow.** Create `.github/workflows/ci.yml`:

```yaml
name: CI
on:
  push:
  pull_request:
permissions:
  contents: read
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install pinned Rust toolchain
        run: rustup toolchain install 1.98.1 --profile minimal --component rustfmt --component clippy
      - run: cargo fmt --check
      - run: cargo clippy --locked --all-targets --all-features -- -D warnings
      - run: cargo test --locked
```

The explicit installation matches the repository pin; Cargo selects it via `rust-toolchain.toml`. No audio packages, matrix, release, or coverage job.

- [ ] **Step 4: Run the acceptance checks and review scope.** Run exactly:

```bash
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo run --locked
git diff --check
git status --short
```

Expected: formatting, clippy, all tests, and whitespace checks pass; the binary emits one initialization event to stderr and exits successfully. Inspect `Cargo.toml` for the exact allowed direct dependency set and `Cargo.lock` for resolved dependencies. Inspect `src/` to confirm only the specified M0 modules exist. Confirm docs carry all contracts using the coverage table below. Local checks establish command success; only a later GitHub run establishes hosted CI success.

- [ ] **Step 5: Commit and hand off M0.** Stage the three documentation/CI files and commit with `git commit -m "docs: publish foundation contracts and add CI"`. Report implemented APIs, actual validation results, and that playback, persistence, HTTP, feeds, and the TUI are deferred. Do not run the Radio-T acceptance case or claim stop/resume is verified in M0.

## Spec coverage and final review

| Approved spec requirement | Implementation / verification |
|---|---|
| §1 purpose, invariant, exclusions | Task 6 architecture and README |
| §2 ownership, cancellation, bounded lossless lifecycle events | Task 6 architecture; implementation deferred to M1 |
| §3 logical position, output accounting, seek failure, coherent generations, diagnostics | Task 6 architecture; implementation/tests deferred to M1 |
| §4 full capability matrix | Task 1, all 12 combinations |
| §4 validated paths, opaque GUIDs, normalized identity URLs, immutable feed identity | Task 2 and tests; Task 6 contracts |
| §4 canonical strings and serde map keys | Task 3 adversarial round-trips and malformed-input tests |
| §4 identity priority, fetch query preservation, optional source, unknown duration | Tasks 2 and 4 tests |
| §4 checkpoint shape, human-readable timestamp, capability independence | Task 4 value/serde test and structural review |
| §§4–5 no merge/reset, completion on drain, XDG, atomic persistence, bounded capture/writer loss | Task 6 architecture; implementation/tests deferred to M2 |
| §6 exact modules, dependencies, lint rules, logging, toolchain, lockfile | Tasks 1–5 |
| §6 CI and README/architecture deliverables | Task 6 |
| §§7–10 milestones, backend rationale, Radio-T, limitations, ALSA prerequisite | Task 6 architecture and README |

Plan self-review: all M0 deliverables map to tasks; future contracts map to documentation rather than speculative runtime code. API names and field types match across tasks. Every code-producing step includes concrete code; documentation-only steps name precise source sections and required content. At execution time, compiler/test feedback may require small mechanical corrections, but scope and binding semantics must remain those of the approved spec.
