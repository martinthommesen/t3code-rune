mod line;
mod pen;
mod tail;

#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt::Write as _;

use line::Line;
pub(crate) use pen::{Color as CommandColor, Pen as CommandPen};
use pen::{Pen, number};
use tail::{trim_tail, utf16_len};

pub(super) const TAIL_CHARS: usize = 64 * 1024;
const LIVE_LINES: usize = 64;
const MAX_PENDING_ESCAPE: usize = 64;
const ESC: u8 = 0x1b;

#[derive(Default)]
pub(crate) struct TerminalOutput {
    screen: Screen,
    text: String,
    pending: String,
    truncated: bool,
}

impl TerminalOutput {
    pub(crate) fn append(&mut self, chunk: &str) {
        self.append_with_limit(chunk, TAIL_CHARS);
    }

    fn append_with_limit(&mut self, chunk: &str, limit: usize) {
        let input = if self.pending.is_empty() {
            Cow::Borrowed(chunk)
        } else {
            let mut input = std::mem::take(&mut self.pending);
            input.push_str(chunk);
            Cow::Owned(input)
        };
        if input.is_empty() {
            return;
        }
        self.pending = self.screen.feed(&input, limit);
        self.screen.trim_window(limit);
        let scrollback_truncated = self.screen.trim_scrollback(limit);
        self.text.clear();
        self.text.push_str(&self.screen.settled);
        for (row, line) in self.screen.lines.iter().enumerate() {
            if row > 0 {
                self.text.push('\n');
            }
            line.serialize(&mut self.text);
        }
        self.truncated |=
            trim_tail(&mut self.text, limit) || scrollback_truncated || self.screen.discarded;
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }

    pub(super) fn resume_text(&self) -> String {
        let mut text = self.text.clone();
        let last_row = self.screen.lines.len() - 1;
        let reachable = self
            .text
            .bytes()
            .filter(|byte| *byte == b'\n')
            .take(LIVE_LINES - 1)
            .count();
        let up = |row| last_row.saturating_sub(row).min(reachable);
        let go_to = |text: &mut String, position: Position| {
            let rows = up(position.row);
            if rows > 0 {
                write!(text, "\x1b[{rows}A").expect("writing into a string succeeds");
            }
            write!(text, "\x1b[{}G", position.col + 1).expect("writing into a string succeeds");
        };
        if let Some(saved) = self.screen.saved {
            go_to(&mut text, saved);
            text.push_str("\x1b7");
            let rows = up(saved.row);
            if rows > 0 {
                write!(text, "\x1b[{rows}B").expect("writing into a string succeeds");
            }
        }
        let cursor = self.screen.cursor;
        if self.screen.saved.is_some()
            || cursor.row < last_row
            || cursor.col != self.screen.lines[cursor.row].width
        {
            go_to(&mut text, cursor);
        }
        if self.screen.pen != Pen::reset() {
            self.screen.pen.write_sgr(&mut text);
        }
        text.push_str(&self.pending);
        text
    }
}

#[derive(Clone, Copy, Default)]
struct Position {
    row: usize,
    col: usize,
}

struct Screen {
    settled: String,
    settled_units: usize,
    lines: VecDeque<Line>,
    cursor: Position,
    pen: Pen,
    saved: Option<Position>,
    discarded: bool,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            settled: String::new(),
            settled_units: 0,
            lines: VecDeque::from([Line::default()]),
            cursor: Position::default(),
            pen: Pen::default(),
            saved: None,
            discarded: false,
        }
    }
}

impl Screen {
    fn write(&mut self, text: &str, limit: usize) {
        if text.is_empty() {
            return;
        }
        let line = &mut self.lines[self.cursor.row];
        self.cursor.col += line.write(self.cursor.col, text, self.pen);
        if line.units > limit {
            let removed = line.trim_start(line.units - limit);
            self.discarded = true;
            self.cursor.col = self.cursor.col.saturating_sub(removed);
            self.shift_saved_col(self.cursor.row, removed);
        }
    }

    fn shift_saved_col(&mut self, row: usize, removed: usize) {
        if let Some(saved) = &mut self.saved
            && saved.row == row
        {
            saved.col = saved.col.saturating_sub(removed);
        }
    }

    fn newline(&mut self, limit: usize) {
        self.cursor.row += 1;
        self.cursor.col = 0;
        if self.cursor.row == self.lines.len() {
            self.lines.push_back(Line::default());
        }
        while self.lines.len() > LIVE_LINES {
            self.scroll_out(limit);
        }
    }

    fn scroll_out(&mut self, limit: usize) -> usize {
        let line = self.lines.pop_front().expect("screen has a live line");
        let appended_at = self.settled.len();
        line.serialize(&mut self.settled);
        self.settled.push('\n');
        self.settled_units += utf16_len(&self.settled[appended_at..]);
        if self.settled_units > 2 * limit {
            self.trim_scrollback(limit);
            self.discarded = true;
        }
        self.cursor.row -= 1;
        if let Some(saved) = &mut self.saved {
            saved.row = saved.row.saturating_sub(1);
        }
        line.units
    }

    fn trim_scrollback(&mut self, limit: usize) -> bool {
        if self.settled_units <= limit {
            return false;
        }
        trim_tail(&mut self.settled, limit);
        self.settled_units = utf16_len(&self.settled);
        true
    }

    fn trim_window(&mut self, limit: usize) {
        let mut units: usize = self.lines.iter().map(|line| line.units).sum();
        while units > limit && self.cursor.row > 0 {
            units -= self.scroll_out(limit);
        }
        if units <= limit {
            return;
        }
        let mut budget = limit;
        for row in 0..self.lines.len() {
            let line = &mut self.lines[row];
            if row > self.cursor.row && line.units > budget {
                let removed = line.trim_start(line.units - budget);
                self.discarded = true;
                self.shift_saved_col(row, removed);
            }
            budget = budget.saturating_sub(self.lines[row].units);
        }
    }

