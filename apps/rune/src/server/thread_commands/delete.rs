use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
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

pub(in crate::server) struct DeletedThreads {
    pub(in crate::server) receipt: CommittedThread,
    pub(in crate::server) thread_ids: Vec<String>,
}

struct Candidate {
    payload: Value,
    project_id: String,
    archived: bool,
}

impl ThreadCommands {
    pub(in crate::server) fn delete_archived(
        &self,
        payload: Value,
    ) -> Result<DeletedThreads, ThreadCommandError> {
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
            return receipt
                .replay(command_id, thread_id)
                .map(|receipt| DeletedThreads {
                    receipt,
                    thread_ids: Vec::new(),
                });
        }

        let root = candidate(&transaction, thread_id)?;
        if !root.archived
            || root.payload["lineage"]["rootThreadId"] != thread_id
            || !root.payload["lineage"]["parentThreadId"].is_null()
        {
            return Err(additional_cleanup());
        }
        let children = side_chats(&transaction, thread_id)?;
        let mut members = Vec::with_capacity(children.len() + 1);
        for child_id in children {
            let child = candidate(&transaction, &child_id)?;
            let lineage = &child.payload["lineage"];
            if child.project_id != root.project_id
                || lineage["rootThreadId"] != thread_id
                || lineage["parentThreadId"] != thread_id
                || lineage["relationshipToParent"] != "fork"
                || lineage["sideChat"] != true
                || !side_chats(&transaction, &child_id)?.is_empty()
            {
                return Err(additional_cleanup());
            }
            let child_command = format!("{command_id}:side-chat:{child_id}");
            members.push((child_id, child_command, child.payload));
        }

        let now = now_timestamp();
        let mut sequence = archive_member(
            &transaction,
            thread_id,
            command_id,
            command_id,
            root.payload,
            &now,
        )?;
        let mut thread_ids = Vec::with_capacity(members.len() + 1);
        thread_ids.push(thread_id.to_owned());
        for (child_id, child_command, projection) in members {
            sequence = archive_member(
                &transaction,
                &child_id,
                command_id,
                &child_command,
                projection,
                &now,
            )?;
            thread_ids.push(child_id);
        }
        record_projected_sequence(&transaction, sequence, &now)?;
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
        Ok(DeletedThreads {
            receipt: CommittedThread {
                sequence: sequence as u64,
                committed: true,
            },
            thread_ids,
        })
    }
}

fn candidate(
    transaction: &Transaction<'_>,
    thread_id: &str,
) -> Result<Candidate, ThreadCommandError> {
    let state = transaction
        .query_row(SELECT_THREAD, [thread_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })
        .optional()?;
    let Some((encoded, project_id, archived, supported)) = state else {
        return Err(ThreadCommandError::MissingProjection(thread_id.to_owned()));
    };
    if !supported {
        return Err(additional_cleanup());
    }
    let payload: Value = serde_json::from_str(&encoded)
        .map_err(|cause| rusqlite::Error::ToSqlConversionFailure(Box::new(cause)))?;
    if !payload.is_object() {
        return Err(rusqlite::Error::InvalidQuery.into());
    }
    Ok(Candidate {
        payload,
        project_id,
        archived,
    })
}

fn side_chats(
    transaction: &Transaction<'_>,
    parent_thread_id: &str,
) -> rusqlite::Result<Vec<String>> {
    transaction
        .prepare_cached(SELECT_SIDE_CHATS)?
        .query_map([parent_thread_id], |row| row.get(0))?
        .collect()
}

fn archive_member(
    transaction: &Transaction<'_>,
    thread_id: &str,
    command_id: &str,
    effect_command_id: &str,
    mut projection: Value,
    now: &str,
) -> rusqlite::Result<i64> {
    projection["deletedAt"] = json!(now);
    projection["titleRegeneration"] = Value::Null;
    projection["updatedAt"] = json!(now);
    let sequence = ThreadEventStream::command_with_event_source(
        transaction,
        thread_id,
        command_id,
        effect_command_id,
    )
    .append(
        "thread.deleted",
        now,
        &json!({"providerInstanceId":projection["providerInstanceId"]}),
        &projection,
    )?;
    for effect in DELETION_EFFECTS {
        transaction.execute(
            INSERT_EFFECT,
            params![
                format!("effect:{effect_command_id}:{effect}"),
                effect_command_id,
                thread_id,
                effect,
                json!({"type":effect}).to_string(),
                now,
            ],
        )?;
    }
    Ok(sequence)
}

fn additional_cleanup() -> ThreadCommandError {
    ThreadCommandError::Rejected(
        "This thread requires additional cleanup before it can be deleted from Archive.".into(),
    )
}

const SELECT_SIDE_CHATS: &str = "
    SELECT child.thread_id FROM orchestration_v2_projection_context_transfers AS transfer
    JOIN orchestration_v2_projection_threads AS child ON child.thread_id = transfer.target_thread_id
    WHERE transfer.source_thread_id = ?1 AND transfer.type = 'fork'
      AND child.deleted_at IS NULL AND json_extract(child.payload_json,'$.lineage.sideChat') = 1
    GROUP BY child.thread_id ORDER BY MIN(transfer.rowid)
";

const INSERT_EFFECT: &str = "
    INSERT INTO orchestration_v2_effect_outbox (
        effect_id, command_id, thread_id, effect_type, payload_json, status, attempt_count,
        available_at, created_at, updated_at
    ) VALUES (?1,?2,?3,?4,?5,'pending',0,?6,?6,?6)
";

const SELECT_THREAD: &str = "
    SELECT thread.payload_json, thread.project_id, thread.archived_at IS NOT NULL,
        thread.deleted_at IS NULL
        AND json_extract(thread.payload_json,'$.worktreePath') IS NULL
        AND json_extract(thread.payload_json,'$.forkedFrom') IS NULL
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
