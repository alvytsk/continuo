//! The playback queue (M5 §5): ordered occurrences with stable IDs. Pure
//! data and policy. It lives inside `PersistedState` and changes only
//! through `Session`; it never renders, decodes or touches the filesystem.

use std::time::Duration;
use url::Url;

use crate::media::id::{AbsolutePath, MediaId, NormalizedUrl};
use crate::playback::provenance::PositionProvenance;

pub const MAX_QUEUE_ENTRIES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct QueueEntryId(u64);

impl QueueEntryId {
    pub fn get(self) -> u64 {
        self.0
    }
    #[allow(dead_code)]
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueueSource {
    LocalFile(AbsolutePath),
    RemoteUrl(NormalizedUrl),
    /// The episode identity lives in the entry's `MediaId`; this is only the
    /// last resolved enclosure, used when the cache cannot answer (§5).
    Podcast {
        fallback: Url,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurationSource {
    Decoded(PositionProvenance),
    Declared,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayDuration {
    pub value: Duration,
    pub source: DurationSource,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DisplayMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<DisplayDuration>,
}

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum QueueError {
    #[error(
        "the queue holds at most {MAX_QUEUE_ENTRIES} entries; {requested} requested, {available} free"
    )]
    Capacity { requested: usize, available: usize },
    #[error("queue entry IDs are exhausted; cannot enqueue more entries in this session")]
    IdExhausted,
    #[error("queue source does not match its media identity")]
    SourceMismatch,
    #[error("queue entry {} is no longer queued", .0.get())]
    UnknownEntry(QueueEntryId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Up,
    Down,
}

pub(crate) fn source_matches(media: &MediaId, source: &QueueSource) -> bool {
    match (media, source) {
        (MediaId::LocalFile(path), QueueSource::LocalFile(source)) => path == source,
        (MediaId::RemoteUrl(url), QueueSource::RemoteUrl(source)) => url == source,
        (MediaId::PodcastEpisode { .. }, QueueSource::Podcast { fallback }) => {
            matches!(fallback.scheme(), "http" | "https") && fallback.host_str().is_some()
        }
        _ => false,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewQueueEntry {
    media: MediaId,
    source: QueueSource,
    display: DisplayMetadata,
}

impl NewQueueEntry {
    pub fn new(
        media: MediaId,
        source: QueueSource,
        display: DisplayMetadata,
    ) -> Result<Self, QueueError> {
        if !source_matches(&media, &source) {
            return Err(QueueError::SourceMismatch);
        }
        Ok(Self {
            media,
            source,
            display,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueEntry {
    id: QueueEntryId,
    media: MediaId,
    source: QueueSource,
    display: DisplayMetadata,
}

impl QueueEntry {
    pub fn id(&self) -> QueueEntryId {
        self.id
    }
    pub fn media(&self) -> &MediaId {
        &self.media
    }
    pub fn source(&self) -> &QueueSource {
        &self.source
    }
    pub fn display(&self) -> &DisplayMetadata {
        &self.display
    }
    #[allow(dead_code)]
    pub(crate) fn display_mut(&mut self) -> &mut DisplayMetadata {
        &mut self.display
    }
    #[allow(dead_code)]
    pub(crate) fn set_source(&mut self, source: QueueSource) -> Result<(), QueueError> {
        if !source_matches(&self.media, &source) {
            return Err(QueueError::SourceMismatch);
        }
        self.source = source;
        Ok(())
    }
}

#[derive(Debug)]
pub struct Removed {
    pub entry: QueueEntry,
    pub was_active: bool,
    pub selection: Option<QueueEntryId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Queue {
    entries: Vec<QueueEntry>,
    active: Option<QueueEntryId>,
    next_id: Option<u64>,
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            active: None,
            next_id: Some(1),
        }
    }
}

impl Queue {
    pub fn entries(&self) -> &[QueueEntry] {
        &self.entries
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn active(&self) -> Option<QueueEntryId> {
        self.active
    }
    pub fn first(&self) -> Option<QueueEntryId> {
        self.entries.first().map(QueueEntry::id)
    }
    pub fn index_of(&self, id: QueueEntryId) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }
    pub fn get(&self, id: QueueEntryId) -> Option<&QueueEntry> {
        self.entries.iter().find(|e| e.id == id)
    }
    #[allow(dead_code)]
    pub(crate) fn get_mut(&mut self, id: QueueEntryId) -> Option<&mut QueueEntry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    pub fn enqueue(&mut self, batch: Vec<NewQueueEntry>) -> Result<Vec<QueueEntryId>, QueueError> {
        let available = MAX_QUEUE_ENTRIES.saturating_sub(self.entries.len());
        if batch.len() > available {
            return Err(QueueError::Capacity {
                requested: batch.len(),
                available,
            });
        }
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        let count = u64::try_from(batch.len()).map_err(|_| QueueError::IdExhausted)?;
        let first = self.next_id.ok_or(QueueError::IdExhausted)?;
        let last = first
            .checked_add(count - 1)
            .ok_or(QueueError::IdExhausted)?;
        let ids = (first..=last)
            .zip(batch)
            .map(|(raw, new)| {
                let id = QueueEntryId(raw);
                self.entries.push(QueueEntry {
                    id,
                    media: new.media,
                    source: new.source,
                    display: new.display,
                });
                id
            })
            .collect();
        self.next_id = last.checked_add(1);
        Ok(ids)
    }

    pub fn move_entry(
        &mut self,
        id: QueueEntryId,
        direction: Direction,
    ) -> Result<bool, QueueError> {
        let index = self.index_of(id).ok_or(QueueError::UnknownEntry(id))?;
        let other = match direction {
            Direction::Up => index.checked_sub(1),
            Direction::Down => Some(index + 1).filter(|next| *next < self.entries.len()),
        };
        match other {
            Some(other) => {
                self.entries.swap(index, other);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn remove(&mut self, id: QueueEntryId) -> Result<Removed, QueueError> {
        let index = self.index_of(id).ok_or(QueueError::UnknownEntry(id))?;
        let entry = self.entries.remove(index);
        let was_active = self.active == Some(id);
        if was_active {
            self.active = None;
        }
        let selection = self
            .entries
            .get(index)
            .or_else(|| self.entries.last())
            .map(QueueEntry::id);
        Ok(Removed {
            entry,
            was_active,
            selection,
        })
    }

    pub fn clear(&mut self) -> Vec<QueueEntry> {
        self.active = None;
        std::mem::take(&mut self.entries)
    }

    pub fn neighbor(&self, anchor: QueueEntryId, direction: Direction) -> Option<QueueEntryId> {
        let index = self.index_of(anchor)?;
        let target = match direction {
            Direction::Up => index.checked_sub(1)?,
            Direction::Down => index + 1,
        };
        self.entries.get(target).map(QueueEntry::id)
    }

    /// Only `Session` adopts or clears an occurrence (§3).
    #[allow(dead_code)]
    pub(crate) fn set_active(&mut self, id: Option<QueueEntryId>) -> Result<(), QueueError> {
        if let Some(id) = id
            && self.get(id).is_none()
        {
            return Err(QueueError::UnknownEntry(id));
        }
        self.active = id;
        Ok(())
    }

    /// Persistence's constructor, used only after `queue_codec` validated
    /// uniqueness, identity and capacity.
    #[allow(dead_code)]
    pub(crate) fn from_parts(entries: Vec<QueueEntry>, active: Option<QueueEntryId>) -> Self {
        let next_id = entries
            .iter()
            .map(|e| e.id.0)
            .max()
            .map_or(Some(1), |max| max.checked_add(1));
        Self {
            entries,
            active,
            next_id,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn entry_from_parts(
        id: QueueEntryId,
        media: MediaId,
        source: QueueSource,
        display: DisplayMetadata,
    ) -> QueueEntry {
        QueueEntry {
            id,
            media,
            source,
            display,
        }
    }
}

#[cfg(test)]
mod exhaustion_tests {
    use super::*;
    #[test]
    fn exhaustion_rejects_the_whole_batch_without_reusing_an_id() {
        let path = AbsolutePath::new("/music/a.flac".into()).expect("absolute");
        let item = NewQueueEntry::new(
            MediaId::LocalFile(path.clone()),
            QueueSource::LocalFile(path),
            DisplayMetadata::default(),
        )
        .expect("entry");
        let mut queue = Queue {
            next_id: Some(u64::MAX),
            ..Queue::default()
        };
        let before = queue.clone();
        assert_eq!(
            queue.enqueue(vec![item.clone(), item.clone()]),
            Err(QueueError::IdExhausted)
        );
        assert_eq!(queue, before, "no partial append or allocator change");
        assert_eq!(
            queue.enqueue(vec![item.clone()]).expect("last ID")[0].get(),
            u64::MAX
        );
        let mut restored = Queue::from_parts(queue.entries().to_vec(), None);
        assert_eq!(
            restored.enqueue(vec![item.clone()]),
            Err(QueueError::IdExhausted)
        );
        queue.clear();
        assert_eq!(queue.enqueue(vec![item]), Err(QueueError::IdExhausted));
        assert!(queue.enqueue(Vec::new()).expect("empty batch").is_empty());
    }
}
