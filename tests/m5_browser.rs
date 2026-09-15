//! The on-demand browser (design doc M5 §8): a one-level directory listing,
//! and the browser's key handling as a pure function over its own state.

use std::path::{Path, PathBuf};

use continuo::application::browse::{
    BrowseRequest, BrowseResult, BrowseWorker, DirEntry, EntryKind, list_directory,
};
use continuo::application::runtime::EnqueueItem;
use continuo::library::{EpisodeCandidate, FeedSummary};
use continuo::media::id::{EpisodeKey, FeedId, MediaId};
use continuo::tui::browser::{BrowserEffect, BrowserState, BrowserTab};
use continuo::tui::render::{Visuals, draw};
use continuo::tui::state::{Overlay, UiState};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Modifier;

#[path = "support/views.rs"]
mod views;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(state: &mut BrowserState, codes: &[KeyCode]) -> Vec<BrowserEffect> {
    codes
        .iter()
        .flat_map(|code| state.handle_key(key(*code)))
        .collect()
}

/// `b.MP3`, `a.flac`, `z/deep.flac` and `notes.txt` in a fresh directory.
fn sample_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
    let root = dir.path().to_path_buf();
    for name in ["b.MP3", "a.flac", "notes.txt"] {
        std::fs::write(root.join(name), b"x").unwrap_or_else(|error| panic!("write: {error}"));
    }
    std::fs::create_dir(root.join("z")).unwrap_or_else(|error| panic!("mkdir: {error}"));
    std::fs::write(root.join("z/deep.flac"), b"x").unwrap_or_else(|error| panic!("write: {error}"));
    (dir, root)
}

fn listed(root: &Path) -> BrowserState {
    let entries = list_directory(root).unwrap_or_else(|error| panic!("list: {error}"));
    let mut state = BrowserState::new(root.to_path_buf());
    state.apply(BrowseResult::Directory {
        path: root.to_path_buf(),
        entries: Ok(entries),
    });
    state
}

#[test]
fn a_listing_is_one_level_directories_first_and_classifies_audio() {
    let (_dir, root) = sample_dir();
    let entries = list_directory(&root).unwrap_or_else(|error| panic!("list: {error}"));
    let expected = [
        ("z", EntryKind::Directory),
        ("a.flac", EntryKind::Audio),
        ("b.MP3", EntryKind::Audio),
        ("notes.txt", EntryKind::Other),
    ]
    .map(|(name, kind)| DirEntry {
        name: name.to_owned(),
        path: root.join(name),
        kind,
    });
    assert_eq!(entries, expected);
    assert!(entries.iter().all(|entry| entry.name != "deep.flac"));
}

#[test]
fn an_unreadable_directory_is_an_error_value() {
    let (_dir, root) = sample_dir();
    assert!(list_directory(&root.join("missing")).is_err());
    assert!(list_directory(&root.join("notes.txt")).is_err());
}

#[test]
fn marked_files_enqueue_together_in_listing_order() {
    let (_dir, root) = sample_dir();
    let mut state = listed(&root);
    let effects = press(
        &mut state,
        &[
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Enter,
        ],
    );
    let (a, b) = (root.join("a.flac"), root.join("b.MP3"));
    assert!(
        matches!(
            &effects[..],
            [BrowserEffect::Enqueue(items)] if matches!(
                &items[..],
                [EnqueueItem::Path(first), EnqueueItem::Path(second)]
                    if *first == a && *second == b
            )
        ),
        "{effects:?}"
    );
    assert!(state.marked.is_empty(), "marks clear after an enqueue");
}

#[test]
fn enter_on_the_cursor_file_enqueues_it_alone_and_marks_skip_other_rows() {
    let (_dir, root) = sample_dir();
    let mut state = listed(&root);
    // A directory and a non-audio file cannot be marked.
    press(&mut state, &[KeyCode::Char(' ')]);
    press(
        &mut state,
        &[
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Down,
            KeyCode::Char(' '),
        ],
    );
    assert!(state.marked.is_empty());
    let effects = press(&mut state, &[KeyCode::Char('k'), KeyCode::Enter]);
    let b = root.join("b.MP3");
    assert!(
        matches!(
            &effects[..],
            [BrowserEffect::Enqueue(items)]
                if matches!(&items[..], [EnqueueItem::Path(only)] if *only == b)
        ),
        "{effects:?}"
    );
}

