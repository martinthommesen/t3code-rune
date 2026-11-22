use unicode_normalization::UnicodeNormalization;

use crate::connection::FolderListing;

pub(super) fn is_relative(path: &str) -> bool {
    matches!(path, "." | "..")
        || ["./", "../", ".\\", "..\\"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

pub(super) fn has_separator(path: &str) -> bool {
    path.ends_with('/') || (!path.starts_with('/') && path.ends_with('\\'))
}

fn last_separator(path: &str) -> Option<usize> {
    if path.starts_with('/') {
        path.rfind('/')
    } else {
        path.rfind(['/', '\\'])
    }
}

fn normalize(path: &str) -> &str {
    if matches!(path, "/" | "\\") || (is_drive(path) && path.len() == 3) {
        return path;
    }
    let trimmed = if path.starts_with('/') {
        path.trim_end_matches('/')
    } else {
        path.trim_end_matches(['/', '\\'])
    };
    if trimmed.is_empty() { path } else { trimmed }
}

fn is_drive(path: &str) -> bool {
    matches!(path.as_bytes(), [drive, b':', b'/' | b'\\', ..] if drive.is_ascii_alphabetic())
}

struct Absolute<'a> {
    root: String,
    separator: &'static str,
    segments: Vec<&'a str>,
}

impl<'a> Absolute<'a> {
    fn parse(path: &'a str) -> Option<Self> {
        let (root, separator, segments) = if is_drive(path) {
            (
                format!("{}\\", &path[..2]),
                "\\",
                path[3..]
                    .split(['/', '\\'])
                    .filter(|p| !p.is_empty())
                    .collect(),
            )
        } else if let Some(path) = path.strip_prefix("\\\\") {
            let mut segments = path.split(['/', '\\']).filter(|p| !p.is_empty());
            let (server, share) = (segments.next()?, segments.next()?);
            (format!("\\\\{server}\\{share}\\"), "\\", segments.collect())
        } else {
            let rest = path.strip_prefix('/')?;
            (
                "/".into(),
                "/",
                rest.split('/').filter(|p| !p.is_empty()).collect(),
            )
        };
        Some(Self {
            root,
            separator,
            segments,
        })
    }

    fn joined(&self) -> String {
        format!("{}{}", self.root, self.segments.join(self.separator))
    }
}

pub(super) fn browse_directory(path: &str) -> &str {
    if has_separator(path) {
        path
    } else {
        last_separator(path).map_or(path, |index| &path[..=index])
    }
}

pub(super) fn parent(path: &str) -> Option<String> {
    let directory = normalize(browse_directory(path));
    if let Some(mut absolute) = Absolute::parse(directory) {
        absolute.segments.pop()?;
        return Some(if absolute.segments.is_empty() {
            absolute.root
        } else {
            format!("{}{}", absolute.joined(), absolute.separator)
        });
    }
    Some(directory[..=last_separator(directory)?].into())
}

pub(super) fn leaf(path: &str) -> &str {
    last_separator(path).map_or(path, |index| &path[index + 1..])
}

pub(super) fn existing<'a>(path: &str, listing: &'a FolderListing) -> Option<&'a str> {
    if path == "~" || has_separator(path) {
        return Some(&listing.parent_path);
    }
    let leaf = leaf(path);
    let exact = listing.entries.iter().find(|entry| entry.name == leaf);
    if let Some(exact) = exact {
        return Some(&exact.full_path);
    }
    let leaf = leaf.nfc().collect::<String>();
    let mut matches = listing
        .entries
        .iter()
        .filter(|entry| entry.name.nfc().eq(leaf.chars()));
    let first = matches.next()?;
    matches.next().is_none().then_some(&first.full_path)
}

pub(super) fn resolve(path: &str, cwd: Option<&str>) -> Option<String> {
    if !is_relative(path) {
        return Some(normalize(path).to_owned());
    }
    let mut absolute = Absolute::parse(cwd?)?;
    for segment in path.split(['/', '\\']) {
        match segment {
            "" | "." => {}
            ".." => {
                absolute.segments.pop();
            }
            name => absolute.segments.push(name),
        }
    }
    Some(absolute.joined())
}

pub(super) fn title(path: &str) -> &str {
    if let Some(absolute) = Absolute::parse(path) {
        return absolute.segments.last().copied().unwrap_or(path);
    }
    path.rsplit(['/', '\\'])
        .find(|name| !name.is_empty())
        .unwrap_or(path)
}
