//! The on-demand browser's own state and keys (design doc M5 §8): a Files
//! tab over one local directory at a time and a Podcasts tab over the cached
//! feeds and a feed's episodes. Everything here is a pure function over a
//! [`KeyEvent`] or a [`BrowseResult`]; reading the filesystem is
//! [`crate::application::browse`]'s worker's job, and `tui::run` is the only
//! thing that executes a [`BrowserEffect`].
//!
//! The visible list is always the answer to the latest request: moving to
//! another directory, feed or tab empties the list and marks it loading, and
//! an answer for anywhere else is dropped.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use crate::application::browse::{BrowseRequest, BrowseResult, DirEntry, EntryKind};
use crate::application::runtime::EnqueueItem;
use crate::library::{EpisodeCandidate, FeedSummary};
use crate::tui::input::blocks_ordinary_bindings;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrowserTab {
    Files,
    Podcasts,
}

#[derive(Clone, Debug)]
pub struct BrowserState {
    pub tab: BrowserTab,
    /// The directory the Files tab shows; absolute, so its parent is known.
    pub cwd: PathBuf,
    pub entries: Vec<DirEntry>,
    pub feeds: Vec<FeedSummary>,
    /// The feed being viewed, by slug, and its episodes; `None` while the
    /// Podcasts tab shows the feed list.
    pub episodes: Option<(String, Vec<EpisodeCandidate>)>,
    /// An index into the visible list.
    pub cursor: usize,
    /// Indices into the visible list, only ever of enqueueable rows.
    pub marked: BTreeSet<usize>,
    /// Whether the visible list still waits on its request.
    pub loading: bool,
    /// Why the visible list could not be read.
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub enum BrowserEffect {
    Request(BrowseRequest),
    Enqueue(Vec<EnqueueItem>),
    Close,
}

impl BrowserState {
    /// A Files tab at `cwd`, loading: the caller issues
    /// `BrowseRequest::Directory(cwd)` alongside.
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            tab: BrowserTab::Files,
            cwd,
            entries: Vec::new(),
            feeds: Vec::new(),
            episodes: None,
            cursor: 0,
            marked: BTreeSet::new(),
            loading: true,
            error: None,
        }
    }

    /// Takes in a worker's answer when it is for the list on screen;
    /// otherwise — a directory already left, a feed no longer viewed, the
    /// other tab — drops it.
    pub fn apply(&mut self, result: BrowseResult) {
        match result {
            BrowseResult::Directory { path, entries } => {
                if self.tab == BrowserTab::Files && path == self.cwd {
                    self.entries = self.settle(entries);
                }
            }
            BrowseResult::Feeds(feeds) => {
                if self.tab == BrowserTab::Podcasts && self.episodes.is_none() {
                    self.feeds = self.settle(feeds);
                }
            }
            BrowseResult::Episodes { slug, episodes } => {
                let viewing = matches!(&self.episodes, Some((current, _)) if *current == slug);
                if self.tab == BrowserTab::Podcasts && viewing {
                    let list = self.settle(episodes);
                    self.episodes = Some((slug, list));
                }
            }
            BrowseResult::Mutation { .. } => {
                // Mutations are handled by a separate part of the UI (M6).
            }
        }
        self.cursor = self.cursor.min(self.len().saturating_sub(1));
    }

    /// Ends loading for the visible list, recording an error in its place.
    fn settle<T>(&mut self, list: Result<Vec<T>, String>) -> Vec<T> {
        self.loading = false;
        self.marked.clear();
        match list {
            Ok(list) => {
                self.error = None;
                list
            }
            Err(message) => {
                self.error = Some(message);
                Vec::new()
            }
        }
    }

    /// Up/Down/`j`/`k` move, Tab switches tabs, Enter opens or enqueues,
    /// Space marks, Backspace/Left goes back, `b`/Esc closes. A Ctrl or Alt
    /// chord does nothing, as in the rest of the keyboard map.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<BrowserEffect> {
        if key.kind != KeyEventKind::Press || blocks_ordinary_bindings(&key) {
            return Vec::new();
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.cursor + 1 < self.len() {
                    self.cursor += 1;
                }
                Vec::new()
            }
            KeyCode::Tab | KeyCode::BackTab => self.switch_tab(),
            KeyCode::Enter => self.activate(),
            KeyCode::Char(' ') => {
                if self.enqueueable(self.cursor) && !self.marked.remove(&self.cursor) {
                    self.marked.insert(self.cursor);
                }
                Vec::new()
            }
            KeyCode::Backspace | KeyCode::Left => self.back(),
            KeyCode::Char('b') | KeyCode::Esc => vec![BrowserEffect::Close],
            _ => Vec::new(),
        }
    }

    /// How many rows the visible list has.
    pub fn len(&self) -> usize {
        match (self.tab, &self.episodes) {
            (BrowserTab::Files, _) => self.entries.len(),
            (BrowserTab::Podcasts, None) => self.feeds.len(),
            (BrowserTab::Podcasts, Some((_, episodes))) => episodes.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether row `index` of the visible list can be marked and enqueued:
    /// an audio file, or an episode with an enclosure.
    pub fn enqueueable(&self, index: usize) -> bool {
        match (self.tab, &self.episodes) {
            (BrowserTab::Files, _) => self
                .entries
                .get(index)
                .is_some_and(|entry| entry.kind == EntryKind::Audio),
            (BrowserTab::Podcasts, None) => false,
            (BrowserTab::Podcasts, Some((_, episodes))) => episodes
                .get(index)
                .is_some_and(|episode| episode.enclosure.is_some()),
        }
    }

    /// Empties the visible list and marks it loading.
    fn start_loading(&mut self) {
        self.cursor = 0;
        self.marked.clear();
        self.loading = true;
        self.error = None;
    }

    fn switch_tab(&mut self) -> Vec<BrowserEffect> {
        self.start_loading();
        let request = match self.tab {
            BrowserTab::Files => {
                self.tab = BrowserTab::Podcasts;
                self.episodes = None;
                self.feeds.clear();
                BrowseRequest::Feeds
            }
            BrowserTab::Podcasts => {
                self.tab = BrowserTab::Files;
                self.entries.clear();
                BrowseRequest::Directory(self.cwd.clone())
            }
        };
        vec![BrowserEffect::Request(request)]
    }

    fn activate(&mut self) -> Vec<BrowserEffect> {
        match self.tab {
            BrowserTab::Files => match self.entries.get(self.cursor) {
                Some(entry) if entry.kind == EntryKind::Directory => {
                    let path = entry.path.clone();
                    self.open_directory(path)
                }
                Some(entry) if entry.kind == EntryKind::Audio => self.enqueue_selection(),
                _ => Vec::new(),
            },
            BrowserTab::Podcasts => match &self.episodes {
                None => match self.feeds.get(self.cursor) {
                    Some(feed) => {
                        let slug = feed.slug.clone();
                        self.start_loading();
                        self.episodes = Some((slug.clone(), Vec::new()));
                        vec![BrowserEffect::Request(BrowseRequest::Episodes { slug })]
                    }
                    None => Vec::new(),
                },
                Some(_) if self.enqueueable(self.cursor) => self.enqueue_selection(),
                Some(_) => Vec::new(),
            },
        }
    }

    fn open_directory(&mut self, path: PathBuf) -> Vec<BrowserEffect> {
        self.start_loading();
        self.entries.clear();
        self.cwd = path.clone();
        vec![BrowserEffect::Request(BrowseRequest::Directory(path))]
    }

    /// The marked rows in listing order, or the cursor's row when nothing is
    /// marked; the marks clear once they are enqueued.
    fn enqueue_selection(&mut self) -> Vec<BrowserEffect> {
        let indices: Vec<usize> = if self.marked.is_empty() {
            vec![self.cursor]
        } else {
            std::mem::take(&mut self.marked).into_iter().collect()
        };
        let items: Vec<EnqueueItem> = indices
            .into_iter()
            .filter(|index| self.enqueueable(*index))
            .filter_map(|index| match (self.tab, &self.episodes) {
                (BrowserTab::Files, _) => self
                    .entries
                    .get(index)
                    .map(|entry| EnqueueItem::Path(entry.path.clone())),
                (BrowserTab::Podcasts, Some((_, episodes))) => episodes
                    .get(index)
                    .map(|episode| EnqueueItem::Episode(episode.clone())),
                (BrowserTab::Podcasts, None) => None,
            })
            .collect();
        if items.is_empty() {
            Vec::new()
        } else {
            vec![BrowserEffect::Enqueue(items)]
        }
    }

    fn back(&mut self) -> Vec<BrowserEffect> {
        match self.tab {
            BrowserTab::Files => match self.cwd.parent() {
                Some(parent) => {
                    let parent = parent.to_path_buf();
                    self.open_directory(parent)
                }
                None => Vec::new(),
            },
            BrowserTab::Podcasts => {
                if let Some((slug, _)) = self.episodes.take() {
                    self.cursor = self
                        .feeds
                        .iter()
                        .position(|feed| feed.slug == slug)
                        .unwrap_or(0);
                    self.marked.clear();
                    self.loading = false;
                    self.error = None;
                }
                Vec::new()
            }
        }
    }
}
