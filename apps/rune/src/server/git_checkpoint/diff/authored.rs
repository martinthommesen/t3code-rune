use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::{CheckpointError, Git};

const HEAD_MOVES_SCAN_LIMIT: usize = 512;

pub(super) async fn paths(
    git: Git<'_>,
    start: &str,
    current: &str,
) -> Result<Option<HashSet<String>>, CheckpointError> {
    let output = git
        .command([
            "log",
            "--no-walk=unsorted",
            "--format=%ct %P",
            start,
            current,
        ])
        .await?;
    let output = String::from_utf8_lossy(&output);
    let mut lines = output.trim_end().lines().map(str::split_whitespace);
    let Some((captured_at, start_head)) = lines
        .next()
        .and_then(|mut line| Some((line.next()?, line.next()?)))
    else {
        return Ok(None);
    };
    let Some(end_head) = lines.next().and_then(|mut line| line.nth(1)) else {
        return Ok(None);
    };
    if start_head == end_head {
        return Ok(None);
    }

    let remerge = supports_remerge(git).await;
    let cutoff = format!("--max-age={captured_at}");
    let later_range = format!("{start_head}..{end_head}");
    let removed_range = format!("{end_head}..{start_head}");
    let (recent_commits, dirty_start, dirty_finish, removed_changes) = tokio::try_join!(
        async { list_lines(git, &["rev-list", &cutoff, &later_range], None).await },
        async { list_paths(git, &["diff", start_head, start], None).await },
        async { list_paths(git, &["diff", end_head, current], None).await },
        async { list_commit_paths(git, remerge, &[&removed_range], None).await },
    )?;

    let imported = if needs_import_lookup(git, &recent_commits, captured_at).await {
        imported_commits(git, &cutoff, start_head)
            .await
            .unwrap_or_default()
    } else {
        HashSet::new()
    };
    let own_commits: Vec<_> = recent_commits
        .into_iter()
        .filter(|commit| !imported.contains(commit))
        .collect();
    let own_changes = if own_commits.is_empty() {
        Vec::new()
    } else {
        let revisions = revision_input(&own_commits);
        list_commit_paths(
            git,
            remerge,
            &["--no-walk=unsorted", "--stdin"],
            Some(&revisions),
        )
        .await?
    };
    Ok(Some(
        dirty_start
            .into_iter()
            .chain(dirty_finish)
            .chain(removed_changes)
            .chain(own_changes)
            .collect(),
    ))
}

async fn needs_import_lookup(git: Git<'_>, commits: &[String], captured_at: &str) -> bool {
    if commits.is_empty() {
        return false;
    }
    if commits.len() > HEAD_MOVES_SCAN_LIMIT {
        return true;
    }
    let scan_limit = format!("--max-count={HEAD_MOVES_SCAN_LIMIT}");
    let since = format!("--since=@{captured_at}");
    let timeout = Duration::from_secs(1);
    let lines = tokio::time::timeout(
        timeout,
        list_lines(
            git,
            &[
                "log",
                "--walk-reflogs",
                &scan_limit,
                "HEAD",
                &since,
                "--format=%H %gD %gs",
            ],
            None,
        ),
    )
    .await;
    let Ok(Ok(lines)) = lines else {
        return true;
    };
    let created: HashSet<_> = lines
        .iter()
        .filter_map(|line| ReflogEntry::parse(line))
        .filter(ReflogEntry::created_locally)
        .map(|entry| entry.commit)
        .collect();
    commits
        .iter()
        .any(|commit| !created.contains(commit.as_str()))
}

async fn imported_commits(
    git: Git<'_>,
    cutoff: &str,
    start_head: &str,
) -> Result<HashSet<String>, CheckpointError> {
    let lines = list_lines(
        git,
        &[
            "log",
            "--walk-reflogs",
            "--all",
            cutoff,
            "--format=%H %gD %gs",
        ],
        None,
    )
    .await?;
    let entries: Vec<_> = lines
        .iter()
        .filter_map(|line| ReflogEntry::parse(line))
        .collect();
    let mut fetched: Vec<_> = entries
        .iter()
        .filter(|entry| entry.fetched && entry.subject != "update by push")
        .map(|entry| entry.commit)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if fetched.is_empty() {
        return Ok(HashSet::new());
    }
    let excluded = format!("^{start_head}");
    fetched.push(&excluded);
    let revisions = revision_input(&fetched);
    let created: HashSet<_> = entries
        .into_iter()
        .filter(ReflogEntry::created_locally)
        .map(|entry| entry.commit)
        .collect();
    let fetched = list_lines(git, &["rev-list", cutoff, "--stdin"], Some(&revisions)).await?;
    Ok(fetched
        .into_iter()
        .filter(|commit| !created.contains(commit.as_str()))
        .collect())
}

