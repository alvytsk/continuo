//! `subscriptions.json`: the durable, user-authored list of feeds (design
//! doc §5.1). Reads and recovery are kept apart, exactly as
//! [`crate::persistence::store::StateStore`] separates them for checkpoints
//! (§5.5):
//!
//! - [`SubscriptionStore::read_snapshot`] is for every read-only command. It
//!   never renames and never writes; a missing file yields an empty set and
//!   anything else unreadable is a visible `Err`.
//! - [`SubscriptionStore::load`] is for the mutating commands (`subscribe`,
//!   `unsubscribe`, `refresh`). It applies `StateStore::load`'s policy:
//!   unreadable or an unsupported schema preserve the file and disable
//!   writing for the session; malformed quarantines it.
//!
//! Unlike `state.json` there is no entry cap and no eviction (§5.1): this is
//! user-authored data, and silently dropping a subscription to respect a
//! limit would be data loss.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use crate::clock::Clock;
use crate::feed::error::FeedError;
use crate::media::id::NormalizedUrl;
use crate::persistence::atomic::replace_bytes;
use crate::persistence::store::{LoadReason, MAX_QUARANTINE_CANDIDATES};

use super::model::{Subscription, validate_feed_id, validate_slug};

/// The only schema version this build writes or accepts (§5.1).
const SCHEMA_VERSION: u32 = 1;

/// The on-disk envelope (§5.1's JSON shape).
#[derive(Serialize, Deserialize)]
struct SubscriptionFile {
    schema_version: u32,
    subscriptions: Vec<SubscriptionRecord>,
}

/// One record inside `subscriptions.json`. Deliberately a plain DTO,
/// separate from [`Subscription`]: deserializing straight into the domain
/// type would let a `feed_id` or `slug` skip §5.6's validation.
#[derive(Clone, Serialize, Deserialize)]
struct SubscriptionRecord {
    feed_id: String,
    slug: String,
    title: Option<String>,
    fetch_url: Url,
    #[serde(with = "time::serde::rfc3339")]
    added_at: OffsetDateTime,
}

/// Only the field every future version is obliged to keep. Read before the
/// full model, mirroring [`crate::persistence::store`]'s version-first decode
/// so that an unsupported version is never misclassified as garbage.
#[derive(Deserialize)]
struct VersionEnvelope {
    schema_version: u32,
}

/// A decoded, validated subscription list.
#[derive(Clone, Debug)]
pub struct SubscriptionSnapshot {
    pub subscriptions: Vec<Subscription>,
}

/// The outcome of [`SubscriptionStore::load`].
pub struct SubscriptionLoad {
    pub snapshot: SubscriptionSnapshot,
    pub writable: bool,
    pub reason: LoadReason,
}

/// The pure decode step shared by [`SubscriptionStore::load`] and
/// [`SubscriptionStore::read_snapshot`]: version envelope first, then the
/// full shape, then §5.6's semantic validation. Touches no filesystem state
/// at all — no read, no quarantine, no write — so what each caller does with
/// a rejection is entirely theirs.
enum DecodeError {
    /// Deliberately carrying only a sanitized, pre-built reason: a
    /// subscription record can hold an untrusted URL, and this must never
    /// echo the file's raw JSON bytes back into a log or an error message,
    /// under `Debug` as much as `Display` (design doc §7.2).
    Malformed(String),
    UnsupportedVersion(u32),
}

fn decode_subscriptions(bytes: &[u8]) -> Result<SubscriptionSnapshot, DecodeError> {
    let envelope =
        serde_json::from_slice::<VersionEnvelope>(bytes).map_err(|source| malformed(&source))?;

    if envelope.schema_version != SCHEMA_VERSION {
        return Err(DecodeError::UnsupportedVersion(envelope.schema_version));
    }

    let file =
        serde_json::from_slice::<SubscriptionFile>(bytes).map_err(|source| malformed(&source))?;

    validate_records(file.subscriptions).map(|subscriptions| SubscriptionSnapshot { subscriptions })
}

