use std::io::ErrorKind;
use std::path::{self, Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::{GitCommandError, git::Git, trim_js};
use crate::worktree_cleanup::KeepWhen;

const OPERATION: &str = "GitVcsDriver.removeWorktree";
const COMMAND: &str = "git worktree remove";
const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Deserialize)]
pub(in crate::server) struct RemoveWorktreeInput {
    cwd: String,
    path: String,
    #[serde(default)]
    force: bool,
}

impl RemoveWorktreeInput {
    pub(in crate::server) fn parse(payload: Value) -> Result<Self, String> {
        let required = "A Git working directory and worktree path are required.";
        let mut input: Self = serde_json::from_value(payload).map_err(|_| required.to_owned())?;
        input.cwd = trim_js(&input.cwd).to_owned();
        input.path = trim_js(&input.path).to_owned();
        if input.cwd.is_empty() || input.path.is_empty() {
            return Err(required.into());
        }
        Ok(input)
    }
}

pub(in crate::server) async fn remove_worktree(
    input: RemoveWorktreeInput,
) -> Result<PathBuf, GitCommandError> {
    let cwd = input.cwd.clone();
    tokio::spawn(async move { remove(input).await })
        .await
        .map_err(|_| {
            GitCommandError::new(OPERATION, COMMAND, &cwd, "Worktree removal task stopped.")
        })?
}

async fn remove(input: RemoveWorktreeInput) -> Result<PathBuf, GitCommandError> {
    let cwd = input.cwd.as_str();
    let path = path::absolute(Path::new(cwd).join(input.path)).map_err(|_| {
        GitCommandError::new(
            OPERATION,
            COMMAND,
            cwd,
            "Could not resolve the worktree path.",
        )
    })?;
    let target = path.to_str().ok_or_else(|| {
        GitCommandError::new(
            OPERATION,
            COMMAND,
            cwd,
            "The worktree path is not valid UTF-8.",
        )
    })?;
    let git = Git::new(cwd);
    let _lease = git.config_lease().await;
    let mut args = vec![
        "-c",
        "status.showUntrackedFiles=normal",
        "worktree",
        "remove",
    ];
    if input.force {
        args.push("--force");
    }
    args.extend(["--", target]);
    let result = git.run(OPERATION, &args, Some(TIMEOUT)).await?;
    if result.status.success() {
        return Ok(path);
    }
    let missing = if result.stderr.contains("is not a working tree") {
        let target = path.clone();
        tokio::task::spawn_blocking(move || {
            matches!(std::fs::symlink_metadata(target), Err(error) if error.kind() == ErrorKind::NotFound)
        })
        .await
        .unwrap_or(false)
    } else {
        false
    };
    if missing {
        git.checked(OPERATION, &["worktree", "prune"], None).await?;
        return Ok(path);
    }
    Err(GitCommandError::failed(
        OPERATION,
        cwd,
        &args,
        &result,
        Some("git worktree remove failed"),
    ))
}

pub(in crate::server) async fn automatic_candidate(
    path: &Path,
    branch: &str,
    keep: KeepWhen,
) -> Option<AutomaticRemoval> {
    let worktree = path.to_str()?;
    let git = Git::new(worktree);
    let current = git
        .checked(OPERATION, &["branch", "--show-current"], None)
        .await
        .ok()?;
    if trim_js(&current.stdout) != branch {
        return None;
    }
    let index_lock = git
        .checked(
            OPERATION,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "index.lock",
            ],
            None,
        )
        .await
        .ok()?;
    let index_lock = PathBuf::from(trim_js(&index_lock.stdout));
    let index_unlocked = tokio::task::spawn_blocking(move || {
        index_lock.is_absolute()
            && matches!(std::fs::symlink_metadata(index_lock), Err(error) if error.kind() == ErrorKind::NotFound)
    }).await.ok()?;
    if !index_unlocked {
        return None;
    }
    let head = git
        .checked(OPERATION, &["rev-parse", "--verify", "HEAD"], None)
        .await
        .ok()?;
    let untracked = if keep == KeepWhen::TrackedChanges {
        "--untracked-files=no"
    } else {
        "--untracked-files=all"
    };
    let status = git
        .checked(
            OPERATION,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--ignore-submodules=none",
                untracked,
            ],
            None,
        )
        .await
        .ok()?;
    if !status.stdout.is_empty() {
        return None;
    }
    let tracked = git
        .checked(OPERATION, &["ls-files", "-v", "-z"], None)
        .await
        .ok()?;
    if !tracked.stdout.is_empty() && !tracked.stdout.ends_with('\0') {
        return None;
    }
    if tracked.stdout.split('\0').any(|entry| {
        entry
            .as_bytes()
            .first()
            .is_some_and(|flag| flag.is_ascii_lowercase() || *flag == b'S')
    }) {
        return None;
    }
    if keep == KeepWhen::AnyLocalFiles {
        let ignored = git
            .checked(
                OPERATION,
                &[
                    "ls-files",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                    "--directory",
                    "-z",
                ],
                None,
            )
            .await
            .ok()?;
        if !ignored.stdout.is_empty() && !ignored.stdout.ends_with('\0') {
            return None;
        }
        if ignored.stdout.split('\0').any(|entry| {
            !entry.is_empty() && entry != "node_modules/" && !entry.ends_with("/node_modules/")
        }) {
            return None;
        }
    }
    let common = git
        .checked(
            OPERATION,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            None,
        )
        .await
        .ok()?;
    let directory = Path::new(trim_js(&common.stdout));
    let cwd = if directory.file_name().is_some_and(|part| part == ".git") {
        directory.parent()?
    } else {
        directory
    };
    let repository = cwd.to_str()?;
    let worktrees = Git::new(repository)
        .checked(OPERATION, &["worktree", "list", "--porcelain", "-z"], None)
        .await
        .ok()?;
    let expected = path.to_owned();
    let registered = tokio::task::spawn_blocking(move || {
        worktrees.stdout.split("\0\0").skip(1).any(|record| {
            let mut fields = record.split('\0');
            let Some(directory) = fields
                .next()
                .and_then(|field| field.strip_prefix("worktree "))
            else {
                return false;
            };
            !fields.any(|field| field == "locked" || field.starts_with("locked "))
                && (Path::new(directory) == expected
                    || Path::new(directory)
                        .canonicalize()
                        .is_ok_and(|directory| directory == expected))
        })
    })
    .await
    .ok()?;
    if !registered {
        return None;
    }
    Some(AutomaticRemoval {
        path: path.to_owned(),
        repository: repository.to_owned(),
        head: trim_js(&head.stdout).to_owned(),
    })
}