#[test]
fn enter_on_a_directory_requests_it_and_backspace_returns_to_the_parent() {
    let (_dir, root) = sample_dir();
    let mut state = listed(&root);
    let effects = press(&mut state, &[KeyCode::Enter]);
    let z = root.join("z");
    assert!(
        matches!(&effects[..], [BrowserEffect::Request(BrowseRequest::Directory(path))] if *path == z),
        "{effects:?}"
    );
    assert_eq!(state.cwd, z);
    assert!(state.loading);

    // A late result for the directory just left is not shown.
    state.apply(BrowseResult::Directory {
        path: root.clone(),
        entries: list_directory(&root).map_err(|error| error.to_string()),
    });
    assert!(state.loading);
    assert!(state.entries.is_empty());

    state.apply(BrowseResult::Directory {
        path: z.clone(),
        entries: list_directory(&z).map_err(|error| error.to_string()),
    });
    assert!(!state.loading);
    assert_eq!(state.entries.len(), 1);

    let effects = press(&mut state, &[KeyCode::Backspace]);
    assert!(
        matches!(&effects[..], [BrowserEffect::Request(BrowseRequest::Directory(path))] if *path == root),
        "{effects:?}"
    );
    assert_eq!(state.cwd, root);
    let effects = press(&mut state, &[KeyCode::Left]);
    assert!(
        matches!(
            &effects[..],
            [BrowserEffect::Request(BrowseRequest::Directory(_))]
        ),
        "Left goes up too: {effects:?}"
    );
}

#[test]
fn b_and_esc_close_and_ctrl_or_alt_chords_do_nothing() {
    let (_dir, root) = sample_dir();
    let mut state = listed(&root);
    assert!(matches!(
        &press(&mut state, &[KeyCode::Char('b')])[..],
        [BrowserEffect::Close]
    ));
    assert!(matches!(
        &press(&mut state, &[KeyCode::Esc])[..],
        [BrowserEffect::Close]
    ));
    for modifier in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
        for code in [KeyCode::Char('b'), KeyCode::Char('j'), KeyCode::Char(' ')] {
            assert!(state.handle_key(KeyEvent::new(code, modifier)).is_empty());
        }
    }
    assert_eq!(state.cursor, 0);
    assert!(state.marked.is_empty());
}

#[test]
fn an_error_listing_is_shown_as_a_value() {
    let root = PathBuf::from("/nonexistent/m5-browser");
    let mut state = BrowserState::new(root.clone());
    state.apply(BrowseResult::Directory {
        path: root,
        entries: Err("No such file or directory".to_owned()),
    });
    assert!(!state.loading);
    assert_eq!(state.error.as_deref(), Some("No such file or directory"));
    assert!(press(&mut state, &[KeyCode::Enter, KeyCode::Char(' ')]).is_empty());
}

fn feed(slug: &str) -> FeedSummary {
    FeedSummary {
        slug: slug.to_owned(),
        title: Some(format!("{slug} title")),
        episodes: Some(2),
        last_refreshed_at: None,
    }
}

fn episode(guid: &str, enclosure: Option<&str>) -> EpisodeCandidate {
    EpisodeCandidate {
        media: MediaId::PodcastEpisode {
            feed: FeedId::new("0123456789abcdef0123456789abcdef".into())
                .unwrap_or_else(|error| panic!("feed id: {error}")),
            episode: EpisodeKey::resolve(Some(guid), None, None)
                .unwrap_or_else(|error| panic!("episode key: {error}")),
        },
        enclosure: enclosure.map(|url| url.parse().unwrap_or_else(|error| panic!("url: {error}"))),
        title: Some(guid.to_owned()),
        declared_duration: None,
    }
}

fn guid_of(candidate: &EpisodeCandidate) -> Option<&str> {
    candidate.title.as_deref()
}

#[test]
fn podcasts_tab_lists_feeds_then_episodes_and_skips_unplayable_marks() {
    let (_dir, root) = sample_dir();
    let mut state = listed(&root);
    let effects = press(&mut state, &[KeyCode::Tab]);
    assert!(
        matches!(&effects[..], [BrowserEffect::Request(BrowseRequest::Feeds)]),
        "{effects:?}"
    );
    assert_eq!(state.tab, BrowserTab::Podcasts);
    assert!(state.loading);
    state.apply(BrowseResult::Feeds(Ok(vec![feed("one"), feed("two")])));
    assert!(!state.loading);
    assert_eq!(state.feeds.len(), 2);

    // Feeds are not enqueueable.
    press(&mut state, &[KeyCode::Char(' ')]);
    assert!(state.marked.is_empty());

    let effects = press(&mut state, &[KeyCode::Char('j'), KeyCode::Enter]);
    assert!(
        matches!(&effects[..], [BrowserEffect::Request(BrowseRequest::Episodes { slug })] if slug == "two"),
        "{effects:?}"
    );
    // A late listing for a feed not being viewed is ignored.
    state.apply(BrowseResult::Episodes {
        slug: "one".to_owned(),
        episodes: Ok(vec![episode("stale", Some("https://example.com/s.mp3"))]),
    });
    assert!(state.loading);
    state.apply(BrowseResult::Episodes {
        slug: "two".to_owned(),
        episodes: Ok(vec![
            episode("e1", Some("https://example.com/1.mp3")),
            episode("e2", None),
            episode("e3", Some("https://example.com/3.mp3")),
        ]),
    });
    assert!(!state.loading);
    assert_eq!(
        state
            .episodes
            .as_ref()
            .map(|(slug, list)| (slug.as_str(), list.len())),
        Some(("two", 3))
    );

    // Mark e1, try to mark e2 (no enclosure), mark e3.
    press(
        &mut state,
        &[
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
            KeyCode::Down,
            KeyCode::Char(' '),
        ],
    );
    assert_eq!(state.marked.iter().copied().collect::<Vec<_>>(), vec![0, 2]);
    let effects = press(&mut state, &[KeyCode::Enter]);
    assert!(
        matches!(
            &effects[..],
            [BrowserEffect::Enqueue(items)] if matches!(
                &items[..],
                [EnqueueItem::Episode(first), EnqueueItem::Episode(second)]
                    if guid_of(first) == Some("e1") && guid_of(second) == Some("e3")
            )
        ),
        "{effects:?}"
    );

    // Enter on an unplayable episode with nothing marked does nothing.
    assert!(press(&mut state, &[KeyCode::Up, KeyCode::Enter]).is_empty());

    // Back to the feed list, with the cursor on the feed just left.
    assert!(press(&mut state, &[KeyCode::Backspace]).is_empty());
    assert!(state.episodes.is_none());
    assert_eq!(state.cursor, 1);

    let effects = press(&mut state, &[KeyCode::Tab]);
    assert_eq!(state.tab, BrowserTab::Files);
    assert!(
        matches!(&effects[..], [BrowserEffect::Request(BrowseRequest::Directory(path))] if *path == root),
        "{effects:?}"
    );
}