/// §5.6's semantic validation: every `feed_id` matches the `FeedId` pattern
/// and is unique, every `slug` matches its pattern and is unique, and every
/// `fetch_url` parses as `http(s)`. Any violation makes the whole file
/// malformed — there is no way to tell which record was corrupted — and an
/// id is validated into a `FeedId` before anything else touches it, so a
/// traversal-shaped id never reaches a path built from it.
fn validate_records(records: Vec<SubscriptionRecord>) -> Result<Vec<Subscription>, DecodeError> {
    let mut seen_ids = BTreeSet::new();
    let mut seen_slugs = BTreeSet::new();
    let mut subscriptions = Vec::with_capacity(records.len());

    for record in records {
        // `validate_feed_id`'s `Err` is a deliberate placeholder (R4, see
        // `subscription::model`'s doc comment on the function): it must be
        // caught and re-mapped here, never propagated with `?`, since this
        // decode step's own `Malformed` is what carries the real meaning for
        // subscriptions.json.
        let feed_id = validate_feed_id(&record.feed_id)
            .map_err(|_| DecodeError::Malformed("a feed id is not valid".to_string()))?;
        if !seen_ids.insert(feed_id.as_str().to_string()) {
            return Err(DecodeError::Malformed(
                "a feed id is used by more than one subscription".to_string(),
            ));
        }

        validate_slug(&record.slug)
            .map_err(|_| DecodeError::Malformed("a slug is not valid".to_string()))?;
        if !seen_slugs.insert(record.slug.clone()) {
            return Err(DecodeError::Malformed(
                "a slug is used by more than one subscription".to_string(),
            ));
        }

        // The DTO already deserialized `fetch_url` as a `Url`, so any scheme
        // parses; `NormalizedUrl::parse` re-checks it is `http(s)` with a
        // host. Only its acceptance is used here — the stored field stays
        // the plain `Url` the DTO carries.
        NormalizedUrl::parse(record.fetch_url.as_str())
            .map_err(|_| DecodeError::Malformed("a fetch_url is not http(s)".to_string()))?;

        subscriptions.push(Subscription {
            feed_id,
            slug: record.slug,
            title: record.title,
            fetch_url: record.fetch_url,
            added_at: record.added_at,
        });
    }

    Ok(subscriptions)
}

/// Sanitizes a `serde_json::Error` into a category, deliberately dropping its
/// `Display` text: it can quote the file's raw content verbatim, and a
/// subscription record can carry an untrusted URL.
fn malformed(source: &serde_json::Error) -> DecodeError {
    use serde_json::error::Category;

    let category = match source.classify() {
        Category::Io => "io",
        Category::Syntax => "syntax",
        Category::Data => "data",
        Category::Eof => "eof",
    };
    DecodeError::Malformed(format!(
        "subscriptions file is malformed ({category} error at line {}, column {})",
        source.line(),
        source.column()
    ))
}

/// Converts a validated domain [`Subscription`] back into the on-disk DTO.
fn to_record(subscription: &Subscription) -> SubscriptionRecord {
    SubscriptionRecord {
        feed_id: subscription.feed_id.as_str().to_string(),
        slug: subscription.slug.clone(),
        title: subscription.title.clone(),
        fetch_url: subscription.fetch_url.clone(),
        added_at: subscription.added_at,
    }
}

pub struct SubscriptionStore {
    path: PathBuf,
    clock: Arc<dyn Clock>,
}

