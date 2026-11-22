use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior, params,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

mod project_icon;

use project_icon::ProjectIcon;

use super::turn_commands::encode_id_part;

const SELECT_PROJECT: &str = "
    SELECT project_id, title, workspace_root, default_model_selection_json,
           default_thread_env_mode, auto_pull, favicon_path, project_icon_json,
           scripts_json, tracked_folders_json, pinned_at, archived_at, created_at,
           updated_at, deleted_at
    FROM projection_projects
    WHERE project_id = ?1";

const SELECT_RECEIPT: &str = "
    SELECT aggregate_kind, aggregate_id, command_type, status, error
    FROM orchestration_command_receipts
    WHERE command_id = ?1";

const WORKSPACE_OWNER: &str = "
    SELECT project_id FROM projection_projects
    WHERE workspace_root = ?1 AND deleted_at IS NULL
    ORDER BY created_at, project_id
    LIMIT 1";

const APPEND_EVENT: &str = "
    INSERT INTO orchestration_events (
        event_id, aggregate_kind, stream_id, stream_version, event_type, occurred_at,
        command_id, causation_event_id, correlation_id, actor_kind, payload_json,
        metadata_json, application_event_version
    ) VALUES (
        ?1, 'project', ?2,
        COALESCE((
            SELECT stream_version + 1 FROM orchestration_events
            WHERE aggregate_kind = 'project' AND stream_id = ?2
            ORDER BY stream_version DESC LIMIT 1
        ), 0),
        ?3, ?4, ?5, NULL, ?5, ?6, ?7, '{}', 2
    ) RETURNING sequence";

const INSERT_PROJECT: &str = "
    INSERT INTO projection_projects (
        project_id, title, workspace_root, scripts_json, created_at, updated_at
    ) VALUES (?1, ?2, ?3, '[]', ?4, ?4)";

const UPDATE_PROJECT: &str = "
    UPDATE projection_projects
    SET title = COALESCE(?1, title),
        has_custom_title = CASE WHEN ?1 IS NULL THEN has_custom_title ELSE 1 END,
        pinned_at = CASE WHEN ?2 THEN ?3 ELSE pinned_at END,
        archived_at = CASE WHEN ?4 THEN ?5 ELSE archived_at END,
        favicon_path = CASE WHEN ?6 THEN ?7 ELSE favicon_path END,
        project_icon_json = CASE WHEN ?8 THEN ?9 ELSE project_icon_json END,
        updated_at = ?10
    WHERE project_id = ?11";

const MARK_PROJECT_DELETED: &str = "
    UPDATE projection_projects
    SET deleted_at = ?2, updated_at = ?2
    WHERE project_id = ?1";

const INSERT_RECEIPT: &str = "
    INSERT INTO orchestration_command_receipts (
        command_id, aggregate_kind, aggregate_id, command_type, accepted_at,
        result_sequence, status, error
    ) VALUES (?1, 'project', ?2, ?3, ?4, ?5, ?6, ?7)";

const APPLICATION_SEQUENCE: &str = "
    SELECT COALESCE(MAX(sequence), 0)
    FROM orchestration_events INDEXED BY idx_orchestration_events_application_high_water
    WHERE aggregate_kind = 'project'
       OR (application_event_version = 2 AND aggregate_kind = 'thread')";

#[derive(Clone)]
pub(super) struct ProjectCommands {
    path: PathBuf,
}

