use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::Path;

use ring::digest::{SHA256, digest};
use serde::Serialize;

use super::super::git_checkpoint::StatusIndex;
use super::super::vcs_refs::worktree_branches;
use super::git::{Checkout, git, text};
use super::local::{Read, numstat, parse_numstat};
use super::remote::comparison_base;

const MAX_UNTRACKED_FILES: usize = 5_000;
const MAX_UNTRACKED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SHALLOW_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BranchChanges {
    base_ref: Option<String>,
    insertions: u64,
    deletions: u64,
}

pub(super) async fn read(checkout: &Checkout, status: &Read) -> Option<BranchChanges> {
    let root = &checkout.root;
    let mut base_ref = None;
    let mut reference = "HEAD".to_owned();
    if let Some(branch) = status.local.ref_name.as_deref()
        && let Some(base) = comparison_base(root, &status.remotes, branch, true).await
    {
        let merge_base = text(
            root,
            &["merge-base", "--end-of-options", &base.reference, "HEAD"],
        )
        .await;
        match merge_base {
            Some(commit) => {
                reference = commit;
                base_ref = Some(base.name);
            }
            None if text(root, &["rev-parse", "--verify", "--quiet", "HEAD"])
                .await
                .is_some() =>
            {
                return None;
            }
            None => (),
        }
    }
    let files = if status.untracked.is_empty() {
        numstat(root, &reference).await.ok()?
    } else {
        let untracked = untracked_paths(root, &status.untracked).await?;
        if untracked.is_empty() {
            numstat(root, &reference).await.ok()?
        } else {
            let index = prepare_index(checkout, &untracked).await?;
            let mut result = index
                .run(
                    root,
                    &["diff-index", "-M", "--numstat", "-z", &reference, "--"],
                    None,
                )
                .await
                .ok()?;
            if !result.status.success() && reference == "HEAD" {
                let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
                let empty = text(root, &["hash-object", "-t", "tree", null]).await?;
                result = index
                    .run(
                        root,
                        &["diff-index", "-M", "--numstat", "-z", &empty, "--"],
                        None,
                    )
                    .await
                    .ok()?;
            }
            if !result.status.success() {
                return None;
            }
            parse_numstat(&result.stdout)
        }
    };
    Some(BranchChanges {
        base_ref,
        insertions: files.iter().map(|entry| entry.insertions).sum(),
        deletions: files.iter().map(|entry| entry.deletions).sum(),
    })
}

async fn untracked_paths(root: &Path, roots: &[String]) -> Option<Vec<String>> {
    let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z", "--"];
    let pathspecs: Vec<_> = if roots.len() <= 128
        && roots
            .iter()
            .map(|root| root.encode_utf16().count() + 16)
            .sum::<usize>()
            < 8_000
    {
        roots
            .iter()
            .map(|root| format!(":(top,literal){root}"))
            .collect()
    } else {
        Vec::new()
    };
    args.extend(pathspecs.iter().map(String::as_str));
    let (listing, deletions) = tokio::try_join!(
        git(root, &args),
        git(
            root,
            &[
                "diff",
                "--cached",
                "--no-renames",
                "--name-only",
                "--diff-filter=D",
                "-z",
                "HEAD",
                "--"
            ]
        ),
    )
    .ok()?;
    if !listing.status.success() {
        return None;
    }
    let paths: Vec<_> = listing
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .collect();
    if paths.len() > MAX_UNTRACKED_FILES {
        return None;
    }
    let staged_deletions: HashSet<_> = deletions.stdout.split('\0').collect();
    let mut files = Vec::new();
    let mut bytes = 0_u64;
    for path in paths {
        if staged_deletions.contains(path) {
            continue;
        }
        let target = root.join(path);
        if path.ends_with('/') {
            let nested = git(&target, &["rev-parse", "--verify", "--quiet", "HEAD"])
                .await
                .ok()?;
            if nested.status.code() == Some(1) {
                continue;
            }
        }
        if let Ok(metadata) = target.symlink_metadata()
            && metadata.is_file()
        {
            bytes = bytes.saturating_add(metadata.len());
            if bytes > MAX_UNTRACKED_BYTES {
                return None;
            }
        }
        files.push(path.to_owned());
    }
    Some(files)
}

async fn prepare_index(checkout: &Checkout, paths: &[String]) -> Option<StatusIndex> {
    let common = checkout.common.clone();
    let source = checkout.index.clone();
    let index = tokio::task::spawn_blocking(move || StatusIndex::copy(&common, &source))
        .await
        .ok()?
        .ok()?;
    let root = &checkout.root;
    if !checkout.index.exists() {
        let result = index
            .run(root, &["read-tree", "--empty"], None)
            .await
            .ok()?;
        if !result.status.success() {
            return None;
        }
    }
    let expanded = index
        .run(root, &["update-index", "--no-split-index"], None)
        .await
        .ok()?;
    if !expanded.status.success() {
        return None;
    }
    let mut input = paths.join("\0").into_bytes();
    input.push(0);
    let intent = index
        .run(
            root,
            &[
                "-c",
                "core.preloadIndex=false",
                "-c",
                "core.sparseCheckout=false",
                "--literal-pathspecs",
                "add",
                "--intent-to-add",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            Some(&input),
        )
        .await
        .ok()?;
    intent.status.success().then_some(index)
}

pub(super) async fn refs_digest(cwd: &Path, checkout: &Checkout) -> Option<String> {
    let (refs, symbolic) = tokio::try_join!(
        git(cwd, &["show-ref", "--head"]),
        git(cwd, &["symbolic-ref", "--quiet", "HEAD"]),
    )
    .ok()?;
    if !refs.status.success() || (symbolic.status.code() != Some(1) && !symbolic.status.success()) {
        return None;
    }
    let shallow = match std::fs::File::open(checkout.common.join("shallow")) {
        Ok(shallow) => {
            let mut data = String::new();
            if shallow
                .take(MAX_SHALLOW_BYTES + 1)
                .read_to_string(&mut data)
                .ok()? as u64
                > MAX_SHALLOW_BYTES
            {
                return None;
            }
            data
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return None,
    };
    let common = &checkout.common;
    let remote_path = common.to_string_lossy();
    let mut args = vec![
        "--git-dir",
        remote_path.as_ref(),
        "worktree",
        "list",
        "--porcelain",
        "-z",
    ];
    let mut worktrees = git(cwd, &args).await.ok()?;
    if !worktrees.status.success() {
        args.pop();
        worktrees = git(cwd, &args).await.ok()?;
        worktrees.stdout = worktrees.stdout.replace('\n', "\0");
    }
    if !worktrees.status.success() {
        return None;
    }
    let paths = worktree_branches(&worktrees.stdout, common);
    let mut entries = paths
        .into_iter()
        .map(|entry| serde_json::to_string(&entry))
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    entries.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    let input = format!(
        "{}\0{}\0{}\0{}",
        refs.stdout,
        symbolic.stdout,
        shallow,
        entries.join("\n")
    );
    let mut hex = String::with_capacity(32);
    for byte in &digest(&SHA256, input.as_bytes()).as_ref()[..16] {
        write!(hex, "{byte:02x}").ok()?;
    }
    Some(hex)
}