impl SubscriptionStore {
    pub fn new(path: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self { path, clock }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn now(&self) -> OffsetDateTime {
        self.clock.sample().wall
    }

    /// The non-mutating counterpart to [`Self::load`] (§5.1, §5.5): every
    /// read-only command (`feeds`, `episodes`) calls this instead, so that
    /// merely displaying subscriptions never quarantines or creates
    /// anything. A missing file is the normal "no subscriptions yet" state;
    /// anything present and unreadable, malformed or an unsupported version
    /// is a visible error, so a listing can never mistake "could not be
    /// read" for "nothing is subscribed".
    pub fn read_snapshot(&self) -> Result<SubscriptionSnapshot, FeedError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(SubscriptionSnapshot {
                    subscriptions: Vec::new(),
                });
            }
            Err(error) => {
                return Err(FeedError::SubscriptionsUnreadable {
                    reason: format!("cannot read subscriptions file: {error}"),
                });
            }
        };

        decode_subscriptions(&bytes).map_err(|error| match error {
            DecodeError::Malformed(reason) => FeedError::SubscriptionsUnreadable { reason },
            DecodeError::UnsupportedVersion(found) => FeedError::SubscriptionsUnreadable {
                reason: format!(
                    "subscriptions file is schema version {found}, and this build supports {SCHEMA_VERSION}"
                ),
            },
        })
    }

    /// Used by the mutating commands (`subscribe`, `unsubscribe`, `refresh`)
    /// (§5.1). Unlike [`Self::read_snapshot`], a malformed file is
    /// quarantined so subscription management is never blocked by a single
    /// corrupt file; an unreadable file or an unsupported schema version is
    /// preserved in place instead, with writing disabled for the session.
    pub fn load(&self) -> SubscriptionLoad {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Self::fresh(true, LoadReason::Missing);
            }
            Err(error) => {
                tracing::warn!(
                    path = ?self.path,
                    %error,
                    "cannot read the subscriptions file; leaving it in place and not writing this session"
                );
                return Self::fresh(false, LoadReason::Unreadable);
            }
        };

        match decode_subscriptions(&bytes) {
            Ok(snapshot) => SubscriptionLoad {
                snapshot,
                writable: true,
                reason: LoadReason::Loaded,
            },
            Err(DecodeError::UnsupportedVersion(found)) => {
                tracing::warn!(
                    path = ?self.path,
                    found,
                    supported = SCHEMA_VERSION,
                    "unsupported subscriptions schema; preserving the file and not writing this session"
                );
                Self::fresh(false, LoadReason::UnsupportedVersion { found })
            }
            Err(DecodeError::Malformed(reason)) => {
                tracing::warn!(path = ?self.path, reason, "subscriptions file is malformed");
                self.reject_malformed()
            }
        }
    }

    /// Validates the complete snapshot (so a public DTO cannot bypass §5.6's
    /// invariants by construction alone), serializes it under the current
    /// schema, and replaces the file atomically. Synchronous on the calling
    /// thread, not through the writer thread: no playback path runs while
    /// `subscribe` or `refresh` executes (§5.1).
    pub fn save(&self, snapshot: &SubscriptionSnapshot) -> Result<(), FeedError> {
        let records: Vec<SubscriptionRecord> =
            snapshot.subscriptions.iter().map(to_record).collect();

        // Re-runs exactly the load-side validation over the records about to
        // be written, so a `SubscriptionSnapshot` built by hand (rather than
        // decoded from disk) cannot write invalid data just because it
        // skipped `read_snapshot`/`load`.
        validate_records(records.clone()).map_err(|error| match error {
            DecodeError::Malformed(reason) => FeedError::SubscriptionsUnreadable { reason },
            DecodeError::UnsupportedVersion(found) => FeedError::SubscriptionsUnreadable {
                reason: format!("unexpected internal schema version {found}"),
            },
        })?;

        let file = SubscriptionFile {
            schema_version: SCHEMA_VERSION,
            subscriptions: records,
        };

        let bytes = serde_json::to_vec_pretty(&file).map_err(|source| {
            FeedError::SubscriptionsUnreadable {
                reason: format!("cannot serialize subscriptions: {source}"),
            }
        })?;

        replace_bytes(&self.path, &bytes)?;
        Ok(())
    }

    fn parent(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new("."))
    }

    fn fresh(writable: bool, reason: LoadReason) -> SubscriptionLoad {
        SubscriptionLoad {
            snapshot: SubscriptionSnapshot {
                subscriptions: Vec::new(),
            },
            writable,
            reason,
        }
    }

    fn reject_malformed(&self) -> SubscriptionLoad {
        match self.quarantine() {
            Some(moved_to) => {
                tracing::warn!(path = ?self.path, ?moved_to, "subscriptions file quarantined");
                Self::fresh(true, LoadReason::Quarantined { moved_to })
            }
            None => {
                tracing::warn!(
                    path = ?self.path,
                    "cannot quarantine the subscriptions file; not writing this session"
                );
                Self::fresh(false, LoadReason::QuarantineFailed)
            }
        }
    }

    /// Move the file aside under a timestamped name, never over one that
    /// already exists — the same policy as
    /// [`crate::persistence::store::StateStore`]'s quarantine, including its
    /// 100-candidate bound. Single-user, single-process by design (§5.7), so
    /// the exists-then-rename window is not a hazard worth more machinery.
    fn quarantine(&self) -> Option<PathBuf> {
        let stamp = stamp(self.clock.sample().wall);
        let dir = self.parent();
        for suffix in 1..=MAX_QUARANTINE_CANDIDATES {
            let name = if suffix == 1 {
                format!("subscriptions.json.rejected-{stamp}")
            } else {
                format!("subscriptions.json.rejected-{stamp}-{suffix}")
            };
            let candidate = dir.join(name);
            if candidate.exists() {
                continue;
            }
            if fs::rename(&self.path, &candidate).is_ok() {
                return Some(candidate);
            }
            return None;
        }
        None
    }
}

/// `20260908T143211Z` — filesystem-safe, no colons.
fn stamp(at: OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
    )
}
