#[path = "support/views.rs"]
mod views;

use std::cell::Cell;
use std::time::Duration;

use continuo::application::transport::PlaybackPhase;
use continuo::application::view::{PersistenceStatus, PlayerView, SavedHistory};
use continuo::queue::{DisplayDuration, DurationSource};
use continuo::tui::layout::{Regions, Tier, regions, tier_for};
use continuo::tui::render::{CoverView, CoverWidget, HitMap, TransportButton, Visuals, draw};
use continuo::tui::state::UiState;
use continuo::tui::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use views::{decoded, ids, playing, view};

fn render(
    view: &PlayerView,
    ui: &UiState,
    visuals: &Visuals<'_>,
    w: u16,
    h: u16,
) -> (Buffer, HitMap) {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("backend");
    let mut hits = HitMap::default();
    terminal
        .draw(|frame| {
            hits = draw(frame, view, ui, visuals);
        })
        .expect("draw");
    (terminal.backend().buffer().clone(), hits)
}

fn screen(buffer: &Buffer) -> String {
    let width = usize::from(buffer.area.width.max(1));
    buffer
        .content()
        .chunks(width)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn text(view: &PlayerView, ui: &UiState, w: u16, h: u16) -> String {
    screen(&render(view, ui, &Visuals::default(), w, h).0)
}

fn within(rect: Rect, area: Rect) -> bool {
    rect.x >= area.x
        && rect.y >= area.y
        && rect.right() <= area.right()
        && rect.bottom() <= area.bottom()
}

fn all_rects(regions: &Regions) -> Vec<Rect> {
    let mut rects = vec![
        regions.status,
        regions.player,
        regions.info,
        regions.transport,
        regions.progress,
        regions.queue,
        regions.footer,
    ];
    rects.extend(regions.cover);
    rects.extend(regions.spectrum);
    rects
}

#[test]
fn tiers_follow_the_smallest_matching_dimension() {
    assert_eq!(tier_for(100, 20), Tier::Compact);
    assert_eq!(tier_for(60, 40), Tier::Compact);
    assert_eq!(tier_for(80, 28), Tier::Normal);
    assert_eq!(tier_for(49, 40), Tier::Minimal);
    assert_eq!(tier_for(120, 17), Tier::Minimal);
    assert_eq!(tier_for(29, 40), Tier::Resize);
    assert_eq!(tier_for(100, 7), Tier::Resize);
}

#[test]
fn regions_are_valid_for_zero_and_tiny_areas() {
    let areas = [
        Rect::new(0, 0, 0, 0),
        Rect::new(0, 0, 1, 1),
        Rect::new(7, 3, 0, 0),
        Rect::new(7, 3, 1, 1),
        Rect::new(7, 3, 2, 3),
        Rect::new(7, 3, 20, 5),
        Rect::new(7, 3, 45, 16),
        Rect::new(7, 3, 100, 20),
        Rect::new(7, 3, 100, 30),
        Rect::new(u16::MAX - 2, u16::MAX - 2, 2, 2),
    ];
    for tier in [Tier::Resize, Tier::Minimal, Tier::Compact, Tier::Normal] {
        for area in areas {
            let regions = regions(area, tier);
            for rect in all_rects(&regions) {
                assert!(within(rect, area), "{tier:?} {area:?}: {rect:?} escapes");
            }
        }
    }
    let ui = UiState::new(true);
    let _ = text(&view(PlaybackPhase::Unloaded, None), &ui, 1, 1);
    let _ = text(&view(PlaybackPhase::Unloaded, None), &ui, 0, 0);
}

#[test]
fn normal_layout_shows_cover_metadata_and_distinct_playing_and_selected_rows() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(185)), false)),
    );
    let mut ui = UiState::new(true);
    ui.selected = Some(v.rows[2].id);
    let screen = text(&v, &ui, 100, 30);
    assert!(
        screen.contains("Harbor"),
        "normal shows secondary metadata\n{screen}"
    );
    assert!(screen.contains("Harbor · Coast"), "{screen}");
    assert!(screen.contains("01:02 / 03:05"), "{screen}");
    assert!(
        screen.contains('░') && screen.contains('♪'),
        "cover placeholder\n{screen}"
    );
    let playing_line = screen
        .lines()
        .find(|l| l.contains("Morning Tide") && l.contains('▶'))
        .expect("playing marker");
    assert!(!playing_line.contains("Done"));
    assert!(
        screen.contains("(01:00:00)"),
        "declared duration is parenthesized\n{screen}"
    );
    assert!(screen.contains("~01:02 saved") && screen.contains("played"));
}