struct ReflogEntry<'a> {
    commit: &'a str,
    fetched: bool,
    subject: &'a str,
}

impl<'a> ReflogEntry<'a> {
    fn parse(line: &'a str) -> Option<Self> {
        let mut parts = line.splitn(3, ' ');
        Some(Self {
            commit: parts.next()?,
            fetched: parts.next()?.starts_with("refs/remotes/"),
            subject: parts.next().unwrap_or_default(),
        })
    }

    fn created_locally(&self) -> bool {
        if self.fetched || self.subject.is_empty() {
            return false;
        }
        let subject = self.subject.to_ascii_lowercase();
        if ["checkout", "reset", "branch", "clone", "fetch"]
            .iter()
            .any(|word| begins_with_word(&subject, word))
        {
            return false;
        }
        let Some((action, detail)) = subject.split_once(": ") else {
            return true;
        };
        if !action.contains(':')
            && ["(start)", "(finish)", "(abort)", "(reset)"]
                .iter()
                .any(|word| action.ends_with(word))
        {
            return false;
        }
        !(detail == "fast-forward"
            && !action.contains(['(', ')'])
            && ["merge", "pull", "rebase", "cherry-pick"]
                .iter()
                .any(|word| begins_with_word(action, word)))
    }
}

fn begins_with_word(text: &str, word: &str) -> bool {
    text.strip_prefix(word).is_some_and(|rest| {
        rest.as_bytes()
            .first()
            .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
    })
}

async fn supports_remerge(git: Git<'_>) -> bool {
    let Ok(version) = git.command(["version"]).await else {
        return false;
    };
    let version = String::from_utf8_lossy(&version);
    let Some(version) = version.strip_prefix("git version ") else {
        return false;
    };
    let mut numbers = version.trim_ascii().split('.');
    numbers
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .zip(numbers.next().and_then(|minor| minor.parse::<u32>().ok()))
        .is_some_and(|version| version >= (2, 36))
}

async fn list_commit_paths(
    git: Git<'_>,
    remerge: bool,
    revisions: &[&str],
    input: Option<&[u8]>,
) -> Result<Vec<String>, CheckpointError> {
    let mut args = vec!["log"];
    args.extend_from_slice(revisions);
    args.extend([
        "--format=",
        if remerge {
            "--diff-merges=remerge"
        } else {
            "--cc"
        },
    ]);
    let mut changed = list_paths(git, &args, input).await?;
    if !remerge {
        changed.extend(merge_shared_paths(git, revisions, input).await?);
    }
    Ok(changed)
}

async fn merge_shared_paths(
    git: Git<'_>,
    revisions: &[&str],
    input: Option<&[u8]>,
) -> Result<Vec<String>, CheckpointError> {
    let mut args = vec!["rev-list", "--merges", "--parents"];
    args.extend_from_slice(revisions);
    let merges = list_lines(git, &args, input).await?;
    let mut changed = Vec::new();
    for merge in merges {
        let parents: Vec<_> = merge.split_whitespace().skip(1).collect();
        let mut args = vec!["merge-base", "--octopus"];
        args.extend_from_slice(&parents);
        let bases = list_lines(git, &args, None).await.unwrap_or_default();
        let Some(base) = bases.first() else {
            continue;
        };
        let mut counts = HashMap::new();
        for parent in parents {
            let paths: HashSet<_> = list_paths(git, &["diff", base, parent], None)
                .await?
                .into_iter()
                .collect();
            for path in paths {
                *counts.entry(path).or_insert(0) += 1;
            }
        }
        changed.extend(
            counts
                .into_iter()
                .filter_map(|(path, count)| (count > 1).then_some(path)),
        );
    }
    Ok(changed)
}

async fn list_lines(
    git: Git<'_>,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<Vec<String>, CheckpointError> {
    split_output(git, args, input, b'\n').await
}

async fn list_paths(
    git: Git<'_>,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<Vec<String>, CheckpointError> {
    let mut args = args.to_vec();
    args.extend(["--name-only", "-z", "--no-renames", "--no-ext-diff"]);
    split_output(git, &args, input, 0).await
}

async fn split_output(
    git: Git<'_>,
    args: &[&str],
    input: Option<&[u8]>,
    separator: u8,
) -> Result<Vec<String>, CheckpointError> {
    let stdout = git.run(args, input).await?.success()?;
    Ok(stdout
        .split(|byte| *byte == separator)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect())
}

fn revision_input(revisions: &[impl AsRef<str>]) -> Vec<u8> {
    let mut input = Vec::new();
    for revision in revisions {
        input.extend_from_slice(revision.as_ref().as_bytes());
        input.push(b'\n');
    }
    input
}
