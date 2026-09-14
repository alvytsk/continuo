//! `SubscriptionStore`: durable subscription snapshots, with reads and
//! recovery kept separate (design doc §5.1, §5.5, §5.6).

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use continuo::clock::FakeClock;
use continuo::persistence::store::{LoadReason, MAX_QUARANTINE_CANDIDATES};
use continuo::subscription::model::Subscription;
use continuo::subscription::store::{SubscriptionSnapshot, SubscriptionStore};
use serde_json::{Value, json};
use time::OffsetDateTime;

/// The stamp a `FakeClock` produces, which starts at the epoch.
const EPOCH_STAMP: &str = "19700101T000000Z";

fn store(path: PathBuf) -> SubscriptionStore {
    SubscriptionStore::new(path, Arc::new(FakeClock::new()))
}

/// One valid, explicit record (design doc §5.1's JSON example) as a mutable
/// `Value`, so each rejection test can flip exactly one field.
fn valid_file() -> Value {
    json!({
        "schema_version": 1,
        "subscriptions": [
            {
                "feed_id": "9f3c1a7e42b58d0c6f19ab3e5d72c840",
                "slug": "radio-t",
                "title": "Радио-Т",
                "fetch_url": "https://radio-t.com/rss/",
                "added_at": "2026-09-11T09:14:22Z"
            }
        ]
    })
}

/// Writes `file`, asserts `read_snapshot` returns `Err` without touching the
/// file, then asserts `load` quarantines it: moved aside, original name
/// gone, writing left on.
fn assert_malformed(path: &PathBuf, file: &Value) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = serde_json::to_vec(file)?;
    fs::write(path, &bytes)?;

    let read_store = store(path.clone());
    assert!(
        read_store.read_snapshot().is_err(),
        "expected a malformed file to be rejected: {file}"
    );
    assert_eq!(
        fs::read(path)?,
        bytes,
        "read_snapshot must never rewrite or move the file"
    );

    let load_store = store(path.clone());
    let result = load_store.load();
    let LoadReason::Quarantined { moved_to } = result.reason else {
        return Err(format!("expected a quarantine for {file}, got {:?}", result.reason).into());
    };
    assert!(!path.exists());
    assert_eq!(fs::read(&moved_to)?, bytes);
    assert!(result.writable);
    Ok(())
}

#[test]
fn only_mutating_load_quarantines() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    std::fs::write(&path, b"broken")?;
    let store = SubscriptionStore::new(path.clone(), Arc::new(FakeClock::new()));
    assert!(store.read_snapshot().is_err());
    assert_eq!(std::fs::read(&path)?, b"broken");
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
    let result = store.load();
    let LoadReason::Quarantined { moved_to } = result.reason else {
        panic!("expected quarantine")
    };
    assert_eq!(std::fs::read(moved_to)?, b"broken");
    assert!(!path.exists());
    Ok(())
}

#[test]
fn a_valid_subscription_survives_save_and_load() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let store = store(path.clone());

    let subscription = Subscription {
        feed_id: continuo::subscription::model::validate_feed_id(
            "9f3c1a7e42b58d0c6f19ab3e5d72c840",
        )?,
        slug: "radio-t".to_string(),
        title: Some("Радио-Т".to_string()),
        fetch_url: url::Url::parse("https://radio-t.com/rss/")?,
        added_at: OffsetDateTime::parse(
            "2026-09-11T09:14:22Z",
            &time::format_description::well_known::Rfc3339,
        )?,
    };
    store.save(&SubscriptionSnapshot {
        subscriptions: vec![subscription.clone()],
    })?;

    let snapshot = store.read_snapshot()?;
    assert_eq!(snapshot.subscriptions, vec![subscription.clone()]);

    let result = store.load();
    assert!(matches!(result.reason, LoadReason::Loaded));
    assert!(result.writable);
    assert_eq!(result.snapshot.subscriptions, vec![subscription]);
    Ok(())
}

#[test]
fn a_missing_read_creates_no_parent_directory() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let nested = root.path().join("continuo");
    let path = nested.join("subscriptions.json");
    let store = store(path.clone());

    let snapshot = store.read_snapshot()?;
    assert!(snapshot.subscriptions.is_empty());
    assert!(
        !nested.exists(),
        "read_snapshot must never create the parent directory"
    );

    let result = store.load();
    assert!(matches!(result.reason, LoadReason::Missing));
    assert!(result.writable);
    assert!(
        !nested.exists(),
        "a missing load must not create the parent directory either"
    );
    Ok(())
}

// --- §5.6 rejection matrix -------------------------------------------------

#[test]
fn traversal_shaped_feed_id_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    file["subscriptions"][0]["feed_id"] = json!("../../etc/passwd");
    assert_malformed(&path, &file)
}

#[test]
fn uppercase_feed_id_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    file["subscriptions"][0]["feed_id"] = json!("9F3C1A7E42B58D0C6F19AB3E5D72C840");
    assert_malformed(&path, &file)
}

#[test]
fn wrong_length_feed_id_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    file["subscriptions"][0]["feed_id"] = json!("9f3c1a7e42b58d0c6f19ab3e5d72c8");
    assert_malformed(&path, &file)
}

#[test]
fn duplicate_feed_id_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    let mut second = file["subscriptions"][0].clone();
    second["slug"] = json!("other-slug");
    file["subscriptions"]
        .as_array_mut()
        .ok_or("subscriptions must be an array")?
        .push(second);
    assert_malformed(&path, &file)
}

#[test]
fn duplicate_slug_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    let mut second = file["subscriptions"][0].clone();
    second["feed_id"] = json!("00000000000000000000000000000000"[..32]);
    file["subscriptions"]
        .as_array_mut()
        .ok_or("subscriptions must be an array")?
        .push(second);
    assert_malformed(&path, &file)
}

