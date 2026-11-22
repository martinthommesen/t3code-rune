pub(super) mod abort_effect;
mod capture_effect;
mod checkpoint_events;
pub(super) mod claude;
pub(super) mod context_handoff;
mod finish_effect;
pub(super) mod interrupt_effect;
mod manual_continuation;
mod projection;
pub(super) mod provider_session;
pub(super) mod provider_turn;
pub(super) mod queued_run;
pub(super) mod queued_worktree;
pub(super) mod reasoning;
pub(super) mod runtime_requests;
mod settlement_events;
pub(super) mod start_effect;
pub(super) mod start_failure;
pub(super) mod start_recovery;
pub(super) mod title_effect;

pub(super) mod native_events;
mod todo_list;
pub(super) mod tool_events;
pub(super) mod turn_text;

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use t3_rune::providers::codex::{InteractionMode, RuntimeMode};

const COMMAND_TYPE: &str = "message.dispatch";
const EFFECT_TYPE: &str = "provider-turn.start";
const CODEX_INSTANCE: &str = "codex";
const CLAUDE_INSTANCE: &str = "claudeAgent";
const FIRST_RUN_ORDINAL: i64 = 1;
const TURN_ITEM_ORDINAL_RANGE: i64 = 1_000_000;

#[derive(Clone)]
pub(super) struct TurnCommands {
    path: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(super) struct MessageDispatch {
    #[serde(rename = "type")]
    command_type: DispatchType,
    command_id: String,
    thread_id: String,
    message_id: String,
    created_by: Actor,
    creation_source: CreationSource,
    text: String,
    attachments: Vec<Value>,
    dispatch_mode: DispatchMode,
    delivery_intent: Option<DeliveryIntent>,
    model_selection: Option<Value>,
    title_seed: Option<String>,
    manual_continuation_of_run_id: Option<String>,
}

#[derive(Deserialize)]
enum DispatchType {
    #[serde(rename = "message.dispatch")]
    MessageDispatch,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Actor {
    User,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum CreationSource {
    Web,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "snake_case")]
enum DispatchMode {
    StartImmediately,
    QueueAfterActive,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeliveryIntent {
    Auto,
}

#[derive(Debug)]
pub(super) struct DispatchReceipt {
    pub(super) sequence: i64,
    #[cfg(test)]
    pub(super) run_id: String,
    pub(super) committed: bool,
}

#[derive(Debug)]
pub(super) enum TurnCommandError {
    Unsupported(&'static str),
    Database(rusqlite::Error),
    InvalidProjection(serde_json::Error),
    Checkpoint(super::git_checkpoint::CheckpointError),
}

impl fmt::Display for TurnCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(reason) => formatter.write_str(reason),
            Self::Database(error) => write!(formatter, "Could not record the Codex turn: {error}"),
            Self::InvalidProjection(error) => write!(formatter, "Invalid thread data: {error}"),
            Self::Checkpoint(error) => error.fmt(formatter),
        }
    }
}

impl Error for TurnCommandError {}

impl From<rusqlite::Error> for TurnCommandError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

impl From<serde_json::Error> for TurnCommandError {
    fn from(error: serde_json::Error) -> Self {
        Self::InvalidProjection(error)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadProjection {
    model_selection: Value,
    runtime_mode: RuntimeMode,
    interaction_mode: InteractionMode,
    worktree_path: Option<String>,
    lineage: ThreadLineage,
    forked_from: Option<Value>,
    history_origin: Option<String>,
    last_visited_at: Option<String>,
    settled_override: Option<Value>,
    snoozed_until: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadLineage {
    parent_thread_id: Option<String>,
    root_thread_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NativeProvider {
    Codex,
    Claude,
}

impl NativeProvider {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Codex => CODEX_INSTANCE,
            Self::Claude => CLAUDE_INSTANCE,
        }
    }
}

struct TurnIds {
    provider: NativeProvider,
    run_id: String,
    attempt_id: String,
    root_node_id: String,
    provider_thread_id: String,
    provider_session_id: String,
    checkpoint_scope_id: String,
    user_turn_item_id: String,
}

impl TurnIds {
    #[cfg(test)]
    fn derive(thread_id: &str, message_id: &str) -> Self {
        Self::for_run(thread_id, message_id, FIRST_RUN_ORDINAL, None)
    }

    fn for_run(
        thread_id: &str,
        message_id: &str,
        run_ordinal: i64,
        provider_thread_id: Option<&str>,
    ) -> Self {
        Self::for_provider(
            thread_id,
            message_id,
            run_ordinal,
            provider_thread_id,
            NativeProvider::Codex,
            None,
        )
    }

    fn for_provider(
        thread_id: &str,
        message_id: &str,
        run_ordinal: i64,
        provider_thread_id: Option<&str>,
        provider: NativeProvider,
        provider_session_id: Option<&str>,
    ) -> Self {
        let thread = encode_id_part(thread_id);
        let run_id = format!("run:thread:{thread}:ordinal:{run_ordinal}");
        let encoded_run = encode_id_part(&run_id);
        let pending_thread = encode_id_part(&format!("pending:{run_id}"));
        let instance_id = provider.as_str();
        let provider_session_id =
            provider_session_id
                .map(str::to_owned)
                .unwrap_or_else(|| match provider {
                    NativeProvider::Codex => {
                        format!("provider-session:provider-instance:{instance_id}:shared")
                    }
                    NativeProvider::Claude => format!(
                        "provider-session:provider-instance:{instance_id}:thread:{thread}:{}",
                        Uuid::new_v4()
                    ),
                });
        Self {
            provider,
            run_id,
            attempt_id: format!("run-attempt:run:{encoded_run}:attempt:1"),
            root_node_id: format!("node:run:{encoded_run}:root"),
            provider_thread_id: provider_thread_id.map(str::to_owned).unwrap_or_else(|| {
                format!("provider-thread:provider:{instance_id}:native-thread:{pending_thread}")
            }),
            provider_session_id,
            checkpoint_scope_id: format!("checkpoint-scope:thread:{thread}:name:root"),
            user_turn_item_id: format!("turn-item:message:{}", encode_id_part(message_id)),
        }
    }

    fn start_effect_id(&self, command_id: &str) -> String {
        format!("effect:{command_id}:{EFFECT_TYPE}:{}", self.run_id)
    }
}

impl TurnCommands {
    pub(super) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(super) fn dispatch(
        &self,
        command: MessageDispatch,
    ) -> Result<DispatchReceipt, TurnCommandError> {
        self.dispatch_with_settings(command, None)
    }

    pub(super) fn dispatch_with_settings(
        &self,
        command: MessageDispatch,
        settings: Option<&super::settings::SettingsSnapshot>,
    ) -> Result<DispatchReceipt, TurnCommandError> {
        if command.manual_continuation_of_run_id.is_some() {
            return self.dispatch_manual_continuation(command);
        }
        if matches!(command.dispatch_mode, DispatchMode::QueueAfterActive) {
            return self.dispatch_queued_worktree(command);
        }
        let MessageDispatch {
            command_type: DispatchType::MessageDispatch,
            command_id,
            thread_id,
            message_id,
            created_by: Actor::User,
            creation_source: CreationSource::Web,
            text,
            attachments,
            dispatch_mode: _,
            delivery_intent: None | Some(DeliveryIntent::Auto),
            model_selection,
            title_seed,
            manual_continuation_of_run_id: _,
        } = command;
        if [&command_id, &thread_id, &message_id, &text]
            .iter()
            .any(|value| value.trim().is_empty())
            || !attachments.is_empty()
            || model_selection.is_some()
            || title_seed.is_some()
        {
            return Err(TurnCommandError::Unsupported(
                "This command starts a Codex turn with a nonempty text message and no attachments.",
            ));
        }
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = existing_dispatch(&transaction, &command_id, &thread_id)? {
            return Ok(receipt);
        }
        let Some(IdleWorkspace {
            mut thread_payload,
            project_id,
            workspace_root,
            run_ordinal,
            mut provider_thread,
        }) = idle_workspace(&transaction, &thread_id, settings.is_some())?
        else {
            return Err(TurnCommandError::Unsupported(
                "This command requires an idle root-workspace Codex thread in an active project.",
            ));
        };
        let thread: ThreadProjection = serde_json::from_value(thread_payload.clone())?;
        if thread.worktree_path.is_some()
            || thread.forked_from.is_some()
            || thread.lineage.parent_thread_id.is_some()
            || thread.lineage.root_thread_id != thread_id
            || thread.history_origin.as_deref() == Some("v1_import")
            || thread.settled_override.is_some()
            || thread.snoozed_until.is_some()
            || !workspace_root.is_dir()
        {
            return Err(TurnCommandError::Unsupported(
                "This command requires a root-workspace thread with an existing workspace directory.",
            ));
        }
        let provider = match thread_payload["providerInstanceId"].as_str() {
            Some(CODEX_INSTANCE) => {
                validate_model_selection(&thread.model_selection)?;
                NativeProvider::Codex
            }
            Some(CLAUDE_INSTANCE) => {
                let config = settings
                    .and_then(|settings| {
                        settings.claude_instance(Some(&project_id), CLAUDE_INSTANCE)
                    })
                    .ok_or(TurnCommandError::Unsupported(
                        "The selected Claude Code instance is unavailable.",
                    ))?;
                validate_claude_turn(&thread, &text, &config)?;
                NativeProvider::Claude
            }
            _ => {
                return Err(TurnCommandError::Unsupported(
                    "The selected provider is unavailable.",
                ));
            }
        };
        let trimmed_text = text.trim();
        if trimmed_text.eq_ignore_ascii_case("/logout")
            || trimmed_text.eq_ignore_ascii_case("/compact")
            || trimmed_text.split_whitespace().next() == Some("/goal")
        {
            return Err(TurnCommandError::Unsupported(
                "This command requires a regular text prompt.",
            ));
        }
        let turn = TurnIds::for_provider(
            &thread_id,
            &message_id,
            run_ordinal,
            provider_thread
                .as_ref()
                .and_then(|provider| provider["id"].as_str()),
            provider,
            provider_thread
                .as_ref()
                .and_then(|provider| provider["providerSessionId"].as_str()),
        );
        let accepted_at = OffsetDateTime::now_utc();
        let activity = if thread.last_visited_at.is_none() {
            let visited_at = accepted_at - time::Duration::milliseconds(1);
            thread_payload["lastVisitedAt"] = json!(timestamp(visited_at));
            Some((timestamp(accepted_at), thread_payload))
        } else {
            None
        };
        let now = timestamp(OffsetDateTime::now_utc());
        if let Some(provider) = &mut provider_thread {
            provider["lastRunOrdinal"] = json!(run_ordinal);
            provider["updatedAt"] = json!(now);
        }
        let ordinal = allocate_turn_item_in_run(
            &transaction,
            &thread_id,
            &turn.user_turn_item_id,
            run_ordinal,
        )?;
        let sequence = append_dispatch(
            &transaction,
            &turn,
            DispatchEvents {
                command_id: &command_id,
                thread_id: &thread_id,
                message_id: &message_id,
                start: DispatchStart::Immediate {
                    workspace_root: &workspace_root,
                },
                run_ordinal,
                provider_thread,
                text: &text,
                model_selection: thread.model_selection,
                ordinal: Some(ordinal),
                now: &now,
                activity,
            },
        )?;
        persist_dispatch_start(transaction, &turn, &command_id, &thread_id, &now, sequence)
    }

    pub(super) fn command_output_item(
        &self,
        thread_id: &str,
        item_id: &str,
    ) -> rusqlite::Result<Option<(bool, String)>> {
        self.read_connection()?
            .query_row(
                "SELECT status IN ('pending', 'running', 'waiting'), payload_json
                 FROM orchestration_v2_projection_turn_items
                 WHERE thread_id = ?1 AND turn_item_id = ?2 AND type = 'command_execution'",
                params![thread_id, item_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
    }

    fn read_connection(&self) -> rusqlite::Result<Connection> {
        let connection = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        Ok(connection)
    }

    fn connect(&self) -> rusqlite::Result<Connection> {
        let connection =
            Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(connection)
    }
}

fn existing_dispatch(
    transaction: &Transaction<'_>,
    command_id: &str,
    thread_id: &str,
) -> Result<Option<DispatchReceipt>, TurnCommandError> {
    let receipt = transaction
        .query_row(
            "SELECT aggregate_kind, aggregate_id, command_type, status, result_sequence
             FROM orchestration_command_receipts WHERE command_id = ?1",
            [command_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, id, command_type, status, sequence)) = receipt else {
        return Ok(None);
    };
    if kind != "thread" || id != thread_id || command_type != COMMAND_TYPE || status != "accepted" {
        return Err(TurnCommandError::Unsupported(
            "This command ID already has a receipt for a different operation or outcome.",
        ));
    }
    let _run_id: String = transaction.query_row(
        "SELECT json_extract(payload_json, '$.id') FROM orchestration_events
         WHERE aggregate_kind = 'thread' AND stream_id = ?1
           AND command_id = ?2 AND event_type = 'run.created' AND sequence <= ?3
         ORDER BY sequence DESC LIMIT 1",
        params![thread_id, command_id, sequence],
        |row| row.get(0),
    )?;
    Ok(Some(DispatchReceipt {
        sequence,
        #[cfg(test)]
        run_id: _run_id,
        committed: false,
    }))
}

struct IdleWorkspace {
    thread_payload: Value,
    project_id: String,
    workspace_root: PathBuf,
    run_ordinal: i64,
    provider_thread: Option<Value>,
}

fn idle_workspace(
    transaction: &Transaction<'_>,
    thread_id: &str,
    claude_available: bool,
) -> Result<Option<IdleWorkspace>, TurnCommandError> {
    let row = transaction.query_row(
        "SELECT thread.payload_json, project.workspace_root, last_run.ordinal,
                provider_thread.payload_json, thread.project_id
         FROM orchestration_v2_projection_threads AS thread
         JOIN projection_projects AS project ON project.project_id = thread.project_id
         LEFT JOIN orchestration_v2_projection_runs AS last_run ON last_run.run_id = (
             SELECT run_id FROM orchestration_v2_projection_runs
             WHERE thread_id = thread.thread_id ORDER BY ordinal DESC LIMIT 1
         )
         LEFT JOIN orchestration_v2_projection_provider_threads AS provider_thread
           ON provider_thread.provider_thread_id = thread.active_provider_thread_id
              AND provider_thread.thread_id = thread.thread_id
         WHERE thread.thread_id = ?1 AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
           AND project.archived_at IS NULL AND project.deleted_at IS NULL
           AND COALESCE(json_array_length(project.tracked_folders_json), 0) = 0
           AND (json_extract(thread.payload_json, '$.providerInstanceId') = 'codex'
                OR (?2 AND json_extract(thread.payload_json, '$.providerInstanceId') = 'claudeAgent'))
           AND NOT EXISTS (
               SELECT 1 FROM orchestration_v2_projection_runs AS active
               WHERE active.thread_id = thread.thread_id
                 AND active.status IN ('preparing','queued','starting','running','waiting')
           )
           AND NOT EXISTS (
               SELECT 1 FROM orchestration_v2_projection_runtime_requests
               WHERE thread_id = thread.thread_id AND status = 'pending'
           )
           AND (
               (last_run.run_id IS NULL AND thread.active_provider_thread_id IS NULL
                 AND NOT EXISTS (
                      SELECT 1 FROM orchestration_v2_projection_messages
                      WHERE thread_id = thread.thread_id
                 )
                 AND NOT EXISTS (
                      SELECT 1 FROM orchestration_v2_projection_provider_threads AS existing
                      WHERE existing.thread_id = thread.thread_id
                 ))
               OR
               (last_run.status IN ('completed', 'interrupted') AND provider_thread.status = 'idle'
                 AND (last_run.status <> 'interrupted'
                      OR json_extract(last_run.payload_json, '$.checkpointId') IS NOT NULL)
                 AND last_run.provider_thread_id = provider_thread.provider_thread_id
                 AND provider_thread.provider_instance_id = json_extract(thread.payload_json, '$.providerInstanceId')
                 AND provider_thread.driver = provider_thread.provider_instance_id
                 AND ((provider_thread.driver = 'codex'
                       AND provider_thread.provider_session_id = 'provider-session:provider-instance:codex:shared')
                      OR (provider_thread.driver = 'claudeAgent' AND last_run.status = 'completed'))
                 AND json_extract(provider_thread.payload_json, '$.nativeThreadRef.strength') = 'strong'
                 AND COALESCE(json_array_length(
                      provider_thread.payload_json,'$.pendingBackgroundTasks'), 0) = 0
                 AND EXISTS (
                      SELECT 1 FROM orchestration_v2_projection_provider_turns AS provider_turn
                      WHERE provider_turn.run_attempt_id = json_extract(last_run.payload_json, '$.activeAttemptId')
                        AND provider_turn.provider_thread_id = provider_thread.provider_thread_id
                        AND provider_turn.status = last_run.status
                 )
                 AND NOT EXISTS (
                      SELECT 1 FROM orchestration_v2_projection_context_transfers AS transfer
                      WHERE transfer.target_thread_id = thread.thread_id
                        AND transfer.status IN ('pending','resolved_native','resolved_portable')
                 ))
               OR
               (last_run.status = 'interrupted' AND provider_thread.status = 'not_loaded'
                 AND last_run.provider_thread_id = provider_thread.provider_thread_id
                 AND provider_thread.driver = 'codex' AND provider_thread.provider_instance_id = 'codex'
                 AND provider_thread.provider_session_id = 'provider-session:provider-instance:codex:shared'
                 AND json_extract(provider_thread.payload_json, '$.nativeThreadRef') IS NULL
                 AND NOT EXISTS (
                      SELECT 1 FROM orchestration_v2_projection_provider_turns AS native_turn
                      WHERE native_turn.provider_thread_id = provider_thread.provider_thread_id
                 )
                 AND EXISTS (
                      SELECT 1 FROM orchestration_v2_projection_turn_items AS result
                      WHERE result.thread_id = thread.thread_id AND result.run_id = last_run.run_id
                        AND result.type = 'run_interrupt_result' AND result.status = 'interrupted'
                        AND result.provider_turn_id IS NULL
                        AND json_extract(result.payload_json, '$.message') = 'Run interrupted before provider start'
                 ))
           )",
        params![thread_id, claude_available],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<String>>(3)?, row.get::<_, String>(4)?)),
    ).optional()?;
    let Some((payload, workspace_root, previous_ordinal, provider_thread, project_id)) = row else {
        return Ok(None);
    };
    let run_ordinal =
        previous_ordinal
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(TurnCommandError::Unsupported(
                "The run ordinal cannot be advanced.",
            ))?;
    Ok(Some(IdleWorkspace {
        thread_payload: serde_json::from_str(&payload)?,
        project_id,
        workspace_root: workspace_root.into(),
        run_ordinal,
        provider_thread: provider_thread
            .map(|payload| serde_json::from_str(&payload))
            .transpose()?,
    }))
}

fn validate_claude_turn(
    thread: &ThreadProjection,
    text: &str,
    config: &super::settings::ClaudeInstance,
) -> Result<(), TurnCommandError> {
    let model = thread.model_selection["model"].as_str().unwrap_or_default();
    let supported = thread.model_selection["instanceId"] == CLAUDE_INSTANCE
        && thread
            .model_selection
            .as_object()
            .is_some_and(|selection| selection.len() == 2)
        && model.starts_with("claude-")
        && thread.runtime_mode == RuntimeMode::FullAccess
        && thread.interaction_mode == InteractionMode::Default
        && supports_plain_claude_config(config, model);
    if !supported || claude_slash_command(text) {
        return Err(TurnCommandError::Unsupported(
            "This Claude Code turn requires a plain text prompt, a default model configuration, and Full access mode.",
        ));
    }
    Ok(())
}

pub(super) fn claude_slash_command(text: &str) -> bool {
    text.split_whitespace().next().is_some_and(|word| {
        word.strip_prefix('/')
            .is_some_and(|command| !command.is_empty() && !command.contains('/'))
    })
}

pub(super) fn supports_plain_claude_config(
    config: &super::settings::ClaudeInstance,
    model: &str,
) -> bool {
    config.launch_args.trim().is_empty()
        && config.auto_compact_window.is_none()
        && !config
            .custom_models
            .iter()
            .any(|entry| entry == model || entry["slug"] == model)
        && config
            .environment
            .iter()
            .all(|entry| matches!(entry.value, super::settings::EnvironmentValue::Literal(_)))
}

pub(super) fn validate_model_selection(selection: &Value) -> Result<&str, TurnCommandError> {
    let Some(model) = selection["model"]
        .as_str()
        .filter(|model| !model.trim().is_empty())
    else {
        return Err(TurnCommandError::Unsupported(
            "This thread does not have a selected Codex model.",
        ));
    };
    if selection["instanceId"] != CODEX_INSTANCE
        || selection
            .as_object()
            .is_some_and(|selection| selection.len() != 2)
    {
        return Err(TurnCommandError::Unsupported(
            "This command requires the built-in Codex provider and a model without overrides.",
        ));
    }
    Ok(model)
}

pub(super) fn allocate_turn_item_in_run(
    transaction: &Transaction<'_>,
    thread_id: &str,
    turn_item_id: &str,
    run_ordinal: i64,
) -> rusqlite::Result<i64> {
    let lower_bound = run_ordinal
        .checked_mul(TURN_ITEM_ORDINAL_RANGE)
        .ok_or(rusqlite::Error::InvalidQuery)?;
    let upper_bound = lower_bound
        .checked_add(TURN_ITEM_ORDINAL_RANGE - 1)
        .ok_or(rusqlite::Error::InvalidQuery)?;
    transaction.query_row(
        "INSERT INTO orchestration_v2_turn_item_positions (thread_id, turn_item_id, ordinal)
         SELECT ?1, ?2, COALESCE(MAX(ordinal), ?3) + 1
         FROM orchestration_v2_turn_item_positions
         WHERE thread_id = ?1 AND ordinal BETWEEN ?3 AND ?4
         RETURNING ordinal",
        params![thread_id, turn_item_id, lower_bound, upper_bound,],
        |row| row.get(0),
    )
}

struct DispatchEvents<'a> {
    command_id: &'a str,
    thread_id: &'a str,
    message_id: &'a str,
    start: DispatchStart<'a>,
    run_ordinal: i64,
    provider_thread: Option<Value>,
    text: &'a str,
    model_selection: Value,
    ordinal: Option<i64>,
    now: &'a str,
    activity: Option<(String, Value)>,
}

enum DispatchStart<'a> {
    Immediate {
        workspace_root: &'a Path,
    },
    ManualContinuation {
        workspace_root: &'a Path,
        source_run_id: &'a str,
    },
    Preparing {
        workspace_strategy: &'a Value,
        preparation_item_id: String,
        preparation_ordinal: i64,
    },
    Queued {
        position: i64,
        checkpoint_workspace: Option<&'a Path>,
        follows_thread_selection: bool,
    },
}

pub(super) struct PreparingDispatch<'a> {
    pub(super) command_id: &'a str,
    pub(super) thread_id: &'a str,
    pub(super) message_id: &'a str,
    pub(super) text: &'a str,
    pub(super) model_selection: Value,
    pub(super) provider: NativeProvider,
    pub(super) workspace_strategy: &'a Value,
    pub(super) now: &'a str,
}

