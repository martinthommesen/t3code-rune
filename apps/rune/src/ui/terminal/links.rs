use std::{ops::RangeInclusive, sync::LazyLock};

use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line, Point},
    term::cell::Flags,
};
use regex_lite::Regex;
use reqwest::Url;
use rune::{Drag, Phase, PointerEvent, Tx};

use super::{CELL_PADDING, CELL_WIDTH, LINE_HEIGHT, Pane, State, control::TerminalScreen, mouse};

const DRAG_THRESHOLD: f32 = 4.0;
static URL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)https?://[^\s"'`<>]+"#).expect("valid terminal URL pattern")
});

#[derive(Default)]
pub(super) struct Links {
    pressed: Option<Link>,
    hovered: Option<PointerEvent>,
    selecting: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Link {
    url: String,
    cells: RangeInclusive<Point>,
}

impl State {
    pub(super) fn terminal_link_drag(&mut self, attachment: u64, drag: &Drag, tx: &mut Tx) {
        let Some(pane) = self.terminal_attachment_mut(attachment) else {
            return;
        };
        if drag.phase == Phase::Down {
            pane.links.pressed = if drag.clicks == 1 && !tx.mods().shift {
                link_at_pointer(&pane.screen, drag.x, drag.y)
            } else {
                None
            };
            if pane.links.pressed.is_some() {
                return;
            }
            pane.links.selecting = true;
            pane.links.hovered = None;
        }
        if let Some(link) = pane.links.pressed.take() {
            let moved = drag.dx.hypot(drag.dy) > DRAG_THRESHOLD;
            if drag.phase == Phase::Up {
                if !drag.cancelled()
                    && !moved
                    && link_at_pointer(&pane.screen, drag.x, drag.y).as_ref() == Some(&link)
                {
                    self.activate_terminal_attachment(attachment, tx);
                    let _ = tx.open_url(&link.url);
                }
                return;
            }
            if !moved {
                pane.links.pressed = Some(link);
                return;
            }
            pane.links.selecting = true;
            pane.links.hovered = None;
            let mut start = *drag;
            start.phase = Phase::Down;
            start.x = drag.start_x;
            start.y = drag.start_y;
            self.drag_terminal_selection(attachment, &start, tx);
        }
        if drag.phase == Phase::Up
            && let Some(pane) = self.terminal_attachment_mut(attachment)
        {
            pane.links.selecting = false;
        }
        self.drag_terminal_selection(attachment, drag, tx);
    }