pub(super) struct CommittedProject {
    pub(super) project: Project,
    pub(super) committed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Project {
    id: String,
    title: String,
    workspace_root: String,
    default_model_selection: Option<Value>,
    default_thread_env_mode: Option<String>,
    auto_pull: bool,
    favicon_path: Option<String>,
    project_icon: Option<Value>,
    scripts: Vec<Value>,
    tracked_folders: Vec<Value>,
    pinned_at: Option<String>,
    archived_at: Option<String>,
    created_at: String,
    updated_at: String,
    deleted_at: Option<String>,
}

impl Project {
    pub(super) fn workspace(&self) -> &Path {
        Path::new(&self.workspace_root)
    }
}

#[derive(Debug)]
pub(super) enum CommandError {
    InvalidRequest(String),
    InvalidProject(String),
    Rejected(String),
    ProjectNotFound { project_id: String },
    ProjectNotEmpty { project_id: String },
    Database(rusqlite::Error),
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message)
            | Self::InvalidProject(message)
            | Self::Rejected(message) => formatter.write_str(message),
            Self::ProjectNotFound { project_id } => {
                write!(formatter, "Project {project_id} was not found.")
            }
            Self::ProjectNotEmpty { project_id } => {
                write!(formatter, "Project {project_id} is not empty.")
            }
            Self::Database(_) => formatter.write_str("Could not save the project."),
        }
    }
}

impl Error for CommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for CommandError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Deserialize)]
#[serde(try_from = "String")]
struct NonBlank(String);

impl TryFrom<String> for NonBlank {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let value = value.trim();
        if value.is_empty() {
            return Err("Expected a non-blank string.");
        }
        Ok(Self(value.to_owned()))
    }
}

#[derive(Deserialize)]
#[serde(try_from = "String")]
struct FaviconPath(String);

impl TryFrom<String> for FaviconPath {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let path = super::project_favicon::trim(&value);
        if path.is_empty() {
            return Err("Expected a non-blank project favicon path.");
        }
        Ok(Self(path.to_owned()))
    }
}

#[derive(Deserialize)]
struct ModelSelection {
    #[serde(rename = "instanceId", alias = "provider")]
    _instance_id: NonBlank,
    #[serde(rename = "model")]
    _model: NonBlank,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all_fields = "camelCase", deny_unknown_fields)]
enum ProjectCommand {
    #[serde(rename = "project.create")]
    Create {
        command_id: NonBlank,
        project_id: NonBlank,
        title: NonBlank,
        workspace_root: NonBlank,
        #[serde(default)]
        create_workspace_root_if_missing: bool,
        #[serde(rename = "defaultModelSelection")]
        _default_model_selection: Option<ModelSelection>,
    },
    #[serde(rename = "project.update")]
    Update {
        command_id: NonBlank,
        project_id: NonBlank,
        #[serde(default, deserialize_with = "present")]
        title: Option<NonBlank>,
        #[serde(default, deserialize_with = "present")]
        pinned: Option<bool>,
        #[serde(default, deserialize_with = "present")]
        archived: Option<bool>,
        #[serde(default, deserialize_with = "present")]
        favicon_path: Option<Option<FaviconPath>>,
        #[serde(default, deserialize_with = "present")]
        project_icon: Option<Option<ProjectIcon>>,
    },
    #[serde(rename = "project.delete")]
    Delete {
        command_id: NonBlank,
        project_id: NonBlank,
        #[serde(rename = "force", default, deserialize_with = "present")]
        _force: Option<bool>,
    },
}

fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

impl ProjectCommand {
    fn identity(&self) -> CommandIdentity<'_> {
        let (command_id, project_id, command_type) = match self {
            Self::Create {
                command_id,
                project_id,
                ..
            } => (command_id, project_id, "project.create"),
            Self::Update {
                command_id,
                project_id,
                ..
            } => (command_id, project_id, "project.meta.update"),
            Self::Delete {
                command_id,
                project_id,
                ..
            } => (command_id, project_id, "project.delete"),
        };
        CommandIdentity {
            command_id: &command_id.0,
            project_id: &project_id.0,
            command_type,
        }
    }
}

struct CommandIdentity<'a> {
    command_id: &'a str,
    project_id: &'a str,
    command_type: &'static str,
}