fn screen(state: &BrowserState) -> (String, ratatui::buffer::Buffer) {
    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap_or_else(|e| panic!("{e}"));
    let mut ui = UiState::new(true);
    ui.overlay = Overlay::Browser;
    let visuals = Visuals {
        browser: Some(state),
        ..Visuals::default()
    };
    terminal
        .draw(|frame| {
            draw(frame, &views::sample_view(), &ui, &visuals);
        })
        .unwrap_or_else(|e| panic!("{e}"));
    let buffer = terminal.backend().buffer().clone();
    let width = usize::from(buffer.area.width);
    let text = buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    (text, buffer)
}

#[test]
fn the_overlay_draws_safe_names_marks_and_dimmed_unplayable_episodes() {
    let root = PathBuf::from("/music");
    let mut state = BrowserState::new(root.clone());
    let (text, _) = screen(&state);
    assert!(text.contains("Loading"), "{text}");

    state.apply(BrowseResult::Directory {
        path: root.clone(),
        entries: Ok(vec![
            DirEntry {
                name: "albums".to_owned(),
                path: root.join("albums"),
                kind: EntryKind::Directory,
            },
            DirEntry {
                name: "evil\u{1b}[2Jname.mp3".to_owned(),
                path: root.join("evil.mp3"),
                kind: EntryKind::Audio,
            },
        ]),
    });
    press(&mut state, &[KeyCode::Down, KeyCode::Char(' ')]);
    let (text, buffer) = screen(&state);
    assert!(text.contains("/music"), "{text}");
    assert!(text.contains("albums/"), "{text}");
    assert!(text.contains("evil\\u{1b}[2Jname.mp3"), "{text}");
    assert!(
        !buffer
            .content()
            .iter()
            .any(|cell| cell.symbol().contains('\u{1b}'))
    );
    assert!(text.contains('●'), "the mark is drawn: {text}");

    press(&mut state, &[KeyCode::Tab]);
    state.apply(BrowseResult::Feeds(Ok(vec![feed("one")])));
    press(&mut state, &[KeyCode::Enter]);
    state.apply(BrowseResult::Episodes {
        slug: "one".to_owned(),
        episodes: Ok(vec![
            episode("playable", Some("https://example.com/1.mp3")),
            episode("gone", None),
        ]),
    });
    let (text, buffer) = screen(&state);
    assert!(text.contains("one title"), "{text}");
    let gone_row = text
        .lines()
        .position(|line| line.contains("gone"))
        .unwrap_or_else(|| panic!("the unplayable episode is listed: {text}"));
    let gone_y = u16::try_from(gone_row).unwrap_or_else(|e| panic!("{e}"));
    let dimmed = (0..buffer.area.width).any(|x| {
        buffer
            .cell((x, gone_y))
            .is_some_and(|cell| cell.symbol() == "g" && cell.modifier.contains(Modifier::DIM))
    });
    assert!(dimmed, "an episode without an enclosure is dimmed");
}

#[test]
fn the_worker_reports_failures_as_error_values() {
    let worker = BrowseWorker::spawn(None);
    let missing = PathBuf::from("/nonexistent/m5-browser-worker");
    worker.request(BrowseRequest::Directory(missing.clone()));
    worker.request(BrowseRequest::Feeds);
    worker.request(BrowseRequest::Episodes {
        slug: "radio-t".to_owned(),
    });
    let mut results = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while results.len() < 3 {
        assert!(std::time::Instant::now() < deadline, "{results:?}");
        match worker.try_result() {
            Some(result) => results.push(result),
            None => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    }
    assert!(
        matches!(
            &results[..],
            [
                BrowseResult::Directory { path, entries: Err(_) },
                BrowseResult::Feeds(Err(_)),
                BrowseResult::Episodes { slug, episodes: Err(_) },
            ] if *path == missing && slug == "radio-t"
        ),
        "{results:?}"
    );
}
