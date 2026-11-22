mod execute;
mod input;
mod persist;
mod release;
mod reservation;
mod retry;
mod setup_progress;
mod setup_script;
mod state;
mod workspace;
mod worktree;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub(super) use self::execute::launch;
pub(super) use self::input::LaunchInput;
use self::input::{Trimmed, WorkspaceStrategy};
pub(super) use self::reservation::{cancel_worktree_setup, request_worktree_cancellation};
pub(super) use self::retry::{COMMAND as RETRY_COMMAND, RetryInput, retry_preparation};
pub(super) use self::setup_progress::subscribe_worktree_setup;
use super::settings::{SettingsFile, SettingsSnapshot};
use super::terminal::TerminalManager;
use super::turn_commands::{NativeProvider, encode_id_part, supports_plain_claude_config};

pub(super) struct LaunchContext<'a> {
    pub(super) database: &'a Path,
    pub(super) scratch_root: &'a Path,
    pub(super) settings_path: Option<&'a Path>,
    pub(super) provider_enabled: bool,
    pub(super) terminals: Option<Arc<TerminalManager>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LaunchError {
    #[serde(rename = "_tag")]
    tag: &'static str,
    command_id: String,
    project_id: String,
    message: String,
    cause: Box<LaunchCause>,
}

#[derive(Debug, Serialize)]
struct LaunchCause {
    name: &'static str,
    #[serde(skip)]
    operation: Option<&'static str>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<Value>,
}

impl fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.cause.message)?;
        if let Some(message) = self
            .cause
            .cause
            .as_ref()
            .and_then(|detail| detail.as_str().or_else(|| detail["message"].as_str()))
        {
            write!(formatter, " {message}")?;
        }
        Ok(())
    }
}

impl LaunchError {
    fn preparation_failure_message(&self) -> String {
        let message = if let Some(operation) = self.cause.operation {
            let operation = operation.replace('-', " ");
            let detail = self
                .cause
                .cause
                .as_ref()
                .and_then(Value::as_str)
                .unwrap_or(&self.cause.message);
            format!("Workspace preparation failed during {operation}: {detail}")
        } else {
            format!("Workspace preparation failed: {}", self.cause.message)
        };
        let clean = message
            .chars()
            .map(|character| match character {
                '\u{0}'..='\u{8}' | '\u{b}'..='\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}' => ' ',
                _ => character,
            })
            .collect::<String>();
        let clean = clean.trim();
        if clean.encode_utf16().count() <= 4096 {
            return clean.to_owned();
        }
        let mut truncated = String::new();
        let mut remaining = 4095_usize;
        for character in clean.chars() {
            let Some(next) = remaining.checked_sub(character.len_utf16()) else {
                break;
            };
            remaining = next;
            truncated.push(character);
        }
        truncated.push('…');
        truncated
    }
}

impl std::error::Error for LaunchError {}

impl LaunchInput {
    pub(super) fn provider_project(&self) -> (&str, &str) {
        (&self.project_id.0, &self.model_selection.instance_id)
    }

    fn native_provider(&self) -> Result<NativeProvider, LaunchError> {
        match self.model_selection.instance_id.as_str() {
            "codex" => Ok(NativeProvider::Codex),
            "claudeAgent"
                if self.model_selection.model.0 == "claude-sonnet-4-6"
                    && self.model_selection.without_options()
                    && matches!(self.runtime_mode, input::RuntimeMode::FullAccess)
                    && matches!(self.interaction_mode, input::InteractionMode::Default)
                    && matches!(self.workspace_strategy, WorkspaceStrategy::Root { .. })
                    && self.reuse_existing_thread != Some(true)
                    && self.initial_message.is_some() =>
            {
                Ok(NativeProvider::Claude)
            }
            "claudeAgent" => Err(self.fail(
                "create-thread",
                "A Claude Code launch requires a new root thread, a plain Sonnet 4.6 prompt, and Full access mode.",
            )),
            _ => Err(self.fail("create-thread", "This launch requires a supported built-in provider.")),
        }
    }

    fn validate_claude_config(
        &self,
        path: &Path,
        operation: &'static str,
    ) -> Result<(), LaunchError> {
        let snapshot = SettingsFile::new(path.to_owned())
            .read()
            .map_err(|cause| self.fail(operation, cause))?;
        let Some(config) = snapshot.claude_instance(Some(&self.project_id.0), "claudeAgent") else {
            return Err(self.fail(operation, self.provider_disabled().message));
        };
        if !supports_plain_claude_config(&config, &self.model_selection.model.0) {
            return Err(self.fail(
                operation,
                "This Claude Code configuration is unavailable for plain text launches.",
            ));
        }
        Ok(())
    }