#[test]
fn the_selected_row_is_highlighted_apart_from_the_playing_marker() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(185)), false)),
    );
    let mut ui = UiState::new(true);
    ui.selected = Some(v.rows[2].id);
    let (buffer, hits) = render(&v, &ui, &Visuals::default(), 100, 30);
    let theme = Theme::default();
    let row = |index: usize| {
        hits.rows
            .iter()
            .find(|(_, id)| *id == v.rows[index].id)
            .map(|(rect, _)| *rect)
            .expect("visible row")
    };
    let (selected, active) = (row(2), row(0));
    let cell = |x: u16, y: u16| buffer.cell((x, y)).expect("cell").clone();
    assert_eq!(cell(selected.x + 4, selected.y).bg, theme.green);
    assert_ne!(cell(active.x + 4, active.y).bg, theme.green);
    assert!((active.x..active.right()).any(|x| cell(x, active.y).symbol() == "▶"));
    assert!(!(selected.x..selected.right()).any(|x| cell(x, selected.y).symbol() == "▶"));
}

#[test]
fn the_hit_map_covers_visible_rows_the_progress_bar_and_transport() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(185)), false)),
    );
    let area = Rect::new(0, 0, 100, 30);
    let (_, hits) = render(&v, &UiState::new(true), &Visuals::default(), 100, 30);
    let listed: Vec<_> = hits.rows.iter().map(|(_, id)| *id).collect();
    assert_eq!(listed, v.rows.iter().map(|row| row.id).collect::<Vec<_>>());
    for (rect, _) in &hits.rows {
        assert!(within(*rect, hits.queue) && !rect.is_empty());
    }
    assert!(!hits.progress.is_empty() && within(hits.progress, area));
    let buttons: Vec<_> = hits.buttons.iter().map(|(_, b)| *b).collect();
    assert_eq!(
        buttons,
        [
            TransportButton::Previous,
            TransportButton::PlayPause,
            TransportButton::Stop,
            TransportButton::Next
        ]
    );
    for pair in hits.buttons.windows(2) {
        assert!(!pair[0].0.intersects(pair[1].0));
    }
}

#[test]
fn compact_drops_secondary_metadata_at_mixed_dimensions() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, None, false)),
    );
    for (w, h) in [(100, 20), (60, 40)] {
        let screen = text(&v, &UiState::new(true), w, h);
        assert!(!screen.contains("Coast"), "{w}x{h}\n{screen}");
        assert!(screen.contains("Morning Tide"));
    }
}

#[test]
fn minimal_hides_cover_and_spectrum_but_keeps_title_state_and_queue() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, None, false)),
    );
    let screen = text(&v, &UiState::new(true), 45, 16);
    assert!(
        screen.contains("Morning Tide") && screen.contains("playing") && screen.contains("Done"),
        "{screen}"
    );
    assert!(!screen.contains('░'));
    assert!(!screen.contains('▁'), "no spectrum in minimal\n{screen}");
}

#[test]
fn the_resize_tier_keeps_quit_and_play_hints() {
    let screen = text(
        &view(PlaybackPhase::Unloaded, None),
        &UiState::new(true),
        25,
        6,
    );
    assert!(
        screen.contains("q quit") && screen.contains("space play"),
        "{screen}"
    );
    let wide = text(
        &view(PlaybackPhase::Unloaded, None),
        &UiState::new(true),
        29,
        40,
    );
    assert!(wide.contains("Terminal too small") && wide.contains("space play · q quit"));
}

#[test]
fn unknown_duration_and_estimated_position_are_honest() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, None, true)),
    );
    let screen = text(&v, &UiState::new(true), 100, 30);
    assert!(screen.contains("~01:02 / --:--"), "{screen}");
    assert!(
        !screen.contains('━'),
        "an unknown duration fills nothing\n{screen}"
    );
}

