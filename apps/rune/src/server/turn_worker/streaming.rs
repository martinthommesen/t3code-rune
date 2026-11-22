use std::collections::HashMap;
use std::time::{Duration, Instant};

const PARAGRAPH_INTERVAL: Duration = Duration::from_millis(400);
const IDLE_FLUSH: Duration = Duration::from_millis(800);

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum StreamingMode {
    Turn,
    Paragraph,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Snapshot<T> {
    pub(super) text: String,
    pub(super) data: T,
}

pub(super) struct AssistantStreaming<T> {
    mode: StreamingMode,
    tracks: HashMap<String, StreamTrack<T>>,
    next_order: usize,
    next_idle_flush_at: Option<Instant>,
}

struct StreamTrack<T> {
    delivered: String,
    delivered_at: Option<Instant>,
    held: Option<HeldSnapshot<T>>,
    order: usize,
    scan: TextScan,
}

struct HeldSnapshot<T> {
    snapshot: Snapshot<T>,
    due_at: Instant,
}

impl<T: Clone> AssistantStreaming<T> {
    pub(super) fn new(mode: StreamingMode) -> Self {
        Self {
            mode,
            tracks: HashMap::new(),
            next_order: 0,
            next_idle_flush_at: None,
        }
    }

    pub(super) fn filter(
        &mut self,
        item_id: &str,
        snapshot: Snapshot<T>,
        now: Instant,
    ) -> Option<Snapshot<T>> {
        if self.mode == StreamingMode::Turn {
            return None;
        }
        let track = self.tracks.entry(item_id.to_owned()).or_insert_with(|| {
            let order = self.next_order;
            self.next_order += 1;
            StreamTrack {
                delivered: String::new(),
                delivered_at: None,
                held: None,
                order,
                scan: TextScan::default(),
            }
        });
        let mut visible = None;
        if track
            .delivered_at
            .is_none_or(|at| now.saturating_duration_since(at) >= PARAGRAPH_INTERVAL)
            && let Some(boundary) = track.scan.advance(&snapshot.text)
        {
            let ready = &snapshot.text[..boundary];
            if !track.delivered.starts_with(ready) {
                track.delivered.clear();
                track.delivered.push_str(ready);
                track.delivered_at = Some(now);
                visible = Some(ready);
            }
        }
        if snapshot.text == track.delivered {
            track.held = None;
            return visible.is_some().then_some(snapshot);
        }
        let view = visible.map(|text| Snapshot {
            text: text.to_owned(),
            data: snapshot.data.clone(),
        });
        let due_at = now + IDLE_FLUSH;
        track.held = Some(HeldSnapshot { snapshot, due_at });
        self.next_idle_flush_at = Some(self.next_idle_flush_at.map_or(due_at, |at| at.min(due_at)));
        view
    }

    pub(super) fn complete(&mut self, item_id: &str) {
        self.tracks.remove(item_id);
    }

    pub(super) fn next_idle_flush_at(&self) -> Option<Instant> {
        self.next_idle_flush_at
    }

    pub(super) fn take_idle_flushes(&mut self, now: Instant) -> Vec<(String, Snapshot<T>)> {
        let mut due = Vec::new();
        self.next_idle_flush_at = None;
        for (item_id, track) in &mut self.tracks {
            if let Some(held) = track.held.take_if(|held| held.due_at <= now) {
                track.delivered.clone_from(&held.snapshot.text);
                track.delivered_at = Some(now);
                due.push((track.order, item_id.clone(), held.snapshot));
            } else if let Some(held) = &track.held {
                self.next_idle_flush_at = Some(
                    self.next_idle_flush_at
                        .map_or(held.due_at, |at| at.min(held.due_at)),
                );
            }
        }
        due.sort_unstable_by_key(|(order, _, _)| *order);
        due.into_iter()
            .map(|(_, item_id, snapshot)| (item_id, snapshot))
            .collect()
    }
}

#[derive(Default)]
struct TextScan {
    text: String,
    line_start: usize,
    open_fence: Option<Fence>,
    boundary: Option<usize>,
    title_awaiting_content: bool,
}

#[derive(Clone, Copy)]
struct Fence {
    marker: u8,
    width: usize,
    indent: usize,
}

impl TextScan {
    fn advance(&mut self, text: &str) -> Option<usize> {
        if !text.starts_with(&self.text) {
            *self = Self::default();
        }
        let mut search_from = self.text.len();
        while let Some(offset) = text[search_from..].find('\n') {
            let newline = search_from + offset;
            let line = text[self.line_start..newline].trim_end_matches([' ', '\t', '\r']);
            self.push_line(line, newline);
            search_from = newline + 1;
            self.line_start = search_from;
        }
        self.text.clear();
        self.text.push_str(text);

        let partial_next_list_item = self.open_fence.is_none()
            && self.line_start > 0
            && !self.title_awaiting_content
            && after_list_marker(&text[self.line_start..])
                .is_some_and(|rest| !rest.trim_start_matches([' ', '\t', '\r']).is_empty());
        if partial_next_list_item {
            Some(self.line_start)
        } else {
            self.boundary
        }
    }

    fn push_line(&mut self, line: &str, newline: usize) {
        if self.open_fence.is_none()
            && self.line_start > 0
            && !self.title_awaiting_content
            && after_list_marker(line).is_some()
        {
            self.boundary = Some(self.line_start);
        }
        if let Some(fence) = markdown_fence(line) {
            if let Some(open) = self.open_fence {
                if fence.marker == open.marker
                    && fence.width >= open.width
                    && fence.indent <= open.indent + 3
                    && line.len() == fence.indent + fence.width
                {
                    self.open_fence = None;
                    self.boundary = Some(newline + 1);
                }
            } else {
                self.open_fence = Some(fence);
                self.title_awaiting_content = false;
            }
        } else if self.open_fence.is_none() {
            if line.is_empty() && self.line_start > 0 {
                if !self.title_awaiting_content {
                    self.boundary = Some(newline + 1);
                }
            } else {
                if self.line_start > 0 && !self.title_awaiting_content && atx_heading(line) {
                    self.boundary = Some(self.line_start);
                }
                self.title_awaiting_content = section_title(line);
            }
        }
    }
}

fn markdown_fence(line: &str) -> Option<Fence> {
    let rest = line.trim_start_matches(' ');
    let marker = *rest.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let width = rest.bytes().take_while(|byte| *byte == marker).count();
    if width < 3
        || (marker == b'`'
            && rest[width..]
                .split('\r')
                .next()
                .is_some_and(|suffix| suffix.contains('`')))
    {
        return None;
    }
    Some(Fence {
        marker,
        width,
        indent: line.len() - rest.len(),
    })
}

fn after_list_marker(line: &str) -> Option<&str> {
    let rest = line.trim_start_matches([' ', '\t']);
    let bytes = rest.as_bytes();
    let marker = match bytes.first()? {
        b'-' | b'*' | b'+' => 1,
        b'0'..=b'9' => {
            let digits = bytes
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits > 9 || !matches!(bytes.get(digits), Some(b'.' | b')')) {
                return None;
            }
            digits + 1
        }
        _ => return None,
    };
    matches!(bytes.get(marker), Some(b' ' | b'\t')).then(|| &rest[marker + 1..])
}

fn atx_heading(line: &str) -> bool {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    (1..=6).contains(&hashes) && matches!(line.as_bytes().get(hashes), None | Some(b' ' | b'\t'))
}

fn section_title(line: &str) -> bool {
    let title = line.trim_start_matches(' ');
    if line.len() - title.len() > 3 {
        return false;
    }
    if atx_heading(title) {
        return true;
    }
    let title = title.strip_suffix(':').unwrap_or(title);
    title
        .strip_prefix("**")
        .and_then(|text| text.strip_suffix("**"))
        .is_some_and(|body| !body.is_empty() && !body.ends_with('*') && !body.contains("**"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_visibility_matches_complete_boundaries() {
        for (text, expected) in [
            ("Intro\n\n```ts\nx()\n\n", "Intro\n\n"),
            ("```ts\nx()\n```\nrest", "```ts\nx()\n```\n"),
            (
                "```a``` is inline.\n\n```\ncode\n\nmore\n",
                "```a``` is inline.\n\n",
            ),
            ("Intro\n\n## Results\n\n", "Intro\n\n"),
            ("**Results**\n\nBody\n\nNext", "**Results**\n\nBody\n\n"),
            ("Intro\n## Results\nBody", "Intro\n"),
            ("Steps:\n\n- one\n- tw", "Steps:\n\n- one\n"),
            (
                "Étapes:\r\n\r\n1. 漢字\r\n2) 🤔",
                "Étapes:\r\n\r\n1. 漢字\r\n",
            ),
            ("Steps:\n\n- one\n- ", "Steps:\n\n"),
            ("    ~~~lang\ncode\n        ~~~\nmore\n", ""),
            (
                "    ~~~lang\ncode\n       ~~~~\nmore",
                "    ~~~lang\ncode\n       ~~~~\n",
            ),
        ] {
            let boundary = TextScan::default().advance(text).unwrap_or(0);
            assert_eq!(&text[..boundary], expected, "input was {text:?}");
        }
    }

    fn snapshot(text: &str, at: u64) -> Snapshot<u64> {
        Snapshot {
            text: text.into(),
            data: at,
        }
    }

    #[test]
    fn throttles_paragraphs_and_idle_delivers_latest_full_snapshot() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut stream = AssistantStreaming::new(StreamingMode::Paragraph);
        assert_eq!(
            stream.filter("m", snapshot("A\n\nB", 0), at(0)),
            Some(snapshot("A\n\n", 0))
        );
        assert_eq!(stream.next_idle_flush_at(), Some(at(800)));
        assert_eq!(
            stream.filter("m", snapshot("A\n\nB\n\nC", 100), at(100)),
            None
        );
        assert_eq!(
            stream.filter("m", snapshot("A\n\nB\n\nC\n\nD", 400), at(400)),
            Some(snapshot("A\n\nB\n\nC\n\n", 400))
        );
        assert!(stream.take_idle_flushes(at(800)).is_empty());
        assert_eq!(stream.next_idle_flush_at(), Some(at(1200)));
        assert!(stream.take_idle_flushes(at(1199)).is_empty());
        assert_eq!(
            stream.take_idle_flushes(at(1200)),
            vec![("m".into(), snapshot("A\n\nB\n\nC\n\nD", 400))]
        );
        assert_eq!(
            stream.filter("m", snapshot("A\n\nB\n\nC\n\nD more", 1700), at(1700)),
            None
        );
        stream.complete("m");
        assert!(stream.take_idle_flushes(at(2500)).is_empty());
        assert_eq!(stream.next_idle_flush_at(), None);
    }

    #[test]
    fn handles_rewrites_and_multiple_simultaneous_idle_messages() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut stream = AssistantStreaming::new(StreamingMode::Paragraph);
        assert_eq!(
            stream.filter("long", snapshot("Intro\n\n```ts\nx()\n", 0), at(0)),
            Some(snapshot("Intro\n\n", 0))
        );
        assert_eq!(
            stream.filter("long", snapshot("Retry\n\n- a\n- b", 400), at(400)),
            Some(snapshot("Retry\n\n- a\n", 400))
        );
        assert_eq!(
            stream.filter("short", snapshot("one line", 400), at(400)),
            None
        );
        assert_eq!(
            stream.take_idle_flushes(at(1200)),
            vec![
                ("long".into(), snapshot("Retry\n\n- a\n- b", 400)),
                ("short".into(), snapshot("one line", 400)),
            ]
        );
        assert_eq!(stream.filter("short", snapshot("", 1700), at(1700)), None);
        assert_eq!(
            stream.take_idle_flushes(at(2500)),
            vec![("short".into(), snapshot("", 1700))]
        );
    }

    #[test]
    fn turn_mode_never_sets_an_idle_deadline() {
        let now = Instant::now();
        let mut stream = AssistantStreaming::new(StreamingMode::Turn);
        assert_eq!(stream.filter("id", snapshot("A\n\nB", 0), now), None);
        assert_eq!(stream.next_idle_flush_at(), None);
        assert!(stream.take_idle_flushes(now + IDLE_FLUSH).is_empty());
    }
}
