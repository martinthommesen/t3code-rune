use std::{cell::RefCell, rc::Rc, time::Instant};

use alacritty_terminal::{
    event::{Event, EventListener},
    grid::Dimensions,
    term::{Config, Osc52, Term},
    vte::ansi,
};

use super::mouse_modes::MouseModes;

const HISTORY_LIMIT: usize = 10_000;
const REPLY_LIMIT: usize = 16_384;

#[derive(Clone, Default)]
pub(super) struct TerminalEvents(Rc<RefCell<String>>);

impl EventListener for TerminalEvents {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            let mut replies = self.0.borrow_mut();
            if replies.len() + text.len() <= REPLY_LIMIT {
                replies.push_str(&text);
            }
        }
    }
}

pub(super) struct TerminalScreen {
    pub(super) term: Term<TerminalEvents>,
    processor: ansi::Processor,
    pub(super) mouse_modes: MouseModes,
    events: TerminalEvents,
}

impl TerminalScreen {
    pub(super) fn new(rows: u16, cols: u16) -> Self {
        let events = TerminalEvents::default();
        let config = Config {
            scrolling_history: HISTORY_LIMIT,
            osc52: Osc52::Disabled,
            ..Config::default()
        };
        let term = Term::new(config, &Size::new(rows, cols), events.clone());
        Self {
            term,
            events,
            processor: ansi::Processor::new(),
            mouse_modes: MouseModes::default(),
        }
    }

    pub(super) fn process(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
        self.mouse_modes.process(bytes);
    }

    pub(super) fn resize(&mut self, rows: u16, cols: u16) {
        self.term.resize(Size::new(rows, cols));
    }

    pub(super) fn synchronized_until(&self) -> Option<Instant> {
        self.processor.sync_timeout().sync_timeout()
    }

    pub(super) fn finish_synchronized_update(&mut self) {
        self.processor.stop_sync(&mut self.term);
    }

    pub(super) fn take_replies(&mut self) -> String {
        let mut replies = std::mem::take(&mut *self.events.0.borrow_mut());
        let waiting = self.synchronized_until().is_some();
        self.mouse_modes.rewrite_replies(&mut replies, waiting);
        replies
    }
}

struct Size {
    rows: usize,
    cols: usize,
}

impl Size {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows: usize::from(rows).clamp(1, 64),
            cols: usize::from(cols).clamp(2, 500),
        }
    }
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::{
        grid::Scroll,
        index::{Column, Line},
        term::TermMode,
    };

    #[test]
    fn tracks_and_reports_focus_mode_across_output_chunks() {
        let mut screen = TerminalScreen::new(20, 80);
        screen.process(b"\x1b[?1004$p\x1b[?10");
        assert!(!screen.term.mode().contains(TermMode::FOCUS_IN_OUT));
        assert_eq!(screen.take_replies(), "\x1b[?1004;2$y");
        screen.process(b"04h\x1b[?1004$p");
        assert!(screen.term.mode().contains(TermMode::FOCUS_IN_OUT));
        assert_eq!(screen.take_replies(), "\x1b[?1004;1$y");
        screen.process(b"\x1b[?1004l");
        assert!(!screen.term.mode().contains(TermMode::FOCUS_IN_OUT));
    }

    #[test]
    fn column_resize_reflows_wrapped_output_without_losing_text() {
        let mut screen = TerminalScreen::new(4, 12);
        let text = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
        screen.process(text.as_bytes());
        for columns in [8, 20, 12] {
            screen.resize(4, columns);
            let grid = screen.term.grid();
            let output: String = (grid.topmost_line().0..=grid.bottommost_line().0)
                .flat_map(|line| {
                    (0..grid.columns()).map(move |col| grid[Line(line)][Column(col)].c)
                })
                .filter(|&cell| cell != ' ')
                .collect();
            assert_eq!(output, text);
        }
    }

    #[test]
    fn erase_saved_lines_clears_history_without_resetting_modes_or_current_output() {
        let mut screen = TerminalScreen::new(3, 16);
        screen.process(b"\x1b[?1004hline-1\r\nline-2\r\nline-3\r\nline-4\r\n");
        screen.term.scroll_display(Scroll::PageUp);
        assert!(screen.term.grid().display_offset() > 0);
        screen.process(b"\x1b[3J");
        let grid = screen.term.grid();
        assert_eq!(grid.history_size(), 0);
        assert_eq!(grid.display_offset(), 0);
        assert_eq!(grid[Line(1)][Column(5)].c, '4');
        assert!(screen.term.mode().contains(TermMode::FOCUS_IN_OUT));
    }

    #[test]
    fn unfinished_synchronized_updates_can_flush_after_the_deadline() {
        let mut screen = TerminalScreen::new(4, 16);
        screen.process(b"\x1b[?2026hsynchronized");
        assert!(screen.synchronized_until().is_some());
        assert_eq!(screen.term.grid()[Line(0)][Column(0)].c, ' ');
        screen.finish_synchronized_update();
        assert!(screen.synchronized_until().is_none());
        assert_eq!(screen.term.grid()[Line(0)][Column(0)].c, 's');
    }
    #[test]
    fn mouse_protocol_tracks_split_sequences_and_resets_any_format() {
        use super::super::mouse_modes::{Encoding, Tracking};

        let mut screen = TerminalScreen::new(16, 80);
        screen.process(b"\x1b[?1000;10");
        assert!(screen.mouse_modes.protocol().tracking == Tracking::Disabled);
        screen.process(b"16h");
        assert!(screen.mouse_modes.protocol().tracking == Tracking::Buttons);
        assert!(screen.mouse_modes.protocol().encoding == Encoding::Pixels);
        screen.process(b"\x1b[?1005l");
        assert!(screen.mouse_modes.protocol().encoding == Encoding::Classic);
    }

    #[test]
    fn mouse_mode_queries_keep_independent_flags_across_synchronization() {
        let mut screen = TerminalScreen::new(16, 80);
        screen.process(b"\x1b[?9;1000;1015;1016h\x1b[?9$p\x1b[?1000$p\x1b[?1015$p");
        assert_eq!(
            screen.take_replies(),
            "\x1b[?9;1$y\x1b[?1000;1$y\x1b[?1015;1$y"
        );
        screen.process(b"\x1b[?2026h\x1b[?9;1015l\x1b[?9$p\x1b[?1015$p\x1b[?1016$p");
        assert!(screen.take_replies().is_empty());
        screen.finish_synchronized_update();
        assert_eq!(
            screen.take_replies(),
            "\x1b[?9;2$y\x1b[?1015;2$y\x1b[?1016;1$y"
        );
    }
}
