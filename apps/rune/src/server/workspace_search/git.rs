use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Entry, Input, Kind, normalize_path};
use crate::server::git_checkpoint::read_command;

pub(super) async fn ignored_paths(root: &Path) -> HashSet<String> {
    let command = read_command(
        root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ],
    );
    let Ok(Ok(result)) = tokio::time::timeout(Duration::from_secs(10), command).await else {
        return HashSet::new();
    };
    if !result.status.success() {
        return HashSet::new();
    }
    result
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| normalize_path(path.to_owned()))
        .collect()
}

pub(super) fn is_ignored(ignored: &HashSet<String>, path: &str) -> bool {
    let mut candidate = path;
    while !candidate.is_empty() {
        if ignored.contains(candidate) {
            return true;
        }
        candidate = candidate.rsplit_once('/').map_or("", |(parent, _)| parent);
    }
    false
}

pub(super) async fn nested_worktree_files(
    root: &Path,
    input: &Input,
    indexed: &[Entry],
) -> Vec<Entry> {
    let query = input.query.replace('\\', "/");
    if input.images
        || input.kind == Some(Kind::Directory)
        || !query.contains('/')
        || query.ends_with('/')
        || Path::new(&query).is_absolute()
        || query.starts_with("//")
        || matches!(query.as_bytes(), [drive, b':', b'/', ..] if drive.is_ascii_alphabetic())
        || query.split('/').any(|segment| segment == "..")
        || indexed.iter().any(|entry| {
            entry.kind == Kind::File
                && (entry.path == query
                    || entry
                        .path
                        .strip_suffix(&query)
                        .is_some_and(|prefix| prefix.ends_with('/')))
        })
    {
        return Vec::new();
    }
    let Some(listed) = worktrees(root).await else {
        return Vec::new();
    };
    let workspace = root.to_owned();
    tokio::task::spawn_blocking(move || {
        let root = workspace.canonicalize().unwrap_or(workspace);
        let mut matches = Vec::new();
        for record in listed.split("\0\0") {
            let mut fields = record.split('\0');
            let Some(path) = fields
                .next()
                .and_then(|field| field.strip_prefix("worktree "))
            else {
                continue;
            };
            if fields.any(|field| field.starts_with("prunable")) {
                continue;
            }
            let path = PathBuf::from(path);
            let Some(relative) = path
                .strip_prefix(&root)
                .ok()
                .filter(|path| !path.as_os_str().is_empty())
            else {
                continue;
            };
            let file = relative.join(&query);
            if std::fs::metadata(root.join(&file)).is_ok_and(|metadata| metadata.is_file())
                && let Some(entry) = Entry::new(file.to_string_lossy().into_owned(), Kind::File)
            {
                matches.push(entry);
            }
        }
        matches
    })
    .await
    .unwrap_or_default()
}

async fn worktrees(root: &Path) -> Option<String> {
    let list = |args: &'static [&'static str]| async move {
        let result = tokio::time::timeout(Duration::from_secs(5), read_command(root, args))
            .await
            .ok()?
            .ok()?;
        result.status.success().then_some(result.stdout)
    };
    match list(&["worktree", "list", "--porcelain", "-z"]).await {
        Some(listed) => Some(listed),
        None => list(&["worktree", "list", "--porcelain"])
            .await
            .map(|text| text.replace('\n', "\0")),
    }
}
