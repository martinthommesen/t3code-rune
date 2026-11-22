use std::collections::VecDeque;

const HISTORY_BYTES: usize = 512 * 1024;
pub(super) const PERSISTED_HISTORY_BYTES: usize = 8 * 1024 * 1024;
pub(super) const PERSISTED_HISTORY_LINES: usize = 5_000;
const CHUNK_BYTES: usize = 8 * 1024;
const INHERITED_INPUT_RESET: &str = concat!(
    "\x1b[?1049h\x1b[<100u\x1b[=0;1u\x1b[?1049l",
    "\x1b[?1;9;66;67;1000;1002;1003;1004;1005;1006;1015;2004l",
    "\x1b[?25;1035;1036h\x1b[<100u\x1b[=0;1u\x1b[>4m",
);

pub(super) fn retain_tail(history: &mut String, bytes: usize) {
    if history.len() <= bytes {
        return;
    }
    let cut = history.ceil_char_boundary(history.len() - bytes);
    let state = Sequence::Ground.after(&history[..cut]);
    let (skip, _) = state.until_ground(&history[cut..]);
    history.drain(..cut + skip);
}

pub(super) struct History {
    chunks: VecDeque<String>,
    front_offset: usize,
    bytes: usize,
    line_breaks: usize,
    max_lines: usize,
    max_bytes: usize,
    start_state: Sequence,
    generation: u64,
    appended: usize,
}

#[derive(Clone, Copy)]
pub(super) struct Cursor {
    generation: u64,
    offset: usize,
    file_bytes: usize,
}

pub(super) struct WriteUpdate {
    pub(super) data: String,
    pub(super) append: bool,
    pub(super) cursor: Cursor,
}

impl Default for History {
    fn default() -> Self {
        Self::with_limits(usize::MAX, HISTORY_BYTES)
    }
}

impl History {
    pub(super) fn with_limits(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            front_offset: 0,
            bytes: 0,
            line_breaks: 0,
            max_lines,
            max_bytes,
            start_state: Sequence::Ground,
            generation: 0,
            appended: 0,
        }
    }

    pub(super) fn append(&mut self, mut text: &str) {
        if text.is_empty() {
            return;
        }
        self.appended += text.len();
        if self.max_bytes == 0 || self.max_lines == 0 {
            self.clear();
            if self.max_bytes > 0 && text.ends_with('\n') {
                self.append_chunk("\n");
            }
            return;
        }
        while !text.is_empty() {
            let end = text.floor_char_boundary(CHUNK_BYTES.min(text.len()));
            let (chunk, rest) = text.split_at(end);
            self.append_chunk(chunk);
            self.trim();
            text = rest;
        }
    }

    fn append_chunk(&mut self, chunk: &str) {
        if let Some(last) = self.chunks.back_mut()
            && last.len() + chunk.len() <= CHUNK_BYTES
        {
            last.push_str(chunk);
        } else {
            self.chunks.push_back(chunk.to_owned());
        }
        self.bytes += chunk.len();
        self.line_breaks += line_breaks(chunk);
    }

    pub(super) fn clear(&mut self) {
        let generation = self.generation + 1;
        *self = Self::with_limits(self.max_lines, self.max_bytes);
        self.generation = generation;
    }

    pub(super) fn text(&self) -> String {
        let mut text = String::with_capacity(self.bytes);
        if let Some(first) = self.front() {
            text.push_str(first);
        }
        for chunk in self.chunks.iter().skip(1) {
            text.push_str(chunk);
        }
        text
    }

    fn front(&self) -> Option<&str> {
        self.chunks.front().map(|chunk| &chunk[self.front_offset..])
    }

    pub(super) fn persistence_update(&self, previous: Option<Cursor>) -> WriteUpdate {
        let append = previous.filter(|cursor| {
            cursor.generation == self.generation
                && self.appended >= cursor.offset
                && self.appended - cursor.offset <= self.bytes
                && cursor.file_bytes + self.appended - cursor.offset <= self.max_bytes * 2
        });
        let data = if let Some(cursor) = append {
            let delta = self.appended - cursor.offset;
            let mut remaining = delta;
            let mut parts = Vec::new();
            for chunk in self.chunks.iter().rev() {
                if remaining == 0 {
                    break;
                }
                let take = remaining.min(chunk.len());
                parts.push(&chunk[chunk.len() - take..]);
                remaining -= take;
            }
            let mut data = String::with_capacity(delta);
            for part in parts.into_iter().rev() {
                data.push_str(part);
            }
            data
        } else {
            self.text()
        };
        let file_bytes = append.map_or(0, |cursor| cursor.file_bytes) + data.len();
        WriteUpdate {
            data,
            append: append.is_some(),
            cursor: Cursor {
                generation: self.generation,
                offset: self.appended,
                file_bytes,
            },
        }
    }

    fn trim(&mut self) {
        let trailing_newline = self
            .chunks
            .back()
            .is_some_and(|chunk| chunk.ends_with('\n'));
        let mut excess =
            (self.line_breaks + usize::from(!trailing_newline)).saturating_sub(self.max_lines);
        while excess > 0 {
            let first = self.front().expect("history has retained lines");
            let offset = first
                .bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .nth(excess - 1)
                .map_or(first.len(), |(index, _)| index + 1);
            excess -= line_breaks(&first[..offset]);
            self.discard_prefix(offset);
        }
        while self.bytes > self.max_bytes {
            let first = self.front().expect("history has retained bytes");
            let excess = (self.bytes - self.max_bytes).min(first.len());
            self.discard_prefix(first.ceil_char_boundary(excess));
        }
        while self.start_state != Sequence::Ground {
            let Some(first) = self.front() else { break };
            let (drop, state) = self.start_state.until_ground(first);
            self.start_state = state;
            self.discard(drop);
        }
    }

    fn discard_prefix(&mut self, bytes: usize) {
        let first = self.front().expect("history has retained bytes");
        self.start_state = self.start_state.after(&first[..bytes]);
        self.discard(bytes);
    }

    fn discard(&mut self, bytes: usize) {
        self.line_breaks -=
            line_breaks(&self.front().expect("history has retained bytes")[..bytes]);
        self.bytes -= bytes;
        self.front_offset += bytes;
        if self
            .chunks
            .front()
            .is_some_and(|chunk| chunk.len() == self.front_offset)
        {
            self.chunks.pop_front();
            self.front_offset = 0;
        }
    }
}

