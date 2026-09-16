//! The browser overlay (design doc M5 §8): tabs, where the list is, the list
//! itself and a key hint, in one bordered box over the player. Every name
//! read from the filesystem or a feed goes through
//! [`displayable`](crate::commands::displayable) before it is drawn.

use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Widget};

use super::{centered_box, clock, row};
use crate::application::browse::EntryKind;
use crate::commands::displayable;
use crate::tui::browser::{BrowserState, BrowserTab, NoticeKind};
use crate::tui::layout::{inset, take_left, take_right, visible_rows};
use crate::tui::theme::Theme;

const HINTS: &str = "enter open/add · space mark · tab files/podcasts · ⌫ back · b close";
const PODCAST_HINTS: &str =
    "enter open/add · space mark · a subscribe · r/R refresh · d remove · ⌫ back · b close";
const NO_FEEDS: &str = "No subscriptions — press a to add a feed URL";
const PROMPT: &str = "Feed URL: ";
const LOADING: &str = "Loading…";
const EMPTY_DIRECTORY: &str = "(empty directory)";
const NO_EPISODES: &str = "No cached episodes";
const UNTITLED: &str = "(untitled)";
const MARK_COLUMNS: u16 = 2;
const DETAIL_COLUMNS: u16 = 14;
/// The row width below which the detail column is dropped.
const DETAIL_MIN_ROW: u16 = 40;

pub(super) fn draw_browser(buffer: &mut Buffer, area: Rect, browser: &BrowserState, theme: &Theme) {
    let box_area = centered_box(
        area,
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    Clear.render(box_area, buffer);
    Block::bordered()
        .title(" browse ")
        .border_style(Style::new().fg(theme.green))
        .style(Style::new().fg(theme.text))
        .render(box_area, buffer);
    let body = inset(box_area);
    if body.is_empty() {
        return;
    }

    tabs(browser.tab, theme).render(row(body, body.y), buffer);
    Line::styled(location(browser), Style::new().fg(theme.muted))
        .render(row(body, body.y.saturating_add(1)), buffer);
    let hint_y = body.bottom().saturating_sub(1);
    let hints = match browser.tab {
        BrowserTab::Files => HINTS,
        BrowserTab::Podcasts => PODCAST_HINTS,
    };
    if body.height >= 4 {
        Line::styled(hints, Style::new().fg(theme.muted)).render(row(body, hint_y), buffer);
    }
    let list = Rect {
        y: body.y.saturating_add(2),
        height: body
            .height
            .saturating_sub(if body.height >= 4 { 3 } else { 2 }),
        ..body
    }
    .intersection(body);
    draw_list(buffer, list, browser, theme);
}

fn tabs(active: BrowserTab, theme: &Theme) -> Line<'static> {
    let style = |tab| {
        if tab == active {
            Style::new()
                .bg(theme.green)
                .fg(theme.ink)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.muted)
        }
    };
    Line::from(vec![
        Span::styled(" Files ", style(BrowserTab::Files)),
        Span::raw(" "),
        Span::styled(" Podcasts ", style(BrowserTab::Podcasts)),
    ])
}

/// The directory shown, the feed whose episodes are shown, or the feed
/// list's own heading.
fn location(browser: &BrowserState) -> String {
    match (browser.tab, &browser.episodes) {
        (BrowserTab::Files, _) => displayable(&browser.cwd.display().to_string()),
        (BrowserTab::Podcasts, None) => "Subscriptions".to_owned(),
        (BrowserTab::Podcasts, Some((slug, _))) => {
            let title = browser
                .feeds
                .iter()
                .find(|feed| feed.slug == *slug)
                .and_then(|feed| feed.title.as_deref())
                .unwrap_or(slug);
            format!("Subscriptions › {}", displayable(title))
        }
    }
}

fn draw_list(buffer: &mut Buffer, area: Rect, browser: &BrowserState, theme: &Theme) {
    if area.is_empty() {
        return;
    }
    let rows = draw_notice_block(buffer, area, browser, theme);
    if rows.is_empty() {
        return;
    }
    let notice = if browser.loading {
        Some((LOADING.to_owned(), theme.muted))
    } else if let Some(error) = &browser.error {
        Some((displayable(error), theme.amber))
    } else if browser.is_empty() {
        let empty = match (browser.tab, &browser.episodes) {
            (BrowserTab::Files, _) => EMPTY_DIRECTORY,
            (BrowserTab::Podcasts, None) => NO_FEEDS,
            (BrowserTab::Podcasts, Some(_)) => NO_EPISODES,
        };
        Some((empty.to_owned(), theme.muted))
    } else {
        None
    };
    if let Some((text, color)) = notice {
        Line::styled(text, Style::new().fg(color)).render(row(rows, rows.y), buffer);
        return;
    }

    let window = visible_rows(
        browser.len(),
        0,
        Some(browser.cursor),
        usize::from(rows.height),
    );
    let mut y = rows.y;
    for index in window {
        let Some(cells) = row_cells(browser, index, theme) else {
            break;
        };
        draw_row(
            buffer,
            row(rows, y),
            cells,
            browser.marked.contains(&index),
            index == browser.cursor,
            theme,
        );
        y = y.saturating_add(1);
    }
}

