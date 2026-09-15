//! Seeding and reading an isolated `continuo tui` profile for process tests
//! (M5 §6, §11): a schema-3 `state.json` written before the child starts,
//! and the child's session logs under `<state dir>/logs/` read afterwards.
//! Every wait here is bounded by the caller's patience.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use continuo::media::id::{AbsolutePath, MediaId, NormalizedUrl};
use serde_json::{Value, json};

use crate::process::Profile;

const POLL: Duration = Duration::from_millis(20);

/// One queue entry to seed.
pub enum Seed {
    /// A local file (canonicalized here), with an optional display title:
    /// an untitled entry is what the metadata workers probe.
    Local {
        path: PathBuf,
        title: Option<&'static str>,
    },
    Remote {
        url: String,
    },
}

impl Seed {
    pub fn local(path: impl AsRef<Path>, title: Option<&'static str>) -> Self {
        Self::Local {
            path: path.as_ref().to_path_buf(),
            title,
        }
    }

    fn media_and_source(&self) -> (MediaId, Value, Option<&'static str>) {
        match self {
            Self::Local { path, title } => {
                let canonical = std::fs::canonicalize(path)
                    .unwrap_or_else(|error| panic!("canonicalize {}: {error}", path.display()));
                let absolute = AbsolutePath::new(canonical)
                    .unwrap_or_else(|error| panic!("absolute path: {error}"));
                let source = json!({ "kind": "local", "path": absolute.as_str() });
                (MediaId::LocalFile(absolute), source, *title)
            }
            Self::Remote { url } => {
                let url =
                    NormalizedUrl::parse(url).unwrap_or_else(|error| panic!("url {url}: {error}"));
                let source = json!({ "kind": "remote", "url": url.as_str() });
                (MediaId::RemoteUrl(url), source, None)
            }
        }
    }
}

/// Writes a schema-3 `state.json` holding `entries` (ids 1, 2, …) with the
/// entry at `active` (an index into `entries`) as the active entry, and
/// returns each entry's media identity as it appears in the file.
pub fn seed_queue(profile: &Profile, entries: &[Seed], active: Option<usize>) -> Vec<String> {
    let mut queue = Vec::new();
    let mut media_ids = Vec::new();
    for (index, seed) in entries.iter().enumerate() {
        let (media, source, title) = seed.media_and_source();
        let mut entry = json!({ "id": index + 1, "media": media.to_string(), "source": source });
        if let Some(title) = title {
            entry["display"] = json!({ "title": title });
        }
        queue.push(entry);
        media_ids.push(media.to_string());
    }
    let current = active.map(|index| media_ids[index].clone());
    let state = json!({
        "schema_version": 3,
        "current_media": current,
        "volume": 1.0,
        "checkpoints": {},
        "queue": queue,
        "active_entry": active.map(|index| index + 1),
    });
    std::fs::create_dir_all(profile.state_dir())
        .unwrap_or_else(|error| panic!("state dir: {error}"));
    std::fs::write(
        profile.state_file(),
        serde_json::to_vec_pretty(&state).unwrap_or_else(|error| panic!("encode: {error}")),
    )
    .unwrap_or_else(|error| panic!("write state: {error}"));
    media_ids
}

/// The flushed state file.
pub fn read_state(profile: &Profile) -> Value {
    let bytes =
        std::fs::read(profile.state_file()).unwrap_or_else(|error| panic!("read state: {error}"));
    serde_json::from_slice(&bytes).unwrap_or_else(|error| panic!("state json: {error}"))
}

pub fn log_dir(profile: &Profile) -> PathBuf {
    profile.state_dir().join("logs")
}

/// Every session log in the profile, oldest first; empty when the child
/// never got as far as opening one.
pub fn session_logs(profile: &Profile) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(log_dir(profile)) else {
        return Vec::new();
    };
    let mut logs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "log"))
        .collect();
    // Names embed a fixed-width UTC stamp, so name order is age order to
    // the second. Two runs within one second may sort either way, which is
    // why the tests read logs only from a profile with one `tui` run so far.
    logs.sort();
    logs
}

/// The newest session log's text, or an empty string when there is none.
pub fn newest_log_text(profile: &Profile) -> String {
    session_logs(profile)
        .last()
        .and_then(|path| std::fs::read(path).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// Whether the newest session log satisfies `test` within `patience`.
pub fn wait_for_log(profile: &Profile, patience: Duration, test: impl Fn(&str) -> bool) -> bool {
    let deadline = Instant::now() + patience;
    loop {
        if test(&newest_log_text(profile)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// Whether `later` occurs in `text` somewhere after the first `earlier`.
pub fn occurs_after(text: &str, earlier: &str, later: &str) -> bool {
    text.find(earlier)
        .is_some_and(|at| text[at + earlier.len()..].contains(later))
}