    fn fail(&self, operation: &'static str, cause: impl ToString) -> LaunchError {
        self.fail_with_defect(operation, Value::String(cause.to_string()))
    }

    fn fail_with_defect(&self, operation: &'static str, cause: Value) -> LaunchError {
        self.error(
            "Failed to launch thread".to_owned(),
            LaunchCause {
                name: "ThreadLaunchError",
                operation: Some(operation),
                message: format!(
                    "Thread launch {} failed during {operation}.",
                    self.command_id.0
                ),
                cause: Some(cause),
            },
        )
    }

    fn provider_disabled(&self) -> LaunchError {
        let message = format!(
            "Provider instance \"{}\" is disabled for project {}.",
            self.model_selection.instance_id, self.project_id.0
        );
        self.error(
            message.clone(),
            LaunchCause {
                name: "ThreadManagementProviderInstanceDisabledError",
                operation: None,
                message,
                cause: None,
            },
        )
    }

    fn cancelled(&self) -> LaunchError {
        let message = "Worktree setup cancelled.".to_owned();
        self.error(
            message.clone(),
            LaunchCause {
                name: "WorktreeSetupCancelled",
                operation: None,
                message,
                cause: None,
            },
        )
    }

    fn preflight_error(&self, name: &'static str, message: String) -> LaunchError {
        self.error(
            message.clone(),
            LaunchCause {
                name,
                operation: None,
                message,
                cause: None,
            },
        )
    }

    fn error(&self, message: String, cause: LaunchCause) -> LaunchError {
        LaunchError {
            tag: "OrchestrationV2ThreadLaunchError",
            command_id: self.command_id.0.clone(),
            project_id: self.project_id.0.clone(),
            message,
            cause: Box::new(cause),
        }
    }
}

pub(super) fn provider_enabled(settings: &SettingsSnapshot, project: &str, instance: &str) -> bool {
    if instance == "claudeAgent" {
        settings.claude_instance(Some(project), instance).is_some()
    } else {
        settings.codex_instance(Some(project), instance).is_ok()
    }
}

async fn branch_for(strategy: &WorkspaceStrategy, workspace_root: &Path) -> Option<String> {
    match strategy {
        WorkspaceStrategy::Root { branch: None } => workspace::current_branch(workspace_root).await,
        WorkspaceStrategy::Root { branch }
        | WorkspaceStrategy::ExistingWorktree { branch, .. }
        | WorkspaceStrategy::Worktree { branch, .. } => {
            branch.as_ref().map(|branch| branch.0.clone())
        }
    }
}

pub(super) struct PreparedLaunch {
    input: LaunchInput,
    provider: NativeProvider,
    database: PathBuf,
    settings_path: Option<PathBuf>,
    terminals: Option<Arc<TerminalManager>>,
    default_worktrees_root: PathBuf,
    pub(super) thread_id: String,
    pub(super) workspace_root: PathBuf,
    branch: Option<String>,
    initial_message_id: Option<String>,
    had_launch_receipt: bool,
    reused_worktree: Option<WorktreeBinding>,
}

struct WorktreeBinding {
    path: Trimmed,
    branch: Trimmed,
}

impl WorktreeBinding {
    fn matches(&self, thread: &state::Thread) -> bool {
        thread.worktree_path.as_deref() == Some(Path::new(&self.path.0))
            && thread.branch.as_deref() == Some(self.branch.0.as_str())
    }
}

pub(super) struct DispatchCommit {
    pub(super) sequence: u64,
    pub(super) replayed: bool,
    pub(super) thread_id: Option<String>,
    pub(super) run_id: Option<String>,
}

pub(super) enum AfterClaim {
    Complete(LaunchCommit),
    Dispatch(InitialDispatch),
}

pub(super) struct InitialDispatch {
    pub(super) command: Value,
    thread_id: String,
    had_receipt: bool,
}

pub(super) struct LaunchCommit {
    pub(super) thread_id: String,
    pub(super) run_id: Option<String>,
    pub(super) schedule_preparation: bool,
    sequence: Option<u64>,
    resumed: bool,
}

