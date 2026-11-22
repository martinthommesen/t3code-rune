use std::{
    ops::{Range, RangeInclusive},
    sync::LazyLock,
    time::{Duration, Instant},
};

use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line, Point},
    term::cell::Flags,
};
use regex_lite::Regex;
use reqwest::Url;
use rune::{Drag, Phase, PointerEvent, Tx};
use uuid::Uuid;

use crate::connection::TerminalFileLink;

use super::{
    CELL_PADDING, CELL_WIDTH, LINE_HEIGHT, Pane, State, control::TerminalScreen, link_paths, mouse,
};

const DRAG_THRESHOLD: f32 = 4.0;
const FILE_HOVER_DELAY: Duration = Duration::from_millis(250);
static URL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)https?://[^\s"'`<>]+"#).expect("valid terminal URL pattern")
});

#[derive(Default)]
pub(super) struct Links {
    pressed: Option<Link>,
    hovered: Option<PointerEvent>,
    selecting: bool,
    file_hover: Option<FileHover>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Link {
    target: Target,
    cells: RangeInclusive<Point>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Target {
    Url(String),
    File(String),
}

struct FileHover {
    link: TerminalFileLink,
    id: Uuid,
    deadline: Option<Instant>,
    available: bool,
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
            pane.links.file_hover = None;
        }
        if let Some(link) = pane.links.pressed.take() {
            let moved = drag.dx.hypot(drag.dy) > DRAG_THRESHOLD;
            if drag.phase == Phase::Up {
                if !drag.cancelled()
                    && !moved
                    && link_at_pointer(&pane.screen, drag.x, drag.y).as_ref() == Some(&link)
                {
                    let file = match &link.target {
                        Target::File(path) => file_request(pane, path),
                        Target::Url(_) => None,
                    };
                    self.activate_terminal_attachment(attachment, tx);
                    match link.target {
                        Target::Url(url) => {
                            let _ = tx.open_url(&url);
                        }
                        Target::File(_) => {
                            if let Some(file) = file
                                && let Some(connection) = self.connection.as_ref()
                            {
                                connection.open_terminal_file_link(
                                    file,
                                    self.client_settings.preferred_editor(),
                                );
                            }
                        }
                    }
                }
                return;
            }
            if !moved {
                pane.links.pressed = Some(link);
                return;
            }
            pane.links.selecting = true;
            pane.links.hovered = None;
            pane.links.file_hover = None;
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
            !pane.links.selecting && !mouse::capture(pane.screen.mouse_modes.protocol(), tx.mods())
        });
        let file = pane.links.hovered.and_then(|point| {
            match link_at_pointer(&pane.screen, point.x, point.y)?.target {
                Target::File(path) => file_request(pane, &path),
                Target::Url(_) => None,
            }
        });
        if file.as_ref() == pane.links.file_hover.as_ref().map(|probe| &probe.link) {
            return;
        }
        pane.links.file_hover = file.map(|link| {
            tx.wake_after(FILE_HOVER_DELAY);
            FileHover {
                link,
                id: Uuid::new_v4(),
                deadline: Some(Instant::now() + FILE_HOVER_DELAY),
                available: false,
            }
        });
    }

    pub(in crate::ui) fn poll_terminal_file_links(&mut self, tx: &mut Tx) {
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        let now = Instant::now();
        for pane in self.terminal.iter_mut().chain(&mut self.terminal_peers) {
            let Some(probe) = pane.links.file_hover.as_mut() else {
                continue;
            };
            let Some(deadline) = probe.deadline else {
                continue;
            };
            if deadline > now {
                tx.wake_after(deadline - now);
                continue;
            }
            probe.deadline = None;
            connection.check_terminal_file_link(probe.link.clone(), probe.id);
        }
    }

    pub(in crate::ui) fn terminal_file_link_checked(
        &mut self,
        attachment: u64,
        id: Uuid,
        available: bool,
    ) {
        if let Some(pane) = self.terminal_attachment_mut(attachment)
            && let Some(probe) = &mut pane.links.file_hover
            && probe.id == id
        {
            probe.available = available;
        }
    }

    pub(in crate::ui) fn terminal_file_link_opened(
        &mut self,
        attachment: u64,
        editor: Option<String>,
        result: Result<(), String>,
    ) {
        if let Some(editor) = editor
            && let Err(error) = self.client_settings.remember_editor(editor)
        {
            eprintln!("Could not remember the preferred editor: {error}");
        }
        if let Err(error) = result
            && let Some(pane) = self.terminal_attachment_mut(attachment)
        {
            pane.message = Some(error);
        }
    }
}

