//! Draws a [`PlayerView`] into the regions of its size tier and reports where
//! the clickable parts landed (design doc M5 §7). Every string it prints was
//! already made safe by the view; the renderer only adds fixed labels and
//! formatted times, except the browser overlay, which makes its own
//! filesystem and feed names safe (see [`browser`]).

mod browser;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget, Wrap};

use crate::application::transport::PlaybackPhase;
use crate::application::view::{NowPlaying, PersistenceStatus, PlayerView, QueueRow, format_saved};
use crate::media::display::format_hms;
use crate::queue::{DisplayDuration, DurationSource, QueueEntryId};
use crate::tui::browser::BrowserState;
use crate::tui::layout::{
    Regions, Tier, inset, regions, take_left, take_right, tier_for, visible_rows,
};
use crate::tui::state::{Overlay, UiState};
use crate::tui::theme::Theme;

/// What the frame shows beyond the view: prepared artwork, spectrum levels
/// and the open browser.
#[derive(Default)]
pub struct Visuals<'a> {
    pub cover: CoverView<'a>,
    /// One level per band, 0.0 to 1.0.
    pub spectrum: Option<&'a [f32]>,
    /// Drawn while `UiState::overlay` is `Overlay::Browser`.
    pub browser: Option<&'a BrowserState>,
}

#[derive(Default)]
pub enum CoverView<'a> {
    #[default]
    Placeholder,
    Image(&'a dyn CoverWidget),
}

pub trait CoverWidget {
    fn render_cover(&self, area: Rect, buffer: &mut Buffer);
}

