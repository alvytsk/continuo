//! Size tiers and the rectangles each tier gives the player (design doc M5
//! §7). Pure arithmetic over a `Rect`: nothing here draws, and every result
//! lies inside the area it was given, however small that area is.

use std::ops::Range;

use ratatui::layout::Rect;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tier {
    /// Too small for the player: a message and the essential key hints.
    Resize,
    /// Title, state, progress, transport and the queue; no cover or spectrum.
    Minimal,
    /// The compact tier squeezed for a short terminal: a small cover, a
    /// one-row spectrum, the time beside the bar; no secondary metadata.
    Short,
    /// The normal design scaled down: a smaller cover and at most two
    /// information lines.
    Compact,
    /// The full design: a cover beside one column of up to three information
    /// lines, the spectrum in the rows they leave, and the time; the bar on
    /// its own row.
    Normal,
}

/// The smallest tier either dimension calls for.
pub fn tier_for(width: u16, height: u16) -> Tier {
    if width < 30 || height < 8 {
        Tier::Resize
    } else if width < 50 || height < 18 {
        Tier::Minimal
    } else if height < 22 {
        Tier::Short
    } else if width < 80 || height < 28 {
        Tier::Compact
    } else {
        Tier::Normal
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Regions {
    /// The brand and session flags. In the bordered tiers this is the inside
    /// of the player's top border; in the minimal tier a row of its own.
    pub status: Rect,
    /// The whole player region: bordered in every tier but the minimal one.
    pub player: Rect,
    pub cover: Option<Rect>,
    /// The title row, plus as many of the artist and album rows as there are
    /// to show: one more in the compact tier, two in the normal one. In the
    /// resize tier, the space for the too-small message.
    pub info: Rect,
    pub spectrum: Option<Rect>,
    pub transport: Rect,
    /// The position label: its own row in the normal and compact tiers,
    /// otherwise the start of `progress`.
    pub time: Rect,
    /// The row the bar is drawn on; the label shares it when `time` is the
    /// same rectangle.
    pub progress: Rect,
    /// The queue region, including its border where the tier has one.
    pub queue: Rect,
    pub footer: Rect,
}

/// Border, air, the cover's rows, the bar, the transport, air, border.
const NORMAL_PLAYER_ROWS: u16 = 16;
/// Square at the usual 2:1 cell aspect.
const NORMAL_COVER: (u16, u16) = (20, 10);
/// Title, artist, album.
const NORMAL_INFO_ROWS: u16 = 3;
/// Six rows of cover beside the information, then the bar and the
/// transport, inside the border.
const COMPACT_PLAYER_ROWS: u16 = 10;
const COMPACT_COVER: (u16, u16) = (12, 6);
/// The title and the artist.
const COMPACT_INFO_ROWS: u16 = 2;
const SHORT_PLAYER_ROWS: u16 = 6;
const SHORT_COVER: (u16, u16) = (8, 4);
const SHORT_SPECTRUM_ROWS: u16 = 1;
const MINIMAL_PLAYER_ROWS: u16 = 3;
/// Between the cover and the information column.
const COVER_GAP: u16 = 1;
/// How far the status text sits inside the border row on each side.
const STATUS_INSET: u16 = 2;
/// The margin the bordered tiers keep from the terminal's edges.
const WINDOW_MARGIN: (u16, u16) = (2, 1);
/// A column of air inside each border of a bordered tier's boxes.
const AIR: u16 = 1;

/// `info_lines` is how many information lines there are to show. Beside the
/// cover the spectrum gets the rows they and the time leave, so it is taller
/// for a track with a title alone. Nothing else depends on it.
pub fn regions(area: Rect, tier: Tier, info_lines: u16) -> Regions {
    let mut rest = area;
    let none = Rect {
        width: 0,
        height: 0,
        ..area
    };
    match tier {
        Tier::Resize => {
            let footer = take_bottom(&mut rest, 1);
            Regions {
                status: none,
                player: none,
                cover: None,
                info: rest,
                spectrum: None,
                transport: none,
                time: none,
                progress: none,
                queue: none,
                footer,
            }
        }
        Tier::Minimal => {
            let status = take_top(&mut rest, 1);
            let footer = take_bottom(&mut rest, 1);
            let player = take_top(&mut rest, MINIMAL_PLAYER_ROWS);
            let mut column = player;
            let info = take_top(&mut column, 1);
            let progress = take_top(&mut column, 1);
            Regions {
                status,
                player,
                cover: None,
                info,
                spectrum: None,
                time: progress,
                progress,
                transport: take_top(&mut column, 1),
                queue: rest,
                footer,
            }
        }
        Tier::Short => {
            let mut rest = margin(area);
            let footer = take_bottom(&mut rest, 1);
            let player = take_top(&mut rest, SHORT_PLAYER_ROWS);
            let mut column = inset(player);
            let mut cover = take_left(&mut column, SHORT_COVER.0);
            cover.height = cover.height.min(SHORT_COVER.1);
            take_left(&mut column, COVER_GAP);
            let info = take_top(&mut column, 1);
            let spectrum = take_top(&mut column, SHORT_SPECTRUM_ROWS);
            let transport = take_top(&mut column, 1);
            let progress = take_top(&mut column, 1);
            Regions {
                status: status_in_border(player),
                player,
                cover: Some(cover),
                info,
                spectrum: Some(spectrum),
                transport,
                time: progress,
                progress,
                queue: rest,
                footer,
            }
        }
        Tier::Compact => {
            let mut rest = margin(area);
            let footer = take_bottom(&mut rest, 1);
            let player = take_top(&mut rest, COMPACT_PLAYER_ROWS);
            let mut column = inset(player);
            take_left(&mut column, AIR);
            take_right(&mut column, AIR);
            let mut top = take_top(&mut column, COMPACT_COVER.1);
            let cover = take_left(&mut top, COMPACT_COVER.0);
            take_left(&mut top, COVER_GAP);
            let info = take_top(&mut top, info_lines.clamp(1, COMPACT_INFO_ROWS));
            let time = take_bottom(&mut top, 1);
            let progress = take_top(&mut column, 1);
            let transport = take_top(&mut column, 1);
            Regions {
                status: status_in_border(player),
                player,
                cover: Some(cover),
                info,
                spectrum: Some(top),
                transport,
                time,
                progress,
                queue: rest,
                footer,
            }
        }
        Tier::Normal => {
            let mut rest = margin(area);
            let footer = take_bottom(&mut rest, 1);
            let player = take_top(&mut rest, NORMAL_PLAYER_ROWS);
            let mut column = inset(player);
            take_top(&mut column, AIR);
            take_left(&mut column, AIR);
            take_right(&mut column, AIR);
            let mut top = take_top(&mut column, NORMAL_COVER.1);
            let cover = take_left(&mut top, NORMAL_COVER.0);
            take_left(&mut top, COVER_GAP);
            let info = take_top(&mut top, info_lines.clamp(1, NORMAL_INFO_ROWS));
            let time = take_bottom(&mut top, 1);
            let progress = take_top(&mut column, 1);
            let transport = take_top(&mut column, 1);
            Regions {
                status: status_in_border(player),
                player,
                cover: Some(cover),
                info,
                spectrum: Some(top),
                transport,
                time,
                progress,
                queue: rest,
                footer,
            }
        }
    }
}

/// `area` less the window margin, or all of it when it is too small.
fn margin(area: Rect) -> Rect {
    let dx = WINDOW_MARGIN.0.min(area.width / 2);
    let dy = WINDOW_MARGIN.1.min(area.height / 2);
    Rect {
        x: area.x.saturating_add(dx),
        y: area.y.saturating_add(dy),
        width: area.width - dx * 2,
        height: area.height - dy * 2,
    }
}

/// The inside of a queue box: its border plus a column of air on each side.
pub(crate) fn queue_body(queue: Rect) -> Rect {
    let mut body = inset(queue);
    take_left(&mut body, AIR);
    take_right(&mut body, AIR);
    body
}

/// The part of the top border row the status text may use.
fn status_in_border(player: Rect) -> Rect {
    let dx = STATUS_INSET.min(player.width / 2);
    Rect {
        x: player.x.saturating_add(dx),
        width: player.width - dx * 2,
        height: player.height.min(1),
        ..player
    }
}

/// The inside of a one-cell border, empty (and still inside `rect`) when
/// there is no inside.
pub(crate) fn inset(rect: Rect) -> Rect {
    let dx = rect.width.min(1);
    let dy = rect.height.min(1);
    Rect {
        x: rect.x.saturating_add(dx),
        y: rect.y.saturating_add(dy),
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(2),
    }
}

/// Which of `len` queue rows a listing with room for `capacity` shows:
/// starting at `offset` where possible, never leaving a gap at the bottom
/// that earlier rows could fill, and always including `selected`.
pub(crate) fn visible_rows(
    len: usize,
    offset: usize,
    selected: Option<usize>,
    capacity: usize,
) -> Range<usize> {
    if capacity == 0 || len == 0 {
        return 0..0;
    }
    let mut start = offset.min(len.saturating_sub(capacity));
    if let Some(selected) = selected.filter(|index| *index < len) {
        if selected < start {
            start = selected;
        } else if selected >= start + capacity {
            start = selected + 1 - capacity;
        }
    }
    start..len.min(start + capacity)
}

fn take_top(rest: &mut Rect, rows: u16) -> Rect {
    let rows = rows.min(rest.height);
    let top = Rect {
        height: rows,
        ..*rest
    };
    rest.y = rest.y.saturating_add(rows);
    rest.height -= rows;
    top
}

fn take_bottom(rest: &mut Rect, rows: u16) -> Rect {
    let rows = rows.min(rest.height);
    rest.height -= rows;
    Rect {
        y: rest.y.saturating_add(rest.height),
        height: rows,
        ..*rest
    }
}

pub(crate) fn take_left(rest: &mut Rect, columns: u16) -> Rect {
    let columns = columns.min(rest.width);
    let left = Rect {
        width: columns,
        ..*rest
    };
    rest.x = rest.x.saturating_add(columns);
    rest.width -= columns;
    left
}

pub(crate) fn take_right(rest: &mut Rect, columns: u16) -> Rect {
    let columns = columns.min(rest.width);
    rest.width -= columns;
    Rect {
        x: rest.x.saturating_add(rest.width),
        width: columns,
        ..*rest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_follows_the_selection_and_fills_the_page() {
        assert_eq!(visible_rows(10, 0, Some(0), 4), 0..4);
        assert_eq!(visible_rows(10, 0, Some(7), 4), 4..8);
        assert_eq!(visible_rows(10, 6, Some(2), 4), 2..6);
        assert_eq!(visible_rows(10, 9, None, 4), 6..10);
        assert_eq!(visible_rows(3, 2, None, 4), 0..3);
        assert_eq!(visible_rows(3, 0, None, 0), 0..0);
        assert_eq!(visible_rows(0, 5, Some(1), 4), 0..0);
    }

    #[test]
    fn the_spectrum_takes_the_rows_the_information_leaves() {
        let area = Rect::new(0, 0, 100, 30);
        let title_only = regions(area, Tier::Normal, 1);
        assert_eq!(title_only.info, Rect::new(25, 3, 71, 1));
        assert_eq!(title_only.spectrum, Some(Rect::new(25, 4, 71, 8)));
        assert_eq!(title_only.time, Rect::new(25, 12, 71, 1));
        // No lines is still the title row; more than the tier shows is its most.
        assert_eq!(regions(area, Tier::Normal, 0), title_only);
        assert_eq!(
            regions(area, Tier::Normal, 9),
            regions(area, Tier::Normal, 3)
        );

        let area = Rect::new(0, 0, 100, 24);
        let compact = regions(area, Tier::Compact, 1);
        assert_eq!(compact.info, Rect::new(17, 2, 79, 1));
        assert_eq!(compact.spectrum, Some(Rect::new(17, 3, 79, 4)));
        assert_eq!(compact.time, Rect::new(17, 7, 79, 1));
        assert_eq!(
            regions(area, Tier::Compact, 3),
            regions(area, Tier::Compact, 2)
        );
        // The other tiers show one line whatever there is.
        for (area, tier) in [
            (Rect::new(0, 0, 100, 20), Tier::Short),
            (Rect::new(0, 0, 45, 16), Tier::Minimal),
        ] {
            assert_eq!(regions(area, tier, 1), regions(area, tier, 3));
        }
    }

    #[test]
    fn normal_compact_and_short_player_geometry() {
        let normal = regions(Rect::new(0, 0, 100, 30), Tier::Normal, 3);
        assert_eq!(normal.status, Rect::new(4, 1, 92, 1));
        assert_eq!(normal.player, Rect::new(2, 1, 96, 16));
        assert_eq!(normal.cover, Some(Rect::new(4, 3, 20, 10)));
        assert_eq!(normal.info, Rect::new(25, 3, 71, 3));
        assert_eq!(normal.spectrum, Some(Rect::new(25, 6, 71, 6)));
        assert_eq!(normal.time, Rect::new(25, 12, 71, 1));
        assert_eq!(normal.progress, Rect::new(4, 13, 92, 1));
        assert_eq!(normal.transport, Rect::new(4, 14, 92, 1));
        assert_eq!(normal.queue, Rect::new(2, 17, 96, 11));
        assert_eq!(queue_body(normal.queue), Rect::new(4, 18, 92, 9));
        assert_eq!(normal.footer, Rect::new(2, 28, 96, 1));

        let compact = regions(Rect::new(0, 0, 100, 24), Tier::Compact, 3);
        assert_eq!(compact.status, Rect::new(4, 1, 92, 1));
        assert_eq!(compact.player, Rect::new(2, 1, 96, 10));
        assert_eq!(compact.cover, Some(Rect::new(4, 2, 12, 6)));
        assert_eq!(compact.info, Rect::new(17, 2, 79, 2));
        assert_eq!(compact.spectrum, Some(Rect::new(17, 4, 79, 3)));
        assert_eq!(compact.time, Rect::new(17, 7, 79, 1));
        assert_eq!(compact.progress, Rect::new(4, 8, 92, 1));
        assert_eq!(compact.transport, Rect::new(4, 9, 92, 1));
        assert_eq!(compact.queue, Rect::new(2, 11, 96, 11));

        let short = regions(Rect::new(0, 0, 100, 20), Tier::Short, 3);
        assert_eq!(short.status, Rect::new(4, 1, 92, 1));
        assert_eq!(short.player, Rect::new(2, 1, 96, 6));
        assert_eq!(short.cover, Some(Rect::new(3, 2, 8, 4)));
        assert_eq!(short.spectrum.map(|r| r.height), Some(1));
        assert_eq!(short.progress.y, 5);
        assert_eq!(short.time, short.progress);

        let minimal = regions(Rect::new(0, 0, 45, 16), Tier::Minimal, 3);
        assert_eq!((minimal.cover, minimal.spectrum), (None, None));
        assert_eq!(
            (minimal.info.y, minimal.progress.y, minimal.transport.y),
            (1, 2, 3)
        );
        assert_eq!(minimal.queue, Rect::new(0, 4, 45, 11));
    }
}