struct Receipt {
    aggregate_kind: String,
    aggregate_id: String,
    command_type: String,
    status: String,
    error: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
enum Rejection {
    #[serde(rename = "ProjectCommandInvariantError")]
    Invariant {
        command_type: String,
        detail: String,
    },
    #[serde(rename = "ProjectCommandMissingProjectError")]
    Missing {
        command_type: String,
        project_id: String,
    },
    #[serde(rename = "ProjectWorkspaceConflictError")]
    WorkspaceConflict {
        workspace_root: String,
        conflicting_project_id: String,
    },
}

impl Rejection {
    fn into_error(self) -> CommandError {
        match self {
            Self::Invariant { detail, .. } => CommandError::InvalidProject(detail),
            Self::Missing {
                command_type,
                project_id,
            } if command_type == "project.delete" => CommandError::ProjectNotFound { project_id },
            Self::Missing { project_id, .. } => {
                CommandError::Rejected(format!("Project '{project_id}' does not exist."))
            }
            Self::WorkspaceConflict {
                workspace_root,
                conflicting_project_id,
            } => CommandError::Rejected(format!(
                "Active project '{conflicting_project_id}' already exists for workspace root '{workspace_root}'."
            )),
        }
    }
}

impl ProjectCommands {
    pub(super) fn open(path: PathBuf) -> rusqlite::Result<Self> {
        let commands = Self { path };
        let connection = commands.connect()?;
        for statement in [
            SELECT_PROJECT,
            SELECT_RECEIPT,
            WORKSPACE_OWNER,
            APPEND_EVENT,
            INSERT_PROJECT,
            UPDATE_PROJECT,
            MARK_PROJECT_DELETED,
            INSERT_RECEIPT,
            APPLICATION_SEQUENCE,
        ] {
            connection.prepare(statement)?;
        }
        Ok(commands)
    }

    pub(super) fn dispatch(&self, payload: Value) -> Result<CommittedProject, CommandError> {
        self.dispatch_with_clock(payload, now_timestamp)
    }

    pub(super) fn has_workspace(&self, workspace: &Path) -> Result<bool, CommandError> {
        Ok(self.workspace_owner(workspace)?.is_some())
    }

    pub(super) fn active_workspace(&self, project_id: &str) -> rusqlite::Result<Option<PathBuf>> {
        self.connect()?
            .query_row(
                "SELECT workspace_root FROM projection_projects
             WHERE project_id = ?1 AND archived_at IS NULL AND deleted_at IS NULL",
                [project_id],
                |row| row.get::<_, String>(0).map(PathBuf::from),
            )
            .optional()
    }

    pub(super) fn workspace_owner(&self, workspace: &Path) -> Result<Option<String>, CommandError> {
        let path = workspace.to_str().ok_or_else(|| {
            CommandError::InvalidRequest(
                "The project folder path contains unsupported characters.".into(),
            )
        })?;
        let workspace = workspace_path(path)?;
        Ok(self
            .connect()?
            .query_row(WORKSPACE_OWNER, [workspace], |row| row.get::<_, String>(0))
            .optional()?)
    }

    fn connect(&self) -> rusqlite::Result<Connection> {
        let connection =
            Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(connection)
    }

    #[cfg(test)]
    fn dispatch_at(
        &self,
        payload: Value,
        occurred_at: &str,
    ) -> Result<CommittedProject, CommandError> {
        self.dispatch_with_clock(payload, || occurred_at.to_owned())
    }