/// Where the last frame put what a mouse can act on.
#[derive(Clone, Debug, Default)]
pub struct HitMap {
    /// Each visible queue row, all of its lines.
    pub rows: Vec<(Rect, QueueEntryId)>,
    pub queue: Rect,
    /// The bar alone, so a click's column maps to a fraction of the track.
    pub progress: Rect,
    pub buttons: Vec<(Rect, TransportButton)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportButton {
    Previous,
    PlayPause,
    Stop,
    Next,
}

const EMPTY_QUEUE: &str = "Queue is empty — press b to browse or a to add";
const KEY_HINTS: &str = "space play · enter play selected · b browse · a add · ? help · q quit";
const TOO_SMALL: &str = "Terminal too small (need 30×8)";
const RESIZE_HINTS: &str = "space play · q quit";
/// Verbatim per the design (§4/§7): confirming discards the queue, not
/// listening history.
const CONFIRM_CLEAR_TEXT: &str = "Clear the queue? Listening history is kept. y to confirm";
/// The §7 key table, one line per row.
const HELP_LINES: [&str; 18] = [
    "Space           Pause/resume; unloaded/ended behavior follows §4",
    "Enter           Play selected queue entry",
    "Up/Down or j/k  Move selection",
    "J/K             Move selected entry down/up",
    "Left/Right      Seek backward/forward 10 seconds",
    "Home            Explicit restart from beginning",
    "- _ / + =       Decrease / increase volume",
    "s / p           Stop / play",
    "[ / ]           Previous / next queue entry",
    "d               Remove selected entry",
    "b               Open/close browser",
    "a               Open path/URL input",
    "c               Request queue clear with confirmation",
    "?               Show help",
    "m               Toggle mouse capture",
    "Ctrl-L          Redraw the whole view",
    "Esc             Close the active overlay or cancel input",
    "q / Ctrl-C      Graceful quit",
];
const NOTHING_PLAYING: &str = "Nothing playing";
const UNKNOWN_TIME: &str = "--:--";
/// Outside the `▁`–`█` block elements, which only the spectrum draws.
const BAR_FILLED: &str = "━";
const BAR_EMPTY: &str = "─";
const LEVELS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// Bars drawn while no analysis is available.
const FLAT_BANDS: usize = 24;
const MARKER_COLUMNS: u16 = 2;
const DURATION_COLUMNS: u16 = 10;
const SAVED_COLUMNS: u16 = 16;
/// The row width below which the saved column is dropped.
const SAVED_MIN_ROW: u16 = 44;
/// The row width below which the duration column is dropped.
const DURATION_MIN_ROW: u16 = 24;

pub fn draw(
    frame: &mut Frame<'_>,
    view: &PlayerView,
    ui: &UiState,
    visuals: &Visuals<'_>,
) -> HitMap {
    let area = frame.area();
    let tier = tier_for(area.width, area.height);
    let regions = regions(area, tier);
    let theme = Theme::default();
    let buffer = frame.buffer_mut();
    buffer.set_style(area, Style::new().bg(theme.background).fg(theme.text));

    if tier == Tier::Resize {
        draw_resize(buffer, &regions, &theme);
        return HitMap::default();
    }

    draw_status(buffer, regions.status, view, ui, &theme);
    if tier != Tier::Minimal {
        bordered(&theme).render(regions.player, buffer);
    }
    if let Some(cover) = regions.cover {
        match visuals.cover {
            CoverView::Placeholder => draw_cover_placeholder(buffer, cover, &theme),
            CoverView::Image(widget) => widget.render_cover(cover, buffer),
        }
    }
    draw_info(buffer, &regions, view, tier, &theme);
    if let Some(spectrum) = regions.spectrum {
        draw_spectrum(buffer, spectrum, visuals.spectrum, &theme);
    }
    let buttons = draw_transport(buffer, regions.transport, view, tier, &theme);
    let progress = draw_progress(buffer, regions.progress, view, tier, &theme);
    let rows = draw_queue(buffer, regions.queue, view, ui, tier, &theme);
    draw_footer(buffer, regions.footer, view, &theme);
    draw_overlay(buffer, area, ui, visuals.browser, &theme);
    HitMap {
        rows,
        queue: regions.queue,
        progress,
        buttons,
    }
}

/// The help, confirm, input and browser overlays float over everything else
/// the frame drew. The browser overlay needs the browser itself; without it
/// there is nothing to draw.
fn draw_overlay(
    buffer: &mut Buffer,
    area: Rect,
    ui: &UiState,
    browser: Option<&BrowserState>,
    theme: &Theme,
) {
    match ui.overlay {
        Overlay::Help => draw_help_overlay(buffer, area, theme),
        Overlay::ConfirmClear => draw_confirm_overlay(buffer, area, theme),
        Overlay::Input => draw_input_overlay(buffer, area, &ui.input, theme),
        Overlay::Browser => {
            if let Some(browser) = browser {
                browser::draw_browser(buffer, area, browser, theme);
            }
        }
        Overlay::None => {}
    }
}

/// A box centred in `area`, clamped to fit it even when `area` is smaller
/// than the requested size.
fn centered_box(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x.saturating_add((area.width - width) / 2),
        y: area.y.saturating_add((area.height - height) / 2),
        width,
        height,
    }
}

fn draw_confirm_overlay(buffer: &mut Buffer, area: Rect, theme: &Theme) {
    let width = u16::try_from(CONFIRM_CLEAR_TEXT.len() + 4)
        .unwrap_or(u16::MAX)
        .min(area.width);
    let box_area = centered_box(area, width, 3);
    Clear.render(box_area, buffer);
    Block::bordered()
        .title(" confirm ")
        .border_style(Style::new().fg(theme.amber))
        .style(Style::new().bg(theme.background))
        .render(box_area, buffer);
    Paragraph::new(CONFIRM_CLEAR_TEXT)
        .style(Style::new().fg(theme.cream))
        .wrap(Wrap { trim: true })
        .render(inset(box_area), buffer);
}

fn draw_help_overlay(buffer: &mut Buffer, area: Rect, theme: &Theme) {
    let content_width = HELP_LINES.iter().map(|line| line.len()).max().unwrap_or(0);
    let width = u16::try_from(content_width + 4)
        .unwrap_or(u16::MAX)
        .min(area.width);
    let height = u16::try_from(HELP_LINES.len() + 2)
        .unwrap_or(u16::MAX)
        .min(area.height);
    let box_area = centered_box(area, width, height);
    Clear.render(box_area, buffer);
    Block::bordered()
        .title(" help — ? or Esc to close ")
        .border_style(Style::new().fg(theme.line))
        .style(Style::new().bg(theme.background))
        .render(box_area, buffer);
    Paragraph::new(HELP_LINES.join("\n"))
        .style(Style::new().fg(theme.text))
        .render(inset(box_area), buffer);
}

