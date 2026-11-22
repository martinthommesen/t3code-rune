use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::super::settings::SettingsSnapshot;
use super::{
    CLAUDE_INSTANCE, CODEX_INSTANCE, InteractionMode, NativeProvider, RuntimeMode,
    ThreadEventStream, ThreadProjection, TurnCommandError, TurnCommands, idle_workspace,
    record_projected_sequence, supports_plain_claude_config, timestamp, validate_model_selection,
};

pub(in crate::server) const COMMAND: &str = "thread.model-selection.set";
const EVENT: &str = "thread.model-selection-updated";
const UNAVAILABLE: &str = "This thread cannot change its saved model selection right now.";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    command_id: String,
    thread_id: String,
    model_selection: Value,
}

impl TurnCommands {
    pub(in crate::server) fn can_change_saved_codex_model(
        &self,
        thread_id: &str,
        settings: &SettingsSnapshot,
    ) -> Result<bool, TurnCommandError> {
        let database = self.read_connection()?;
        Ok(
            saved_model_thread(&database, thread_id, settings, NativeProvider::Codex, None)?
                .is_some(),
        )
    }

    pub(in crate::server) fn set_saved_model(
        &self,
        payload: Value,
        settings: &SettingsSnapshot,
    ) -> Result<(i64, bool), TurnCommandError> {
        let Input {
            command_id,
            thread_id,
            model_selection,
        } = serde_json::from_value(payload)?;
        let provider = match model_selection["instanceId"].as_str() {
            Some(CODEX_INSTANCE) => {
                validate_model_selection(&model_selection)?;
                NativeProvider::Codex
            }
            Some(CLAUDE_INSTANCE) if switchable_claude_model(&model_selection).is_some() => {
                NativeProvider::Claude
            }
            _ => return Err(TurnCommandError::Unsupported(UNAVAILABLE)),
        };
        if command_id.trim().is_empty() || thread_id.trim().is_empty() {
            return Err(TurnCommandError::Unsupported(
                "A command and thread ID are required to change the model selection.",
            ));
        }
        let mut database = self.connect()?;
        let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((kind, aggregate_id, command_type, status, sequence)) = transaction
            .query_row(
                "SELECT aggregate_kind, aggregate_id, command_type, status, result_sequence
                   FROM orchestration_command_receipts WHERE command_id = ?1",
                [&command_id],
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
            .optional()?
        {
            if kind == "thread"
                && aggregate_id == thread_id
                && command_type == COMMAND
                && status == "accepted"
            {
                return Ok((sequence, false));
            }
            return Err(TurnCommandError::Unsupported(
                "This command ID already has a receipt for a different operation or outcome.",
            ));
        }
        let mut thread = saved_model_thread(
            &transaction,
            &thread_id,
            settings,
            provider,
            Some(&model_selection),
        )?
        .ok_or(TurnCommandError::Unsupported(UNAVAILABLE))?;
        let now = timestamp(OffsetDateTime::now_utc());
        thread["modelSelection"] = model_selection;
        thread["updatedAt"] = json!(now);
        let sequence = ThreadEventStream::command(&transaction, &thread_id, &command_id).append(
            EVENT,
            &now,
            &json!({"providerInstanceId": provider.as_str()}),
            &thread,
        )?;
        record_projected_sequence(&transaction, sequence, &now)?;
        transaction.execute(
            "INSERT INTO orchestration_command_receipts (
                command_id, aggregate_kind, aggregate_id, command_type, accepted_at,
                result_sequence, status
             ) VALUES (?1, 'thread', ?2, ?3, ?4, ?5, 'accepted')",
            params![command_id, thread_id, COMMAND, now, sequence],
        )?;
        transaction.commit()?;
        Ok((sequence, true))
    }
}