#[test]
fn invalid_slug_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    file["subscriptions"][0]["slug"] = json!("Not A Slug!");
    assert_malformed(&path, &file)
}

#[test]
fn non_http_fetch_url_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    file["subscriptions"][0]["fetch_url"] = json!("ftp://radio-t.com/rss/");
    assert_malformed(&path, &file)
}

#[test]
fn bad_rfc3339_timestamp_is_malformed() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    file["subscriptions"][0]["added_at"] = json!("not a timestamp");
    assert_malformed(&path, &file)
}

#[test]
fn unsupported_schema_version_is_preserved_not_quarantined()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let mut file = valid_file();
    file["schema_version"] = json!(2);
    let bytes = serde_json::to_vec(&file)?;
    fs::write(&path, &bytes)?;

    let read_store = store(path.clone());
    assert!(read_store.read_snapshot().is_err());
    assert_eq!(fs::read(&path)?, bytes);

    let load_store = store(path.clone());
    let result = load_store.load();
    assert!(matches!(
        result.reason,
        LoadReason::UnsupportedVersion { found: 2 }
    ));
    assert!(!result.writable);
    assert_eq!(
        fs::read(&path)?,
        bytes,
        "an unsupported version is preserved in place, not quarantined"
    );
    assert_eq!(
        fs::read_dir(dir.path())?.count(),
        1,
        "no quarantine file was created"
    );
    Ok(())
}

// --- No cap, no eviction ----------------------------------------------------

/// design doc §5.1: unlike `state.json`, subscriptions have no entry cap and
/// no eviction — silently dropping one to respect a limit would be data
/// loss. Saving many more subscriptions than any checkpoint cap must not
/// drop a single one.
#[test]
fn saving_513_distinct_subscriptions_all_survive() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    let store = store(path.clone());

    const COUNT: usize = 513;
    let mut subscriptions = Vec::with_capacity(COUNT);
    for index in 0..COUNT {
        let feed_id = continuo::subscription::model::validate_feed_id(&format!("{index:032x}"))?;
        subscriptions.push(Subscription {
            feed_id,
            slug: format!("feed-{index}"),
            title: None,
            fetch_url: url::Url::parse(&format!("https://example.org/feed/{index}"))?,
            added_at: OffsetDateTime::UNIX_EPOCH,
        });
    }

    store.save(&SubscriptionSnapshot {
        subscriptions: subscriptions.clone(),
    })?;

    let result = store.load();
    assert!(matches!(result.reason, LoadReason::Loaded));
    assert_eq!(result.snapshot.subscriptions.len(), COUNT);

    let ids: BTreeSet<_> = result
        .snapshot
        .subscriptions
        .iter()
        .map(|subscription| subscription.feed_id.as_str().to_string())
        .collect();
    assert_eq!(ids.len(), COUNT, "no subscription was dropped or merged");

    let slugs: BTreeSet<_> = result
        .snapshot
        .subscriptions
        .iter()
        .map(|subscription| subscription.slug.clone())
        .collect();
    assert_eq!(slugs.len(), COUNT);

    let snapshot = store.read_snapshot()?;
    assert_eq!(snapshot.subscriptions.len(), COUNT);
    Ok(())
}

// --- Quarantine collision handling ------------------------------------------

#[test]
fn a_quarantine_name_collision_is_suffixed_rather_than_clobbered()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    fs::write(&path, b"garbage")?;
    let taken = dir
        .path()
        .join(format!("subscriptions.json.rejected-{EPOCH_STAMP}"));
    fs::write(&taken, b"an earlier rejection")?;

    let result = store(path.clone()).load();
    let LoadReason::Quarantined { moved_to } = result.reason else {
        return Err("expected quarantine".into());
    };
    assert_eq!(
        moved_to
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string),
        Some(format!("subscriptions.json.rejected-{EPOCH_STAMP}-2"))
    );
    assert_eq!(fs::read(&taken)?, b"an earlier rejection");
    Ok(())
}

#[test]
fn a_quarantine_with_every_candidate_taken_disables_writing()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    fs::write(&path, b"garbage")?;
    for suffix in 1..=MAX_QUARANTINE_CANDIDATES {
        let name = if suffix == 1 {
            format!("subscriptions.json.rejected-{EPOCH_STAMP}")
        } else {
            format!("subscriptions.json.rejected-{EPOCH_STAMP}-{suffix}")
        };
        fs::write(dir.path().join(name), b"taken")?;
    }

    let result = store(path.clone()).load();
    assert!(matches!(result.reason, LoadReason::QuarantineFailed));
    assert!(
        !result.writable,
        "the only way to preserve a file whose quarantine failed is to stop writing"
    );
    assert_eq!(fs::read(&path)?, b"garbage");
    Ok(())
}

// --- Unreadable path ---------------------------------------------------------

/// A directory at the subscription path is deterministically unreadable
/// (even for root, which can `open` a directory but not read it as a file).
#[test]
fn a_directory_at_the_path_is_unreadable() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("subscriptions.json");
    fs::create_dir(&path)?;

    let read_store = store(path.clone());
    assert!(read_store.read_snapshot().is_err());
    assert!(path.is_dir());

    let result = store(path.clone()).load();
    assert!(matches!(result.reason, LoadReason::Unreadable));
    assert!(!result.writable);
    assert!(
        path.is_dir(),
        "the directory must be left exactly as it was"
    );
    let entries: Vec<_> = fs::read_dir(dir.path())?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(
        entries,
        vec![std::ffi::OsString::from("subscriptions.json")],
        "no file was created, renamed, or quarantined alongside it"
    );
    Ok(())
}
