use std::{collections::HashSet, fmt::Write, ops::Range, sync::LazyLock};

use regex_lite::Regex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const CONTEXT_VERSION: u8 = 1;
const LABEL_UNITS: usize = 200;
const EXCERPT_UNITS: usize = 64_000;
const TRUNCATED: &str = "\n… truncated …";
static REFERENCES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"!?\[([^\[\]\n]{0,512})\]\(t3-context://v1/terminal/([a-zA-Z0-9_-]{1,128})\)")
        .expect("terminal context reference pattern")
});

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TerminalExcerpt {
    version: u8,
    kind: TerminalKind,
    pub(crate) context_id: String,
    pub(crate) label: String,
    terminal_id: String,
    terminal_label: String,
    line_start: usize,
    line_end: usize,
    pub(crate) text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
enum TerminalKind {
    #[serde(rename = "terminal")]
    Terminal,
}

impl TerminalExcerpt {
    pub(crate) fn new(
        terminal_id: String,
        terminal_label: String,
        line_start: usize,
        line_end: usize,
        text: &str,
    ) -> Option<Self> {
        let text = text.replace("\r\n", "\n");
        let text = text.trim_matches('\n');
        if text.is_empty() || line_end < line_start {
            return None;
        }
        let range = if line_start == line_end {
            format!("line {line_start}")
        } else {
            format!("lines {line_start}-{line_end}")
        };
        let label = clean_label(&format!("{terminal_label} {range}"));
        let text = if text.encode_utf16().count() <= EXCERPT_UNITS {
            text.to_owned()
        } else {
            let end = EXCERPT_UNITS - TRUNCATED.encode_utf16().count();
            format!("{}{TRUNCATED}", utf16_prefix(text, end))
        };
        Some(Self {
            version: CONTEXT_VERSION,
            kind: TerminalKind::Terminal,
            context_id: format!("terminal_{}", Uuid::new_v4()),
            label,
            terminal_id,
            terminal_label,
            line_start,
            line_end,
            text,
        })
    }

    pub(crate) fn same_selection(&self, other: &Self) -> bool {
        self.terminal_id == other.terminal_id
            && self.line_start == other.line_start
            && self.line_end == other.line_end
            && self.text == other.text
    }

    pub(crate) fn reference(&self) -> String {
        format!(
            "[{}](t3-context://v1/terminal/{})",
            self.label, self.context_id
        )
    }

    fn provider_body(&self) -> String {
        let mut body = format!("terminal: {}", self.terminal_label);
        let count = self
            .line_end
            .saturating_sub(self.line_start)
            .saturating_add(1);
        for (index, line) in self.text.split('\n').take(count).enumerate() {
            let number = self.line_start.saturating_add(index);
            let _ = write!(body, "\n{number} | {line}");
        }
        body
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct MessageContext {
    version: u8,
    records: Vec<serde_json::Value>,
}

impl MessageContext {
    pub(crate) fn from_excerpts<'a>(
        records: impl Iterator<Item = &'a TerminalExcerpt>,
    ) -> Option<Self> {
        let mut seen = HashSet::new();
        let records: Vec<_> = records
            .filter(|record| seen.insert(&record.context_id))
            .filter_map(|record| serde_json::to_value(record).ok())
            .take(200)
            .collect();
        (!records.is_empty()).then_some(Self {
            version: CONTEXT_VERSION,
            records,
        })
    }

    pub(crate) fn display_text(&self, text: &str) -> (String, Vec<Range<usize>>) {
        if self.version != CONTEXT_VERSION || !text.contains("](t3-context:") {
            return (text.to_owned(), Vec::new());
        }
        let mut display = String::new();
        let mut pills = Vec::new();
        let mut cursor = 0;
        for reference in REFERENCES.captures_iter(text) {
            let source = reference.get(0).expect("full context reference");
            display.push_str(&text[cursor..source.start()]);
            let start = display.len();
            display.push_str(&clean_label(&reference[1]));
            pills.push(start..display.len());
            cursor = source.end();
        }
        display.push_str(&text[cursor..]);
        (display, pills)
    }

    pub(crate) fn provider_text(&self, text: &str) -> String {
        if self.version != CONTEXT_VERSION || !text.contains("](t3-context:") {
            return text.to_owned();
        }
        let records = self
            .records
            .iter()
            .filter_map(|value| {
                let record: TerminalExcerpt = serde_json::from_value(value.clone()).ok()?;
                (record.version == CONTEXT_VERSION && record.line_end >= record.line_start)
                    .then_some(record)
            })
            .collect::<Vec<_>>();
        let mut body = String::new();
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor = 0;
        for reference in REFERENCES.captures_iter(text) {
            let source = reference.get(0).expect("full match");
            let label = clean_label(&reference[1]).replace(';', " ");
            let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
            let id = reference.get(2).expect("context ID").as_str();
            body.push_str(&text[cursor..source.start()]);
            let _ = write!(body, "[Terminal: {}; ref={id}]", escape_payload(&label));
            cursor = source.end();
            if !seen.insert(id) {
                continue;
            }
            let mut matching = records.iter().filter(|record| record.context_id == id);
            let first = matching.next();
            let record = match matching.next() {
                Some(_) => None,
                None => first,
            };
            entries.push(match record {
                Some(record) => format!(
                    "<context kind=\"terminal\" id=\"{id}\">\n{}\n</context>",
                    escape_payload(&record.provider_body())
                ),
                None => format!("<context kind=\"terminal\" id=\"{id}\" unavailable=\"true\"/>"),
            });
        }
        if entries.is_empty() {
            return text.to_owned();
        }
        body.push_str(&text[cursor..]);
        let _ = write!(
            body,
            "\n\n<t3_context version=\"1\">\n{}\n</t3_context>",
            entries.join("\n")
        );
        body
    }
}

fn clean_label(label: &str) -> String {
    let cleaned = label.replace(['[', ']', '\\', '\r', '\n'], " ");
    let normalized = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        "terminal".into()
    } else {
        utf16_prefix(&normalized, LABEL_UNITS).to_owned()
    }
}

fn utf16_prefix(value: &str, limit: usize) -> &str {
    let mut units = 0;
    for (index, ch) in value.char_indices() {
        units += ch.len_utf16();
        if units > limit {
            return &value[..index];
        }
    }
    value
}

fn escape_payload(value: &str) -> String {
    static ENVELOPE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)</?(?:t3_context|context)\b").expect("context markup pattern")
    });
    ENVELOPE
        .replace_all(value, |tag: &regex_lite::Captures<'_>| {
            format!("&lt;{}", &tag[0][1..])
        })
        .into_owned()
}

pub(crate) fn provider_text(text: String, context: Option<&str>) -> String {
    let Some(context) =
        context.and_then(|value| serde_json::from_str::<MessageContext>(value).ok())
    else {
        return text;
    };
    context.provider_text(&text)
}