/// Typed text reaches this overlay only through `tui::input`, which already
/// drops raw control characters before they land in `ui.input`, so the
/// string drawn here is always inert.
fn draw_input_overlay(buffer: &mut Buffer, area: Rect, input: &str, theme: &Theme) {
    let width = area.width.min(60);
    let box_area = centered_box(area, width, 3);
    Clear.render(box_area, buffer);
    Block::bordered()
        .title(" add path or URL — Enter to add, Esc to cancel ")
        .border_style(Style::new().fg(theme.green))
        .style(Style::new().bg(theme.background))
        .render(box_area, buffer);
    Paragraph::new(format!("{input}▏"))
        .style(Style::new().fg(theme.cream))
        .render(inset(box_area), buffer);
}

fn bordered(theme: &Theme) -> Block<'static> {
    Block::bordered().border_style(Style::new().fg(theme.line))
}

fn draw_resize(buffer: &mut Buffer, regions: &Regions, theme: &Theme) {
    let message = regions.info;
    let middle = Rect {
        y: message.y.saturating_add(message.height / 2),
        height: message.height.min(1),
        ..message
    };
    let too_small = Line::styled(TOO_SMALL, Style::new().fg(theme.cream));
    centred(too_small, middle).render(middle, buffer);
    let hints = Line::styled(RESIZE_HINTS, Style::new().fg(theme.muted));
    centred(hints, regions.footer).render(regions.footer, buffer);
}

