use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

use super::{GitCommandError, git};

struct Thread {
    id: String,
    project_id: String,
    title: String,
    worktree: Option<PathBuf>,
}

pub(super) async fn assert_idle(
    operation: &'static str,
    database: &Path,
    cwd: &str,
    root: &Path,
) -> Result<(), GitCommandError> {
    let database = database.to_owned();
    let records = tokio::task::spawn_blocking(move || read(&database)).await;
    let (projects, threads) = records.ok().and_then(Result::ok).ok_or_else(|| {
        GitCommandError::new(
            operation,
            "git checkout",
            cwd,
            "Could not check which threads are working in this checkout.",
        )
    })?;
    if threads.is_empty() {
        return Ok(());
    }

    let cwd_path = Path::new(cwd);
    let mut project_ids = Vec::new();
    for (id, path) in projects {
        if (path == cwd_path || path == root)
            || (path.starts_with(root)
                && git::repository_root(&path).await.as_deref() == Some(root))
        {
            project_ids.push(id);
        }
    }

    let mut seen = HashSet::new();
    let mut titles = Vec::new();
    for path in [cwd_path, root] {
        for thread in &threads {
            if thread.worktree.as_deref() == Some(path) && seen.insert(&thread.id) {
                titles.push(format!("'{}'", thread.title));
            }
        }
    }
    for id in project_ids {
        for thread in &threads {
            if thread.worktree.is_none() && thread.project_id == id && seen.insert(&thread.id) {
                titles.push(format!("'{}'", thread.title));
            }
        }
    }

    let detail = match titles.as_slice() {
        [] => return Ok(()),
        [title] => format!(
            "{title} is still working in this checkout. Wait for it to finish, or use a worktree for a separate branch."
        ),
        [first, second] => group_detail(&format!("{first} and {second}")),
        [first, second, rest @ ..] => {
            group_detail(&format!("{first}, {second} and {} more", rest.len()))
        }
    };
    Err(GitCommandError::new(operation, "git checkout", cwd, detail))
}

fn group_detail(titles: &str) -> String {
    format!(
        "{titles} are still working in this checkout. Wait for them to finish, or use a worktree for a separate branch."
    )
}

type Projects = Vec<(String, PathBuf)>;

fn read(path: &Path) -> rusqlite::Result<(Projects, Vec<Thread>)> {
    let mut database = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let snapshot = database.transaction()?;
    let projects = snapshot.prepare(
        "SELECT project_id, workspace_root FROM projection_projects WHERE deleted_at IS NULL ORDER BY created_at, project_id"
    )?.query_map([], |row| Ok((row.get(0)?, PathBuf::from(row.get::<_, String>(1)?))))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let query = concat!(
        "SELECT busy.thread_id, t.project_id, t.title, json_extract(busy.payload_json, '$.worktreePath') FROM (",
        include_str!("../store/shell_threads.sql"),
        " WHERE t.deleted_at IS NULL) busy
          JOIN orchestration_v2_projection_threads t ON t.thread_id=busy.thread_id
          WHERE busy.activity_run_status IS NOT NULL
          ORDER BY t.archived_at IS NOT NULL, t.updated_at ASC, t.thread_id ASC"
    );
    let threads = snapshot
        .prepare(query)?
        .query_map([], |row| {
            Ok(Thread {
                id: row.get(0)?,
                project_id: row.get(1)?,
                title: row.get(2)?,
                worktree: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((projects, threads))
}
