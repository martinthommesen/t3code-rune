use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::turn_commands::{ThreadEventStream, record_projected_sequence};

use super::delete_cleanup::DELETION_EFFECTS;
use super::{
    CommittedThread, INSERT_RECEIPT, Receipt, SELECT_RECEIPT, ThreadCommandError, ThreadCommands,
    now_timestamp, trim_js,
};

pub(super) const COMMAND: &str = "thread.delete";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Delete {
    #[serde(rename = "type")]
    _kind: String,
    command_id: String,
    thread_id: String,
}

impl ThreadCommands {
    pub(super) fn delete_archived(
        &self,
        payload: Value,
    ) -> Result<CommittedThread, ThreadCommandError> {
        let input: Delete = serde_json::from_value(payload)
            .map_err(|cause| ThreadCommandError::InvalidRequest(cause.to_string()))?;
        let command_id = trim_js(&input.command_id);
        let thread_id = trim_js(&input.thread_id);
        if command_id.is_empty() || thread_id.is_empty() {
            return Err(ThreadCommandError::InvalidRequest(
                "A command ID and thread ID are required.".into(),
            ));
        }
        let mut database = self.connect()?;
        let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = transaction
            .query_row(SELECT_RECEIPT, [command_id], Receipt::read)
            .optional()?
        {
            return receipt.replay(command_id, thread_id);
        }
        let state = transaction
            .query_row(SELECT_THREAD, [thread_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })
            .optional()?;
        let Some((encoded, ordinary_archive)) = state else {
            return Err(ThreadCommandError::MissingProjection(thread_id.to_owned()));
        };
        if !ordinary_archive {
            return Err(ThreadCommandError::Rejected(
                "This thread requires additional cleanup before it can be deleted from Archive."
                    .into(),
            ));
        }
        let mut projection: Value = serde_json::from_str(&encoded)
            .map_err(|cause| rusqlite::Error::ToSqlConversionFailure(Box::new(cause)))?;
        if !projection.is_object() {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        let now = now_timestamp();
        projection["deletedAt"] = json!(now);
        projection["titleRegeneration"] = Value::Null;
        projection["updatedAt"] = json!(now);
        let sequence = ThreadEventStream::command(&transaction, thread_id, command_id).append(
            "thread.deleted",
            &now,
            &json!({"providerInstanceId":projection["providerInstanceId"]}),
            &projection,
        )?;
        record_projected_sequence(&transaction, sequence, &now)?;
        for effect in DELETION_EFFECTS {
            transaction.execute(
                INSERT_EFFECT,
                params![
                    format!("effect:{command_id}:{effect}"),
                    command_id,
                    thread_id,
                    effect,
                    json!({"type":effect}).to_string(),
                    now,
                ],
            )?;
        }
        transaction.execute(
            INSERT_RECEIPT,
            params![
                command_id,
                thread_id,
                COMMAND,
                now,
                sequence,
                "accepted",
                None::<String>
            ],
        )?;
        transaction.commit()?;
        Ok(CommittedThread {
            sequence: sequence as u64,
            committed: true,
        })
    }
}

const INSERT_EFFECT: &str = "
    INSERT INTO orchestration_v2_effect_outbox (
        effect_id, command_id, thread_id, effect_type, payload_json, status, attempt_count,
        available_at, created_at, updated_at
    ) VALUES (?1,?2,?3,?4,?5,'pending',0,?6,?6,?6)
";

const SELECT_THREAD: &str = "
    SELECT thread.payload_json,
        thread.archived_at IS NOT NULL AND thread.deleted_at IS NULL
        AND json_extract(thread.payload_json,'$.lineage.rootThreadId') IS thread.thread_id
        AND json_extract(thread.payload_json,'$.lineage.parentThreadId') IS NULL
        AND json_extract(thread.payload_json,'$.worktreePath') IS NULL
        AND json_extract(thread.payload_json,'$.forkedFrom') IS NULL
        AND NOT EXISTS (
            SELECT 1 FROM orchestration_v2_projection_context_transfers AS transfer
            JOIN orchestration_v2_projection_threads AS child ON child.thread_id = transfer.target_thread_id
            WHERE transfer.source_thread_id = thread.thread_id AND transfer.type = 'fork'
              AND child.deleted_at IS NULL AND json_extract(child.payload_json,'$.lineage.sideChat') = 1
        )
        AND NOT EXISTS (
            SELECT 1 FROM orchestration_v2_projection_runs AS run WHERE run.thread_id = thread.thread_id
              AND (run.status IN ('preparing','queued','starting','running','waiting')
                   OR json_extract(run.payload_json,'$.delegatedCompletion') IS NOT NULL)
        )
        AND NOT EXISTS (
            SELECT 1 FROM orchestration_v2_projection_runtime_requests AS request
            WHERE request.thread_id = thread.thread_id AND request.status = 'pending'
        )
        AND NOT EXISTS (
            SELECT 1 FROM orchestration_v2_projection_subagents AS task WHERE task.thread_id = thread.thread_id
              AND json_extract(task.payload_json,'$.origin') = 'app_owned'
        )
        AND NOT EXISTS (
            SELECT 1 FROM orchestration_v2_projection_provider_session_bindings AS binding
            JOIN orchestration_v2_projection_provider_sessions AS session USING(provider_session_id)
            WHERE binding.thread_id = thread.thread_id AND session.status IS NOT 'stopped'
        )
        AND NOT EXISTS (
            SELECT 1 FROM orchestration_v2_projection_messages AS message WHERE message.thread_id = thread.thread_id
              AND json_array_length(message.payload_json,'$.attachments') > 0
        )
        AND NOT EXISTS (
            SELECT 1 FROM orchestration_v2_projection_turn_items AS item WHERE item.thread_id = thread.thread_id
              AND item.type = 'dynamic_tool'
              AND (item.payload_json LIKE '%htmlRender%' OR item.payload_json LIKE '%mcpAppOutput%'
                  OR item.payload_json LIKE '%preview_recording_stop%')
        )
        AND NOT EXISTS (
            SELECT 1 FROM orchestration_v2_projection_turn_items AS item,
                json_each(item.payload_json,'$.questionAnswer.attachmentsByQuestionId') AS question,
                json_each(question.value)
            WHERE item.thread_id = thread.thread_id AND item.type = 'user_input_request'
        )
        AND NOT EXISTS (
            SELECT 1 FROM json_each(thread.payload_json,'$.guestShare.messages') AS message
            WHERE json_extract(message.value,'$.status') = 'pending'
              AND json_array_length(message.value,'$.attachments') > 0
        )
        AND NOT EXISTS (
            SELECT 1 FROM scheduled_tasks AS task WHERE task.thread_id = thread.thread_id
              AND task.webhook_token IS NOT NULL
        )
    FROM orchestration_v2_projection_threads AS thread WHERE thread.thread_id=?1
";