/// Centred when it fits; otherwise left-aligned, so a cut keeps the start of
/// the line instead of losing both ends.
fn centred(line: Line<'_>, area: Rect) -> Line<'_> {
    if line.width() <= usize::from(area.width) {
        line.alignment(Alignment::Center)
    } else {
        line
    }
}

fn draw_status(buffer: &mut Buffer, area: Rect, view: &PlayerView, ui: &UiState, theme: &Theme) {
    let mut rest = area;
    let brand = Line::styled(
        "continuo",
        Style::new().fg(theme.green).add_modifier(Modifier::BOLD),
    );
    let brand_columns = u16::try_from(brand.width()).unwrap_or(u16::MAX);
    brand.render(take_left(&mut rest, brand_columns), buffer);
    // A gap after the name; on a narrow row the right side loses its start
    // rather than writing over the name.
    take_left(&mut rest, 1);
    let muted = Style::new().fg(theme.muted);
    let mouse = if ui.mouse_capture { "on" } else { "off" };
    let mut spans = vec![Span::styled(
        format!("vol {}% · mouse {mouse}", view.volume.percent()),
        muted,
    )];
    let persistence = match view.persistence {
        PersistenceStatus::Saving => None,
        PersistenceStatus::Unsaved => Some("unsaved"),
        PersistenceStatus::Failing => Some("not saving"),
    };
    if let Some(label) = persistence {
        spans.push(Span::styled(" · ", muted));
        spans.push(Span::styled(label, Style::new().fg(theme.amber)));
    }
    Line::from(spans)
        .alignment(Alignment::Right)
        .render(rest, buffer);
}

/// A stable stand-in until artwork is prepared: a shaded square with a note.
fn draw_cover_placeholder(buffer: &mut Buffer, area: Rect, theme: &Theme) {
    let shade = Style::new().fg(theme.line).bg(theme.panel);
    for y in area.top()..area.bottom() {
        Line::styled("░".repeat(usize::from(area.width)), shade).render(row(area, y), buffer);
    }
    let centre = (
        area.x.saturating_add(area.width / 2),
        area.y.saturating_add(area.height / 2),
    );
    if !area.is_empty()
        && let Some(cell) = buffer.cell_mut(centre)
    {
        cell.set_symbol("♪").set_fg(theme.amber);
    }
}

fn draw_info(buffer: &mut Buffer, regions: &Regions, view: &PlayerView, tier: Tier, theme: &Theme) {
    let area = regions.info;
    match &view.now_playing {
        Some(now) => {
            Line::styled(
                now.title.as_str(),
                Style::new().fg(theme.cream).add_modifier(Modifier::BOLD),
            )
            .render(area, buffer);
            let secondary: Vec<&str> = [now.artist.as_deref(), now.album.as_deref()]
                .into_iter()
                .flatten()
                .collect();
            if tier == Tier::Normal && !secondary.is_empty() {
                Line::styled(secondary.join(" · "), Style::new().fg(theme.muted))
                    .render(row(area, area.y.saturating_add(1)), buffer);
            }
        }
        None => Line::styled(NOTHING_PLAYING, Style::new().fg(theme.muted)).render(area, buffer),
    }
}

/// Flat bars without analysis; otherwise each band's level in eighths of a
/// row, never below the floor glyph, alternating green and cyan.
fn draw_spectrum(buffer: &mut Buffer, area: Rect, levels: Option<&[f32]>, theme: &Theme) {
    let levels = levels.filter(|levels| !levels.is_empty());
    let bands = levels.map_or(FLAT_BANDS, <[f32]>::len);
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    if width == 0 || height == 0 {
        return;
    }
    for (column, x) in (area.left()..area.right()).enumerate() {
        let band = if bands <= width {
            let slot = width / bands;
            if slot > 1 && column % slot == slot - 1 {
                continue;
            }
            column / slot
        } else {
            column * bands / width
        };
        if band >= bands {
            continue;
        }
        let level = levels
            .and_then(|levels| levels.get(band))
            .copied()
            .filter(|level| level.is_finite())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        // The float is clamped to [0, height × 8] before the cast.
        let eighths = ((level * (height * 8) as f32).round() as usize).max(1);
        let color = if band % 2 == 0 {
            theme.green
        } else {
            theme.cyan
        };
        for (from_bottom, y) in (area.top()..area.bottom()).rev().enumerate() {
            let fill = eighths.saturating_sub(from_bottom * 8).min(8);
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_symbol(LEVELS[fill]).set_fg(color);
            }
        }
    }
}

fn draw_transport(
    buffer: &mut Buffer,
    area: Rect,
    view: &PlayerView,
    tier: Tier,
    theme: &Theme,
) -> Vec<(Rect, TransportButton)> {
    let play_pause = if view.phase == PlaybackPhase::Playing {
        " Ⅱ "
    } else {
        " ▶ "
    };
    let labels = [
        (" |◀ ", TransportButton::Previous),
        (play_pause, TransportButton::PlayPause),
        (" ■ ", TransportButton::Stop),
        (" ▶| ", TransportButton::Next),
    ];
    let mut buttons = Vec::with_capacity(labels.len());
    let mut x = area.x;
    for (label, button) in labels {
        let line = Line::styled(label, Style::new().fg(button_color(button, theme)));
        let width = u16::try_from(line.width()).unwrap_or(u16::MAX);
        let rect = Rect { x, width, ..area }.intersection(area);
        line.render(rect, buffer);
        if !rect.is_empty() {
            buttons.push((rect, button));
        }
        x = x.saturating_add(width).saturating_add(1);
    }
    if tier != Tier::Minimal {
        let rest = Rect {
            x: x.saturating_add(1),
            ..area
        }
        .intersection(area);
        state_line(view, theme).render(rest, buffer);
    }
    buttons
}

fn button_color(button: TransportButton, theme: &Theme) -> Color {
    match button {
        TransportButton::PlayPause => theme.green,
        _ => theme.text,
    }
}