    pub(super) fn terminal_link_hover(
        &mut self,
        attachment: u64,
        point: Option<PointerEvent>,
        tx: &mut Tx,
    ) {
        let Some(pane) = self.terminal_attachment_mut(attachment) else {
            return;
        };
        pane.links.hovered = point.filter(|_| {
            !pane.links.selecting && !mouse::capture(*pane.screen.term.mode(), tx.mods())
        });
    }
}

pub(super) fn highlighted_cells(pane: &Pane) -> Option<RangeInclusive<Point>> {
    let point = pane.links.hovered?;
    link_at_pointer(&pane.screen, point.x, point.y).map(|link| link.cells)
}

fn link_at_pointer(screen: &TerminalScreen, x: f32, y: f32) -> Option<Link> {
    let grid = screen.term.grid();
    let col = ((x - CELL_PADDING) / CELL_WIDTH).floor();
    let row = ((y - CELL_PADDING) / LINE_HEIGHT).floor();
    if !(0.0..grid.columns() as f32).contains(&col)
        || !(0.0..grid.screen_lines() as f32).contains(&row)
    {
        return None;
    }
    link_at(
        screen,
        Point::new(
            Line(row as i32 - grid.display_offset() as i32),
            Column(col as usize),
        ),
    )
}

fn link_at(screen: &TerminalScreen, mut point: Point) -> Option<Link> {
    let grid = screen.term.grid();
    let first = Line(-(grid.display_offset() as i32));
    let last = first + (grid.screen_lines() as i32 - 1);
    if point.line < first || point.line > last || point.column.0 >= grid.columns() {
        return None;
    }
    let last_column = Column(grid.columns() - 1);
    let wraps = |line: Line| grid[line][last_column].flags.contains(Flags::WRAPLINE);
    if point.column.0 > 0
        && grid[point.line][point.column]
            .flags
            .contains(Flags::WIDE_CHAR_SPACER)
    {
        point.column.0 -= 1;
    }
    let mut start = point.line;
    while start > first && wraps(start - 1) {
        start -= 1;
    }
    if start == first && first > screen.term.topmost_line() && wraps(first - 1) {
        return None;
    }
    let mut end = point.line;
    while end < last && wraps(end) {
        end += 1;
    }
    let mut text = String::new();
    let mut offsets = Vec::new();
    let mut hit = None;
    for line in start.0..=end.0 {
        let line = Line(line);
        for col in 0..grid.columns() {
            let current = Point::new(line, Column(col));
            let cell = &grid[current.line][current.column];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            if current == point {
                hit = Some(text.len());
            }
            offsets.push((text.len(), current));
            if cell.flags.contains(Flags::HIDDEN) {
                text.push(' ');
            } else {
                text.push(cell.c);
                text.extend(cell.zerowidth().unwrap_or_default());
            }
        }
    }
    let hit = hit?;
    // A visible last line with a continuation may contain only the beginning of the URL.
    let text = text.trim_end();
    for found in URL_PATTERN.find_iter(text) {
        let url = trim_url(found.as_str());
        let end_offset = found.start() + url.len();
        if hit < found.start() || hit >= end_offset {
            continue;
        }
        if end_offset == text.len() && wraps(end) {
            return None;
        }
        let first = offsets[offsets.partition_point(|(byte, _)| *byte < found.start())].1;
        let last = offsets[offsets.partition_point(|(byte, _)| *byte < end_offset) - 1].1;
        return Some(Link {
            url: Url::parse(url).ok()?.into(),
            cells: first..=last,
        });
    }
    None
}

fn trim_url(url: &str) -> &str {
    let url = url
        .split(|character: char| character.is_whitespace() || character == '\u{feff}')
        .next()
        .unwrap_or_default()
        .trim_end_matches(['.', ',', ';', '!', '?']);
    let mut end = url.len();
    for (open, close) in [(b'(', b')'), (b'[', b']'), (b'{', b'}')] {
        let opens = url.bytes().filter(|byte| *byte == open).count();
        let closes = url.bytes().filter(|byte| *byte == close).count();
        let suffix = url.as_bytes()[..end]
            .iter()
            .rev()
            .take_while(|byte| **byte == close)
            .count();
        end -= suffix.min(closes.saturating_sub(opens));
    }
    &url[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_only_printed_http_url_cells_and_keeps_balanced_delimiters() {
        let mut screen = TerminalScreen::new(4, 60);
        screen.process("好 (https://example.invalid/(a)).\r\n./docs/demo.txt:3:5".as_bytes());
        let link = link_at(&screen, Point::new(Line(0), Column(20))).unwrap();
        assert_eq!(link.url, "https://example.invalid/(a)");
        assert!(!link.cells.contains(&Point::new(Line(0), Column(3))));
        assert!(link_at(&screen, Point::new(Line(1), Column(8))).is_none());
    }

    #[test]
    fn normalizes_host_only_and_unicode_urls_for_the_system_opener() {
        let mut screen = TerminalScreen::new(4, 60);
        screen.process("https://example.invalid\r\nHTTPS://例え.invalid/雪\r\nhttp://.".as_bytes());
        assert_eq!(
            link_at(&screen, Point::new(Line(0), Column(13)))
                .unwrap()
                .url,
            "https://example.invalid/"
        );
        assert_eq!(
            link_at(&screen, Point::new(Line(1), Column(13)))
                .unwrap()
                .url,
            "https://xn--r8jz45g.invalid/%E9%9B%AA"
        );
        assert!(link_at(&screen, Point::new(Line(2), Column(4))).is_none());
    }

    #[test]
    fn detects_the_same_url_on_both_rows_of_a_visible_wrap() {
        let mut screen = TerminalScreen::new(4, 18);
        screen.process(b"visit https://example.invalid/path safely");
        let first = link_at(&screen, Point::new(Line(0), Column(8))).unwrap();
        let second = link_at(&screen, Point::new(Line(1), Column(6))).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.url, "https://example.invalid/path");
    }
}