struct PendingWorkspace {
    workspace_root: PathBuf,
    strategy: WorkspaceStrategy,
    checkout: Option<PendingCheckout>,
}

enum PendingCheckout {
    Provision {
        parent: PathBuf,
    },
    FinalizeBinding {
        requested: WorkspaceStrategy,
        path: PathBuf,
        original_branch: String,
    },
    ReuseBinding {
        base_ref: Trimmed,
    },
}

pub(super) struct WorkspacePreparation {
    newly_claimed_checkout: bool,
    pub(super) workspace_root: PathBuf,
    pub(super) metadata_command: Value,
    pub(super) release_command: Option<Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LaunchResult {
    thread_id: String,
    projection: Value,
    resumed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    sequence: Option<u64>,
}

pub(super) async fn prepare(
    input: LaunchInput,
    context: LaunchContext<'_>,
) -> Result<PreparedLaunch, LaunchError> {
    if input
        .initial_message
        .as_ref()
        .is_some_and(|message| !message.attachments.is_empty() || message.context.is_some())
    {
        return Err(input.fail(
            "dispatch-message",
            "Attachments and composer context require launch attachment intake.",
        ));
    }
    let database = context.database.to_owned();
    let reading_database = database.clone();
    let scratch_root = context.scratch_root.to_owned();
    let root_for_read = scratch_root.clone();
    let settings_path = context.settings_path.map(Path::to_owned);
    let settings_for_read = settings_path.clone();
    let terminals = context.terminals;
    let provider_enabled = context.provider_enabled;
    let read_error = input.fail("resolve-project", "The launch state read was interrupted.");
    let (
        mut input,
        ResolvedLaunch {
            workspace_root,
            project_title,
            thread_id,
            provider,
            had_launch_receipt,
            managed_scratch,
        },
    ) = tokio::task::spawn_blocking(move || {
        let resolved = resolve_launch(
            &input,
            &LaunchContext {
                database: &reading_database,
                scratch_root: &root_for_read,
                settings_path: settings_for_read.as_deref(),
                provider_enabled,
                terminals: None,
            },
        )?;
        Ok::<_, LaunchError>((input, resolved))
    })
    .await
    .map_err(|_| read_error)??;
    let scratch = if let Some(base_dir) = scratch_root.parent().filter(|_| managed_scratch) {
        match super::git_checkpoint::is_workspace(base_dir).await {
            Ok(is_workspace) => !is_workspace,
            Err(super::git_checkpoint::CheckpointError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                true
            }
            Err(_) => false,
        }
    } else {
        false
    };
    if scratch && matches!(input.workspace_strategy, WorkspaceStrategy::Worktree { .. }) {
        input.workspace_strategy = WorkspaceStrategy::Root { branch: None };
    }
    let claim_scratch = !had_launch_receipt
        && scratch
        && matches!(input.workspace_strategy, WorkspaceStrategy::Root { .. });
    if !had_launch_receipt
        && (claim_scratch || !matches!(input.workspace_strategy, WorkspaceStrategy::Root { .. }))
    {
        let candidate_database = database.clone();
        let candidate_settings = settings_path.clone();
        let project_id = input.project_id.0.clone();
        let scripts_supported = terminals.is_some()
            && matches!(input.workspace_strategy, WorkspaceStrategy::Worktree { .. });
        let operation = "provision-worktree";
        let interrupted = input.fail(operation, "Worktree setup settings could not be read.");
        let result = tokio::task::spawn_blocking(move || {
            let snapshot = state::open(&candidate_database).map_err(|error| error.to_string())?;
            workspace::check_setup(
                &snapshot,
                candidate_settings.as_deref(),
                &project_id,
                scripts_supported,
            )
        })
        .await
        .map_err(|_| interrupted)?;
        result.map_err(|detail| input.fail(operation, detail))?;
    }
    if claim_scratch {
        let reason = input.fail(
            "provision-worktree",
            "Failed to create the folder for threads without a project.",
        );
        let candidate = thread_id.clone();
        let root = workspace_root.clone();
        let text = input
            .initial_message
            .as_ref()
            .map_or_else(|| input.title.0.clone(), |message| message.text.clone());
        let path = tokio::task::spawn_blocking(move || {
            workspace::claim_scratch_folder(&root, &candidate, &text)
        })
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|path| path.into_os_string().into_string().ok())
        .ok_or(reason)?;
        input.workspace_strategy = WorkspaceStrategy::ExistingWorktree {
            worktree_path: Trimmed(path),
            branch: None,
        };
    }
    worktree::preflight(
        &input,
        &input.workspace_strategy,
        &workspace_root,
        &project_title,
        had_launch_receipt,
    )
    .await?;
    let branch = branch_for(&input.workspace_strategy, &workspace_root).await;
    let initial_message_id = input.initial_message.as_ref().map(|message| {
        message
            .message_id
            .as_ref()
            .map(|id| id.0.clone())
            .unwrap_or_else(|| {
                format!(
                    "message:thread:{}:ordinal:1:{}",
                    encode_id_part(&thread_id),
                    Uuid::new_v4()
                )
            })
    });
    Ok(PreparedLaunch {
        input,
        provider,
        database,
        settings_path,
        terminals,
        default_worktrees_root: scratch_root.with_file_name("worktrees"),
        thread_id,
        workspace_root,
        branch,
        initial_message_id,
        had_launch_receipt,
        reused_worktree: None,
    })
}

