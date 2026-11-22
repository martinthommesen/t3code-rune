use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use super::{GitCommandError, checkout_lease, git::Git};

const OPERATION: &str = "GitWorkflowService.restoreMissingWorktree";

pub(in crate::server) async fn worktree_can_restore(
    worktree: &Path,
    project: &Path,
    branch: &str,
) -> Result<bool, GitCommandError> {
    let (Some(worktree), Some(project)) = (worktree.to_str(), project.to_str()) else {
        return Ok(false);
    };
    let local = format!("refs/heads/{branch}");
    let args = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
    let checkout_git = Git::new(worktree);
    let source = Git::new(project);
    let (checkout, repository, branch_exists) = tokio::try_join!(
        checkout_git.checked(OPERATION, &args, None),
        source.checked(OPERATION, &args, None),
        source.has_ref(OPERATION, &local),
    )?;
    if !branch_exists {
        return Ok(false);
    }
    tokio::task::spawn_blocking(move || {
        let checkout = Path::new(checkout.stdout.trim()).canonicalize();
        let repository = Path::new(repository.stdout.trim()).canonicalize();
        matches!((checkout, repository), (Ok(checkout), Ok(repository)) if checkout == repository)
    })
    .await
    .map_err(|_| {
        GitCommandError::new(
            OPERATION,
            "git rev-parse",
            project,
            "Could not verify the managed worktree's repository.",
        )
    })
}

pub(in crate::server) async fn restore_existing_branch(
    project: &Path,
    worktree: &Path,
    branch: &str,
    managed_roots: &[PathBuf],
) -> Result<bool, GitCommandError> {
    let _checkout = checkout_lease(worktree).await;
    restore_existing_branch_locked(project, worktree, branch, managed_roots).await
}

pub(in crate::server) async fn restore_existing_branch_locked(
    project: &Path,
    worktree: &Path,
    branch: &str,
    managed_roots: &[PathBuf],
) -> Result<bool, GitCommandError> {
    let (Some(project_cwd), Some(target)) = (project.to_str(), worktree.to_str()) else {
        return Ok(false);
    };
    let target_path = worktree.to_owned();
    let roots = managed_roots.to_vec();
    let applicable = tokio::task::spawn_blocking(move || eligible(&target_path, &roots))
        .await
        .map_err(|_| {
            GitCommandError::new(
                OPERATION,
                "git worktree add",
                project_cwd,
                "Could not check the managed worktree path.",
            )
        })?;
    if !applicable {
        return Ok(false);
    }
    let git = Git::new(project_cwd);
    let _repository = git.config_lease().await;
    let reference = format!("refs/heads/{branch}");
    if !git.has_ref(OPERATION, &reference).await? {
        return Ok(false);
    }
    git.checked(OPERATION, &["worktree", "prune"], None).await?;
    git.checked(OPERATION, &["worktree", "add", "--", target, branch], None)
        .await?;
    Ok(true)
}

fn eligible(worktree: &Path, roots: &[PathBuf]) -> bool {
    if !worktree.is_absolute()
        || !matches!(worktree.symlink_metadata(), Err(error) if error.kind() == ErrorKind::NotFound)
    {
        return false;
    }
    roots.iter().any(|root| {
        let Ok(canonical_root) = root.canonicalize() else {
            return false;
        };
        if canonical_root.parent().is_none() {
            return false;
        }
        let Some(relative) = worktree
            .strip_prefix(root)
            .ok()
            .or_else(|| worktree.strip_prefix(&canonical_root).ok())
        else {
            return false;
        };
        let mut components = relative.components();
        let (Some(Component::Normal(project_name)), Some(Component::Normal(_)), None) =
            (components.next(), components.next(), components.next())
        else {
            return false;
        };
        let parent = canonical_root.join(project_name);
        match parent.symlink_metadata() {
            Ok(metadata) => metadata.is_dir() && !metadata.file_type().is_symlink(),
            Err(error) => error.kind() == ErrorKind::NotFound,
        }
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use super::*;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let result = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    }

    fn commit(cwd: &Path, message: &str) {
        git(cwd, &["add", "source"]);
        git(
            cwd,
            &[
                "-c",
                "user.name=Fixture Author",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                message,
            ],
        );
    }

    #[tokio::test]
    async fn restores_the_saved_local_branch_tip_in_the_same_managed_folder() {
        let fixture = tempfile::tempdir().unwrap();
        let project = fixture.path().join("source-project");
        let managed = fixture.path().join("managed");
        let path = managed.join("project-name/checkout");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&managed).unwrap();
        git(&project, &["init", "-b", "main"]);
        fs::write(project.join("source"), b"main checkout\n").unwrap();
        commit(&project, "Project main");
        let main = git(&project, &["rev-parse", "HEAD"]);
        let branch = "feature/restore";
        let target = path.to_str().unwrap();
        git(&project, &["worktree", "add", "-b", branch, target]);
        fs::write(path.join("source"), b"saved worktree commit\n").unwrap();
        commit(&path, "Keep worktree changes");
        let tip = git(&path, &["rev-parse", "HEAD"]);
        assert_ne!(tip, main);
        git(&project, &["worktree", "remove", target]);
        assert!(!path.exists());
        let roots = [managed];

        assert!(
            restore_existing_branch(&project, &path, branch, &roots)
                .await
                .unwrap()
        );
        assert_eq!(git(&path, &["branch", "--show-current"]), branch);
        assert_eq!(git(&path, &["rev-parse", "HEAD"]), tip);
        assert_eq!(
            fs::read(path.join("source")).unwrap(),
            b"saved worktree commit\n"
        );
        assert_eq!(git(&project, &["rev-parse", "HEAD"]), main);
        assert!(
            !restore_existing_branch(&project, &path, branch, &roots)
                .await
                .unwrap()
        );
        let unrelated = fixture.path().join("other-project/checkout");
        assert!(
            !restore_existing_branch(&project, &unrelated, branch, &roots)
                .await
                .unwrap()
        );
        assert!(!unrelated.exists());
    }
}
