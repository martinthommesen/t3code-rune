use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex_lite::Regex;
use serde::Serialize;
use serde_json::Value;

use super::internal_paths;

const MAX_BATCH: usize = 64;
const MAX_PATH: usize = 512;

pub(super) struct Input(Vec<String>);

#[derive(Serialize)]
pub(super) struct ResultSet {
    entries: Vec<Option<Entry>>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Entry {
    Directory,
    Other,
    File {
        #[serde(rename = "byteLength")]
        byte_length: u64,
        #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
        mime_type: Option<&'static str>,
    },
}

impl Input {
    pub(super) fn decode(payload: &Value) -> Result<Self, String> {
        let values = payload
            .as_object()
            .ok_or("Expected object")?
            .get("paths")
            .ok_or("Missing key\n  at [\"paths\"]")?
            .as_array()
            .ok_or("Expected array\n  at [\"paths\"]")?;
        if !(1..=MAX_BATCH).contains(&values.len()) {
            return Err(format!(
                "Expected between 1 and {MAX_BATCH} entries\n  at [\"paths\"]"
            ));
        }
        let paths = values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let path = value
                    .as_str()
                    .map(str::trim)
                    .filter(|path| !path.is_empty() && path.encode_utf16().count() <= MAX_PATH)
                    .ok_or_else(|| format!("Expected a nonempty path up to {MAX_PATH} characters\n  at [\"paths\"][{index}]"))?;
                Ok(path.to_owned())
            })
            .collect::<Result<_, String>>()?;
        Ok(Self(paths))
    }
}

pub(super) fn get(input: Input, state_dir: &Path) -> ResultSet {
    let mut checked = HashMap::new();
    ResultSet {
        entries: input
            .0
            .iter()
            .map(|requested| {
                checked
                    .entry(requested.as_str())
                    .or_insert_with(|| metadata(requested, state_dir))
                    .clone()
            })
            .collect(),
    }
}

pub(super) fn expand_home(path: &str) -> PathBuf {
    let relative = path.strip_prefix("~/").or_else(|| {
        if cfg!(windows) {
            path.strip_prefix("~\\")
        } else {
            None
        }
    });
    let Some(relative) = relative.or_else(|| (path == "~").then_some("")) else {
        return PathBuf::from(path);
    };
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env::var_os(home).map_or_else(
        || PathBuf::from(path),
        |home| PathBuf::from(home).join(relative),
    )
}

fn metadata(requested: &str, state_dir: &Path) -> Option<Entry> {
    if requested.starts_with("//") || requested.starts_with("\\\\") {
        return None;
    }
    let target = expand_home(requested);
    if !target.is_absolute() || internal_paths::protects(state_dir, &target) {
        return None;
    }
    let canonical = target.canonicalize().ok()?;
    if internal_paths::protects(state_dir, &canonical) {
        return None;
    }
    let stat = fs::metadata(&canonical).ok()?;
    if stat.is_dir() {
        return Some(Entry::Directory);
    }
    if !stat.is_file() {
        return Some(Entry::Other);
    }
    Some(Entry::File {
        byte_length: stat.len(),
        mime_type: (target.extension().is_none() && stat.len() > 0)
            .then(|| header_mime_type(&canonical))
            .flatten(),
    })
}

fn header_mime_type(path: &Path) -> Option<&'static str> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = options.open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut header = [0; 512];
    let read = file.read(&mut header).ok()?;
    mime_type(&header[..read])
}

fn mime_type(header: &[u8]) -> Option<&'static str> {
    for (signature, mime) in [
        (&b"\x89PNG\r\n\x1a\n"[..], "image/png"),
        (&b"\xff\xd8\xff"[..], "image/jpeg"),
        (&b"PK\x03\x04"[..], "application/zip"),
        (&b"GIF87a"[..], "image/gif"),
        (&b"GIF89a"[..], "image/gif"),
        (&b"%PDF-"[..], "application/pdf"),
        (&b"SQLite format 3\0"[..], "application/vnd.sqlite3"),
    ] {
        if header.starts_with(signature) {
            return Some(mime);
        }
    }
    if header.starts_with(b"RIFF") && header.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    let interpreter = header.strip_prefix(b"#!")?.split(|&b| b == b'\n').next()?;
    static INTERPRETERS: LazyLock<[(Regex, &str); 4]> = LazyLock::new(|| {
        [
            (r"\bpython[\d.]*\b", "text/x-python"),
            (r"\b(?:node|nodejs)\b", "text/javascript"),
            (r"\bruby\b", "text/x-ruby"),
            (r"\b(?:sh|bash|zsh|fish)\b", "text/x-shellscript"),
        ]
        .map(|(pattern, mime)| {
            (
                Regex::new(pattern).expect("valid MIME interpreter pattern"),
                mime,
            )
        })
    });
    let interpreter = String::from_utf8_lossy(interpreter);
    INTERPRETERS
        .iter()
        .find_map(|(pattern, mime)| pattern.is_match(&interpreter).then_some(*mime))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn metadata_keeps_order_and_ignores_missing_relative_and_internal_paths() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let state = root.join("state");
        fs::create_dir_all(state.join("secrets")).unwrap();
        let protected = state.join("secrets/token");
        let script = root.join("tool");
        let content = b"#!/usr/bin/env node\n";
        fs::write(&protected, b"test-only credential").unwrap();
        fs::write(&script, content).unwrap();
        let result = get(
            Input::decode(
                &json!({"paths":[script,root,root.join("missing"),protected,"relative",script]}),
            )
            .unwrap(),
            &state,
        );
        let file = json!({"kind":"file","byteLength":content.len(),"mimeType":"text/javascript"});
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({"entries":[file,{"kind":"directory"},null,null,null,file]})
        );
    }
}