fn file_request(pane: &Pane, path: &str) -> Option<TerminalFileLink> {
    Some(link_paths::request(
        pane.attachment,
        path,
        pane.cwd.as_deref()?,
        pane.worktree_path.as_deref(),
    ))
}

pub(super) fn highlighted_cells(pane: &Pane) -> Option<RangeInclusive<Point>> {
    let point = pane.links.hovered?;
    let link = link_at_pointer(&pane.screen, point.x, point.y)?;
    match &link.target {
        Target::Url(_) => Some(link.cells),
        Target::File(path) => (pane.links.file_hover.as_ref().is_some_and(|probe| {
            probe.available && file_request(pane, path).as_ref() == Some(&probe.link)
        }))
        .then_some(link.cells),
    }
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
    let (target, range) = url_at(text, hit).or_else(|| {
        link_paths::matching(text, hit).map(|(path, range)| (Target::File(path.to_owned()), range))
    })?;
    if range.end == text.len() && wraps(end) {
        return None;
    }
    let first = offsets[offsets.partition_point(|(byte, _)| *byte < range.start)].1;
    let last = offsets[offsets.partition_point(|(byte, _)| *byte < range.end) - 1].1;
    Some(Link {
        target,
        cells: first..=last,
    })
}

fn url_at(text: &str, hit: usize) -> Option<(Target, Range<usize>)> {
    for found in URL_PATTERN.find_iter(text) {
        let url = trim_closing_delimiters(found.as_str(), false);
        let range = found.start()..found.start() + url.len();
        if range.contains(&hit) {
            return Some((Target::Url(Url::parse(url).ok()?.into()), range));
        }
    }
    None
}

pub(super) fn trim_closing_delimiters(url: &str, path: bool) -> &str {
    let url = url
        .split(|character: char| character.is_whitespace() || character == '\u{feff}')
        .next()
        .unwrap_or_default();
    let punctuation = if path {
        &['.', ',', ';', ':', '!', '?'][..]
    } else {
        &['.', ',', ';', '!', '?'][..]
    };
    let url = url.trim_end_matches(punctuation);
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
    fn distinguishes_printed_file_paths_from_urls_and_keeps_balanced_delimiters() {
        let mut screen = TerminalScreen::new(4, 60);
        screen.process("好 (https://example.invalid/(a)).\r\n./docs/demo.txt:3:5".as_bytes());
        let link = link_at(&screen, Point::new(Line(0), Column(20))).unwrap();
        assert_eq!(
            link.target,
            Target::Url("https://example.invalid/(a)".into())
        );
        assert!(!link.cells.contains(&Point::new(Line(0), Column(3))));
        assert_eq!(
            link_at(&screen, Point::new(Line(1), Column(8)))
                .unwrap()
                .target,
            Target::File("./docs/demo.txt:3:5".into())
        );
    }

    #[test]
    fn normalizes_host_only_and_unicode_urls_for_the_system_opener() {
        let mut screen = TerminalScreen::new(4, 60);
        screen.process("https://example.invalid\r\nHTTPS://例え.invalid/雪\r\nhttp://.".as_bytes());
        assert_eq!(
            link_at(&screen, Point::new(Line(0), Column(13)))
                .unwrap()
                .target,
            Target::Url("https://example.invalid/".into())
        );
        assert_eq!(
            link_at(&screen, Point::new(Line(1), Column(13)))
                .unwrap()
                .target,
            Target::Url("https://xn--r8jz45g.invalid/%E9%9B%AA".into())
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
        assert_eq!(
            first.target,
            Target::Url("https://example.invalid/path".into())
        );
    }
}
