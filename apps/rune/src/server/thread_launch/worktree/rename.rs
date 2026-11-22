use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use super::git;

pub(super) async fn branch(
    checkout: &Path,
    old: &str,
    requested: &str,
    exact: bool,
) -> io::Result<String> {
    if old == requested {
        return Ok(old.to_owned());
    }
    let _lock = git::lock_config(checkout).await?;
    for _ in 0..4 {
        let target = if exact {
            requested.to_owned()
        } else {
            available(checkout, requested).await?
        };
        let renamed = git::metadata(checkout, &["branch", "-m", "--", old, &target]).await?;
        if renamed.status.success() {
            return Ok(target);
        }
        let current = git::text(checkout, &["symbolic-ref", "--quiet", "HEAD"]).await?;
        if current.trim_end() == format!("refs/heads/{target}") && !exists(checkout, old).await? {
            return Ok(target);
        }
        if exact || !exists(checkout, &target).await? {
            break;
        }
    }
    Err(io::Error::other(
        "Git could not rename the worktree branch.",
    ))
}

async fn exists(checkout: &Path, branch: &str) -> io::Result<bool> {
    let reference = format!("refs/heads/{branch}");
    Ok(
        git::metadata(checkout, &["show-ref", "--verify", "--quiet", &reference])
            .await?
            .status
            .success(),
    )
}

async fn available(checkout: &Path, requested: &str) -> io::Result<String> {
    let output = git::text(
        checkout,
        &["for-each-ref", "--format=%(refname:strip=2)", "refs/heads/"],
    )
    .await?;
    let branches = output
        .lines()
        .filter(|name| !name.is_empty())
        .map(str::to_lowercase)
        .collect::<BTreeSet<_>>();
    let mut blocked = branches.clone();
    for branch in &branches {
        blocked.extend(
            branch
                .match_indices('/')
                .map(|(position, _)| branch[..position].to_owned()),
        );
    }
    let blocked_by_parent = requested
        .match_indices('/')
        .any(|(position, _)| branches.contains(&requested[..position].to_lowercase()));
    let desired = if blocked_by_parent {
        requested.replace('/', "-")
    } else {
        requested.to_owned()
    };
    if !blocked.contains(&desired.to_lowercase()) {
        return Ok(desired);
    }
    for suffix in 1..=100 {
        let candidate = format!("{desired}-{suffix}");
        if !blocked.contains(&candidate.to_lowercase()) {
            return Ok(candidate);
        }
    }
    Err(io::Error::other(
        "Git could not find an available worktree branch name.",
    ))
}
