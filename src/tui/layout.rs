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
    /// A smaller cover and a one-row spectrum; no secondary metadata.
    Compact,
    Normal,
}

/// The smallest tier either dimension calls for.
pub fn tier_for(width: u16, height: u16) -> Tier {
    if width < 30 || height < 8 {
        Tier::Resize
    } else if width < 50 || height < 18 {
        Tier::Minimal
    } else if width < 80 || height < 28 {
        Tier::Compact
    } else {
        Tier::Normal
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Regions {
    pub status: Rect,
    /// The whole player region: bordered in the normal and compact tiers.
    pub player: Rect,
    pub cover: Option<Rect>,
    /// The title row, plus the artist · album row in the normal tier. In the
    /// resize tier, the space for the too-small message.
    pub info: Rect,
    pub spectrum: Option<Rect>,
    pub transport: Rect,
    pub progress: Rect,
    /// The queue region, including its border where the tier has one.
    pub queue: Rect,
    pub footer: Rect,
}

const NORMAL_PLAYER_ROWS: u16 = 9;
const NORMAL_COVER: (u16, u16) = (14, 7);
const NORMAL_INFO_ROWS: u16 = 2;
const NORMAL_SPECTRUM_ROWS: u16 = 3;
const COMPACT_PLAYER_ROWS: u16 = 6;
const COMPACT_COVER: (u16, u16) = (8, 4);
const COMPACT_SPECTRUM_ROWS: u16 = 1;
const MINIMAL_PLAYER_ROWS: u16 = 3;
/// Between the cover and the information column.
const COVER_GAP: u16 = 1;

pub fn regions(area: Rect, tier: Tier) -> Regions {
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
            Regions {
                status,
                player,
                cover: None,
                info: take_top(&mut column, 1),
                spectrum: None,
                progress: take_top(&mut column, 1),
                transport: take_top(&mut column, 1),
                queue: rest,
                footer,
            }
        }
        Tier::Compact | Tier::Normal => {
            let (player_rows, (cover_width, cover_rows), info_rows, spectrum_rows) =
                if tier == Tier::Normal {
                    (
                        NORMAL_PLAYER_ROWS,
                        NORMAL_COVER,
                        NORMAL_INFO_ROWS,
                        NORMAL_SPECTRUM_ROWS,
                    )
                } else {
                    (COMPACT_PLAYER_ROWS, COMPACT_COVER, 1, COMPACT_SPECTRUM_ROWS)
                };
            let status = take_top(&mut rest, 1);
            let footer = take_bottom(&mut rest, 1);
            let player = take_top(&mut rest, player_rows);
            let mut column = inset(player);
            let mut cover = take_left(&mut column, cover_width);
            cover.height = cover.height.min(cover_rows);
            take_left(&mut column, COVER_GAP);
            Regions {
                status,
                player,
                cover: Some(cover),
                info: take_top(&mut column, info_rows),
                spectrum: Some(take_top(&mut column, spectrum_rows)),
                transport: take_top(&mut column, 1),
                progress: take_top(&mut column, 1),
                queue: rest,
                footer,
            }
        }
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
    fn normal_and_compact_player_geometry() {
        let normal = regions(Rect::new(0, 0, 100, 30), Tier::Normal);
        assert_eq!(normal.player, Rect::new(0, 1, 100, 9));
        assert_eq!(normal.cover, Some(Rect::new(1, 2, 14, 7)));
        assert_eq!(normal.info, Rect::new(16, 2, 83, 2));
        assert_eq!(normal.spectrum, Some(Rect::new(16, 4, 83, 3)));
        assert_eq!(normal.transport.y, 7);
        assert_eq!(normal.progress.y, 8);
        assert_eq!(normal.queue, Rect::new(0, 10, 100, 19));
        assert_eq!(normal.footer, Rect::new(0, 29, 100, 1));

        let compact = regions(Rect::new(0, 0, 100, 20), Tier::Compact);
        assert_eq!(compact.player.height, 6);
        assert_eq!(compact.cover, Some(Rect::new(1, 2, 8, 4)));
        assert_eq!(compact.spectrum.map(|r| r.height), Some(1));
        assert_eq!(compact.progress.y, 5);

        let minimal = regions(Rect::new(0, 0, 45, 16), Tier::Minimal);
        assert_eq!((minimal.cover, minimal.spectrum), (None, None));
        assert_eq!(
            (minimal.info.y, minimal.progress.y, minimal.transport.y),
            (1, 2, 3)
        );
        assert_eq!(minimal.queue, Rect::new(0, 4, 45, 11));
    }
}