/// What one list row shows: its label, an optional right-hand detail, and
/// the style both take when the row is not under the cursor.
struct RowCells {
    label: String,
    detail: Option<String>,
    style: Style,
}

fn row_cells(browser: &BrowserState, index: usize, theme: &Theme) -> Option<RowCells> {
    match (browser.tab, &browser.episodes) {
        (BrowserTab::Files, _) => browser.entries.get(index).map(|entry| {
            let name = displayable(&entry.name);
            let (label, color) = match entry.kind {
                EntryKind::Directory => (format!("{name}/"), theme.cyan),
                EntryKind::Audio => (name, theme.text),
                EntryKind::Other => (name, theme.muted),
            };
            RowCells {
                label,
                detail: None,
                style: Style::new().fg(color),
            }
        }),
        (BrowserTab::Podcasts, None) => browser.feeds.get(index).map(|feed| RowCells {
            label: displayable(feed.title.as_deref().unwrap_or(&feed.slug)),
            detail: Some(match feed.episodes {
                Some(count) => format!("{count} episodes"),
                None => "not refreshed".to_owned(),
            }),
            style: Style::new().fg(theme.text),
        }),
        (BrowserTab::Podcasts, Some((_, episodes))) => episodes.get(index).map(|episode| {
            let style = if episode.enclosure.is_some() {
                Style::new().fg(theme.text)
            } else {
                Style::new().fg(theme.muted).add_modifier(Modifier::DIM)
            };
            RowCells {
                label: episode
                    .title
                    .as_deref()
                    .map_or_else(|| UNTITLED.to_owned(), displayable),
                // A feed's declared duration, in parentheses as in the queue.
                detail: episode
                    .declared_duration
                    .map(|value| format!("({})", clock(value))),
                style,
            }
        }),
    }
}

fn draw_row(
    buffer: &mut Buffer,
    rect: Rect,
    cells: RowCells,
    marked: bool,
    under_cursor: bool,
    theme: &Theme,
) {
    let style = if under_cursor {
        cells.style.bg(theme.green).fg(theme.ink)
    } else {
        cells.style
    };
    buffer.set_style(rect, style);
    let mut rest = rect;
    let mark = take_left(&mut rest, MARK_COLUMNS);
    if marked {
        let mark_style = if under_cursor {
            style
        } else {
            Style::new().fg(theme.amber)
        };
        Line::styled("●", mark_style).render(mark, buffer);
    }
    if let Some(detail) = cells.detail
        && rest.width >= DETAIL_MIN_ROW
    {
        let column = take_right(&mut rest, DETAIL_COLUMNS);
        take_right(&mut rest, 1);
        Line::styled(detail, style)
            .alignment(Alignment::Right)
            .render(column, buffer);
    }
    Line::styled(cells.label, style).render(rest, buffer);
}

/// The prompt, the confirmation question or the mutation notice, as a block
/// of up to a third of `area` above the rows (M6 §5); returns what is left
/// for the rows. A notice with more lines than fit ends in a marker; the
/// worker logged the whole text.
fn draw_notice_block(
    buffer: &mut Buffer,
    area: Rect,
    browser: &BrowserState,
    theme: &Theme,
) -> Rect {
    let Some((lines, color)) = notice_lines(browser, theme) else {
        return area;
    };
    let budget = usize::from(area.height / 3).max(1);
    let shown = lines.len().min(budget);
    let hidden = lines.len() - shown;
    let height = u16::try_from(shown + usize::from(hidden > 0)).unwrap_or(u16::MAX);
    let mut y = area.y;
    for line in lines.iter().take(shown) {
        Line::styled(line.clone(), Style::new().fg(color)).render(row(area, y), buffer);
        y = y.saturating_add(1);
    }
    if hidden > 0 {
        Line::styled(
            format!("+{hidden} more lines, see log"),
            Style::new().fg(color).add_modifier(Modifier::DIM),
        )
        .render(row(area, y), buffer);
    }
    Rect {
        y: area.y.saturating_add(height),
        height: area.height.saturating_sub(height),
        ..area
    }
    .intersection(area)
}

fn notice_lines(browser: &BrowserState, theme: &Theme) -> Option<(Vec<String>, Color)> {
    if let Some(prompt) = &browser.prompt {
        return Some((
            vec![format!("{PROMPT}{}▏", displayable(prompt))],
            theme.text,
        ));
    }
    if let Some(slug) = &browser.confirm {
        return Some((
            vec![format!("Remove {}? y/N", displayable(slug))],
            theme.amber,
        ));
    }
    let notice = browser.notice.as_ref()?;
    let color = match notice.kind {
        NoticeKind::Err => theme.amber,
        NoticeKind::Working | NoticeKind::Ok => theme.muted,
    };
    Some((notice.text.lines().map(displayable).collect(), color))
}