fn line_breaks(text: &str) -> usize {
    text.bytes().filter(|byte| *byte == b'\n').count()
}

pub(super) fn neutralize_inherited(history: &str, max_lines: usize, max_bytes: usize) -> String {
    let reset_bytes = if history.contains('\x1b') {
        INHERITED_INPUT_RESET.len() + "\x1b7\x1b[?1049l".len()
    } else {
        0
    };
    let lines = if reset_bytes > 0 && history.ends_with('\n') {
        max_lines.saturating_sub(1)
    } else {
        max_lines
    };
    let mut replay = History::with_limits(
        lines,
        max_bytes.min(HISTORY_BYTES).saturating_sub(reset_bytes),
    );
    replay.append(history);
    let mut replay = replay.text();
    if !replay.contains('\x1b') {
        return replay;
    }

    let mut alternate = None;
    for (index, _) in replay.match_indices('\x1b') {
        let sequence = &replay[index + 1..];
        if sequence.starts_with('c') {
            alternate = None;
        }
        let Some(modes) = sequence.strip_prefix("[?") else {
            continue;
        };
        let end = modes
            .bytes()
            .position(|byte| !byte.is_ascii_digit() && byte != b';');
        let Some(end) = end.filter(|end| *end > 0) else {
            continue;
        };
        let choice = modes.as_bytes()[end];
        if !matches!(choice, b'h' | b'l') {
            continue;
        }
        for mode in modes[..end]
            .split(';')
            .filter_map(|mode| mode.parse::<u16>().ok())
        {
            if matches!(mode, 47 | 1047 | 1049) {
                if choice == b'h' {
                    alternate.get_or_insert(mode);
                } else {
                    alternate = None;
                }
            }
        }
    }
    if let Some(mode) = alternate {
        use std::fmt::Write;
        write!(&mut replay, "\x1b7\x1b[?{mode}l").expect("writing to String succeeds");
    }
    replay.push_str(INHERITED_INPUT_RESET);
    replay
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Sequence {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    ControlString,
}

impl Sequence {
    pub(super) fn after(self, text: &str) -> Self {
        let (state, tail) = text
            .rsplit_once('\u{1b}')
            .map_or((self, text), |(_, tail)| (Self::Escape, tail));
        state.until_ground(tail).1
    }

    pub(super) fn until_ground(mut self, text: &str) -> (usize, Self) {
        if self == Self::Ground {
            return (0, self);
        }
        for (offset, character) in text.char_indices() {
            self = self.advance(character);
            if self == Self::Ground {
                return (offset + character.len_utf8(), self);
            }
        }
        (text.len(), self)
    }

    fn advance(self, character: char) -> Self {
        match character {
            '\u{1b}' => return Self::Escape,
            '\u{18}' | '\u{1a}' => return Self::Ground,
            _ if self == Self::Ground => return Self::Ground,
            _ => {}
        }

        if character.is_ascii() {
            return match (self, character) {
                (Self::Escape, '\0'..='\u{1f}' | '\u{7f}') => self,
                (Self::Escape, ' '..='/') => Self::EscapeIntermediate,
                (Self::Escape, '[') => Self::Csi,
                (Self::Escape, ']') => Self::Osc,
                (Self::Escape, 'P' | 'X' | '^' | '_') => Self::ControlString,
                (Self::Escape, _)
                | (Self::EscapeIntermediate, '0'..='~')
                | (Self::Csi, '@'..='~')
                | (Self::Osc, '\u{7}') => Self::Ground,
                _ => self,
            };
        }

        // Outside ground and OSC, libghostty-vt reads UTF-8 continuation bytes as C1 controls.
        if self == Self::Osc {
            return self;
        }
        let mut state = self;
        let mut bytes = [0; 4];
        for &byte in &character.encode_utf8(&mut bytes).as_bytes()[1..] {
            state = match byte {
                0x90 | 0x98 | 0x9e | 0x9f => Self::ControlString,
                0x9b => Self::Csi,
                0x9d => return Self::Osc,
                0x80..=0x9f => return Self::Ground,
                _ => state,
            };
        }
        state
    }
}

#[derive(Default)]
pub(super) struct Utf8Output {
    pending: Vec<u8>,
}

impl Utf8Output {
    pub(super) fn decode(&mut self, bytes: &[u8]) -> String {
        let mut joined;
        let mut remaining = if self.pending.is_empty() {
            bytes
        } else {
            joined = std::mem::take(&mut self.pending);
            joined.extend_from_slice(bytes);
            &joined
        };
        let mut output = String::with_capacity(remaining.len());

        while !remaining.is_empty() {
            match std::str::from_utf8(remaining) {
                Ok(valid) => {
                    output.push_str(valid);
                    break;
                }
                Err(error) => {
                    let (valid, rest) = remaining.split_at(error.valid_up_to());
                    output.push_str(std::str::from_utf8(valid).expect("valid UTF-8 prefix"));
                    let Some(invalid_len) = error.error_len() else {
                        self.pending.extend_from_slice(rest);
                        break;
                    };
                    output.push(char::REPLACEMENT_CHARACTER);
                    remaining = &rest[invalid_len..];
                }
            }
        }
        output
    }

    pub(super) fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            String::new()
        } else {
            self.pending.clear();
            char::REPLACEMENT_CHARACTER.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{HISTORY_BYTES, History, Utf8Output};

    #[test]
    fn retains_a_bounded_suffix_at_a_unicode_boundary() {
        let mut history = History::default();
        let marker = "\u{feff}😀";
        let retained = format!("{marker}{}", "x".repeat(HISTORY_BYTES - marker.len() - 1));

        history.append("older line\n名");
        history.append(&retained);

        assert_eq!(history.text(), retained);
    }

    #[test]
    fn discards_cut_control_sequences_across_appends() {
        for (start, fill, end) in [
            ("\x1b]0;", "x", "\x07"),
            ("\x1b[", ";", "m"),
            ("\x1bP", "x", "\x1b\\"),
        ] {
            let mut history = History::default();
            history.append(start);
            history.append(&fill.repeat(HISTORY_BYTES));
            assert!(history.text().is_empty());

            history.append(fill);
            history.append(end);
            history.append("visible\x1b[0m");
            assert_eq!(history.text(), "visible\x1b[0m");
        }
    }

    #[test]
    fn clearing_history_resets_a_truncated_sequence() {
        let mut history = History::default();
        history.append("\x1b]0;");
        history.append(&"x".repeat(HISTORY_BYTES));
        assert!(history.text().is_empty());

        history.clear();
        history.append("new line");
        assert_eq!(history.text(), "new line");
    }

    #[test]
    fn decodes_valid_scalars_at_every_chunk_boundary() {
        let text = "a\u{feff}é名😀z";
        for split in 0..=text.len() {
            let mut decoder = Utf8Output::default();
            let (left, right) = text.as_bytes().split_at(split);
            let decoded = decoder.decode(left) + &decoder.decode(right) + &decoder.finish();
            assert_eq!(decoded, text);
        }
    }

    #[test]
    fn replaces_malformed_bytes_and_flushes_an_incomplete_scalar_at_eof() {
        let mut decoder = Utf8Output::default();
        assert_eq!(decoder.decode(&[b'a', 0xf0, 0x90]), "a");
        assert_eq!(decoder.decode(b"!\xff\xe2"), "�!�");
        assert_eq!(decoder.finish(), "�");
        assert_eq!(decoder.decode(b"next"), "next");
        assert!(decoder.finish().is_empty());
    }
}