    fn dispatch_with_clock(
        &self,
        payload: Value,
        timestamp: impl FnOnce() -> String,
    ) -> Result<CommittedProject, CommandError> {
        let command: ProjectCommand = serde_json::from_value(payload).map_err(|error| {
            CommandError::InvalidRequest(format!("Invalid project command: {error}"))
        })?;
        let identity = command.identity();
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = read_receipt(&transaction, identity.command_id)? {
            if receipt.aggregate_kind != "project"
                || receipt.aggregate_id != identity.project_id
                || receipt.command_type != identity.command_type
            {
                return Err(CommandError::Rejected(format!(
                    "Command '{}' was already used for another operation.",
                    identity.command_id
                )));
            }
            return match receipt.status.as_str() {
                "accepted" => Ok(CommittedProject {
                    project: read_project(&transaction, identity.project_id)?,
                    committed: false,
                }),
                "rejected" => Err(receipt
                    .error
                    .as_deref()
                    .and_then(|error| serde_json::from_str::<Rejection>(error).ok())
                    .map_or_else(
                        || {
                            CommandError::Rejected(
                                "The command was previously rejected.".to_owned(),
                            )
                        },
                        Rejection::into_error,
                    )),
                _ => Err(rusqlite::Error::InvalidQuery.into()),
            };
        }

        let occurred_at = timestamp();
        let occurred_at = occurred_at.as_str();
        let sequence = match &command {
            ProjectCommand::Create {
                title,
                workspace_root,
                create_workspace_root_if_missing,
                ..
            } => {
                let workspace = workspace_path(&workspace_root.0)?;
                if transaction
                    .query_row(SELECT_PROJECT, [identity.project_id], project_from_row)
                    .optional()?
                    .is_some()
                {
                    let rejection = Rejection::Invariant {
                        command_type: identity.command_type.to_owned(),
                        detail: format!(
                            "Project '{}' already exists and cannot be created twice.",
                            identity.project_id
                        ),
                    };
                    return reject(transaction, &identity, occurred_at, rejection);
                }
                if let Some(owner) = transaction
                    .query_row(WORKSPACE_OWNER, [&workspace], |row| row.get(0))
                    .optional()?
                {
                    let rejection = Rejection::WorkspaceConflict {
                        workspace_root: workspace,
                        conflicting_project_id: owner,
                    };
                    return reject(transaction, &identity, occurred_at, rejection);
                }
                ensure_workspace(Path::new(&workspace), *create_workspace_root_if_missing)?;
                let sequence = append_event(
                    &transaction,
                    &identity,
                    "project.created",
                    occurred_at,
                    json!({
                        "projectId": identity.project_id,
                        "title": title.0,
                        "workspaceRoot": workspace,
                        "defaultModelSelection": null,
                        "faviconPath": null,
                        "projectIcon": null,
                        "scripts": [],
                        "createdAt": occurred_at,
                        "updatedAt": occurred_at,
                    }),
                )?;
                transaction.execute(
                    INSERT_PROJECT,
                    params![identity.project_id, title.0, workspace, occurred_at],
                )?;
                sequence
            }
            ProjectCommand::Update {
                title,
                pinned,
                archived,
                favicon_path,
                project_icon,
                ..
            } => {
                let Some(project) = transaction
                    .query_row(SELECT_PROJECT, [identity.project_id], project_from_row)
                    .optional()?
                    .filter(|project| project.deleted_at.is_none())
                else {
                    return Err(CommandError::Rejected(format!(
                        "Project '{}' does not exist.",
                        identity.project_id
                    )));
                };
                if project_icon
                    .as_ref()
                    .and_then(Option::as_ref)
                    .is_some_and(ProjectIcon::exceeds_monogram_limit)
                {
                    return reject(
                        transaction,
                        &identity,
                        occurred_at,
                        Rejection::Invariant {
                            command_type: identity.command_type.to_owned(),
                            detail: "Project monograms must contain at most two characters."
                                .to_owned(),
                        },
                    );
                }
                let title = title.as_ref().map(|title| title.0.as_str());
                let pinned_at = flag_timestamp(*pinned, project.pinned_at.as_deref(), occurred_at);
                let archived_at =
                    flag_timestamp(*archived, project.archived_at.as_deref(), occurred_at);
                let favicon = favicon_path
                    .as_ref()
                    .map(|path| path.as_ref().map(|path| path.0.as_str()));
                let icon = project_icon
                    .as_ref()
                    .map(|icon| icon.as_ref().map(|icon| json!(icon)));
                let mut payload =
                    json!({"projectId": identity.project_id, "updatedAt": occurred_at});
                if let Some(title) = title {
                    payload["title"] = json!(title);
                }
                if let Some(pinned_at) = pinned_at {
                    payload["pinnedAt"] = json!(pinned_at);
                }
                if let Some(archived_at) = archived_at {
                    payload["archivedAt"] = json!(archived_at);
                }
                if let Some(favicon) = favicon {
                    payload["faviconPath"] = json!(favicon);
                }
                if let Some(icon) = &icon {
                    payload["projectIcon"] = json!(icon);
                }
                let sequence = append_event(
                    &transaction,
                    &identity,
                    "project.meta-updated",
                    occurred_at,
                    payload,
                )?;
                transaction.execute(
                    UPDATE_PROJECT,
                    params![
                        title,
                        pinned.is_some(),
                        pinned_at.flatten(),
                        archived.is_some(),
                        archived_at.flatten(),
                        favicon.is_some(),
                        favicon.flatten(),
                        icon.is_some(),
                        icon.flatten().map(|icon| icon.to_string()),
                        occurred_at,
                        identity.project_id,
                    ],
                )?;
                sequence
            }
            ProjectCommand::Delete { .. } => {
                let project = transaction
                    .query_row(
                        "SELECT deleted_at FROM projection_projects WHERE project_id = ?1",
                        [identity.project_id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()?;
                let Some(deleted_at) = project else {
                    return Err(CommandError::ProjectNotFound {
                        project_id: identity.project_id.to_owned(),
                    });
                };
                if deleted_at.is_some() {
                    let rejection = Rejection::Missing {
                        command_type: identity.command_type.to_owned(),
                        project_id: identity.project_id.to_owned(),
                    };
                    return reject(transaction, &identity, occurred_at, rejection);
                }
                let has_threads = transaction.query_row(
                    "SELECT EXISTS (
                        SELECT 1 FROM orchestration_v2_projection_threads
                        WHERE project_id = ?1 AND deleted_at IS NULL
                    )",
                    [identity.project_id],
                    |row| row.get::<_, bool>(0),
                )?;
                if has_threads {
                    return Err(CommandError::ProjectNotEmpty {
                        project_id: identity.project_id.to_owned(),
                    });
                }
                let sequence = append_event(
                    &transaction,
                    &identity,
                    "project.deleted",
                    occurred_at,
                    json!({"projectId": identity.project_id, "deletedAt": occurred_at}),
                )?;
                transaction.execute(
                    MARK_PROJECT_DELETED,
                    params![identity.project_id, occurred_at],
                )?;
                sequence
            }
        };
        transaction.execute(
            INSERT_RECEIPT,
            params![
                identity.command_id,
                identity.project_id,
                identity.command_type,
                occurred_at,
                sequence,
                "accepted",
                Option::<String>::None,
            ],
        )?;
        let project = read_project(&transaction, identity.project_id)?;
        transaction.commit()?;
        Ok(CommittedProject {
            project,
            committed: true,
        })
    }
}

fn now_timestamp() -> String {
    let now = OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
    )
}

fn flag_timestamp<'a>(
    flag: Option<bool>,
    previous: Option<&'a str>,
    occurred_at: &'a str,
) -> Option<Option<&'a str>> {
    flag.map(|enabled| enabled.then_some(previous.unwrap_or(occurred_at)))
}

