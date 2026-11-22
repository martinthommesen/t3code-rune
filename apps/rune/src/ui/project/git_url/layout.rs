use std::sync::LazyLock;

use regex_lite::Regex;

use super::super::path_input::whitespace;

static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:ssh|https?)://(?:[^/\s]*@)?(\[[^\]@/\s]+\]|[^@:/\s]+)(?::[0-9]+)?/(.+)$")
        .expect("static clone URL pattern")
});
static SCP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[^@/\s]+@)?(\[[^\]@/\s]+\]|[^:/\s\[\]]+):(.+)$")
        .expect("static clone SCP pattern")
});

pub(super) fn host_path(remote: &str) -> Option<String> {
    let remote = remote.trim_matches(whitespace);
    if matches!(remote.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic()
        && (remote.len() == 2 || matches!(remote.as_bytes()[2], b'/' | b'\\')))
    {
        return None;
    }
    let url = URL.captures(remote);
    let scp = url.is_none();
    let parts = url.or_else(|| SCP.captures(remote))?;
    let host = parts.get(1)?.as_str();
    let path = parts.get(2)?;
    if remote[..path.start()].chars().any(whitespace) {
        return None;
    }
    let path = path.as_str();
    if scp && path.starts_with('/') || path.contains(['\r', '\u{2028}', '\u{2029}']) {
        return None;
    }
    let path = path.split(['?', '#']).next()?;
    let decoded;
    let path = if scp {
        path
    } else {
        decoded = decode_path(path)?;
        &decoded
    };
    let mut segments: Vec<_> = std::iter::once(host)
        .chain(path.split('/').filter(|part| !part.is_empty()))
        .collect();
    let last = segments.last_mut()?;
    *last = last.strip_suffix(".git").unwrap_or(last);
    (segments.len() >= 2 && segments.iter().all(|part| safe_segment(part)))
        .then(|| segments.join("/"))
}

fn decode_path(path: &str) -> Option<String> {
    let mut remaining = path.bytes();
    let mut decoded = Vec::with_capacity(path.len());
    while let Some(byte) = remaining.next() {
        if byte == b'%' {
            let high = char::from(remaining.next()?).to_digit(16)?;
            let low = char::from(remaining.next()?).to_digit(16)?;
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    String::from_utf8(decoded).ok()
}

fn safe_segment(segment: &str) -> bool {
    if segment.is_empty()
        || segment.ends_with([' ', '.'])
        || segment.contains(['\\', ':', '*', '?', '"', '<', '>', '|'])
        || segment.chars().any(char::is_control)
    {
        return false;
    }
    let device = segment
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_lowercase();
    if matches!(
        device.as_str(),
        "con" | "prn" | "aux" | "nul" | "conin$" | "conout$"
    ) {
        return false;
    }
    !device
        .strip_prefix("com")
        .or_else(|| device.strip_prefix("lpt"))
        .is_some_and(|number| {
            matches!(
                number,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

pub(super) fn join(directory: &str, file: &str) -> String {
    let file = file.trim_matches(whitespace);
    if directory.ends_with('\\') {
        format!("{directory}{}", file.replace('/', "\\"))
    } else {
        format!("{directory}{file}")
    }
}