/// The transport phase as a word, with ` buffering` while it waits on data.
fn state_line(view: &PlayerView, theme: &Theme) -> Line<'static> {
    let (label, color) = match view.phase {
        PlaybackPhase::Unloaded => ("idle", theme.muted),
        PlaybackPhase::Loading => ("loading", theme.amber),
        PlaybackPhase::LoadFailed => ("failed", theme.amber),
        PlaybackPhase::Playing => ("playing", theme.green),
        PlaybackPhase::Paused => ("paused", theme.cyan),
        PlaybackPhase::Stopped => ("stopped", theme.muted),
        PlaybackPhase::Ended => ("ended", theme.muted),
    };
    let mut spans = vec![Span::styled(label, Style::new().fg(color))];
    if view.now_playing.as_ref().is_some_and(|now| now.buffering) {
        spans.push(Span::styled(" buffering", Style::new().fg(theme.amber)));
    }
    Line::from(spans)
}

/// Draws the time label and the bar after it; returns the bar's rectangle.
fn draw_progress(
    buffer: &mut Buffer,
    area: Rect,
    view: &PlayerView,
    tier: Tier,
    theme: &Theme,
) -> Rect {
    let mut spans = Vec::new();
    if tier == Tier::Minimal {
        spans.extend(state_line(view, theme).spans);
        spans.push(Span::raw("  "));
    }
    let (label, ratio) = progress_label(view.now_playing.as_ref());
    spans.push(Span::styled(label, Style::new().fg(theme.text)));
    spans.push(Span::raw(" "));
    let line = Line::from(spans);
    let used = u16::try_from(line.width()).unwrap_or(u16::MAX);
    line.render(area, buffer);

    let bar = Rect {
        x: area.x.saturating_add(used),
        width: area.width.saturating_sub(used),
        ..area
    }
    .intersection(area);
    let width = usize::from(bar.width);
    // `ratio` is within [0, 1], so the product is within [0, width].
    let filled = ratio.map_or(0, |ratio| (ratio * width as f64).round() as usize);
    Line::from(vec![
        Span::styled(BAR_FILLED.repeat(filled), Style::new().fg(theme.green)),
        Span::styled(
            BAR_EMPTY.repeat(width.saturating_sub(filled)),
            Style::new().fg(theme.line),
        ),
    ])
    .render(bar, buffer);
    bar
}

/// The time label and how much of the bar it fills. Only a loaded entry with
/// a decoder-confirmed duration fills anything; before loading the label is
/// the saved history instead of a position.
fn progress_label(now: Option<&NowPlaying>) -> (String, Option<f64>) {
    let Some(now) = now else {
        return (format!("{UNKNOWN_TIME} / {UNKNOWN_TIME}"), None);
    };
    let duration = now.duration.map(duration_label);
    if !now.loaded {
        let saved = now
            .saved
            .map_or_else(|| UNKNOWN_TIME.to_owned(), format_saved);
        let label = match duration {
            Some(duration) => format!("{saved} · {duration}"),
            None => saved,
        };
        return (label, None);
    }
    let mark = if now.estimated_position { "~" } else { "" };
    let label = format!(
        "{mark}{} / {}",
        clock(now.position),
        duration.as_deref().unwrap_or(UNKNOWN_TIME)
    );
    let ratio = match now.duration {
        Some(DisplayDuration {
            value,
            source: DurationSource::Decoded(_),
        }) if !value.is_zero() => {
            Some((now.position.as_secs_f64() / value.as_secs_f64()).clamp(0.0, 1.0))
        }
        _ => None,
    };
    (label, ratio)
}

/// `mm:ss` under an hour, `HH:MM:SS` from then on.
fn clock(duration: std::time::Duration) -> String {
    let full = format_hms(duration);
    if let Some(short) = full.strip_prefix("00:") {
        return short.to_owned();
    }
    full
}

/// A declared duration is the feed's claim, not the decoder's, so it is
/// shown in parentheses.
fn duration_label(duration: DisplayDuration) -> String {
    match duration.source {
        DurationSource::Decoded(_) => clock(duration.value),
        DurationSource::Declared => format!("({})", clock(duration.value)),
    }
}

