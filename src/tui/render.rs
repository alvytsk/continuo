//! Draws a [`PlayerView`] into the regions of its size tier and reports where
//! the clickable parts landed (design doc M5 §7). Every string it prints was
//! already made safe by the view; the renderer only adds fixed labels and
//! formatted times, except the browser overlay, which makes its own
//! filesystem and feed names safe (see `browser`).

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
use crate::playback::provenance::PositionProvenance;
use crate::queue::{DisplayDuration, DurationSource, QueueEntryId};
use crate::tui::browser::BrowserState;
use crate::tui::layout::{
    Regions, Tier, inset, queue_body, regions, take_left, take_right, tier_for, visible_rows,
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
const BRAND: &str = "CONTINUO";
/// Key, then what it does; drawn as bold key and muted label.
const KEY_HINTS: [(&str, &str); 6] = [
    ("Space", "Play/Pause"),
    ("Enter", "Play selected"),
    ("b", "Browse"),
    ("a", "Add"),
    ("?", "Help"),
    ("q", "Quit"),
];
const TOO_SMALL: &str = "Terminal too small (need 30×8)";
const RESIZE_HINTS: &str = "space play · q quit";
/// Verbatim per the design (§4/§7): confirming discards the queue, not
/// listening history.
const CONFIRM_CLEAR_TEXT: &str = "Clear the queue? Listening history is kept. y to confirm";
/// The §7 key table, one line per row.
const HELP_LINES: [&str; 19] = [
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
    "  in Podcasts   a subscribe · r/R refresh one/all · d unsubscribe",
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
/// The played part of the bar in green, the rest in the line colour.
const BAR_FILLED: &str = "━";
const BAR_EMPTY: &str = "─";
const LEVELS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
/// Bars drawn while no analysis is available.
const FLAT_BANDS: usize = 24;
/// A band at or above this level gets an amber cap on its top cell.
const PEAK_LEVEL: f32 = 0.8;
const VOLUME_COLUMNS: usize = 10;
const MARKER_COLUMNS: u16 = 2;
const NUMBER_COLUMNS: u16 = 4;
/// Rows below the last entry, like an editor past the end of a file.
const FILLER: &str = "~";
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
    buffer.set_style(area, Style::new().fg(theme.text));

    if tier == Tier::Resize {
        draw_resize(buffer, &regions, &theme);
        return HitMap::default();
    }

    if tier != Tier::Minimal {
        bordered(&theme).render(regions.player, buffer);
    }
    draw_status(buffer, regions.status, view, ui, tier, &theme);
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
    let progress = draw_progress(buffer, &regions, view, tier, &theme);
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

/// The brand on the left and the session flags on the right. The volume
/// lives here except in the normal tier, whose transport row has a slider.
fn draw_status(
    buffer: &mut Buffer,
    area: Rect,
    view: &PlayerView,
    ui: &UiState,
    tier: Tier,
    theme: &Theme,
) {
    let mut rest = area;
    let brand = Line::styled(
        format!(" {BRAND} "),
        Style::new().fg(theme.cream).add_modifier(Modifier::BOLD),
    );
    let brand_columns = u16::try_from(brand.width()).unwrap_or(u16::MAX);
    brand.render(take_left(&mut rest, brand_columns), buffer);
    // A gap after the name; on a narrow row the right side loses its start
    // rather than writing over the name.
    take_left(&mut rest, 1);
    let muted = Style::new().fg(theme.muted);
    let mouse = if ui.mouse_capture { "on" } else { "off" };
    let flags = if tier == Tier::Normal {
        format!(" mouse {mouse} ")
    } else {
        format!(" vol {}% · mouse {mouse} ", view.volume.percent())
    };
    let mut spans = vec![Span::styled(flags, muted)];
    let persistence = match view.persistence {
        PersistenceStatus::Saving => None,
        PersistenceStatus::Unsaved => Some("unsaved"),
        PersistenceStatus::Failing => Some("not saving"),
    };
    if let Some(label) = persistence {
        spans.insert(1, Span::styled("· ", muted));
        spans.insert(
            2,
            Span::styled(format!("{label} "), Style::new().fg(theme.amber)),
        );
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

/// The title, and in the normal tier the artist beneath it and then the
/// album with its year.
fn draw_info(buffer: &mut Buffer, regions: &Regions, view: &PlayerView, tier: Tier, theme: &Theme) {
    let area = regions.info;
    let Some(now) = &view.now_playing else {
        Line::styled(NOTHING_PLAYING, Style::new().fg(theme.muted)).render(area, buffer);
        return;
    };
    Line::styled(
        now.title.as_str(),
        Style::new().fg(theme.cream).add_modifier(Modifier::BOLD),
    )
    .render(row(area, area.y), buffer);
    if tier != Tier::Normal {
        return;
    }
    if let Some(artist) = &now.artist {
        Line::styled(artist.as_str(), Style::new().fg(theme.text))
            .render(row(area, area.y.saturating_add(1)), buffer);
    }
    let release: Vec<&str> = [now.album.as_deref(), now.year.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    if !release.is_empty() {
        Line::styled(release.join(" · "), Style::new().fg(theme.muted))
            .render(row(area, area.y.saturating_add(2)), buffer);
    }
}

/// Flat bars without analysis; otherwise each bar's level in eighths of a
/// row, never below the floor glyph, with an amber cap on a loud bar. Bars
/// are equally wide with a one-column gap between them: one per band when
/// the width allows, otherwise as many as fit, each sampling the nearest
/// band. When every bar is at least two columns wide and the area at least
/// three rows tall, the bottom row numbers the bands instead.
fn draw_spectrum(buffer: &mut Buffer, area: Rect, levels: Option<&[f32]>, theme: &Theme) {
    let levels = levels.filter(|levels| !levels.is_empty());
    let bands = levels.map_or(FLAT_BANDS, <[f32]>::len);
    let width = usize::from(area.width);
    let bars_count = bands.min(width.div_ceil(2));
    if width == 0 || bars_count == 0 {
        return;
    }
    let slot = (width + 1) / bars_count;
    let labelled = slot >= 3 && bars_count == bands && area.height >= 3;
    let bars = Rect {
        height: area.height.saturating_sub(u16::from(labelled)),
        ..area
    };
    let height = usize::from(bars.height);
    if height == 0 {
        return;
    }
    let labels = Style::new().fg(theme.muted);
    for (column, x) in (area.left()..area.right()).enumerate() {
        let bar = column / slot;
        if bar >= bars_count || column % slot == slot - 1 {
            continue;
        }
        let band = bar * bands / bars_count;
        if labelled && column % slot == 0 {
            let number = format!("{:02}", band + 1);
            let cell = Rect {
                x,
                y: bars.bottom(),
                width: 2,
                height: 1,
            }
            .intersection(area);
            Line::styled(number, labels).render(cell, buffer);
        }
        let level = levels
            .and_then(|levels| levels.get(band))
            .copied()
            .filter(|level| level.is_finite())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        // The float is clamped to [0, height × 8] before the cast.
        let eighths = ((level * (height * 8) as f32).round() as usize).max(1);
        let top = (eighths - 1) / 8;
        for (from_bottom, y) in (bars.top()..bars.bottom()).rev().enumerate() {
            let fill = eighths.saturating_sub(from_bottom * 8).min(8);
            let color = if from_bottom == top && level >= PEAK_LEVEL {
                theme.amber
            } else {
                theme.green
            };
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
        "[||]"
    } else {
        "[>]"
    };
    let labels = [
        ("[|<]", TransportButton::Previous),
        (play_pause, TransportButton::PlayPause),
        ("[■]", TransportButton::Stop),
        ("[>|]", TransportButton::Next),
    ];
    let mut rest = area;
    if tier == Tier::Normal {
        draw_volume(buffer, &mut rest, view, theme);
    }
    let mut buttons = Vec::with_capacity(labels.len());
    for (label, button) in labels {
        let line = Line::styled(label, Style::new().fg(button_color(button, theme)));
        let width = u16::try_from(line.width()).unwrap_or(u16::MAX);
        let rect = take_left(&mut rest, width);
        line.render(rect, buffer);
        if !rect.is_empty() {
            buttons.push((rect, button));
        }
        take_left(&mut rest, 2);
    }
    if tier != Tier::Minimal {
        take_left(&mut rest, 1);
        state_line(view, theme).render(rest, buffer);
    }
    buttons
}

/// `VOL ━━━━━━━───  70%` at the right end of the row, taken off `rest`.
fn draw_volume(buffer: &mut Buffer, rest: &mut Rect, view: &PlayerView, theme: &Theme) {
    let percent = view.volume.percent();
    let filled = (usize::from(percent) * VOLUME_COLUMNS)
        .div_ceil(100)
        .min(VOLUME_COLUMNS);
    let line = Line::from(vec![
        Span::styled("VOL ", Style::new().fg(theme.muted)),
        Span::styled(BAR_FILLED.repeat(filled), Style::new().fg(theme.text)),
        Span::styled(
            BAR_EMPTY.repeat(VOLUME_COLUMNS - filled),
            Style::new().fg(theme.line),
        ),
        Span::styled(format!("  {percent:>3}%"), Style::new().fg(theme.text)),
    ]);
    let width = u16::try_from(line.width()).unwrap_or(u16::MAX);
    let slider = take_right(rest, width);
    take_right(rest, 2);
    line.render(slider, buffer);
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

/// Draws the time label and the bar; returns the bar's rectangle. With a
/// time row of its own the label goes there, with the phase glyph before
/// it, and the bar fills its row between brackets; otherwise the bar follows
/// the label on the shared row.
fn draw_progress(
    buffer: &mut Buffer,
    regions: &Regions,
    view: &PlayerView,
    tier: Tier,
    theme: &Theme,
) -> Rect {
    let (label, ratio) = progress_label(view.now_playing.as_ref());
    let mut bar = regions.progress;
    if regions.time == regions.progress {
        let mut spans = Vec::new();
        if tier == Tier::Minimal {
            spans.extend(state_line(view, theme).spans);
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(label, Style::new().fg(theme.text)));
        spans.push(Span::raw(" "));
        let line = Line::from(spans);
        let used = u16::try_from(line.width()).unwrap_or(u16::MAX);
        line.render(bar, buffer);
        take_left(&mut bar, used);
    } else {
        let glyph = match view.phase {
            PlaybackPhase::Playing => "▶ ",
            PlaybackPhase::Paused => "Ⅱ ",
            _ => "",
        };
        Line::from(vec![
            Span::styled(glyph, Style::new().fg(theme.green)),
            Span::styled(label, Style::new().fg(theme.green)),
        ])
        .render(regions.time, buffer);
        let bracket = Style::new().fg(theme.muted);
        Line::styled("[", bracket).render(take_left(&mut bar, 1), buffer);
        Line::styled("]", bracket).render(take_right(&mut bar, 1), buffer);
    }
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
/// shown in parentheses; a decoded one derived from a byte-offset estimate
/// carries the same `~` an estimated position does.
fn duration_label(duration: DisplayDuration) -> String {
    match duration.source {
        DurationSource::Decoded(PositionProvenance::Established) => clock(duration.value),
        DurationSource::Decoded(PositionProvenance::Estimated) => {
            format!("~{}", clock(duration.value))
        }
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
        let count = match view.rows.len() {
            1 => " 1 track ".to_owned(),
            n => format!(" {n} tracks "),
        };
        bordered(theme)
            .title(Line::styled(
                " PLAYLIST ",
                Style::new().fg(theme.cream).add_modifier(Modifier::BOLD),
            ))
            .title(Line::styled(count, Style::new().fg(theme.muted)).right_aligned())
            .render(area, buffer);
        queue_body(area)
    };
    if view.rows.is_empty() {
        Line::styled(EMPTY_QUEUE, Style::new().fg(theme.muted)).render(body, buffer);
        return Vec::new();
    }

    let selected = ui
        .selected
        .and_then(|id| view.rows.iter().position(|row| row.id == id));
    let capacity = usize::from(body.height);
    let window = visible_rows(view.rows.len(), ui.queue_offset, selected, capacity);
    let mut hits = Vec::with_capacity(window.len());
    let mut y = body.y;
    for index in window {
        let Some(entry) = view.rows.get(index) else {
            break;
        };
        let rect = row(body, y);
        draw_queue_row(
            buffer,
            rect,
            index,
            entry,
            view.active == Some(entry.id),
            selected == Some(index),
            theme,
        );
        hits.push((rect, entry.id));
        y = y.saturating_add(1);
    }
    if tier != Tier::Minimal {
        while y < body.bottom() {
            Line::styled(FILLER, Style::new().fg(theme.line)).render(row(body, y), buffer);
            y += 1;
        }
    }
    hits
}

/// One row: playing marker, number, title, duration, saved history. Narrow
/// rows drop the saved column, then the duration, before the title.
fn draw_queue_row(
    buffer: &mut Buffer,
    rect: Rect,
    index: usize,
    entry: &QueueRow,
    playing: bool,
    selected: bool,
    theme: &Theme,
) {
    let (base, accent, muted) = if selected {
        let on_amber = Style::new().bg(theme.amber).fg(theme.ink);
        (on_amber.add_modifier(Modifier::BOLD), on_amber, on_amber)
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
    let number = take_left(&mut rest, NUMBER_COLUMNS);
    Line::styled(format!("{:02}", index + 1), muted).render(number, buffer);
    let saved = column(&mut rest, SAVED_COLUMNS, SAVED_MIN_ROW);
    let duration = column(&mut rest, DURATION_COLUMNS, DURATION_MIN_ROW);

    Line::styled(entry.title.as_str(), base).render(rest, buffer);
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
    let line = match &view.status {
        Some(status) => Line::styled(status.as_str(), Style::new().fg(theme.amber)),
        None => {
            let key = Style::new().fg(theme.cream).add_modifier(Modifier::BOLD);
            let label = Style::new().fg(theme.muted);
            let mut spans = vec![Span::raw(" ")];
            for (name, action) in KEY_HINTS {
                spans.push(Span::styled(name, key));
                spans.push(Span::styled(format!(" {action}   "), label));
            }
            Line::from(spans)
        }
    };
    line.render(area, buffer);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spectrum_fills_the_width_with_evenly_spaced_bars() {
        let area = Rect::new(0, 0, 38, 4);
        let mut buffer = Buffer::empty(area);
        draw_spectrum(&mut buffer, area, Some(&[1.0; 24]), &Theme::default());
        let drawn: Vec<bool> = (0..38).map(|x| buffer[(x, 3)].symbol() != " ").collect();
        // 19 one-column bars, one gap each, the last column left over.
        let expected: Vec<bool> = (0..38).map(|x| x % 2 == 0 && x < 37).collect();
        assert_eq!(drawn, expected);

        let area = Rect::new(0, 0, 100, 4);
        let mut buffer = Buffer::empty(area);
        draw_spectrum(&mut buffer, area, Some(&[1.0; 24]), &Theme::default());
        let drawn: Vec<bool> = (0..100).map(|x| buffer[(x, 2)].symbol() != " ").collect();
        // 24 three-column bars in four-column slots: 95 columns used.
        let expected: Vec<bool> = (0..100).map(|x| x % 4 != 3 && x < 95).collect();
        assert_eq!(drawn, expected);
    }
}
