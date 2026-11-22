mod live;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::server::{
    settings::SettingsFile,
    storage_cleanup::{self as cleanup, Entry, Outcome, Trigger},
    terminal::TerminalManager,
    thread_launch,
    vcs_checkout::{self, AutomaticInspectionError},
};
use crate::worktree_cleanup::{DeletePolicy, KeepWhen};

use super::ThreadCommands;

const INTERVAL: Duration = Duration::from_secs(60 * 60);
const DELETED_WORKTREES: &str = "
    SELECT t.thread_id, t.project_id, json_extract(t.payload_json, '$.worktreePath'),
           json_extract(t.payload_json, '$.branch'), t.title, p.workspace_root
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

#[derive(Clone, PartialEq, Eq)]
struct Candidate {
    thread_id: String,
    project_id: String,
    title: String,
    project_path: String,
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
    mut reports: cleanup::Worker,
) {
    let mut settings_changes = settings.subscribe_changes();
    let mut observed_settings = settings
        .read()
        .ok()
        .map(|snapshot| cleanup_settings(&snapshot.for_client()));
    let mut interval = tokio::time::interval(INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let (trigger, request) = tokio::select! {
            _ = interval.tick() => (Trigger::Automatic, None),
            () = notifications.notified() => (Trigger::Automatic, None),
            snapshot = settings_changes.recv() => {
                let Some(snapshot) = snapshot else { break; };
                let signature = cleanup_settings(&snapshot);
                if observed_settings.as_ref() == Some(&signature) {
                    continue;
                }
                observed_settings = Some(signature);
                (Trigger::Automatic, None)
            }
            manual = reports.manual.recv() => match manual {
                Some(request) => (Trigger::Manual, Some(request)),
                None => break,
            },
        };
        let started_at = time::OffsetDateTime::now_utc();
        let mut entries = match sweep(&commands, &settings, &terminals, &default_root).await {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("Could not inspect worktrees for storage cleanup: {error}");
                vec![Entry {
                    kind: cleanup::Kind::Worktree,
                    outcome: Outcome::Failed,
                    reason: "Could not inspect worktree storage.".to_owned(),
                    path: None,
                    project_path: None,
                    thread_id: None,
                    thread_title: None,
                    bytes: None,
                    files: None,
                }]
            }
        };
        let file_settings = settings.clone();
        let file_entries = tokio::task::spawn_blocking(move || {
            cleanup::files::sweep(&file_settings, started_at.into())
        })
        .await;
        match file_entries {
            Ok(Ok(files)) => {
                entries.extend(files);
                reports.publish(
                    cleanup::Report::finish(trigger, started_at, entries),
                    request,
                );
            }
            Err(_) | Ok(Err(_)) => {
                if let Some(request) = request {
                    let _ = request.send(Err("Could not inspect workspace storage.".to_owned()));
                }
            }
        }
    }
}

fn cleanup_settings(settings: &Value) -> Value {
    let project_rules = settings["projectSettingsOverrides"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(id, project)| {
            project
                .get("worktreeCleanup")
                .map(|rule| (id.clone(), rule.clone()))
        })
        .collect::<serde_json::Map<_, _>>();
    json!([
        settings["storageCleanup"],
        settings["worktreeCleanup"],
        project_rules,
    ])
}

async fn sweep(
    commands: &ThreadCommands,
    settings: &SettingsFile,
    terminals: &Arc<TerminalManager>,
    default_root: &Path,
) -> rusqlite::Result<Vec<Entry>> {
    let (candidate_commands, candidate_settings, candidate_root) =
        (commands.clone(), settings.clone(), default_root.to_owned());
    let candidates = tokio::task::spawn_blocking(move || {
        candidates(&candidate_commands, &candidate_settings, &candidate_root)
    })
    .await
    .map_err(|_| rusqlite::Error::InvalidQuery)??;
    let mut entries = live::sweep(commands, settings, terminals, default_root).await?;
    entries.reserve(candidates.len());
    for candidate in candidates {
        let (outcome, reason, bytes) =
            match inspect_and_remove(&candidate, commands, settings, terminals, default_root).await
            {
                Ok(bytes) => (
                    Outcome::Removed,
                    "Thread was deleted".to_owned(),
                    Some(bytes),
                ),
                Err(AutomaticInspectionError::Kept(reason)) => (Outcome::Kept, reason, None),
                Err(AutomaticInspectionError::Failed(reason)) => (Outcome::Failed, reason, None),
            };
        entries.push(Entry {
            kind: cleanup::Kind::Worktree,
            outcome,
            reason,
            path: Some(candidate.path.to_string_lossy().into_owned()),
            project_path: Some(candidate.project_path),
            thread_id: Some(candidate.thread_id),
            thread_title: Some(candidate.title),
            bytes,
            files: None,
        });
    }
    Ok(entries)
}

async fn inspect_and_remove(
    candidate: &Candidate,
    commands: &ThreadCommands,
    settings: &SettingsFile,
    terminals: &Arc<TerminalManager>,
    default_root: &Path,
) -> Result<u64, AutomaticInspectionError> {
    use AutomaticInspectionError::{Failed, Kept};

    let removal =
        vcs_checkout::inspect_automatic(&candidate.path, &candidate.branch, candidate.keep).await?;
    let bytes = vcs_checkout::measure_worktree(&candidate.path).await?;
    let _checkout = vcs_checkout::checkout_lease(&candidate.path).await;
    let (guard_commands, guard_settings, guard_root, guard_terminals, guard_candidate) = (
        commands.clone(),
        settings.clone(),
        default_root.to_owned(),
        Arc::clone(terminals),
        candidate.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let eligible = candidates(&guard_commands, &guard_settings, &guard_root)
            .map_err(|error| Failed(format!("Could not confirm worktree eligibility: {error}")))?;
        if !eligible.contains(&guard_candidate) {
            return Err(Kept(
                "Thread or cleanup settings changed since check".to_owned(),
            ));
        }
        if guard_terminals.has_running_terminal_under(&guard_candidate.path) {
            return Err(Kept("Open terminal".to_owned()));
        }
        Ok(())
    })
    .await
    .map_err(|_| Failed("Worktree eligibility check stopped.".to_owned()))??;
    vcs_checkout::remove_automatic(removal, &candidate.branch, candidate.keep).await?;
    Ok(bytes)
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
    let roots = physical_roots(settings, &snapshot, default_root);
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
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?
        .filter_map(|row| {
            let row: rusqlite::Result<(String, String, String, String, String, String)> = row;
            let (thread_id, project_id, path, branch, title, project_path) = match row {
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
                title,
                project_path,
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

fn physical_roots(
    settings: &SettingsFile,
    snapshot: &Value,
    default_root: &Path,
) -> HashSet<PathBuf> {
    thread_launch::managed_worktrees_roots(settings.path(), snapshot, default_root)
        .into_iter()
        .filter_map(|root| root.canonicalize().ok())
        .filter(|root| root.parent().is_some())
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