fn draw_queue(
    buffer: &mut Buffer,
    area: Rect,
    view: &PlayerView,
    ui: &UiState,
    tier: Tier,
    theme: &Theme,
) -> Vec<(Rect, QueueEntryId)> {
    let body = if tier == Tier::Minimal {
        area
    } else {
        bordered(theme)
            .title(Line::styled(
                format!(" queue {:02} ", view.rows.len()),
                Style::new().fg(theme.muted),
            ))
            .render(area, buffer);
        inset(area)
    };
    if view.rows.is_empty() {
        Line::styled(EMPTY_QUEUE, Style::new().fg(theme.muted)).render(body, buffer);
        return Vec::new();
    }

    let two_line = tier == Tier::Normal;
    let row_height: u16 = if two_line { 2 } else { 1 };
    let selected = ui
        .selected
        .and_then(|id| view.rows.iter().position(|row| row.id == id));
    let capacity = usize::from(body.height / row_height);
    let window = visible_rows(view.rows.len(), ui.queue_offset, selected, capacity);
    let mut hits = Vec::with_capacity(window.len());
    let mut y = body.y;
    for index in window {
        let Some(entry) = view.rows.get(index) else {
            break;
        };
        let rect = Rect {
            y,
            height: row_height,
            ..body
        }
        .intersection(body);
        draw_queue_row(
            buffer,
            rect,
            entry,
            view.active == Some(entry.id),
            selected == Some(index),
            two_line,
            theme,
        );
        hits.push((rect, entry.id));
        y = y.saturating_add(row_height);
    }
    hits
}

/// One row: playing marker, title (and in the normal tier its artist and
/// album beneath), duration, saved history. Narrow rows drop the saved
/// column, then the duration, before the title.
fn draw_queue_row(
    buffer: &mut Buffer,
    rect: Rect,
    entry: &QueueRow,
    playing: bool,
    selected: bool,
    two_line: bool,
    theme: &Theme,
) {
    let (base, accent, muted) = if selected {
        let on_green = Style::new().bg(theme.green).fg(theme.background);
        (on_green, on_green, on_green)
    } else {
        (
            Style::new().fg(theme.text),
            Style::new().fg(theme.green),
            Style::new().fg(theme.muted),
        )
    };
    buffer.set_style(rect, base);

    let mut rest = rect;
    let marker = take_left(&mut rest, MARKER_COLUMNS);
    if playing {
        Line::styled("▶", accent).render(marker, buffer);
    }
    let saved = column(&mut rest, SAVED_COLUMNS, SAVED_MIN_ROW);
    let duration = column(&mut rest, DURATION_COLUMNS, DURATION_MIN_ROW);

    Line::styled(entry.title.as_str(), base).render(rest, buffer);
    if two_line && let Some(subtitle) = &entry.subtitle {
        Line::styled(subtitle.as_str(), muted).render(row(rest, rest.y.saturating_add(1)), buffer);
    }
    if let Some(value) = entry.duration {
        Line::styled(duration_label(value), base)
            .alignment(Alignment::Right)
            .render(row(duration, duration.y), buffer);
    }
    if let Some(history) = entry.saved {
        Line::styled(format_saved(history), muted)
            .alignment(Alignment::Right)
            .render(row(saved, saved.y), buffer);
    }
}

fn draw_footer(buffer: &mut Buffer, area: Rect, view: &PlayerView, theme: &Theme) {
    match &view.status {
        Some(status) => Line::styled(status.as_str(), Style::new().fg(theme.amber)),
        None => Line::styled(KEY_HINTS, Style::new().fg(theme.muted)),
    }
    .render(area, buffer);
}

/// A right-hand column of `columns` plus a one-cell gap before it, taken
/// only while the row is at least `min_row` wide; otherwise an empty rect.
fn column(rest: &mut Rect, columns: u16, min_row: u16) -> Rect {
    if rest.width < min_row {
        return Rect { width: 0, ..*rest };
    }
    let column = take_right(rest, columns);
    take_right(rest, 1);
    column
}

/// Row `y` of `area`, empty when `y` is outside it.
fn row(area: Rect, y: u16) -> Rect {
    Rect {
        y,
        height: 1,
        ..area
    }
    .intersection(area)
}
