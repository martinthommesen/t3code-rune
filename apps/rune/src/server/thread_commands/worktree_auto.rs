use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tokio::sync::Notify;

use crate::server::{
    settings::SettingsFile, terminal::TerminalManager, thread_launch, vcs_checkout,
};
use crate::worktree_cleanup::{DeletePolicy, KeepWhen};

use super::ThreadCommands;

const INTERVAL: Duration = Duration::from_secs(60);
const DELETED_WORKTREES: &str = "
    SELECT t.thread_id, t.project_id, json_extract(t.payload_json, '$.worktreePath'),
           json_extract(t.payload_json, '$.branch')
    FROM orchestration_v2_projection_threads t
    JOIN projection_projects p ON p.project_id = t.project_id AND p.deleted_at IS NULL
    WHERE t.deleted_at IS NOT NULL
      AND json_valid(t.payload_json)
      AND json_type(t.payload_json, '$.worktreePath') = 'text'
      AND json_type(t.payload_json, '$.branch') = 'text'
      AND EXISTS (
          SELECT 1 FROM orchestration_command_receipts r
          WHERE r.aggregate_id = t.thread_id AND r.aggregate_kind = 'thread'
            AND r.command_type = 'thread.delete' AND r.status = 'accepted'
      )
      AND NOT EXISTS (
          SELECT 1 FROM orchestration_v2_effect_outbox e
          WHERE e.thread_id = t.thread_id AND e.status NOT IN ('succeeded', 'cancelled')
      )
    ORDER BY t.deleted_at DESC, t.thread_id";
const ACTIVE_WORKTREES: &str = "
    SELECT json_extract(payload_json, '$.worktreePath')
    FROM orchestration_v2_projection_threads
    WHERE deleted_at IS NULL AND json_valid(payload_json)
      AND json_type(payload_json, '$.worktreePath') = 'text'
    UNION
    SELECT workspace_root FROM projection_projects WHERE deleted_at IS NULL
    UNION
    SELECT json_extract(payload_json, '$.cwd') FROM orchestration_v2_projection_provider_sessions
    WHERE status <> 'stopped' AND json_valid(payload_json) AND json_type(payload_json, '$.cwd') = 'text'";

#[derive(PartialEq, Eq)]
struct Candidate {
    thread_id: String,
    project_id: String,
    path: PathBuf,
    branch: String,
    keep: KeepWhen,
}

pub(in crate::server) async fn run(
    commands: ThreadCommands,
    settings: SettingsFile,
    terminals: Arc<TerminalManager>,
    default_root: PathBuf,
    notifications: Arc<Notify>,
) {
    let mut interval = tokio::time::interval(INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            () = notifications.notified() => {}
        }
        let (candidate_commands, candidate_settings, candidate_root) =
            (commands.clone(), settings.clone(), default_root.clone());
        let scan = tokio::task::spawn_blocking(move || {
            candidates(&candidate_commands, &candidate_settings, &candidate_root)
        })
        .await;
        let Ok(Ok(eligible)) = scan else {
            continue;
        };
        for candidate in eligible {
            let Some(removal) = vcs_checkout::automatic_candidate(
                &candidate.path,
                &candidate.branch,
                candidate.keep,
            )
            .await
            else {
                continue;
            };
            let _checkout = vcs_checkout::checkout_lease(&candidate.path).await;
            let (guard_commands, guard_settings, guard_root, guard_terminals) = (
                commands.clone(),
                settings.clone(),
                default_root.clone(),
                Arc::clone(&terminals),
            );
            let guard_branch = candidate.branch.clone();
            let keep = candidate.keep;
            let checked = tokio::task::spawn_blocking(move || {
                let eligible = candidates(&guard_commands, &guard_settings, &guard_root)?;
                Ok::<_, rusqlite::Error>(
                    eligible.contains(&candidate)
                        && !guard_terminals.has_running_terminal_under(&candidate.path),
                )
            })
            .await;
            if matches!(checked, Ok(Ok(true))) {
                let _ = vcs_checkout::remove_automatic(removal, &guard_branch, keep).await;
            }
        }
    }
}

fn candidates(
    commands: &ThreadCommands,
    settings: &SettingsFile,
    default_root: &Path,
) -> rusqlite::Result<Vec<Candidate>> {
    let Ok(snapshot) = settings.read() else {
        return Ok(Vec::new());
    };
    let snapshot = snapshot.for_client();
    let policy = DeletePolicy::from_settings(&snapshot);
    if !policy.automatically_removes_any() {
        return Ok(Vec::new());
    }
    let configured = thread_launch::configured_worktrees_root(Some(settings.path()), default_root);
    let mut roots = HashSet::new();
    let previous_roots = snapshot["previousWorktreesDirectories"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|root| root.as_str().map(Path::new))
        .filter(|root| root.is_absolute());
    for root in configured
        .iter()
        .map(PathBuf::as_path)
        .chain([default_root])
        .chain(previous_roots)
    {
        if let Ok(root) = root.canonicalize()
            && root.parent().is_some()
        {
            roots.insert(root);
        }
    }
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let mut database = commands.connect()?;
    let transaction = database.transaction()?;
    let occupied: Vec<PathBuf> = paths(&transaction, ACTIVE_WORKTREES)?
        .into_iter()
        .flat_map(|path| [path.canonicalize().unwrap_or_else(|_| path.clone()), path])
        .collect();
    let mut seen = HashSet::new();
    let mut query = transaction.prepare(DELETED_WORKTREES)?;
    query
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .filter_map(|row| {
            let row: rusqlite::Result<(String, String, String, String)> = row;
            let (thread_id, project_id, path, branch) = match row {
                Ok(row) => row,
                Err(error) => return Some(Err(error)),
            };
            let keep = policy.when_deleted(&project_id)?;
            let path = PathBuf::from(path);
            let path = live_checkout(&path, &roots)?;
            if occupied.iter().any(|usage| usage.starts_with(&path)) || !seen.insert(path.clone()) {
                return None;
            }
            Some(Ok(Candidate {
                thread_id,
                project_id,
                path,
                branch,
                keep,
            }))
        })
        .collect()
}

fn paths(database: &Connection, query: &str) -> rusqlite::Result<Vec<PathBuf>> {
    database
        .prepare(query)?
        .query_map([], |row| row.get::<_, String>(0).map(PathBuf::from))?
        .collect()
}

fn live_checkout(path: &Path, roots: &HashSet<PathBuf>) -> Option<PathBuf> {
    if !path.is_absolute() || path.symlink_metadata().ok()?.is_symlink() {
        return None;
    }
    let physical = path.canonicalize().ok()?;
    if !roots
        .iter()
        .any(|root| physical != *root && physical.starts_with(root))
    {
        return None;
    }
    physical
        .join(".git")
        .symlink_metadata()
        .ok()?
        .is_file()
        .then_some(physical)
}
