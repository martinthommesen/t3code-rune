use std::collections::VecDeque;

const ESC: char = '\x1b';
const STRING_TERMINATOR: &str = "\x1b\\";
const MAX_PENDING_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Scrubber {
    state: State,
}

impl Scrubber {
    pub(super) fn push(&mut self, data: &str) -> String {
        if matches!(self.state, State::Ground) && !data.contains(ESC) {
            return data.to_owned();
        }

        let mut visible = String::new();
        for character in data.chars() {
            if let Some(state) = std::mem::take(&mut self.state).advance(character, &mut visible) {
                self.state = state;
            }
        }
        visible
    }

    pub(super) fn clear(&mut self) {
        self.state = State::Ground;
    }
}

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate(Pending),
    Csi(Csi),
    ControlString(ControlString),
}

impl State {
    fn advance(self, character: char, visible: &mut String) -> Option<Self> {
        match self {
            Self::Ground if character == ESC => Some(Self::Escape),
            Self::Ground => {
                visible.push(character);
                None
            }
            Self::Escape => Self::after_escape(character, visible),
            Self::EscapeIntermediate(mut pending) => {
                if matches!(character, ' '..='/') {
                    pending.push(character);
                    return Some(Self::EscapeIntermediate(pending));
                }
                let emitted = pending.emit(visible);
                if character == ESC {
                    return Some(Self::Escape);
                }
                if emitted || !matches!(character, '0'..='~') {
                    visible.push(character);
                }
                None
            }
            Self::Csi(csi) => csi.advance(character, visible),
            Self::ControlString(string) => string.advance(character, visible),
        }
    }

    fn after_escape(character: char, visible: &mut String) -> Option<Self> {
        match character {
            '[' => Some(Self::Csi(Csi::Buffered(String::from("\x1b[")))),
            ']' | 'P' | '^' | '_' => {
                let mut introducer = String::from(ESC);
                introducer.push(character);
                Some(Self::ControlString(ControlString::new(introducer)))
            }
            ' '..='/' => {
                let mut pending = Pending::new(String::from(ESC));
                pending.push(character);
                Some(Self::EscapeIntermediate(pending))
            }
            ESC => {
                visible.push(ESC);
                Some(Self::Escape)
            }
            'Z' => None,
            _ => {
                visible.push(ESC);
                visible.push(character);
                None
            }
        }
    }

    fn from_c1(next: String) -> Option<Self> {
        match next.as_bytes().get(1) {
            Some(b'[') => Some(Self::Csi(Csi::Buffered(next))),
            Some(b']' | b'P' | b'_') => Some(Self::ControlString(ControlString::new(next))),
            _ => None,
        }
    }
}

struct Pending {
    text: String,
    oversized: bool,
}

impl Pending {
    fn new(text: String) -> Self {
        Self {
            text,
            oversized: false,
        }
    }

    fn push(&mut self, character: char) {
        if self.oversized {
            return;
        }
        if self.text.len() + character.len_utf8() <= MAX_PENDING_BYTES {
            self.text.push(character);
        } else {
            // Keep framing even after the sequence can no longer fit in retained history.
            self.text.truncate(2);
            self.text.shrink_to_fit();
            self.oversized = true;
        }
    }

    fn emit(&self, visible: &mut String) -> bool {
        if self.oversized {
            return false;
        }
        visible.push_str(&self.text);
        true
    }
}

enum Csi {
    Buffered(String),
    Oversized(VecDeque<u8>),
}

impl Csi {
    fn advance(mut self, character: char, visible: &mut String) -> Option<State> {
        if cancelled(character) || matches!(character, '@'..='~') {
            match &self {
                Self::Buffered(text) if cancelled(character) || !strip_csi(text, character) => {
                    visible.push_str(text);
                    visible.push(character);
                }
                _ => self.emit_controls(visible),
            }
            return None;
        }
        if character == ESC {
            self.emit_controls(visible);
            return Some(State::Escape);
        }
        if let Some(effect) = c1_effect(character) {
            match &self {
                Self::Buffered(text) if effect.next.is_empty() && !effect.query => {
                    visible.push_str(text);
                    visible.push(character);
                }
                _ => {
                    self.emit_controls(visible);
                    visible.push_str(&effect.safe);
                }
            }
            return State::from_c1(effect.next);
        }
        self.push(character);
        Some(State::Csi(self))
    }

