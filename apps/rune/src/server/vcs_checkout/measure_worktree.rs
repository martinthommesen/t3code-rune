use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::AutomaticInspectionError;

const DEADLINE: Duration = Duration::from_secs(30);
const MAX_ENTRIES: usize = 2_000_000;
const INCOMPLETE: &str = "Worktree could not be fully inspected";
const NESTED: &str = "Contains a nested Git repository";

pub(in crate::server) async fn measure_worktree(
    path: &Path,
) -> Result<u64, AutomaticInspectionError> {
    let root = path.to_owned();
    tokio::time::timeout(DEADLINE, tokio::task::spawn_blocking(move || walk(&root)))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(Err(INCOMPLETE))
        .map_err(|reason| AutomaticInspectionError::Kept(reason.to_owned()))
}

fn walk(root: &Path) -> Result<u64, &'static str> {
    let started = Instant::now();
    let mut pending = vec![root.to_owned()];
    let mut entries = 1;
    let mut bytes = 0_u64;
    while let Some(path) = pending.pop() {
        if started.elapsed() >= DEADLINE {
            return Err(INCOMPLETE);
        }
        let metadata = fs::symlink_metadata(&path).map_err(|_| INCOMPLETE)?;
        if metadata.is_symlink() {
            continue;
        }
        if path.canonicalize().map_err(|_| INCOMPLETE)? != path {
            return Err(INCOMPLETE);
        }
        if metadata.is_file() {
            bytes = bytes.saturating_add(allocated_bytes(&metadata));
        } else if metadata.is_dir() {
            append_children(root, &path, &mut pending, &mut entries)?;
        }
    }
    Ok(bytes)
}

fn append_children(
    root: &Path,
    path: &Path,
    pending: &mut Vec<PathBuf>,
    entries: &mut usize,
) -> Result<(), &'static str> {
    let mut bare_markers = [false; 3];
    for child in fs::read_dir(path).map_err(|_| INCOMPLETE)? {
        let child = child.map_err(|_| INCOMPLETE)?;
        let child_path = child.path();
        if !child_path.starts_with(root) || *entries >= MAX_ENTRIES {
            return Err(INCOMPLETE);
        }
        if path != root {
            let name = child.file_name();
            if name.eq_ignore_ascii_case(".git") {
                return Err(NESTED);
            }
            for (present, marker) in bare_markers.iter_mut().zip(["head", "objects", "refs"]) {
                *present |= name.eq_ignore_ascii_case(marker);
            }
        }
        *entries += 1;
        pending.push(child_path);
    }
    if bare_markers.into_iter().all(|present| present) {
        return Err(NESTED);
    }
    Ok(())
}

#[cfg(unix)]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    if metadata.nlink() == 1 {
        metadata.blocks().saturating_mul(512)
    } else {
        0
    }
}

#[cfg(not(unix))]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}