fn read_receipt(
    transaction: &Transaction<'_>,
    command_id: &str,
) -> rusqlite::Result<Option<Receipt>> {
    transaction
        .query_row(SELECT_RECEIPT, [command_id], |row| {
            Ok(Receipt {
                aggregate_kind: row.get(0)?,
                aggregate_id: row.get(1)?,
                command_type: row.get(2)?,
                status: row.get(3)?,
                error: row.get(4)?,
            })
        })
        .optional()
}

fn read_project(transaction: &Transaction<'_>, project_id: &str) -> rusqlite::Result<Project> {
    transaction.query_row(SELECT_PROJECT, [project_id], project_from_row)
}

fn project_from_row(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        title: row.get(1)?,
        workspace_root: row.get(2)?,
        default_model_selection: json_column(row, 3)?,
        default_thread_env_mode: row.get(4)?,
        auto_pull: row.get(5)?,
        favicon_path: row.get(6)?,
        project_icon: json_column(row, 7)?,
        scripts: json_column(row, 8)?,
        tracked_folders: json_column(row, 9)?,
        pinned_at: row.get(10)?,
        archived_at: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
        deleted_at: row.get(14)?,
    })
}

fn json_column<T: DeserializeOwned>(row: &Row<'_>, index: usize) -> rusqlite::Result<T> {
    let value = row.get::<_, Option<String>>(index)?;
    serde_json::from_str(value.as_deref().unwrap_or("null")).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn append_event(
    transaction: &Transaction<'_>,
    identity: &CommandIdentity<'_>,
    event_type: &str,
    occurred_at: &str,
    payload: Value,
) -> rusqlite::Result<u64> {
    let actor_kind = if identity.command_id.starts_with("provider:") {
        "provider"
    } else if identity.command_id.starts_with("server:") {
        "server"
    } else {
        "client"
    };
    transaction.query_row(
        APPEND_EVENT,
        params![
            format!(
                "event:command:{}:{}",
                encode_id_part(identity.command_id),
                Uuid::new_v4()
            ),
            identity.project_id,
            event_type,
            occurred_at,
            identity.command_id,
            actor_kind,
            payload.to_string(),
        ],
        |row| row.get(0),
    )
}

fn reject(
    transaction: Transaction<'_>,
    identity: &CommandIdentity<'_>,
    occurred_at: &str,
    rejection: Rejection,
) -> Result<CommittedProject, CommandError> {
    let sequence = transaction.query_row(APPLICATION_SEQUENCE, [], |row| row.get::<_, u64>(0))?;
    let stored = serde_json::to_string(&rejection)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    transaction.execute(
        INSERT_RECEIPT,
        params![
            identity.command_id,
            identity.project_id,
            identity.command_type,
            occurred_at,
            sequence,
            "rejected",
            stored,
        ],
    )?;
    transaction.commit()?;
    Err(rejection.into_error())
}

pub(super) fn workspace_path(input: &str) -> Result<String, CommandError> {
    #[cfg(windows)]
    if is_wsl_share(input) {
        return Err(CommandError::InvalidRequest(format!(
            "{input} is inside WSL. Add it to the WSL backend instead of the Windows one."
        )));
    }
    let path = if input == "~" || input.starts_with("~/") || input.starts_with("~\\") {
        let home = std::env::home_dir().ok_or_else(|| {
            CommandError::InvalidRequest("Home folder is unavailable.".to_owned())
        })?;
        if input.len() > 1 {
            home.join(input[2..].trim_start_matches(std::path::is_separator))
        } else {
            home
        }
    } else {
        PathBuf::from(input)
    };
    let absolute = std::path::absolute(path).map_err(|error| workspace_error(input, error))?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        if component == Component::ParentDir {
            normalized.pop();
        } else {
            normalized.push(component.as_os_str());
        }
    }
    normalized.into_os_string().into_string().map_err(|_| {
        CommandError::InvalidRequest(
            "The project folder path contains unsupported characters.".to_owned(),
        )
    })
}

#[cfg(windows)]
fn is_wsl_share(input: &str) -> bool {
    let path = input.replace('/', "\\").to_ascii_lowercase();
    let host = path
        .strip_prefix("\\\\?\\unc\\")
        .or_else(|| path.strip_prefix("\\\\.\\unc\\"))
        .or_else(|| path.strip_prefix("\\\\"));
    host.is_some_and(|host| host.starts_with("wsl.localhost\\") || host.starts_with("wsl$\\"))
}

fn ensure_workspace(path: &Path, create: bool) -> Result<(), CommandError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(CommandError::InvalidRequest(format!(
            "'{}' is not a folder.",
            path.display()
        ))),
        Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|error| workspace_error(&path.display().to_string(), error))
        }
        Err(error) => Err(workspace_error(&path.display().to_string(), error)),
    }
}

fn workspace_error(input: &str, error: std::io::Error) -> CommandError {
    CommandError::InvalidRequest(format!("Could not open project folder '{input}': {error}"))
}

#[cfg(test)]
mod tests;