struct ResolvedLaunch {
    workspace_root: PathBuf,
    project_title: String,
    thread_id: String,
    provider: NativeProvider,
    had_launch_receipt: bool,
    managed_scratch: bool,
}

fn resolve_launch(
    input: &LaunchInput,
    context: &LaunchContext<'_>,
) -> Result<ResolvedLaunch, LaunchError> {
    let mut connection =
        state::open(context.database).map_err(|cause| input.fail("resolve-project", cause))?;
    let snapshot = connection
        .transaction()
        .map_err(|cause| input.fail("resolve-project", cause))?;
    let project_id = &input.project_id.0;
    let workspace_root = state::project_workspace(&snapshot, project_id)
        .map_err(|cause| input.fail("resolve-project", cause))?
        .ok_or_else(|| input.fail("resolve-project", "Project not found."))?;
    let reuse = input.reuse_existing_thread == Some(true);
    if reuse && input.thread_id.is_none() {
        return Err(input.fail(
            "update-thread",
            "Reusing an existing thread requires a thread id.",
        ));
    }
    let receipt = state::receipt(&snapshot, &input.command_id.0)
        .map_err(|cause| input.fail("read-receipt", cause))?;
    let provider = input.native_provider()?;
    if receipt.is_none() && !context.provider_enabled {
        return Err(input.provider_disabled());
    }
    if provider == NativeProvider::Claude
        && receipt.is_none()
        && let Some(path) = context.settings_path
    {
        input.validate_claude_config(path, "create-thread")?;
    }
    let recovered = receipt.as_ref().filter(|receipt| {
        input.thread_id.is_none() && receipt.accepted && receipt.command_type == "thread.create"
    });
    let candidate = input
        .thread_id
        .as_ref()
        .map(|thread_id| thread_id.0.clone())
        .or_else(|| recovered.map(|receipt| receipt.thread_id.clone()))
        .unwrap_or_else(|| {
            format!(
                "thread:project:{}:{}",
                encode_id_part(project_id),
                Uuid::new_v4()
            )
        });
    if recovered.is_some() || (reuse && receipt.is_none()) {
        let operation = if recovered.is_some() {
            "create-thread"
        } else {
            "update-thread"
        };
        let existing = state::thread(&snapshot, &candidate)
            .map_err(|cause| input.fail(operation, cause))?
            .ok_or_else(|| input.fail(operation, "Thread not found."))?;
        if recovered.is_some() && existing.project_id != *project_id {
            return Err(input.fail("resolve-project", "Project identity changed."));
        }
        if recovered.is_none()
            && (existing.project_id != *project_id || !existing.active || !existing.empty)
        {
            return Err(input.fail(
                "update-thread",
                "Only an empty active thread in the target project can change workspace during launch.",
            ));
        }
    }
    let managed_scratch =
        workspace::is_scratch_project(&snapshot, &workspace_root, context.scratch_root)
            .map_err(|cause| input.fail("resolve-project", cause))?;
    if provider == NativeProvider::Claude && receipt.is_none() {
        let ordinary_root: bool = snapshot
            .query_row(
                "SELECT archived_at IS NULL AND COALESCE(json_array_length(tracked_folders_json), 0) = 0
                 FROM projection_projects WHERE project_id = ?1",
                [project_id],
                |row| row.get(0),
            )
            .map_err(|cause| input.fail("resolve-project", cause))?;
        if managed_scratch || !ordinary_root {
            return Err(input.fail(
                "resolve-project",
                "Claude Code requires an active project root without tracked folders.",
            ));
        }
    }
    let project_title = snapshot
        .query_row(
            "SELECT title FROM projection_projects WHERE project_id=?1",
            [project_id],
            |row| row.get(0),
        )
        .map_err(|cause| input.fail("resolve-project", cause))?;
    Ok(ResolvedLaunch {
        workspace_root,
        project_title,
        thread_id: candidate,
        provider,
        had_launch_receipt: receipt.is_some(),
        managed_scratch,
    })
}