    fn push(&mut self, character: char) {
        if let Self::Buffered(text) = self {
            if text.len() + character.len_utf8() <= MAX_PENDING_BYTES {
                text.push(character);
                return;
            }
            let mut bytes = std::mem::take(text).into_bytes();
            bytes.retain(|byte| *byte < 0x10);
            *self = Self::Oversized(bytes.into());
        }
        if let Self::Oversized(controls) = self
            && (character as u32) < 0x10
        {
            if controls.len() == MAX_PENDING_BYTES {
                controls.pop_front();
            }
            controls.push_back(character as u8);
        }
    }

    fn emit_controls(&self, visible: &mut String) {
        match self {
            Self::Buffered(text) => {
                visible.extend(text.bytes().filter(|byte| *byte < 0x10).map(char::from))
            }
            Self::Oversized(controls) => visible.extend(controls.iter().copied().map(char::from)),
        }
    }
}

fn strip_csi(text: &str, final_byte: char) -> bool {
    let mut body = text
        .bytes()
        .skip(2)
        .filter(|byte| (0x20..=0x7e).contains(byte));
    let is_parameter = |byte: u8| byte.is_ascii_digit() || matches!(byte, b';' | b'?');
    match final_byte {
        'n' => true,
        'R' => body.all(is_parameter),
        'c' => body.all(|byte| is_parameter(byte) || matches!(byte, b'>' | b'=')),
        'p' | 'y' => {
            let Some(last) = body.next_back() else {
                return false;
            };
            last == b'$' && body.all(is_parameter)
        }
        'q' => body.next() == Some(b'>') && body.all(|byte| byte.is_ascii_digit() || byte == b';'),
        'u' => body.next() == Some(b'?'),
        _ => false,
    }
}

struct ControlString {
    pending: Pending,
    escape_pending: bool,
    palette: Option<PaletteQuery>,
}

impl ControlString {
    fn new(introducer: String) -> Self {
        let palette = introducer.strip_prefix("\x1b]").map(|body| {
            let mut palette = PaletteQuery::default();
            for character in body.chars() {
                palette.push(character);
            }
            palette
        });
        Self {
            pending: Pending::new(introducer),
            escape_pending: false,
            palette,
        }
    }

    fn advance(mut self, character: char, visible: &mut String) -> Option<State> {
        let is_osc = self.palette.is_some();
        if self.escape_pending {
            if character == '\\' {
                self.finish(STRING_TERMINATOR, visible);
                return None;
            }
            if !self.should_strip(self.aborted_body()) {
                self.pending.emit(visible);
            }
            return State::after_escape(character, visible);
        }
        let terminal = cancelled(character) || (is_osc && character == '\x07');
        if terminal {
            let mut encoded = [0; 4];
            self.finish(character.encode_utf8(&mut encoded), visible);
            return None;
        }
        if character == ESC {
            self.escape_pending = true;
        } else if !is_osc && let Some(effect) = c1_effect(character) {
            let strip = self.should_strip(self.aborted_body());
            if self.pending.oversized {
                visible.push_str(&effect.safe);
            } else if !strip && effect.next.is_empty() && !effect.query {
                self.pending.emit(visible);
                visible.push(character);
            } else {
                if !strip && self.pending.emit(visible) {
                    visible.push('\u{009c}');
                }
                visible.push_str(&effect.safe);
            }
            return State::from_c1(effect.next);
        } else {
            if let Some(palette) = &mut self.palette {
                palette.push(character);
            }
            self.pending.push(character);
        }
        Some(State::ControlString(self))
    }

    fn finish(&self, terminator: &str, visible: &mut String) {
        let body = &self.pending.text[2..];
        if self.palette.is_some_and(PaletteQuery::confirmed) {
            visible.push_str(STRING_TERMINATOR);
        } else if !self.should_strip(body) && self.pending.emit(visible) {
            visible.push_str(terminator);
        }
    }

    fn aborted_body(&self) -> &str {
        let body = &self.pending.text[2..];
        body.strip_suffix(['\x07', '\u{009c}']).unwrap_or(body)
    }

    fn should_strip(&self, body: &str) -> bool {
        match self.pending.text.as_bytes()[1] {
            b']' => strip_osc(body),
            b'P' => strip_dcs(body),
            _ => false,
        }
    }
}

fn strip_dcs(body: &str) -> bool {
    let candidate = body.strip_prefix(['0', '1']).unwrap_or(body);
    let bytes = candidate.as_bytes();
    matches!(bytes, [b'$' | b'+', b'q' | b'r', ..])
}

fn strip_osc(body: &str) -> bool {
    let bytes = body.as_bytes();
    matches!(
        bytes,
        [b'1', b'0'..=b'2', b';', b'?', ..] | [b'1', b'0'..=b'2', b';', b'r', b'g', b'b', b':', ..]
    )
}