pub(super) fn append_preparing_dispatch(
    transaction: &Transaction<'_>,
    dispatch: PreparingDispatch<'_>,
) -> rusqlite::Result<(i64, String)> {
    let turn = TurnIds::for_provider(
        dispatch.thread_id,
        dispatch.message_id,
        FIRST_RUN_ORDINAL,
        None,
        dispatch.provider,
        None,
    );
    let preparation_item_id = format!(
        "turn-item:provider:{}:native-item:{}",
        dispatch.provider.as_str(),
        encode_id_part(&format!("workspace-preparation:{}", turn.run_id)),
    );
    let ordinal = allocate_turn_item_in_run(
        transaction,
        dispatch.thread_id,
        &turn.user_turn_item_id,
        FIRST_RUN_ORDINAL,
    )?;
    let preparation_ordinal = allocate_turn_item_in_run(
        transaction,
        dispatch.thread_id,
        &preparation_item_id,
        FIRST_RUN_ORDINAL,
    )?;
    let sequence = append_dispatch(
        transaction,
        &turn,
        DispatchEvents {
            command_id: dispatch.command_id,
            thread_id: dispatch.thread_id,
            message_id: dispatch.message_id,
            start: DispatchStart::Preparing {
                workspace_strategy: dispatch.workspace_strategy,
                preparation_item_id,
                preparation_ordinal,
            },
            run_ordinal: FIRST_RUN_ORDINAL,
            provider_thread: None,
            text: dispatch.text,
            model_selection: dispatch.model_selection,
            ordinal: Some(ordinal),
            now: dispatch.now,
            activity: None,
        },
    )?;
    Ok((sequence, turn.run_id))
}