impl PreparedLaunch {
    pub(super) fn persist_initial_claim(
        &self,
        provider_enabled: bool,
    ) -> Result<DispatchCommit, LaunchError> {
        persist::initial_claim(self, provider_enabled)
    }

    pub(super) fn claim_command(&self) -> Value {
        let input = &self.input;
        if input.reuse_existing_thread == Some(true) {
            return json!({
                "type": "thread.metadata.update",
                "commandId": input.command_id,
                "threadId": self.thread_id,
                "expectedEmpty": true
            });
        }
        let mut command = json!({
            "type": "thread.create",
            "commandId": input.command_id,
            "threadId": self.thread_id,
            "projectId": input.project_id,
            "title": input.title,
            "modelSelection": input.model_selection,
            "runtimeMode": input.runtime_mode,
            "interactionMode": input.interaction_mode,
            "branch": self.branch,
            "worktreePath": input.workspace_strategy.worktree_path(),
            "createdBy": "user",
            "creationSource": input.creation_source.unwrap_or_default()
        });
        if let Some(mut initial) = self.initial_message() {
            initial["workspaceStrategy"] = self.workspace_strategy();
            command["initialMessage"] = initial;
        }
        command
    }

    pub(super) fn after_claim(&self, claim: DispatchCommit) -> Result<AfterClaim, LaunchError> {
        let thread_id = claim.thread_id.unwrap_or_else(|| self.thread_id.clone());
        let database = state::open(&self.database)
            .map_err(|cause| self.input.fail("resolve-project", cause))?;
        let thread = state::thread(&database, &thread_id)
            .map_err(|cause| self.input.fail("resolve-project", cause))?;
        if !thread
            .as_ref()
            .is_some_and(|thread| thread.project_id == self.input.project_id.0)
        {
            return Err(self
                .input
                .fail("resolve-project", "Project identity changed."));
        }
        let Some(mut message) = self.initial_message() else {
            return Ok(AfterClaim::Complete(LaunchCommit {
                thread_id,
                run_id: None,
                sequence: None,
                resumed: self.had_launch_receipt || claim.replayed,
                schedule_preparation: !self.had_launch_receipt,
            }));
        };
        if let Some(run_id) = claim.run_id {
            return Ok(AfterClaim::Complete(LaunchCommit {
                thread_id,
                run_id: Some(run_id),
                sequence: Some(claim.sequence),
                resumed: claim.replayed,
                schedule_preparation: true,
            }));
        }
        let message_command_id = format!("{}:initial-message", self.input.command_id.0);
        let had_receipt = state::receipt(&database, &message_command_id)
            .map_err(|cause| self.input.fail("read-receipt", cause))?
            .is_some();
        message["type"] = json!("message.dispatch");
        message["commandId"] = json!(message_command_id);
        message["threadId"] = json!(thread_id);
        message["modelSelection"] = json!(self.input.model_selection);
        message["createdBy"] = json!("user");
        message["creationSource"] = json!(self.input.creation_source.unwrap_or_default());
        message["dispatchMode"] = json!({
            "type": "defer_start",
            "workspaceStrategy": self.workspace_strategy(),
        });
        Ok(AfterClaim::Dispatch(InitialDispatch {
            command: message,
            thread_id,
            had_receipt,
        }))
    }

    pub(super) fn after_initial_message(
        &self,
        dispatch: InitialDispatch,
        commit: DispatchCommit,
    ) -> Result<LaunchCommit, LaunchError> {
        let run_id = commit.run_id.ok_or_else(|| {
            self.input.fail(
                "dispatch-message",
                "Initial message was accepted without a durable run.",
            )
        })?;
        Ok(LaunchCommit {
            thread_id: dispatch.thread_id,
            run_id: Some(run_id),
            sequence: Some(commit.sequence),
            resumed: dispatch.had_receipt || commit.replayed,
            schedule_preparation: true,
        })
    }

