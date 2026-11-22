mod authored;

use std::ffi::OsString;
use std::path::Path;

use serde::Serialize;

use super::super::locale::path_collator;
use super::{CheckpointError, Git, exclude_spec, revision, validate_ref};

const DIFF_LIMIT: u64 = 10_000_000;

pub(in super::super) struct DiffInput<'a> {
    pub(in super::super) cwd: &'a Path,
    pub(in super::super) from_checkpoint_ref: &'a str,
    pub(in super::super) to_checkpoint_ref: &'a str,
    pub(in super::super) exclude_paths: &'a [&'a str],
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(in super::super) struct CheckpointFile {
    pub(in super::super) path: String,
    kind: CheckpointFileKind,
    pub(in super::super) additions: u64,
    pub(in super::super) deletions: u64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum CheckpointFileKind {
    Modified,
}

pub(in super::super) async fn summarize(
    input: DiffInput<'_>,
) -> Result<Vec<CheckpointFile>, CheckpointError> {
    let git = Git::new(input.cwd).with_stdout_limit(DIFF_LIMIT);
    validate_ref(git, input.from_checkpoint_ref).await?;
    validate_ref(git, input.to_checkpoint_ref).await?;
    let previous = format!("{}^{{commit}}", input.from_checkpoint_ref);
    let Some(previous) = revision(git, &previous).await? else {
        return Ok(Vec::new());
    };
    let current = format!("{}^{{commit}}", input.to_checkpoint_ref);
    let current = revision(git, &current)
        .await?
        .ok_or(CheckpointError::InvalidOutput(
            "current checkpoint ref is missing",
        ))?;
    let start = resolve_turn_start(git, previous, &current).await?;
    let (numstat, authored_paths) = tokio::join!(
        numstat(git, &start, &current, input.exclude_paths),
        authored::paths(git, &start, &current)
    );
    let authored_paths = authored_paths.ok().flatten();
    let output = numstat?;
    let mut records = output.split(|byte| *byte == 0);
    let mut files = Vec::new();

    while let Some(record) = records.next() {
        let mut fields = record.splitn(3, |byte| *byte == b'\t');
        let (Some(additions), Some(deletions), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Some(additions), Some(deletions)) = (line_count(additions), line_count(deletions))
        else {
            continue;
        };
        let (previous_path, path) = if path.is_empty() {
            (records.next(), records.next().unwrap_or_default())
        } else {
            (None, path)
        };
        if path.is_empty() {
            continue;
        }
        let path = String::from_utf8_lossy(path);
        if authored_paths.as_ref().is_some_and(|paths| {
            !paths.contains(path.as_ref())
                && !previous_path
                    .is_some_and(|old| paths.contains(String::from_utf8_lossy(old).as_ref()))
        }) {
            continue;
        }
        files.push(CheckpointFile {
            path: path.into_owned(),
            kind: CheckpointFileKind::Modified,
            additions,
            deletions,
        });
    }
    let collator = path_collator().ok_or(CheckpointError::InvalidOutput(
        "could not initialize path collation",
    ))?;
    files.sort_by(|left, right| collator.compare(&left.path, &right.path));
    Ok(files)
}

async fn resolve_turn_start(
    git: Git<'_>,
    previous: String,
    current: &str,
) -> Result<String, CheckpointError> {
    let second_parent = format!("{current}^2");
    let output = git
        .run(
            ["log", "--no-walk", "--format=%H %P", &second_parent, "--"],
            None,
        )
        .await?;
    if !output.status.success() {
        return Ok(previous);
    }
    let output = String::from_utf8_lossy(&output.stdout);
    let mut values = output.split_whitespace();
    if let (Some(start), Some(_), Some(started_from)) =
        (values.next(), values.next(), values.next())
        && started_from == previous
    {
        return Ok(start.to_owned());
    }
    Ok(previous)
}

async fn numstat(
    git: Git<'_>,
    start: &str,
    current: &str,
    exclusions: &[&str],
) -> Result<Vec<u8>, CheckpointError> {
    let mut args = [
        "diff",
        "--numstat",
        "-z",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--submodule=short",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        start,
        current,
    ]
    .map(OsString::from)
    .to_vec();
    if !exclusions.is_empty() {
        args.extend([OsString::from("--"), OsString::from(":(top)")]);
        args.extend(exclusions.iter().map(|path| exclude_spec(path.as_ref())));
    }
    git.command(args).await
}

fn line_count(raw: &[u8]) -> Option<u64> {
    if raw == b"-" {
        return Some(0);
    }
    if !raw.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(raw).ok()?.parse().ok()
}