fn append_dispatch(
    transaction: &Transaction<'_>,
    ids: &TurnIds,
    dispatch: DispatchEvents<'_>,
) -> rusqlite::Result<i64> {
    let DispatchEvents {
        command_id,
        thread_id,
        message_id,
        start,
        run_ordinal,
        provider_thread,
        text,
        model_selection,
        ordinal,
        now,
        activity,
    } = dispatch;
    let driver = ids.provider.as_str();
    let provider_metadata = json!({ "driver": driver, "providerInstanceId": driver });
    let run_metadata = json!({
        "runId": ids.run_id,
        "nodeId": ids.root_node_id,
        "providerInstanceId": driver,
    });
    let mut run = json!({
        "id": ids.run_id,
        "threadId": thread_id,
        "ordinal": run_ordinal,
        "providerInstanceId": driver,
        "modelSelection": model_selection,
        "providerThreadId": ids.provider_thread_id,
        "userMessageId": message_id,
        "rootNodeId": ids.root_node_id,
        "activeAttemptId": ids.attempt_id,
        "status": "starting",
        "queuePosition": null,
        "requestedAt": now,
        "startedAt": null,
        "completedAt": null,
        "checkpointId": null,
        "contextHandoffId": null,
    });
    let checkpoint_workspace = match &start {
        DispatchStart::Immediate { workspace_root } => Some(*workspace_root),
        DispatchStart::ManualContinuation {
            workspace_root,
            source_run_id,
        } => {
            run["manualContinuationOfRunId"] = json!(source_run_id);
            Some(*workspace_root)
        }
        DispatchStart::Preparing {
            workspace_strategy, ..
        } => {
            run["status"] = json!("preparing");
            run["workspacePreparation"] = (*workspace_strategy).clone();
            None
        }
        DispatchStart::Queued {
            position,
            checkpoint_workspace,
            ..
        } => {
            run["status"] = json!("queued");
            run["queuePosition"] = json!(position);
            *checkpoint_workspace
        }
    };
    let mut message = json!({
        "createdBy": "user",
        "creationSource": "web",
        "id": message_id,
        "threadId": thread_id,
        "runId": ids.run_id,
        "nodeId": ids.root_node_id,
        "role": "user",
        "text": text,
        "attachments": [],
        "streaming": false,
        "createdAt": now,
        "updatedAt": now,
    });
    let provider_thread = if matches!(start, DispatchStart::Queued { .. }) {
        None
    } else {
        Some(provider_thread.unwrap_or_else(|| {
            let mut provider = json!({
                    "id": ids.provider_thread_id,
                    "driver": driver,
                    "providerInstanceId": driver,
                    "providerSessionId": ids.provider_session_id,
                    "appThreadId": thread_id,
                    "ownerNodeId": null,
                    "nativeThreadRef": null,
                    "nativeConversationHeadRef": null,
                    "status": "not_loaded",
                    "firstRunOrdinal": run_ordinal,
                    "lastRunOrdinal": run_ordinal,
                    "handoffIds": [],
                    "forkedFrom": null,
                    "createdAt": now,
                    "updatedAt": now,
            });
            if ids.provider == NativeProvider::Claude
                && !matches!(&start, DispatchStart::Preparing { .. })
            {
                provider["pendingBackgroundTasks"] = json!([]);
                provider["contextUsage"] = Value::Null;
                provider["nativeMetadata"] = Value::Null;
                provider["goal"] = Value::Null;
            }
            provider
        }))
    };
    if matches!(
        start,
        DispatchStart::Queued {
            follows_thread_selection: true,
            ..
        }
    ) {
        message["followsThreadSelection"] = json!(true);
    }
    if matches!(start, DispatchStart::ManualContinuation { .. }) {
        message["notification"] = json!({
            "source": {"kind": "system"}, "outcome": "updated", "summary": "Resumed thread",
        });
    }
    let events = [
        ("run.created", &run_metadata, run),
        (
            "run-attempt.created",
            &run_metadata,
            json!({
                "id": ids.attempt_id,
                "runId": ids.run_id,
                "attemptOrdinal": 1,
                "rootNodeId": ids.root_node_id,
                "providerInstanceId": driver,
                "providerThreadId": ids.provider_thread_id,
                "providerTurnId": null,
                "reason": "initial",
                "status": "pending",
                "startedAt": null,
                "completedAt": null,
            }),
        ),
        (
            "node.updated",
            &run_metadata,
            json!({
                "id": ids.root_node_id,
                "threadId": thread_id,
                "runId": ids.run_id,
                "parentNodeId": null,
                "rootNodeId": ids.root_node_id,
                "kind": "root_turn",
                "status": "pending",
                "countsForRun": true,
                "providerThreadId": ids.provider_thread_id,
                "providerTurnId": null,
                "nativeItemRef": null,
                "runtimeRequestId": null,
                "checkpointScopeId": checkpoint_workspace.map(|_| &ids.checkpoint_scope_id),
                "startedAt": null,
                "completedAt": null,
            }),
        ),
    ];
    let stream = ThreadEventStream::command(transaction, thread_id, command_id);
    if let Some((occurred_at, payload)) = activity {
        stream.append(
            "thread.metadata-updated",
            &occurred_at,
            &json!({ "providerInstanceId": driver }),
            &payload,
        )?;
    }
    if let Some(provider_thread) = provider_thread {
        stream.append(
            "provider-thread.updated",
            now,
            &provider_metadata,
            &provider_thread,
        )?;
    }
    for (event_type, metadata, payload) in events {
        stream.append(event_type, now, metadata, &payload)?;
    }
    if let Some(workspace_root) = checkpoint_workspace {
        stream.append(
            "checkpoint-scope.created",
            now,
            &run_metadata,
            &root_checkpoint_scope(ids, thread_id, workspace_root, now),
        )?;
    }
    let mut sequence = stream.append("message.updated", now, &run_metadata, &message)?;
    if let Some(ordinal) = ordinal {
        let item = if matches!(start, DispatchStart::ManualContinuation { .. }) {
            json!({
                "id": ids.user_turn_item_id, "threadId": thread_id, "runId": ids.run_id,
                "nodeId": ids.root_node_id, "providerThreadId": ids.provider_thread_id,
                "providerTurnId": null, "nativeItemRef": null, "parentItemId": null,
                "ordinal": ordinal, "status": "completed", "title": null, "startedAt": now,
                "completedAt": now, "updatedAt": now, "type": "notification",
                "source": message["notification"]["source"], "outcome": message["notification"]["outcome"],
                "summary": message["notification"]["summary"],
            })
        } else {
            user_message_item(ids, &message, ordinal, now, "turn_start")
        };
        sequence = stream.append("turn-item.updated", now, &run_metadata, &item)?;
    }
    if let DispatchStart::Preparing {
        preparation_item_id,
        preparation_ordinal,
        ..
    } = start
    {
        sequence = stream.append(
            "turn-item.updated",
            now,
            &run_metadata,
            &json!({
                "id": preparation_item_id,
                "threadId": thread_id,
                "runId": ids.run_id,
                "nodeId": ids.root_node_id,
                "providerThreadId": ids.provider_thread_id,
                "providerTurnId": null,
                "nativeItemRef": null,
                "parentItemId": null,
                "ordinal": preparation_ordinal,
                "status": "running",
                "title": "Preparing workspace",
                "startedAt": now,
                "completedAt": null,
                "updatedAt": now,
                "type": "command_execution",
                "input": "Preparing workspace",
            }),
        )?;
    }
    record_projected_sequence(transaction, sequence, now)?;
    Ok(sequence)
}

