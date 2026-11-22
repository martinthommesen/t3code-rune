use std::borrow::Cow;

pub(crate) fn comparison_path(path: &str) -> Cow<'_, str> {
    let path = path.trim_matches(|character: char| {
        character == '\u{feff}' || (character.is_whitespace() && character != '\u{85}')
    });
    let drive_root =
        matches!(path.as_bytes(), [letter, b':', b'/' | b'\\'] if letter.is_ascii_alphabetic());
    let trimmed = if path.starts_with('/') {
        path.trim_end_matches('/')
    } else {
        path.trim_end_matches(['/', '\\'])
    };
    let normalized = if trimmed.is_empty() || drive_root {
        Cow::Borrowed(path)
    } else if matches!(trimmed.as_bytes(), [letter, b':'] if letter.is_ascii_alphabetic()) {
        Cow::Owned(format!("{trimmed}\\"))
    } else {
        Cow::Borrowed(trimmed)
    };
    let drive = normalized.as_bytes().get(..2).is_some_and(|prefix| {
        prefix[0].is_ascii_alphabetic()
            && prefix[1] == b':'
            && normalized
                .as_bytes()
                .get(2)
                .is_none_or(|byte| matches!(byte, b'/' | b'\\'))
    });
    if drive || normalized.starts_with("\\\\") {
        Cow::Owned(normalized.replace('/', "\\").to_lowercase())
    } else {
        normalized
    }
}
