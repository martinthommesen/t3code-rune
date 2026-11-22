use std::collections::HashSet;
use std::fs::{self, File, FileTimes};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{CheckpointError, Git, Scratch, config_bool, git_path};

pub(super) async fn prepare_index(
    git: Git<'_>,
    scratch: &Arc<Scratch>,
    cwd: &Path,
    head: &str,
) -> Result<(), CheckpointError> {
    if reuse_index(git, scratch, cwd, head).await.is_ok() {
        return Ok(());
    }
    if !config_bool(git, "core.sparseCheckoutCone").await? {
        return Err(CheckpointError::UnsupportedSparseCheckout);
    }
    match fs::remove_file(scratch.index()) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    git.scratch(scratch)
        .sparse_index()
        .command(["read-tree", "--reset", head])
        .await?;
    Ok(())
}

async fn reuse_index(
    git: Git<'_>,
    scratch: &Arc<Scratch>,
    cwd: &Path,
    head: &str,
) -> Result<(), CheckpointError> {
    let source = git.command(["rev-parse", "--git-path", "index"]).await?;
    let source = cwd.join(PathBuf::from(git_path(source)?));
    let files = Arc::clone(scratch);
    let timestamp = tokio::task::spawn_blocking(move || older_index_copy(&source, files.index()))
        .await
        .map_err(|_| CheckpointError::InvalidOutput("index copy was interrupted"))??;
    let staging = git.scratch(scratch);
    staging.command(["read-tree", "--reset", head]).await?;
    write_timestamp(scratch.index(), timestamp)?;
    let records = staging
        .command(["ls-files", "--full-name", "--sparse", "--stage", "-v", "-z"])
        .await?;

    let mut flagged = Vec::new();
    let mut skipped = Vec::new();
    let mut paths = Vec::new();
    for record in records
        .split(|byte| *byte == 0)
        .filter(|row| !row.is_empty())
    {
        let [tag, b' ', entry @ ..] = record else {
            return Err(CheckpointError::InvalidOutput("invalid sparse index entry"));
        };
        match tag {
            b's' => {
                return Err(CheckpointError::InvalidOutput(
                    "sparse entry has both skip-worktree and assume-unchanged set",
                ));
            }
            b'S' if !entry.ends_with(b"/") => {
                let (_, path) = entry.split_at(
                    entry
                        .iter()
                        .position(|byte| *byte == b'\t')
                        .ok_or(CheckpointError::InvalidOutput("missing sparse index path"))?
                        + 1,
                );
                paths.extend_from_slice(path);
                paths.push(0);
                skipped.push((entry, path));
            }
            b'a'..=b'z' => flagged.push(entry),
            _ => {}
        }
    }
    if !paths.is_empty() {
        let selected = staging
            .run(["sparse-checkout", "check-rules", "-z"], Some(&paths))
            .await?
            .success()?;
        let selected: HashSet<_> = selected.split(|byte| *byte == 0).collect();
        for (entry, path) in skipped {
            if selected.contains(path) {
                flagged.push(entry);
            }
        }
    }
    if !flagged.is_empty() {
        let mut input = Vec::new();
        for entry in flagged {
            if entry.starts_with(b"160000 ") {
                return Err(CheckpointError::InvalidOutput(
                    "flagged sparse gitlink entry",
                ));
            }
            input.extend_from_slice(entry);
            input.push(0);
        }
        staging
            .run(["update-index", "-z", "--index-info"], Some(&input))
            .await?
            .success()?;
        write_timestamp(scratch.index(), timestamp)?;
    }
    Ok(())
}

fn older_index_copy(source: &Path, destination: &Path) -> io::Result<SystemTime> {
    let time = fs::metadata(source)?
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_sub(Duration::from_millis(1))
        .as_secs();
    if time == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid index timestamp",
        ));
    }
    fs::copy(source, destination)?;
    Ok(UNIX_EPOCH + Duration::from_secs(time))
}

fn write_timestamp(index: &Path, timestamp: SystemTime) -> io::Result<()> {
    // A copy with a newer timestamp can hide worktree edits that Git still needs to check.
    File::options().write(true).open(index)?.set_times(
        FileTimes::new()
            .set_accessed(timestamp)
            .set_modified(timestamp),
    )
}