fn persist_dispatch_start(
    transaction: Transaction<'_>,
    turn: &TurnIds,
    command_id: &str,
    thread_id: &str,
    now: &str,
    sequence: i64,
) -> Result<DispatchReceipt, TurnCommandError> {
    transaction.execute(
        "INSERT INTO orchestration_v2_effect_outbox (
            effect_id, command_id, thread_id, effect_type, payload_json, status, attempt_count,
            available_at, created_at, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6, ?6)",
        params![
            turn.start_effect_id(command_id),
            command_id,
            thread_id,
            EFFECT_TYPE,
            json!({ "type": EFFECT_TYPE, "runId": turn.run_id }).to_string(),
            now,
        ],
    )?;
    transaction.execute(
        "INSERT INTO orchestration_command_receipts (
            command_id, aggregate_kind, aggregate_id, accepted_at, result_sequence, status, command_type
         ) VALUES (?1, 'thread', ?2, ?3, ?4, 'accepted', ?5)",
        params![command_id, thread_id, now, sequence, COMMAND_TYPE],
    )?;
    transaction.commit()?;
    Ok(DispatchReceipt {
        sequence,
        #[cfg(test)]
        run_id: turn.run_id.clone(),
        committed: true,
    })
}

fn root_checkpoint_scope(ids: &TurnIds, thread_id: &str, workspace: &Path, now: &str) -> Value {
    json!({
        "id": ids.checkpoint_scope_id, "threadId": thread_id, "runId": ids.run_id,
        "nodeId": ids.root_node_id, "parentScopeId": null, "providerThreadId": ids.provider_thread_id,
        "kind": "root_run", "ordinalWithinParent": 0, "advancesAppRunCount": true,
        "cwd": workspace, "createdAt": now,
    })
}