    #[cfg(test)]
    pub(super) async fn prepare_workspace(
        &self,
        commit: &LaunchCommit,
    ) -> Result<Option<WorkspacePreparation>, LaunchError> {
        if !commit.schedule_preparation {
            return Ok(None);
        }
        let Some(workspace_root) = self.pending_workspace(commit)? else {
            return Ok(None);
        };
        Ok(Some(
            self.workspace_preparation(commit, workspace_root).await,
        ))
    }

    async fn workspace_preparation(
        &self,
        commit: &LaunchCommit,
        pending: PendingWorkspace,
    ) -> WorkspacePreparation {
        let workspace_root = pending.workspace_root;
        let branch = branch_for(&pending.strategy, &workspace_root).await;
        WorkspacePreparation {
            newly_claimed_checkout: false,
            workspace_root,
            metadata_command: json!({
                "type": "thread.metadata.update",
                "commandId": format!("{}:workspace", self.input.command_id.0),
                "threadId": commit.thread_id,
                "branch": branch,
                "worktreePath": pending.strategy.worktree_path()
            }),
            release_command: self.release_command(commit),
        }
    }

    fn pending_workspace(
        &self,
        commit: &LaunchCommit,
    ) -> Result<Option<PendingWorkspace>, LaunchError> {
        let operation = "update-thread";
        let mut database = state::open(&self.database)
            .map_err(|cause| self.input.fail("resolve-project", cause))?;
        let snapshot = database
            .transaction()
            .map_err(|cause| self.input.fail("resolve-project", cause))?;
        let mut strategy = self.input.workspace_strategy.clone();
        if let Some(run_id) = &commit.run_id {
            let Some(accepted) = state::preparing_strategy(&snapshot, &commit.thread_id, run_id)
                .map_err(|cause| self.input.fail(operation, cause))?
            else {
                return Ok(None);
            };
            if self.had_launch_receipt {
                if let Some(
                    accepted @ (WorkspaceStrategy::Worktree { .. }
                    | WorkspaceStrategy::ExistingWorktree { .. }),
                ) = accepted
                {
                    strategy = accepted;
                } else if let WorkspaceStrategy::Root { ref branch } = strategy
                    && let Some(bound_path) = state::thread(&snapshot, &commit.thread_id)
                        .map_err(|cause| self.input.fail(operation, cause))?
                        .and_then(|thread| thread.worktree_path)
                {
                    let worktree_path =
                        bound_path.into_os_string().into_string().map_err(|_| {
                            self.input
                                .fail(operation, "Thread workspace path is not Unicode.")
                        })?;
                    strategy = WorkspaceStrategy::ExistingWorktree {
                        worktree_path: Trimmed(worktree_path),
                        branch: branch.clone(),
                    };
                }
            }
        }
        let workspace_root = state::project_workspace(&snapshot, &self.input.project_id.0)
            .map_err(|cause| self.input.fail("resolve-project", cause))?
            .ok_or_else(|| {
                self.input
                    .fail("resolve-project", "Project no longer exists.")
            })?;

        let mut checkout = None;
        if let Some(reused) = &self.reused_worktree
            && let WorkspaceStrategy::Worktree { base_ref, .. } = &strategy
        {
            let current = state::thread(&snapshot, &commit.thread_id)
                .map_err(|cause| self.input.fail(operation, cause))?;
            if !current.is_some_and(|thread| thread.active && reused.matches(&thread)) {
                return Err(self
                    .input
                    .fail(operation, "The recorded worktree changed before setup."));
            }
            checkout = Some(PendingCheckout::ReuseBinding {
                base_ref: base_ref.clone(),
            });
            strategy = WorkspaceStrategy::ExistingWorktree {
                worktree_path: reused.path.clone(),
                branch: Some(reused.branch.clone()),
            };
        } else if matches!(strategy, WorkspaceStrategy::Worktree { .. })
            && state::receipt(&snapshot, &format!("{}:workspace", self.input.command_id.0))
                .map_err(|cause| self.input.fail(operation, cause))?
                .is_some_and(|receipt| receipt.accepted && receipt.thread_id == commit.thread_id)
            && let Some(bound) = state::thread(&snapshot, &commit.thread_id)
                .map_err(|cause| self.input.fail(operation, cause))?
            && let Some(path) = bound.worktree_path
            && let Some(branch) = bound.branch
        {
            let original_branch: Option<String> = snapshot.query_row(
                "SELECT json_extract(payload_json,'$.branch') FROM orchestration_events
                 WHERE command_id=?1 AND stream_id=?2 AND event_type='thread.metadata-updated' ORDER BY sequence LIMIT 1",
                rusqlite::params![format!("{}:workspace",self.input.command_id.0),commit.thread_id], |row| row.get(0),
            ).map_err(|cause| self.input.fail(operation,cause))?;
            checkout = Some(PendingCheckout::FinalizeBinding {
                requested: strategy.clone(),
                path: path.clone(),
                original_branch: original_branch.unwrap_or_else(|| branch.clone()),
            });
            strategy = WorkspaceStrategy::ExistingWorktree {
                worktree_path: Trimmed(path.into_os_string().into_string().map_err(|_| {
                    self.input
                        .fail(operation, "Thread workspace path is not Unicode.")
                })?),
                branch: Some(Trimmed(branch)),
            };
        }
        if matches!(strategy, WorkspaceStrategy::Worktree { .. }) {
            let parent = workspace::configured_worktrees_root(
                self.settings_path.as_deref(),
                &self.default_worktrees_root,
            )
            .map_err(|detail| self.input.fail("provision-worktree", detail))?;
            checkout = Some(PendingCheckout::Provision { parent });
        }
        if !matches!(strategy, WorkspaceStrategy::Root { .. }) {
            workspace::check_setup(
                &snapshot,
                self.settings_path.as_deref(),
                &self.input.project_id.0,
                self.terminals.is_some() && checkout.is_some(),
            )
            .map_err(|detail| self.input.fail(operation, detail))?;
        }
        Ok(Some(PendingWorkspace {
            workspace_root,
            strategy,
            checkout,
        }))
    }

