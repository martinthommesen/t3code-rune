use std::fmt::Write;

use rune::ClipboardContent;
use serde::Serialize;

use super::{CONTEXT_VERSION, LABEL_UNITS, utf16_prefix};

const HOSTED_APP: &str = "https://app.t3.codes";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Source<'a> {
    environment_id: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadRecord<'a> {
    version: u8,
    kind: &'static str,
    context_id: String,
    label: &'a str,
    environment_id: &'a str,
    thread_id: &'a str,
    title: &'a str,
}

#[derive(Serialize)]
struct Fragment<'a> {
    version: u8,
    source: Source<'a>,
    records: [ThreadRecord<'a>; 1],
}

pub(crate) fn thread_clipboard(
    environment: &str,
    thread_id: &str,
    title: &str,
) -> ClipboardContent {
    let url = format!(
        "{HOSTED_APP}/threads/{}/{}",
        encode(environment, b"-_"),
        encode(thread_id, b"-_")
    );
    let sanitized = title.replace(['[', ']', '\\', '\r', '\n'], " ");
    let normalized = sanitized
        .split(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let label = if normalized.is_empty() {
        "thread"
    } else {
        utf16_prefix(&normalized, LABEL_UNITS)
    };
    let fragment = Fragment {
        version: CONTEXT_VERSION,
        source: Source {
            environment_id: environment,
        },
        records: [ThreadRecord {
            version: CONTEXT_VERSION,
            kind: "thread",
            context_id: context_id(thread_id),
            label,
            environment_id: environment,
            thread_id,
            title: label,
        }],
    };
    let encoded = encode(
        &serde_json::to_string(&fragment).expect("serializable thread clipboard record"),
        b"-_.!~*'()",
    );
    let html = format!(r#"<pre data-t3-context-fragment="{encoded}">{url}</pre>"#);
    ClipboardContent::new(url).with_html(html)
}

fn context_id(thread: &str) -> String {
    let producer = format!("thread_{thread}");
    let safe = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_');
    if producer.len() <= 128 && producer.chars().all(safe) {
        return producer;
    }
    let slug = producer
        .split(|c| !safe(c))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug = slug.trim_matches('-');
    let units: Vec<_> = producer.encode_utf16().collect();
    let forward = digest(0x811c9dc5, units.iter().copied());
    let reverse = digest(0x9dc5811c, units.iter().rev().copied());
    format!("{}-{forward:08x}{reverse:08x}", &slug[..slug.len().min(48)])
}

fn digest(seed: u32, units: impl Iterator<Item = u16>) -> u32 {
    units.fold(seed, |hash, word| {
        (hash ^ u32::from(word)).wrapping_mul(0x01000193)
    })
}

fn encode(value: &str, allowed: &[u8]) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || allowed.contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("write to string");
        }
    }
    encoded
}