fn user_message_item(
    ids: &TurnIds,
    message: &Value,
    ordinal: i64,
    now: &str,
    intent: &str,
) -> Value {
    json!({
        "createdBy": message["createdBy"], "creationSource": message["creationSource"],
        "id": ids.user_turn_item_id, "threadId": message["threadId"], "runId": ids.run_id,
        "nodeId": ids.root_node_id, "providerThreadId": ids.provider_thread_id,
        "providerTurnId": null, "nativeItemRef": null, "parentItemId": null,
        "ordinal": ordinal, "status": "completed", "title": null, "startedAt": now,
        "completedAt": now, "updatedAt": now, "type": "user_message",
        "messageId": message["id"], "inputIntent": intent, "text": message["text"],
        "attachments": message["attachments"],
    })
}

pub(super) fn record_projected_sequence(
    transaction: &Transaction<'_>,
    sequence: i64,
    now: &str,
) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT INTO orchestration_v2_projection_metadata (
            projection_name, schema_version, last_sequence, updated_at
         ) VALUES ('thread-projections', 2, ?1, ?2)
         ON CONFLICT (projection_name) DO UPDATE SET
            schema_version = excluded.schema_version, last_sequence = excluded.last_sequence,
            updated_at = excluded.updated_at",
        params![sequence, now],
    )?;
    Ok(())
}

