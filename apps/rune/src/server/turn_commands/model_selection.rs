use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;

use super::super::settings::SettingsSnapshot;
use super::{
    CODEX_INSTANCE, ReasoningEffort, ThreadEventStream, ThreadProjection, TurnCommandError,
    TurnCommands, idle_workspace, record_projected_sequence, timestamp, validate_model_selection,
};

pub(in crate::server) const COMMAND: &str = "thread.model-selection.set";
const EVENT: &str = "thread.model-selection-updated";
const UNAVAILABLE: &str = "This thread cannot change its saved Codex reasoning effort right now.";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Input {
    command_id: String,
    thread_id: String,
    model_selection: Value,
}

impl TurnCommands {
    pub(in crate::server) fn can_change_codex_reasoning(
        &self,
        thread_id: &str,
        settings: &SettingsSnapshot,
    ) -> Result<bool, TurnCommandError> {
        let database = self.read_connection()?;
        Ok(saved_codex_thread(&database, thread_id, settings)?.is_some())
    }

    pub(in crate::server) fn set_codex_reasoning(
        &self,
        payload: Value,
        settings: &SettingsSnapshot,
    ) -> Result<(i64, bool), TurnCommandError> {
        let Input {
            command_id,
            thread_id,
            model_selection,
        } = serde_json::from_value(payload)?;
        let model = validate_model_selection(&model_selection)?;
        if command_id.trim().is_empty()
            || thread_id.trim().is_empty()
            || model.reasoning_effort.is_none()
        {
            return Err(TurnCommandError::Unsupported(
                "Set a supported GPT-5.4 reasoning effort for this thread.",
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
        let mut thread = saved_codex_thread(&transaction, &thread_id, settings)?
            .ok_or(TurnCommandError::Unsupported(UNAVAILABLE))?;
        let now = timestamp(OffsetDateTime::now_utc());
        thread["modelSelection"] = model_selection;
        thread["updatedAt"] = json!(now);
        let sequence = ThreadEventStream::command(&transaction, &thread_id, &command_id).append(
            EVENT,
            &now,
            &json!({"providerInstanceId": CODEX_INSTANCE}),
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

fn saved_codex_thread(
    database: &Connection,
    thread_id: &str,
    settings: &SettingsSnapshot,
) -> Result<Option<Value>, TurnCommandError> {
    let Some(workspace) = idle_workspace(database, thread_id, false)? else {
        return Ok(None);
    };
    let thread: ThreadProjection = serde_json::from_value(workspace.thread_payload.clone())?;
    let valid = thread.supports_root_turn(thread_id, &workspace.workspace_root)
        && validate_model_selection(&thread.model_selection)
            .is_ok_and(|selection| selection.model == ReasoningEffort::MODEL_ID)
        && has_native_codex_thread(workspace.provider_thread.as_ref())
        && settings
            .codex_instance(Some(&workspace.project_id), CODEX_INSTANCE)
            .is_ok();
    Ok(valid.then_some(workspace.thread_payload))
}

pub(super) fn has_native_codex_thread(provider: Option<&Value>) -> bool {
    provider.is_some_and(|provider| {
        provider["driver"] == CODEX_INSTANCE
            && provider["nativeThreadRef"]["nativeId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
    })
}