#[test]
fn a_declared_duration_is_parenthesized_and_never_fills_the_bar() {
    let declared = DisplayDuration {
        value: Duration::from_secs(120),
        source: DurationSource::Declared,
    };
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(declared), false)),
    );
    let screen = text(&v, &UiState::new(true), 100, 30);
    assert!(screen.contains("01:02 / (02:00)"), "{screen}");
    assert!(!screen.contains('━'), "{screen}");

    let decoded = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(120)), false)),
    );
    assert!(text(&decoded, &UiState::new(true), 100, 30).contains('━'));
}

#[test]
fn a_saved_but_unloaded_entry_shows_saved_history_not_live_progress() {
    let mut now = playing(ids()[0], false, None, false);
    now.saved = Some(SavedHistory::Position {
        at: Duration::from_secs(123),
        estimated: false,
    });
    let v = view(PlaybackPhase::Unloaded, Some(now));
    let screen = text(&v, &UiState::new(true), 100, 30);
    assert!(screen.contains("02:03 saved"), "{screen}");
    assert!(!screen.contains("01:02 / "));
}

#[test]
fn buffering_and_unsaved_are_labelled() {
    let mut now = playing(ids()[0], true, Some(decoded(185)), false);
    now.buffering = true;
    let mut v = view(PlaybackPhase::Playing, Some(now));
    v.persistence = PersistenceStatus::Unsaved;
    let screen = text(&v, &UiState::new(true), 100, 30);
    assert!(screen.contains("playing buffering"), "{screen}");
    let status = screen.lines().next().expect("status row");
    assert!(
        status.contains("continuo")
            && status.contains("vol 80%")
            && status.contains("mouse on")
            && status.contains("unsaved"),
        "{status}"
    );
}

#[test]
fn empty_loading_and_failed_screens() {
    let mut empty = view(PlaybackPhase::Unloaded, None);
    empty.rows.clear();
    let screen = text(&empty, &UiState::new(true), 100, 30);
    assert!(
        screen.contains("Queue is empty — press b to browse or a to add"),
        "{screen}"
    );
    assert!(
        screen.contains("space play · enter play selected · b browse · a add · ? help · q quit")
    );
    let loading = view(PlaybackPhase::Loading, None);
    assert!(text(&loading, &UiState::new(true), 100, 30).contains("loading"));
    let mut failed = view(PlaybackPhase::LoadFailed, None);
    failed.status = Some("cannot open media \"/x.flac\"".into());
    assert!(text(&failed, &UiState::new(false), 100, 30).contains("cannot open media"));
    assert!(text(&failed, &UiState::new(false), 100, 30).contains("mouse off"));
}

struct Probe(Cell<Option<Rect>>);

impl CoverWidget for Probe {
    fn render_cover(&self, area: Rect, _buffer: &mut Buffer) {
        self.0.set(Some(area));
    }
}

#[test]
fn a_prepared_cover_replaces_the_placeholder_in_the_cover_region() {
    let v = view(
        PlaybackPhase::Playing,
        Some(playing(ids()[0], true, Some(decoded(185)), false)),
    );
    let probe = Probe(Cell::new(None));
    let visuals = Visuals {
        cover: CoverView::Image(&probe),
        ..Default::default()
    };
    let (buffer, _) = render(&v, &UiState::new(true), &visuals, 100, 30);
    let expected = regions(Rect::new(0, 0, 100, 30), Tier::Normal).cover;
    assert_eq!(probe.0.get(), expected);
    assert_eq!(expected.map(|r| (r.width, r.height)), Some((14, 7)));
    assert!(!screen(&buffer).contains('░'));
}

#[test]
fn selection_reconciles_to_the_hint_then_a_surviving_row_then_the_first() {
    let mut v = view(PlaybackPhase::Unloaded, None);
    let mut ui = UiState::new(true);
    ui.reconcile(&v, None);
    assert_eq!(ui.selected, Some(v.rows[0].id));
    ui.reconcile(&v, Some(v.rows[2].id));
    assert_eq!(ui.selected, Some(v.rows[2].id));
    ui.reconcile(&v, None);
    assert_eq!(
        ui.selected,
        Some(v.rows[2].id),
        "a surviving selection stays"
    );
    v.rows.remove(2);
    ui.queue_offset = 9;
    ui.reconcile(&v, None);
    assert_eq!(ui.selected, Some(v.rows[0].id));
    assert!(ui.queue_offset < v.rows.len());
    v.rows.clear();
    ui.reconcile(&v, Some(ids()[1]));
    assert_eq!((ui.selected, ui.queue_offset), (None, 0));
}