pub(super) struct ThreadEventStream<'a> {
    transaction: &'a Transaction<'a>,
    thread_id: &'a str,
    command_id: Option<&'a str>,
    event_id_prefix: String,
}

impl<'a> ThreadEventStream<'a> {
    pub(super) fn command(
        transaction: &'a Transaction<'a>,
        thread_id: &'a str,
        command_id: &'a str,
    ) -> Self {
        Self {
            transaction,
            thread_id,
            command_id: Some(command_id),
            event_id_prefix: format!(
                "event:thread:{}:command:{}",
                encode_id_part(thread_id),
                encode_id_part(command_id)
            ),
        }
    }

    pub(super) fn server(transaction: &'a Transaction<'a>, thread_id: &'a str) -> Self {
        Self {
            transaction,
            thread_id,
            command_id: None,
            event_id_prefix: format!("event:thread:{}", encode_id_part(thread_id)),
        }
    }

    fn provider(
        transaction: &'a Transaction<'a>,
        thread_id: &'a str,
        provider_session_id: &str,
    ) -> Self {
        Self {
            transaction,
            thread_id,
            command_id: None,
            event_id_prefix: format!(
                "event:thread:{}:provider-session:{}",
                encode_id_part(thread_id),
                encode_id_part(provider_session_id)
            ),
        }
    }

