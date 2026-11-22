use std::sync::OnceLock;

use regex_lite::Regex;

pub(in crate::ui) fn normalize(previous: &str, next: &str) -> String {
    let is_typing = if let Some(added) = next.strip_prefix(previous) {
        added.encode_utf16().count() == 1
    } else {
        previous.starts_with(next)
    };
    if is_typing {
        return next.to_owned();
    }

    static QUOTED_PATH: OnceLock<Regex> = OnceLock::new();
    let quoted_path = QUOTED_PATH.get_or_init(|| {
        Regex::new(r#"(?:'((?:[^']|'\\'')+)'|"([^"]+)")$"#).expect("valid quoted path")
    });
    if let Some(captures) = quoted_path.captures(next.trim_end_matches(whitespace))
        && let Some(whole) = captures.get(0)
        && previous.starts_with(next[..whole.start()].trim_end_matches(whitespace))
    {
        let path = captures.get(1).map_or_else(
            || captures[2].to_owned(),
            |single| single.as_str().replace("'\\''", "'"),
        );
        if is_folder_path(&path) {
            return path;
        }
    }

    let path = if previous.ends_with(['/', '\\']) {
        next.strip_prefix(previous).unwrap_or(next)
    } else {
        next
    };
    if !is_folder_path(path) {
        return next.to_owned();
    }
    if is_windows_absolute(path) {
        return path.to_owned();
    }
    let mut chars = path.chars().peekable();
    let mut result = String::with_capacity(path.len());
    while let Some(character) = chars.next() {
        if character == '\\'
            && chars
                .peek()
                .is_some_and(|&next| whitespace(next) || "'\"\\()[]{}<>|&;$`!*?#~^=".contains(next))
        {
            result.extend(chars.next());
        } else {
            result.push(character);
        }
    }
    result
}

fn is_folder_path(path: &str) -> bool {
    path.starts_with('/') || path.starts_with("~/") || is_windows_absolute(path)
}

pub(in crate::ui) fn is_windows_absolute(path: &str) -> bool {
    path.starts_with("\\\\")
        || match path.as_bytes() {
            [drive, b':'] | [drive, b':', b'/' | b'\\', ..] => drive.is_ascii_alphabetic(),
            _ => false,
        }
}

pub(in crate::ui) fn whitespace(character: char) -> bool {
    matches!(character, '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn normalizes_copied_folders_without_changing_single_character_edits() {
        for (previous, next, expected) in [
            ("", "'/a b' ", "/a b"),
            ("~/", r"~/'/a'\''s b'", "/a's b"),
            ("~/", r"~//a\ b", "/a b"),
            ("/a", "/a/b", "/a/b"),
            ("", r"C:\worktrees\foo", r"C:\worktrees\foo"),
            ("", r#""D:\worktree folders""#, r"D:\worktree folders"),
            ("", r"\\host\worktree folders", r"\\host\worktree folders"),
            ("", r"search '/a b'", r"search '/a b'"),
            ("/a\\", "/a\\ ", "/a\\ "),
        ] {
            assert_eq!(normalize(previous, next), expected);
        }
    }
}
