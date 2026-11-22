use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::server::turn_commands::{ThreadEventStream, record_projected_sequence, title_effect};

use super::{
    CommittedThread, INSERT_RECEIPT, LATEST_THREAD_SEQUENCE, Receipt, SELECT_RECEIPT,
    ThreadCommandError, ThreadCommands, create::trim, now_timestamp,
};

pub(super) const COMMAND: &str = "thread.metadata.update";

impl ThreadCommands {
    pub(super) fn rename(&self, payload: Value) -> Result<CommittedThread, ThreadCommandError> {
        let command_id = text(&payload, "commandId")?;
        let thread_id = text(&payload, "threadId")?;
        let title = text(&payload, "title")?;
        if payload.as_object().is_some_and(|fields| {
            fields
                .keys()
                .any(|key| !matches!(key.as_str(), "type" | "commandId" | "threadId" | "title"))
        }) {
            return Err(ThreadCommandError::InvalidRequest(
                "Send only the title when renaming a thread.".into(),
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
        let now = now_timestamp();
        let encoded = transaction
            .query_row(
                "SELECT payload_json FROM orchestration_v2_projection_threads
             WHERE thread_id = ?1 AND deleted_at IS NULL",
                [thread_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(encoded) = encoded else {
            let sequence: u64 =
                transaction.query_row(LATEST_THREAD_SEQUENCE, [thread_id], |row| row.get(0))?;
            transaction.execute(
                INSERT_RECEIPT,
                params![
                    command_id,
                    thread_id,
                    COMMAND,
                    now,
                    sequence,
                    "rejected",
                    format!("Failed to load orchestration projection for thread {thread_id}.")
                ],
            )?;
            transaction.commit()?;
            return Err(ThreadCommandError::MissingProjection(thread_id.to_owned()));
        };
        let mut projection: Value = serde_json::from_str(&encoded)
            .map_err(|cause| rusqlite::Error::ToSqlConversionFailure(Box::new(cause)))?;
        if !projection.is_object() {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        let changed = projection["title"] != title;
        projection["title"] = json!(title);
        projection["titleRenamed"] = Value::Bool(true);
        projection["titleRegeneration"] = Value::Null;
        projection["updatedAt"] = json!(now);
        let sequence = ThreadEventStream::command(&transaction, thread_id, command_id).append(
            "thread.metadata-updated",
            &now,
            &json!({"providerInstanceId":projection["providerInstanceId"]}),
            &projection,
        )?;
        if changed {
            transaction.execute(
                "UPDATE orchestration_v2_projection_threads SET title = ?2 WHERE thread_id = ?1",
                params![thread_id, title],
            )?;
            title_effect::enqueue(&transaction, thread_id, command_id, title, &now)?;
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
        Ok(CommittedThread {
            sequence: sequence as u64,
            committed: true,
        })
    }
}

fn text<'a>(payload: &'a Value, field: &str) -> Result<&'a str, ThreadCommandError> {
    payload[field]
        .as_str()
        .map(trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ThreadCommandError::InvalidRequest(format!(
                "Expected a non-blank string\n  at [\"{field}\"]"
            ))
        })
}
