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

pub(in crate::server) enum AutomaticInspectionError {
    Kept(String),
    Failed(String),
}

impl From<GitCommandError> for AutomaticInspectionError {
    fn from(error: GitCommandError) -> Self {
        Self::Failed(error.into_detail())
    }
}

pub(in crate::server) async fn inspect_automatic(
    path: &Path,
    branch: &str,
    keep: KeepWhen,
) -> Result<AutomaticRemoval, AutomaticInspectionError> {
    use AutomaticInspectionError::{Failed, Kept};

    let worktree = path
        .to_str()
        .ok_or_else(|| Kept("Path is not valid UTF-8".into()))?;
    let git = Git::new(worktree);
    let current = git
        .checked(OPERATION, &["branch", "--show-current"], None)
        .await?;
    if trim_js(&current.stdout) != branch {
        return Err(Kept("Repository or branch changed".into()));
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
        .await?;
    let index_lock = PathBuf::from(trim_js(&index_lock.stdout));
    let index_unlocked = tokio::task::spawn_blocking(move || {
        index_lock.is_absolute()
            && matches!(std::fs::symlink_metadata(index_lock), Err(error) if error.kind() == ErrorKind::NotFound)
    }).await.map_err(|_| Failed("Git index lock inspection stopped.".into()))?;
    if !index_unlocked {
        return Err(Failed(
            "git: Git index is locked. Status will resume when the index lock is removed.".into(),
        ));
    }
    let head = git
        .checked(OPERATION, &["rev-parse", "--verify", "HEAD"], None)
        .await?;
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
        .await?;
    if !status.stdout.is_empty() && !status.stdout.ends_with('\0') {
        return Err(Kept("Local changes exceed the inspection limit".into()));
    }
    let mut records = status.stdout.split_terminator('\0');
    let mut changed = 0;
    while let Some(record) = records.next() {
        changed += 1;
        if record
            .as_bytes()
            .iter()
            .take(2)
            .any(|state| matches!(state, b'R' | b'C'))
        {
            records.next();
        }
    }
    if changed != 0 {
        let files = if changed == 1 { "file" } else { "files" };
        return Err(Kept(format!("Has uncommitted changes ({changed} {files})")));
    }
    if let Some(path) = super::hidden_changes::first_change(git, path).await? {
        return Err(Kept(format!(
            "Has uncommitted changes hidden from git status ({path})"
        )));
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
            .await?;
        if !ignored.stdout.is_empty() && !ignored.stdout.ends_with('\0') {
            return Err(Kept("Ignored files exceed the inspection limit".into()));
        }
        if let Some(entry) = ignored.stdout.split_terminator('\0').find(|entry| {
            !entry.is_empty() && *entry != "node_modules/" && !entry.ends_with("/node_modules/")
        }) {
            return Err(Kept(format!("Has ignored files ({entry})")));
        }
    }
    let common = git
        .checked(
            OPERATION,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            None,
        )
        .await?;
    let directory = Path::new(trim_js(&common.stdout));
    let root = if directory.file_name().is_some_and(|part| part == ".git") {
        directory
            .parent()
            .ok_or_else(|| Kept("Not a linked worktree".into()))?
    } else {
        directory
    };
    let repository = root
        .to_str()
        .ok_or_else(|| Kept("Repository path is not valid UTF-8".into()))?;
    let worktrees = Git::new(repository)
        .checked(OPERATION, &["worktree", "list", "--porcelain", "-z"], None)
        .await?;
    let expected = path.to_owned();
    tokio::task::spawn_blocking(move || registered_checkout(&worktrees.stdout, &expected))
        .await
        .map_err(|_| Failed("Git worktree registration inspection stopped.".into()))??;
    Ok(AutomaticRemoval {
        path: path.to_owned(),
        repository: repository.to_owned(),
        head: trim_js(&head.stdout).to_owned(),
    })
}

fn registered_checkout(records: &str, expected: &Path) -> Result<(), AutomaticInspectionError> {
    use AutomaticInspectionError::{Failed, Kept};

    for record in records.split("\0\0").skip(1) {
        let mut fields = record.split('\0');
        let Some(directory) = fields
            .next()
            .and_then(|field| field.strip_prefix("worktree "))
        else {
            continue;
        };
        let directory = Path::new(directory);
        if directory != expected && !directory.canonicalize().is_ok_and(|path| path == expected) {
            continue;
        }
        for field in fields {
            if field == "locked" {
                return Err(Failed(
                    "git worktree remove: fatal: cannot remove a locked working tree".into(),
                ));
            }
            if let Some(reason) = field.strip_prefix("locked ") {
                return Err(Failed(format!(
                    "git worktree remove: fatal: cannot remove a locked working tree, lock reason: {reason}"
                )));
            }
        }
        return Ok(());
    }
    Err(Kept("Not a linked worktree".into()))
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
) -> Result<PathBuf, AutomaticInspectionError> {
    use AutomaticInspectionError::Kept;

    let current = inspect_automatic(&before.path, branch, keep).await?;
    if current != before {
        return Err(Kept("Commit changed since check".to_owned()));
    }
    let worktree = before
        .path
        .to_str()
        .ok_or_else(|| Kept("Path is not valid UTF-8".to_owned()))?;
    let git = Git::new(worktree);
    if keep == KeepWhen::TrackedChanges {
        let index = git
            .checked(OPERATION, &["ls-files", "--stage", "-z"], None)
            .await?;
        if !index.stdout.is_empty() && !index.stdout.ends_with('\0') {
            return Err(Kept("Git index exceeds the inspection limit".to_owned()));
        }
        if index
            .stdout
            .split('\0')
            .any(|entry| entry.starts_with("160000 "))
        {
            return Err(Kept("Has nested Git submodules".to_owned()));
        }
        // A repository created after inspection must survive the cleanup.
        git.checked(OPERATION, &["clean", "-fdx"], None).await?;
    }
    remove_worktree(RemoveWorktreeInput {
        cwd: before.repository,
        path: worktree.to_owned(),
        force: false,
    })
    .await
    .map_err(Into::into)
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
        let inspect = |path| inspect_automatic(path, "disposable", KeepWhen::UncommittedChanges);
        assert!(inspect(&to_remove).await.is_ok());
        assert!(inspect(&to_keep).await.is_err());
        let git_directory = fs::read_to_string(to_remove.join(".git")).unwrap();
        let git_directory = Path::new(git_directory.trim().strip_prefix("gitdir: ").unwrap());
        let index_lock = git_directory.join("index.lock");
        fs::write(&index_lock, b"busy").unwrap();
        assert!(inspect(&to_remove).await.is_err());
        fs::remove_file(index_lock).unwrap();
        git(
            &to_remove,
            &["update-index", "--assume-unchanged", "source"],
        );
        assert!(inspect(&to_remove).await.is_ok());
        fs::write(to_remove.join("source"), b"hidden user bytes").unwrap();
        assert!(inspect(&to_remove).await.is_err());
        fs::write(to_remove.join("source"), b"tracked bytes\n").unwrap();
        git(
            &to_remove,
            &["update-index", "--no-assume-unchanged", "source"],
        );
        git(&checkout, &["config", "status.showUntrackedFiles", "no"]);
        fs::write(to_remove.join("untracked"), b"user bytes").unwrap();
        assert!(inspect(&to_remove).await.is_err());
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