#[derive(Clone, Copy, Default)]
struct PaletteQuery {
    phase: PalettePhase,
    field: PaletteField,
    queried: bool,
}

#[derive(Clone, Copy, Default)]
enum PalettePhase {
    #[default]
    Code,
    Index,
    Color,
    Rejected,
}

#[derive(Clone, Copy, Default)]
enum PaletteField {
    #[default]
    Empty,
    Four,
    Question,
    Other,
}

impl PaletteQuery {
    fn push(&mut self, character: char) {
        if character < ' ' || matches!(self.phase, PalettePhase::Rejected) {
            return;
        }
        if character != ';' {
            self.field = match (self.field, character) {
                (PaletteField::Empty, '4') => PaletteField::Four,
                (PaletteField::Empty, '?') => PaletteField::Question,
                _ => PaletteField::Other,
            };
            return;
        }

        self.phase = match (self.phase, std::mem::take(&mut self.field)) {
            (PalettePhase::Code, PaletteField::Four) => PalettePhase::Index,
            (PalettePhase::Index, PaletteField::Empty) => PalettePhase::Index,
            (PalettePhase::Index, _) => PalettePhase::Color,
            (PalettePhase::Color, PaletteField::Empty) => PalettePhase::Color,
            (PalettePhase::Color, PaletteField::Question) => {
                self.queried = true;
                PalettePhase::Index
            }
            _ => PalettePhase::Rejected,
        };
    }

    fn confirmed(mut self) -> bool {
        self.push(';');
        self.queried && !matches!(self.phase, PalettePhase::Rejected)
    }
}

fn cancelled(character: char) -> bool {
    matches!(character, '\x18' | '\x1a')
}

struct C1Effect {
    next: String,
    query: bool,
    safe: String,
}

fn c1_effect(character: char) -> Option<C1Effect> {
    if character.is_ascii() {
        return None;
    }
    let mut encoded = [0; 4];
    let mut next: Option<String> = None;
    let mut query = false;
    let mut safe = String::new();
    // Ghostty interprets continuation bytes as C1 controls outside ground and OSC.
    for &byte in &character.encode_utf8(&mut encoded).as_bytes()[1..] {
        match next.as_deref() {
            Some("") => safe.push('\u{fffd}'),
            Some(sequence) if sequence.starts_with("\x1b]") => {
                next.as_mut().expect("OSC introducer").push('\u{fffd}');
            }
            _ => match byte {
                0x9b => next = Some(String::from("\x1b[")),
                0x9d => next = Some(String::from("\x1b]")),
                0x90 => next = Some(String::from("\x1bP")),
                0x98 | 0x9e | 0x9f => next = Some(String::from("\x1b_")),
                0x80..=0x9f => {
                    next = Some(String::new());
                    query = byte == 0x9a;
                    if !query && byte != 0x9c {
                        safe.push(ESC);
                        safe.push(char::from(byte - 0x40));
                    }
                }
                _ => {}
            },
        }
    }
    next.map(|next| C1Effect { next, query, safe })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_split_queries_and_keeps_visible_effects() {
        let mut scrubber = Scrubber::default();
        assert_eq!(scrubber.push("before\x1b[5\r"), "before");
        assert_eq!(scrubber.push("nafter\x1bP$qm\x07hidden\x1b"), "\rafter");
        assert_eq!(
            scrubber.push("\\\x1b[嚠text\u{009b}6n"),
            "\u{fffd}text\u{009b}6n"
        );
        assert_eq!(
            scrubber.push("\x1b]4;1;red;2;?\x07"),
            "\x1b]4;1;red;2;?\x07"
        );
        assert_eq!(scrubber.push("\x1b]\n4;1;?;;2;?;3\x18"), STRING_TERMINATOR);
        assert_eq!(scrubber.push("\x1b[?5"), "");
        scrubber.clear();
        assert_eq!(scrubber.push("n"), "n");
    }

    #[test]
    fn oversized_control_string_payload_does_not_escape_into_history() {
        let mut scrubber = Scrubber::default();
        assert_eq!(scrubber.push("prompt\x1b]0;"), "prompt");
        let chunk = "a".repeat(8192);
        for _ in 0..=MAX_PENDING_BYTES / chunk.len() {
            assert!(scrubber.push(&chunk).is_empty());
        }
        assert!(scrubber.push("unretained\x1b").is_empty());
        assert_eq!(scrubber.push("\\visible"), "visible");
    }
}