    fn automatic_setup(
        &self,
        commit: &LaunchCommit,
    ) -> Result<Option<setup_script::Script>, LaunchError> {
        let operation = "run-setup-script";
        let database =
            state::open(&self.database).map_err(|cause| self.input.fail(operation, cause))?;
        let fenced = database
            .query_row(
                "SELECT json_extract(payload_json,'$.linkOrigin') IS NOT NULL
                 AND json_extract(payload_json,'$.linkOrigin.access') IS NOT 'full-access'
             FROM orchestration_v2_projection_threads WHERE thread_id=?1",
                [&commit.thread_id],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|cause| self.input.fail(operation, cause))?;
        if fenced {
            return Ok(None);
        }
        workspace::configured_setup(
            &database,
            self.settings_path.as_deref(),
            &self.input.project_id.0,
        )
        .map_err(|cause| self.input.fail(operation, cause))
    }

    fn release_command(&self, commit: &LaunchCommit) -> Option<Value> {
        let run_id = commit.run_id.as_ref()?;
        Some(json!({
            "type": "prepared-run.release",
            "commandId": format!("{}:release", self.input.command_id.0),
            "threadId": commit.thread_id,
            "runId": run_id
        }))
    }

    pub(super) fn result(
        &self,
        commit: &LaunchCommit,
        projection: Value,
    ) -> Result<LaunchResult, LaunchError> {
        if projection["thread"]["id"] != commit.thread_id
            || projection["thread"]["projectId"] != self.input.project_id.0
            || commit.run_id.as_ref().is_some_and(|run_id| {
                !projection["runs"]
                    .as_array()
                    .is_some_and(|runs| runs.iter().any(|run| run["id"] == *run_id))
            })
        {
            return Err(self.input.fail(
                "create-thread",
                "The accepted launch projection is unavailable.",
            ));
        }
        Ok(LaunchResult {
            thread_id: commit.thread_id.clone(),
            projection,
            resumed: commit.resumed,
            sequence: commit.sequence,
        })
    }

    fn initial_message(&self) -> Option<Value> {
        let initial = self.input.initial_message.as_ref()?;
        let message_id = self.initial_message_id.as_ref()?;
        let mut message = json!({
            "messageId": message_id,
            "text": initial.text,
            "attachments": initial.attachments
        });
        if self.input.generate_title == Some(true) {
            message["titleSeed"] = json!(self.input.title);
        }
        Some(message)
    }

    fn workspace_strategy(&self) -> Value {
        json!(self.input.workspace_strategy)
    }
}

#[cfg(test)]
mod tests;
