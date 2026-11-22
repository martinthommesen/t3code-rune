use std::collections::HashMap;
use std::path::Path;

use super::super::locale::path_collator;
use super::git::{Checkout, config, git, text};
use super::remote::{RemoteRefs, hosting_provider};
use super::{GitManagerError, LocalStatus, WorkingFile, WorkingTree, trim};

pub(super) struct Read {
    pub local: LocalStatus,
    pub remotes: RemoteRefs,
    pub untracked: Vec<String>,
}

pub(super) async fn read(cwd: &Path, checkout: &Checkout) -> Result<Option<Read>, GitManagerError> {
    let hidden_untracked = config(cwd, "status.showUntrackedFiles")
        .await
        .is_some_and(|setting| {
            !matches!(
                setting.to_ascii_lowercase().as_str(),
                "normal" | "all" | "true" | "yes" | "on" | "1"
            )
        });
    let mut args = vec![
        "status",
        "--porcelain=2",
        "--branch",
        "-z",
        "--no-ahead-behind",
    ];
    if hidden_untracked {
        args.push("--untracked-files=normal");
    }
    let (status, stats, refs) =
        tokio::join!(git(cwd, &args), numstat(cwd, "HEAD"), RemoteRefs::read(cwd));
    let status = status?;
    if !status.status.success() {
        if status
            .stderr
            .to_ascii_lowercase()
            .contains("not a git repository")
        {
            return Ok(None);
        }
        return Err(GitManagerError::local(cwd, "Git status failed."));
    }
    let changed = parse_porcelain(&status.stdout);
    let stats = stats?;
    let mut insertions = 0;
    let mut deletions = 0;
    let mut files = Vec::<WorkingFile>::new();
    let mut by_path = HashMap::new();
    for stat in stats {
        insertions += stat.insertions;
        deletions += stat.deletions;
        let index = *by_path.entry(stat.path.clone()).or_insert(files.len());
        if let Some(existing) = files.get_mut(index) {
            existing.insertions += stat.insertions;
            existing.deletions += stat.deletions;
        } else {
            files.push(stat);
        }
    }
    for path in changed.paths {
        if let std::collections::hash_map::Entry::Vacant(entry) = by_path.entry(path.clone()) {
            entry.insert(files.len());
            files.push(WorkingFile {
                path,
                insertions: 0,
                deletions: 0,
            });
        }
    }
    if let Some(collator) = path_collator() {
        files.sort_by(|left, right| collator.compare(&left.path, &right.path));
    } else {
        files.sort_by(|left, right| left.path.cmp(&right.path));
    }
    let source_control_provider = if refs.primary.is_some() {
        hosting_provider(cwd, changed.branch.as_deref()).await
    } else {
        None
    };
    Ok(Some(Read {
        local: LocalStatus {
            is_repo: true,
            source_control_provider,
            has_primary_remote: refs.primary.is_some(),
            is_default_ref: refs.is_default(changed.branch.as_deref()),
            ref_name: changed.branch,
            has_working_tree_changes: changed.dirty,
            working_tree: WorkingTree {
                root_path: Some(checkout.root.to_string_lossy().into_owned()),
                files,
                insertions,
                deletions,
            },
            branch_changes: None,
        },
        remotes: refs,
        untracked: changed.untracked,
    }))
}

struct Changed {
    branch: Option<String>,
    paths: Vec<String>,
    untracked: Vec<String>,
    dirty: bool,
}

fn parse_porcelain(stdout: &str) -> Changed {
    let mut changed = Changed {
        branch: None,
        paths: Vec::new(),
        untracked: Vec::new(),
        dirty: false,
    };
    let mut records = stdout.split('\0');
    while let Some(record) = records.next() {
        if let Some(branch) = record.strip_prefix("# branch.head ") {
            let branch = trim(branch);
            if !matches!(branch, "" | "(detached)" | "(unknown)") {
                changed.branch = Some(branch.to_owned());
            }
        } else if !record.is_empty() && !record.starts_with('#') {
            changed.dirty = true;
            let count = match record.as_bytes().first() {
                Some(b'1') => 8,
                Some(b'2') => 9,
                Some(b'u') => 10,
                Some(b'?' | b'!') => 1,
                _ => continue,
            };
            if let Some(path) = record
                .splitn(count + 1, ' ')
                .nth(count)
                .filter(|path| !path.is_empty())
            {
                if record.starts_with("? ") {
                    changed.untracked.push(path.to_owned());
                }
                changed.paths.push(path.to_owned());
            }
            // A rename supplies its previous path as a second NUL record.
            if count == 9
                && let Some(original) = records.next().filter(|path| !path.is_empty())
            {
                changed.paths.push(original.to_owned());
            }
        }
    }
    changed
}

pub(super) async fn numstat(
    cwd: &Path,
    reference: &str,
) -> Result<Vec<WorkingFile>, GitManagerError> {
    let args = ["diff-index", "-M", "--numstat", "-z", reference, "--"];
    let mut result = git(cwd, &args).await?;
    if !result.status.success() && reference == "HEAD" {
        let diagnostic = result.stderr.to_ascii_lowercase();
        if diagnostic.contains("bad revision 'head'")
            || (diagnostic.contains("unknown revision")
                && diagnostic.contains("path not in the working tree"))
        {
            let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
            if let Some(tree) = text(cwd, &["hash-object", "-t", "tree", null]).await {
                result = git(cwd, &["diff-index", "-M", "--numstat", "-z", &tree, "--"]).await?;
            }
        }
    }
    if !result.status.success() {
        return Err(GitManagerError::local(
            cwd,
            "Git could not read file change totals.",
        ));
    }
    Ok(parse_numstat(&result.stdout))
}

pub(super) fn parse_numstat(stdout: &str) -> Vec<WorkingFile> {
    let mut records = stdout.split('\0');
    let mut files = Vec::new();
    while let Some(record) = records.next() {
        let mut fields = record.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Some(insertions), Some(deletions)) = (numstat_count(added), numstat_count(deleted))
        else {
            continue;
        };
        let path = if path.is_empty() {
            records.next();
            let Some(destination) = records.next().filter(|path| !path.is_empty()) else {
                continue;
            };
            destination
        } else {
            path
        };
        files.push(WorkingFile {
            path: path.to_owned(),
            insertions,
            deletions,
        });
    }
    files
}

fn numstat_count(value: &str) -> Option<u64> {
    if value == "-" {
        Some(0)
    } else {
        value.parse().ok()
    }
}
