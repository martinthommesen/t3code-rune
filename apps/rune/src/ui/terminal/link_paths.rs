use std::ops::Range;
use std::sync::LazyLock;

use regex_lite::Regex;

use crate::connection::TerminalFileLink;

static PATH_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:~/|\.\.?/|/)[^\s"'`<>]+|[\w.$+@-]+(?:/[\w.$+@-]+)+(?::[0-9]+){0,2}|[\w.$+@-]+\.[A-Za-z][\w-]*:[0-9]+:[0-9]+"#)
        .expect("valid terminal path pattern")
});
static SCHEME_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"[A-Za-z][A-Za-z0-9+.-]+://[^\s"'`<>]+"#).expect("valid terminal scheme pattern")
});
static POSITION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r":([0-9]+)(?::([0-9]+)|-([0-9]+))?$").expect("valid terminal file position pattern")
});

pub(super) fn matching(text: &str, hit: usize) -> Option<(&str, Range<usize>)> {
    let candidate = PATH_PATTERN
        .find_iter(text)
        .find(|found| found.start() <= hit && hit < found.end())?;
    let path = super::links::trim_closing_delimiters(candidate.as_str(), true);
    let range = candidate.start()..candidate.start() + path.len();
    if !range.contains(&hit)
        || path.is_empty()
        || path.starts_with("//")
        || SCHEME_PATTERN
            .find_iter(text)
            .any(|found| found.start() < range.end && range.start < found.end())
    {
        return None;
    }
    Some((path, range))
}

pub(super) fn request(
    attachment: u64,
    raw_path: &str,
    cwd: &str,
    worktree: Option<&str>,
) -> TerminalFileLink {
    let position = POSITION.captures(raw_path);
    let path = position.as_ref().map_or(raw_path, |position| {
        &raw_path[..position.get(0).expect("position match").start()]
    });
    let path = if path.starts_with('/') || path.starts_with("~/") {
        path.to_owned()
    } else {
        let mut path = path;
        while let Some(relative) = path.strip_prefix("./") {
            path = relative;
        }
        format!("{}/{path}", cwd.trim_end_matches('/'))
    };
    let target = position.map_or_else(
        || path.clone(),
        |position| {
            let line = &position[1];
            match position.get(2) {
                Some(column) => format!("{path}:{line}:{}", column.as_str()),
                None => format!("{path}:{line}"),
            }
        },
    );
    TerminalFileLink {
        attachment,
        path,
        target,
        project_root: worktree.unwrap_or(cwd).to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_unquoted_terminal_paths_and_keeps_metadata_and_editor_positions_separate() {
        let output = "See ./docs/demo.txt:3:5. or https://example.invalid/docs/demo.txt:4:1";
        let (path, range) = matching(output, 13).unwrap();
        assert_eq!(path, "./docs/demo.txt:3:5");
        assert_eq!(&output[range], path);
        let link = request(3, path, "/checkout", Some("/worktree"));
        assert_eq!(link.path, "/checkout/docs/demo.txt");
        assert_eq!(link.target, "/checkout/docs/demo.txt:3:5");
        assert_eq!(link.project_root, "/worktree");
        assert!(matching(output, 60).is_none());
    }
}
