use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::git_checkpoint::{CheckpointError, ReadOutput, read_command};
use super::{GitManagerError, trim};

pub(super) struct Checkout {
    pub root: PathBuf,
    pub index: PathBuf,
    pub common: PathBuf,
}

pub(super) async fn checkout(cwd: &Path) -> Result<Option<Checkout>, GitManagerError> {
    match cwd.metadata() {
        Ok(metadata) if !metadata.is_dir() => return Ok(None),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        _ => (),
    }
    let probe = read_command(
        cwd,
        &["rev-parse", "--is-inside-work-tree", "--show-toplevel"],
    );
    let result = match tokio::time::timeout(Duration::from_secs(5), probe).await {
        Ok(Err(CheckpointError::Io(error))) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Ok(Ok(output)) => output,
        _ => {
            return Err(GitManagerError::local(
                cwd,
                "Failed to detect a Git repository.",
            ));
        }
    };
    let Some((inside, root)) = line_feed(&result.stdout).split_once('\n') else {
        return Ok(None);
    };
    if !result.status.success() || trim(inside) != "true" || root.is_empty() {
        return Ok(None);
    }
    let (index, common) = tokio::try_join!(
        git(cwd, &["rev-parse", "--git-path", "index"]),
        git(cwd, &["rev-parse", "--git-common-dir"]),
    )?;
    if !index.status.success() || line_feed(&index.stdout).is_empty() {
        return Err(GitManagerError::local(
            cwd,
            "Failed to resolve the Git index.",
        ));
    }
    if !common.status.success() || line_feed(&common.stdout).is_empty() {
        return Err(GitManagerError::local(
            cwd,
            "Failed to resolve the Git common directory.",
        ));
    }
    let resolved_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned());
    let index = resolved_cwd.join(line_feed(&index.stdout));
    let common = resolved_cwd.join(line_feed(&common.stdout));
    let common = common.canonicalize().unwrap_or(common);
    let mut lock = index.as_os_str().to_owned();
    lock.push(".lock");
    match Path::new(&lock).try_exists() {
        Ok(true) => {
            return Err(GitManagerError::local(
                cwd,
                "Git index is locked. Status will resume when the index lock is removed.",
            ));
        }
        Err(_) => {
            return Err(GitManagerError::local(
                cwd,
                "Failed to check the Git index lock.",
            ));
        }
        Ok(false) => (),
    }
    Ok(Some(Checkout {
        root: cwd.join(root).components().collect(),
        index,
        common,
    }))
}

pub(super) async fn git(cwd: &Path, args: &[&str]) -> Result<ReadOutput, GitManagerError> {
    read_command(cwd, args)
        .await
        .map_err(|_| GitManagerError::local(cwd, "Could not read Git status."))
}

pub(super) async fn text(cwd: &Path, args: &[&str]) -> Option<String> {
    let result = git(cwd, args).await.ok()?;
    let text = trim(&result.stdout);
    (result.status.success() && !text.is_empty()).then(|| text.to_owned())
}

pub(super) async fn ref_exists(cwd: &Path, reference: &str) -> bool {
    git(cwd, &["show-ref", "--verify", "--quiet", reference])
        .await
        .is_ok_and(|result| result.status.success())
}

pub(super) async fn config(cwd: &Path, key: &str) -> Option<String> {
    text(cwd, &["config", "--get", key]).await
}

pub(super) fn line_feed(text: &str) -> &str {
    text.strip_suffix('\n').unwrap_or(text)
}