    pub(super) fn append(
        &self,
        event_type: &str,
        occurred_at: &str,
        metadata: &Value,
        payload: &Value,
    ) -> rusqlite::Result<i64> {
        let sequence = self.transaction.query_row(
            "INSERT INTO orchestration_events (
                event_id, aggregate_kind, stream_id, stream_version, event_type, occurred_at,
                command_id, causation_event_id, correlation_id, actor_kind, payload_json,
                metadata_json, application_event_version
             ) VALUES (
                ?1, 'thread', ?2, COALESCE((
                    SELECT MAX(stream_version) + 1 FROM orchestration_events
                    WHERE aggregate_kind = 'thread' AND stream_id = ?2
                ), 0), ?3, ?4, ?5, NULL, ?5, 'server', ?6, ?7, 2
             ) RETURNING sequence",
            params![
                format!("{}:{}", self.event_id_prefix, Uuid::new_v4()),
                self.thread_id,
                event_type,
                occurred_at,
                self.command_id,
                payload.to_string(),
                metadata.to_string(),
            ],
            |row| row.get(0),
        )?;
        projection::apply(
            self.transaction,
            self.thread_id,
            event_type,
            occurred_at,
            payload,
        )?;
        Ok(sequence)
    }
}

pub(super) fn timestamp(now: OffsetDateTime) -> String {
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

pub(super) fn encode_id_part(value: &str) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            encoded.push(char::from(byte));
        } else {
            write!(&mut encoded, "%{byte:02X}").expect("writing into a String");
        }
    }
    encoded
}

#[cfg(test)]
pub(in crate::server) mod tests;