#[derive(PartialEq, Eq)]
pub(in crate::server) struct AutomaticRemoval {
    path: PathBuf,
    repository: String,
    head: String,
}

pub(in crate::server) async fn remove_automatic(
    before: AutomaticRemoval,
    branch: &str,
    keep: KeepWhen,
) -> Option<PathBuf> {
    let current = automatic_candidate(&before.path, branch, keep).await?;
    if current != before {
        return None;
    }
    let worktree = before.path.to_str()?;
    let git = Git::new(worktree);
    if keep == KeepWhen::TrackedChanges {
        let index = git
            .checked(OPERATION, &["ls-files", "--stage", "-z"], None)
            .await
            .ok()?;
        if !index.stdout.is_empty() && !index.stdout.ends_with('\0')
            || index
                .stdout
                .split('\0')
                .any(|entry| entry.starts_with("160000 "))
        {
            return None;
        }
        git.checked(OPERATION, &["clean", "-ffdx"], None)
            .await
            .ok()?;
    }
    remove_worktree(RemoveWorktreeInput {
        cwd: before.repository,
        path: worktree.to_owned(),
        force: false,
    })
    .await
    .ok()
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use super::*;

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn deleting_one_worktree_preserves_other_checkouts_and_honors_container_locks() {
        let fixture = tempfile::tempdir().unwrap();
        let checkout = fixture.path().join("checkout");
        let to_remove = fixture.path().join("remove this");
        let to_keep = fixture.path().join("keep this");
        fs::create_dir(&checkout).unwrap();
        git(&checkout, &["init", "-b", "main"]);
        fs::write(checkout.join("source"), "tracked bytes\n").unwrap();
        git(&checkout, &["add", "source"]);
        git(
            &checkout,
            &[
                "-c",
                "user.name=Fixture Author",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "Add fixture source",
            ],
        );
        git(
            &checkout,
            &[
                "worktree",
                "add",
                "-b",
                "disposable",
                to_remove.to_str().unwrap(),
            ],
        );
        git(
            &checkout,
            &[
                "worktree",
                "add",
                "-b",
                "retained",
                to_keep.to_str().unwrap(),
            ],
        );
        let inspect = |path| automatic_candidate(path, "disposable", KeepWhen::UncommittedChanges);
        assert!(inspect(&to_remove).await.is_some());
        assert!(inspect(&to_keep).await.is_none());
        let git_directory = fs::read_to_string(to_remove.join(".git")).unwrap();
        let git_directory = Path::new(git_directory.trim().strip_prefix("gitdir: ").unwrap());
        let index_lock = git_directory.join("index.lock");
        fs::write(&index_lock, b"busy").unwrap();
        assert!(inspect(&to_remove).await.is_none());
        fs::remove_file(index_lock).unwrap();
        git(
            &to_remove,
            &["update-index", "--assume-unchanged", "source"],
        );
        fs::write(to_remove.join("source"), b"hidden user bytes").unwrap();
        assert!(inspect(&to_remove).await.is_none());
        fs::write(to_remove.join("source"), b"tracked bytes\n").unwrap();
        git(
            &to_remove,
            &["update-index", "--no-assume-unchanged", "source"],
        );
        git(&checkout, &["config", "status.showUntrackedFiles", "no"]);
        fs::write(to_remove.join("untracked"), b"user bytes").unwrap();
        assert!(inspect(&to_remove).await.is_none());
        let command = |path: &Path, force| RemoveWorktreeInput {
            cwd: checkout.to_str().unwrap().to_owned(),
            path: path.to_str().unwrap().to_owned(),
            force,
        };

        assert!(remove_worktree(command(&to_remove, false)).await.is_err());
        assert_eq!(
            fs::read(to_remove.join("untracked")).unwrap(),
            b"user bytes"
        );
        git(
            &checkout,
            &[
                "worktree",
                "lock",
                "--reason",
                "fixture",
                to_keep.to_str().unwrap(),
            ],
        );
        assert!(remove_worktree(command(&to_keep, true)).await.is_err());
        assert!(to_keep.join("source").is_file());
        remove_worktree(command(&to_remove, true)).await.unwrap();
        remove_worktree(command(&to_remove, true)).await.unwrap();
        assert!(!to_remove.exists());
        assert_eq!(
            fs::read(checkout.join("source")).unwrap(),
            b"tracked bytes\n"
        );
        assert_eq!(
            fs::read(to_keep.join("source")).unwrap(),
            b"tracked bytes\n"
        );
    }
}
