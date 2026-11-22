use super::super::setup_progress::Progress;

const BRACKET_ON: &str = "\x1b[?2004h";
const BRACKET_OFF: &str = "\x1b[?2004l";
const MAX_LINE: usize = 400;
const MAX_PARTIAL: usize = 4096;

pub(super) struct Capture<'a> {
    sentinel: &'a str,
    echo: Vec<&'a str>,
    progress: &'a Progress,
    partial: String,
    carry: String,
    reading: bool,
    exit: Option<i32>,
}

impl<'a> Capture<'a> {
    pub(super) fn new(sentinel: &'a str, command: &'a str, progress: &'a Progress) -> Self {
        Self {
            sentinel,
            echo: command
                .split('\r')
                .filter(|line| !line.is_empty())
                .collect(),
            progress,
            partial: String::new(),
            carry: String::new(),
            reading: false,
            exit: None,
        }
    }

    pub(super) fn output(&mut self, data: &str) {
        self.carry.push_str(data);
        let on = self.carry.rfind(BRACKET_ON);
        let off = self.carry.rfind(BRACKET_OFF);
        if on != off {
            self.reading = on > off;
        }
        let mut carry_start = self.carry.len().saturating_sub(BRACKET_ON.len() - 1);
        while !self.carry.is_char_boundary(carry_start) {
            carry_start += 1;
        }
        self.carry.drain(..carry_start);

        let mut lines = data.split(['\r', '\n']).peekable();
        while let Some(line) = lines.next() {
            self.partial.push_str(line);
            if lines.peek().is_some() {
                self.flush_partial_line();
            }
        }
        let mut remaining = MAX_PARTIAL;
        let start = self
            .partial
            .char_indices()
            .rev()
            .find_map(|(index, character)| {
                if let Some(next) = remaining.checked_sub(character.len_utf16()) {
                    remaining = next;
                    None
                } else {
                    Some(index + character.len_utf8())
                }
            });
        if let Some(start) = start {
            self.partial.drain(..start);
        }
    }

    pub(super) fn reading(&self) -> bool {
        self.reading
    }

    pub(super) fn exit_code(&self) -> Option<i32> {
        self.exit
    }

    pub(super) fn has_partial_line(&self) -> bool {
        !self.partial.is_empty()
    }

    pub(super) fn flush_partial_line(&mut self) {
        let line = std::mem::take(&mut self.partial);
        if let Some((_, suffix)) = line.split_once(self.sentinel) {
            let numbers = suffix.strip_prefix('-').unwrap_or(suffix);
            let digits = numbers
                .as_bytes()
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits > 0 {
                self.exit.get_or_insert_with(|| {
                    suffix[..suffix.len() - numbers.len() + digits]
                        .parse()
                        .unwrap_or(1)
                });
            }
            return;
        }
        let cleaned = strip_control(&line);
        let cleaned = cleaned.trim_end_matches(|character: char| {
            character == '\u{feff}' || character.is_whitespace() && character != '\u{85}'
        });
        if cleaned.is_empty() || self.echo.iter().any(|echo| cleaned.ends_with(echo)) {
            return;
        }
        let mut remaining = MAX_LINE;
        let end = cleaned.char_indices().find_map(|(index, character)| {
            if let Some(next) = remaining.checked_sub(character.len_utf16()) {
                remaining = next;
                None
            } else {
                Some(index)
            }
        });
        self.progress
            .append_tail(cleaned[..end.unwrap_or(cleaned.len())].to_owned());
    }
}

fn strip_control(text: &str) -> String {
    let mut rest = text;
    let mut result = String::with_capacity(text.len());
    while let Some(index) = rest.find('\x1b') {
        append_without_control(&mut result, &rest[..index]);
        let escape = &rest[index + 1..];
        let length = escape_length(escape.as_bytes()).unwrap_or_default();
        rest = &escape[length..];
    }
    append_without_control(&mut result, rest);
    result
}

fn append_without_control(target: &mut String, text: &str) {
    target.extend(text.chars().filter(|character| {
        !matches!(*character, '\u{0}'..='\u{8}' | '\u{b}'..='\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}')
    }));
}

fn escape_length(bytes: &[u8]) -> Option<usize> {
    if let Some(rest) = bytes.strip_prefix(b"[") {
        let params = rest
            .iter()
            .take_while(|byte| (0x30..=0x3f).contains(*byte))
            .count();
        if let Some(end) = sequence_end(&rest[params..], 0x40, 0x7e) {
            return Some(1 + params + end);
        }
    }
    if let Some(rest) = bytes.strip_prefix(b"]") {
        let end = rest.iter().position(|byte| matches!(*byte, 0x07 | 0x1b));
        if let Some(end) = end {
            match &rest[end..] {
                [0x07, ..] => return Some(2 + end),
                [0x1b, b'\\', ..] => return Some(3 + end),
                _ => (),
            }
        }
    }
    sequence_end(bytes, 0x30, 0x7e)
}

fn sequence_end(bytes: &[u8], min: u8, max: u8) -> Option<usize> {
    let prefix = bytes
        .iter()
        .take_while(|byte| (0x20..=0x2f).contains(*byte))
        .count();
    bytes
        .get(prefix)
        .filter(|byte| (min..=max).contains(*byte))
        .map(|_| prefix + 1)
}

#[tokio::test]
async fn keeps_live_output_and_accepts_only_the_setup_completion_line() {
    use super::super::setup_progress::subscribe_worktree_setup;
    use std::path::Path;

    let database = Path::new("test:setup-output");
    let thread_id = "test:pty";
    let progress = Progress::begin(database, thread_id, None, "main");
    let mut subscription = subscribe_worktree_setup(database, thread_id);
    let mut capture = Capture::new("__setup_token__:", "wrapper __setup_token__:%s", &progress);
    capture.output("\x1b[?20");
    assert!(!capture.reading());
    capture.output("04hprompt$ wrapper __setup_token__:%s\r\n");
    assert!(capture.reading());
    capture.output("\x1b[?2004l\x1b[32m  colored\x1b[0m \r\nfalse-token:99\n");
    assert!(!capture.reading());
    assert_eq!(capture.exit_code(), None);
    capture.output("__setup_");
    capture.output("token__:7\r\nprompt$");
    assert_eq!(capture.exit_code(), Some(7));
    assert!(capture.has_partial_line());
    let snapshot = subscription.recv().await.unwrap();
    assert_eq!(
        snapshot["stages"][3]["tail"],
        serde_json::json!(["  colored", "false-token:99"])
    );
}