fn saved_model_thread(
    database: &Connection,
    thread_id: &str,
    settings: &SettingsSnapshot,
    provider: NativeProvider,
    requested: Option<&Value>,
) -> Result<Option<Value>, TurnCommandError> {
    let Some(workspace) = idle_workspace(database, thread_id, provider == NativeProvider::Claude)?
    else {
        return Ok(None);
    };
    let thread: ThreadProjection = serde_json::from_value(workspace.thread_payload.clone())?;
    if workspace.thread_payload["providerInstanceId"] != provider.as_str()
        || !thread.supports_root_turn(thread_id, &workspace.workspace_root)
        || !has_native_thread(provider, workspace.provider_thread.as_ref())
    {
        return Ok(None);
    }
    let project = Some(workspace.project_id.as_str());
    let valid = match provider {
        NativeProvider::Codex => {
            validate_model_selection(&thread.model_selection).is_ok()
                && settings.codex_instance(project, CODEX_INSTANCE).is_ok()
        }
        NativeProvider::Claude => {
            thread.runtime_mode == RuntimeMode::FullAccess
                && thread.interaction_mode == InteractionMode::Default
                && settings
                    .claude_instance(project, CLAUDE_INSTANCE)
                    .is_some_and(|config| {
                        switchable_claude_model(&thread.model_selection)
                            .is_some_and(|model| supports_plain_claude_config(&config, model))
                            && requested
                                .and_then(switchable_claude_model)
                                .is_some_and(|model| supports_plain_claude_config(&config, model))
                    })
        }
    };
    Ok(valid.then_some(workspace.thread_payload))
}

pub(super) fn plain_claude_model(selection: &Value) -> Option<&str> {
    let model = selection["model"].as_str()?;
    (selection.as_object()?.len() == 2
        && selection["instanceId"] == CLAUDE_INSTANCE
        && model.starts_with("claude-"))
    .then_some(model)
}

fn switchable_claude_model(selection: &Value) -> Option<&str> {
    plain_claude_model(selection)
        .filter(|model| matches!(*model, "claude-sonnet-4-6" | "claude-haiku-4-5"))
}

pub(super) fn has_native_thread(provider: NativeProvider, thread: Option<&Value>) -> bool {
    thread
        .filter(|thread| thread["driver"] == provider.as_str())
        .and_then(|thread| thread["nativeThreadRef"]["nativeId"].as_str())
        .is_some_and(|id| match provider {
            NativeProvider::Codex => !id.is_empty(),
            NativeProvider::Claude => Uuid::parse_str(id).is_ok(),
        })
}

pub(super) fn model_change(
    database: &Connection,
    thread_id: &str,
    provider: NativeProvider,
    selection: &Value,
) -> Result<Option<Value>, TurnCommandError> {
    let encoded = database.query_row(
        "SELECT json_extract(run.payload_json, '$.modelSelection')
         FROM orchestration_v2_projection_runs AS run
         WHERE run.thread_id = ?1 AND run.status NOT IN ('rolled_back', 'queued')
           AND (json_extract(run.payload_json, '$.contextHandoffId') IS NOT NULL OR EXISTS (
                 SELECT 1 FROM orchestration_v2_projection_turn_items AS item
                 WHERE item.thread_id = run.thread_id AND item.run_id = run.run_id
                   AND item.type = 'user_message'
                   AND json_extract(item.payload_json, '$.messageId') = json_extract(run.payload_json, '$.userMessageId')
                   AND NOT (item.status = 'pending' AND json_extract(item.payload_json, '$.inputIntent') IS 'queued_turn')
                   AND substr(json_extract(item.payload_json, '$.messageId'), 1, 13) <> 'async-answer:'
                   AND NOT (lower(trim(json_extract(item.payload_json, '$.text'))) = '/compact'
                           AND COALESCE(json_array_length(item.payload_json, '$.attachments'), 0) = 0)
               ))
         ORDER BY run.completed_at IS NULL DESC, run.completed_at DESC, run.ordinal DESC LIMIT 1",
        [thread_id],
        |row| row.get::<_, Option<String>>(0),
    ).optional()?.flatten();
    let Some(encoded) = encoded else {
        return Ok(None);
    };
    let previous: Value = serde_json::from_str(&encoded)?;
    let comparable = match provider {
        NativeProvider::Codex => validate_model_selection(&previous).is_ok(),
        NativeProvider::Claude => {
            switchable_claude_model(&previous).is_some()
                && switchable_claude_model(selection).is_some()
        }
    };
    Ok((previous != *selection && comparable).then(|| json!({"from":previous,"to":selection})))
}
