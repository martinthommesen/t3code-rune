use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tokio::sync::broadcast;
use uuid::Uuid;

use super::ProjectionChange;
use super::git_checkpoint::{CheckpointError, read_command};
use super::project_commands::ProjectCommands;

pub(super) enum ScratchError {
    Unavailable {
        base: PathBuf,
        repository: Option<PathBuf>,
    },
    Folder,
    Project,
}

impl ScratchError {
    pub(super) fn payload(self) -> Value {
        let (name, message) = match self {
            Self::Unavailable { base, repository } => {
                let message = if let Some(repository) = &repository {
                    let base = base.display();
                    let repository = repository.display();
                    format!(
                        "Threads without a project are off because the T3 home {base} is inside the Git repository {repository}. If that repository was created by mistake, move {repository}/.git out of the way; otherwise point T3CODE_HOME (or --base-dir) at a folder outside it. Then restart T3 Code."
                    )
                } else {
                    "Threads without a project are not available on this environment.".to_owned()
                };
                ("ScratchUnavailableError", message)
            }
            Self::Folder => (
                "ScratchFolderError",
                "Failed to create the folder for threads without a project.".to_owned(),
            ),
            Self::Project => (
                "ScratchProjectError",
                "Failed to create the project for threads without a project.".to_owned(),
            ),
        };
        json!({"_tag":"OrchestrationDispatchCommandError", "message":message,
               "cause":{"name":name, "message":message}})
    }
}

pub(super) async fn available_root(base: &Path) -> Result<PathBuf, ScratchError> {
    let probe = read_command(base, &["rev-parse", "--show-toplevel"]).await;
    let repository = match probe {
        Ok(output) if output.status.success() => Some(PathBuf::from(output.stdout.trim_end())),
        Ok(output) if output.stderr.starts_with("fatal: not a git repository") => {
            return Ok(base.join("scratch"));
        }
        Err(CheckpointError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(base.join("scratch"));
        }
        _ => None,
    };
    Err(ScratchError::Unavailable {
        base: base.to_owned(),
        repository,
    })
}

pub(super) async fn ensure(
    projects: ProjectCommands,
    base: &Path,
    changes: broadcast::Sender<ProjectionChange>,
) -> Result<String, ScratchError> {
    let folder = available_root(base).await?;
    tokio::task::spawn_blocking(move || ensure_project(&projects, folder, &changes))
        .await
        .map_err(|_| ScratchError::Project)?
}

fn ensure_project(
    projects: &ProjectCommands,
    folder: PathBuf,
    changes: &broadcast::Sender<ProjectionChange>,
) -> Result<String, ScratchError> {
    fs::create_dir_all(&folder).map_err(|_| ScratchError::Folder)?;
    let workspace_root = folder.to_str().ok_or(ScratchError::Project)?;
    if let Ok(target) = fs::canonicalize(&folder)
        && target != folder
        && let Some(id) = projects
            .workspace_owner(&target)
            .map_err(|_| ScratchError::Project)?
    {
        return Ok(id);
    }
    let owner = || {
        projects
            .workspace_owner(&folder)
            .map_err(|_| ScratchError::Project)
    };
    if let Some(id) = owner()? {
        return Ok(id);
    }
    let id = Uuid::new_v4().to_string();
    if projects
        .dispatch(json!({
            "type":"project.create", "commandId":format!("scratch-project:{id}"),
            "projectId":id, "title":"No project", "workspaceRoot":workspace_root,
        }))
        .is_err()
    {
        return owner()?.ok_or(ScratchError::Project);
    }
    let _ = changes.send(ProjectionChange::Project);
    projects
        .dispatch(json!({
            "type":"project.update", "commandId":format!("scratch-project-icon:{id}"),
            "projectId":id,
            "projectIcon":{"kind":"lucide", "name":"message-square-dashed", "color":"gray"},
        }))
        .map_err(|_| ScratchError::Project)?;
    let _ = changes.send(ProjectionChange::Project);
    Ok(id)
}