    fn move_to(&mut self, row: usize, col: usize) {
        self.cursor = Position {
            row: row.min(self.lines.len() - 1),
            col: col.min(2 * TAIL_CHARS),
        };
    }

    fn erase_display(&mut self, mode: usize) {
        let Position { row, col } = self.cursor;
        match mode {
            0 => {
                self.lines[row].erase(col, 0);
                self.lines.truncate(row + 1);
            }
            1 => {
                for line in self.lines.iter_mut().take(row) {
                    *line = Line::default();
                }
                self.lines[row].erase(col, 1);
            }
            2 => {
                self.lines = VecDeque::from([Line::default()]);
                self.cursor = Position::default();
            }
            3 => {
                self.discarded |= !self.settled.is_empty();
                self.settled.clear();
                self.settled_units = 0;
            }
            _ => {}
        }
    }

    fn restore_position(&mut self) {
        if let Some(saved) = self.saved {
            self.move_to(saved.row, saved.col);
        }
    }

    fn escape(&mut self, sequence: &str) {
        match sequence {
            "\x1b7" => self.saved = Some(self.cursor),
            "\x1b8" => self.restore_position(),
            "\x1bM" => self.move_to(self.cursor.row.saturating_sub(1), self.cursor.col),
            _ if sequence.starts_with("\x1b[") => {
                let bytes = sequence.as_bytes();
                self.csi(&sequence[2..bytes.len() - 1], bytes[bytes.len() - 1]);
            }
            _ => {}
        }
    }

    fn csi(&mut self, params: &str, final_byte: u8) {
        if !params
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b';' | b':'))
            || (final_byte != b'm' && params.contains(':'))
        {
            return;
        }
        if final_byte == b'm' {
            self.pen.apply(params);
            return;
        }
        let mut numbers = params.split(';').map(number);
        let first = numbers.next().unwrap_or(0.0) as usize;
        let count = first.max(1);
        let Position { row, col } = self.cursor;
        match final_byte {
            b'A' => self.move_to(row.saturating_sub(count), col),
            b'B' => self.move_to(row.saturating_add(count), col),
            b'C' => self.move_to(row, col.saturating_add(count)),
            b'D' => self.move_to(row, col.saturating_sub(count)),
            b'E' => self.move_to(row.saturating_add(count), 0),
            b'F' => self.move_to(row.saturating_sub(count), 0),
            b'G' => self.move_to(row, count - 1),
            b'H' | b'f' => self.move_to(
                count - 1,
                (numbers.next().unwrap_or(1.0) as usize).max(1) - 1,
            ),
            b'K' => self.lines[row].erase(col, first),
            b'J' => self.erase_display(first),
            b's' => self.saved = Some(self.cursor),
            b'u' => self.restore_position(),
            _ => {}
        }
    }

    fn feed(&mut self, input: &str, limit: usize) -> String {
        let mut index = 0;
        let mut printable = 0;
        while index < input.len() {
            let character = input[index..].chars().next().expect("remaining input");
            let code = u32::from(character);
            let next = index + character.len_utf8();
            if character == '\t' || (code >= 0x20 && !(0x7f..=0x9f).contains(&code)) {
                index = next;
                continue;
            }
            self.write(&input[printable..index], limit);
            if character == char::from(ESC) {
                let Some(end) = escape_end(input, index) else {
                    return pending_escape(&input[index..]);
                };
                self.escape(&input[index..end]);
                index = end;
            } else {
                match character {
                    '\n' => self.newline(limit),
                    '\r' => self.cursor.col = 0,
                    '\x08' => self.cursor.col = self.cursor.col.saturating_sub(1),
                    _ => {}
                }
                index = next;
            }
            printable = index;
        }
        self.write(&input[printable..], limit);
        String::new()
    }
}

fn string_introducer(byte: u8) -> bool {
    matches!(byte, b']' | b'P' | b'X' | b'_' | b'^')
}

fn escape_end(input: &str, start: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let next = *bytes.get(start + 1)?;
    if next == ESC {
        return Some(start + 1);
    }
    if next == b'[' {
        let offset = bytes[start + 2..]
            .iter()
            .position(|byte| (0x40..=0x7e).contains(byte))?;
        return Some(start + 3 + offset);
    }
    if string_introducer(next) {
        let terminator = start
            + 2
            + bytes[start + 2..]
                .iter()
                .position(|byte| matches!(*byte, 7 | b'\n' | ESC))?;
        return match bytes[terminator] {
            7 => Some(terminator + 1),
            b'\n' => Some(terminator),
            _ if *bytes.get(terminator + 1)? == b'\\' => Some(terminator + 2),
            _ => Some(terminator),
        };
    }
    let index = start
        + 1
        + bytes[start + 1..]
            .iter()
            .take_while(|byte| (0x20..=0x2f).contains(*byte))
            .count();
    let code = *bytes.get(index)?;
    if index > start + 1 && !(0x30..=0x7e).contains(&code) {
        return Some(index);
    }
    let character = input[index..].chars().next()?;
    Some(index + character.len_utf8())
}

fn pending_escape(input: &str) -> String {
    if utf16_len(input) <= MAX_PENDING_ESCAPE {
        return input.to_owned();
    }
    if input
        .as_bytes()
        .get(1)
        .copied()
        .is_some_and(string_introducer)
    {
        let mut introducer = input[..2].to_owned();
        if input.ends_with(char::from(ESC)) {
            introducer.push(char::from(ESC));
        }
        return introducer;
    }
    String::new()
}
