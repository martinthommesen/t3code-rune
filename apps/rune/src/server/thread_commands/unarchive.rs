use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::turn_commands::{ThreadEventStream, record_projected_sequence};

use super::{
    CommittedThread, INSERT_RECEIPT, LATEST_THREAD_SEQUENCE, Receipt, SELECT_RECEIPT,
    ThreadCommandError, ThreadCommands, now_timestamp, trim_js,
};

pub(super) const COMMAND: &str = "thread.unarchive";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Unarchive {
    #[serde(rename = "type")]
    _kind: String,
    command_id: String,
    thread_id: String,
}

impl ThreadCommands {
    pub(super) fn unarchive(&self, payload: Value) -> Result<CommittedThread, ThreadCommandError> {
        let input: Unarchive = serde_json::from_value(payload)
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
        let now = now_timestamp();
        let thread = transaction
            .query_row(SELECT_THREAD, [thread_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            })
            .optional()?;
        let Some((encoded, deleted, archived, archived_side_chats)) = thread else {
            record_rejection(
                &transaction,
                thread_id,
                command_id,
                &now,
                &format!("Failed to load orchestration projection for thread {thread_id}."),
            )?;
            transaction.commit()?;
            return Err(ThreadCommandError::MissingProjection(thread_id.to_owned()));
        };
        if deleted.is_some() {
            let message = format!("Thread {thread_id} is deleted.");
            record_rejection(&transaction, thread_id, command_id, &now, &message)?;
            transaction.commit()?;
            return Err(ThreadCommandError::Rejected(message));
        }
        if archived.is_none() {
            record_rejection(
                &transaction,
                thread_id,
                command_id,
                &now,
                &format!("Failed to dispatch orchestration command {COMMAND} ({command_id})."),
            )?;
            transaction.commit()?;
            return Err(ThreadCommandError::NotArchived(thread_id.to_owned()));
        }
        if archived_side_chats {
            return Err(ThreadCommandError::Rejected(
                "This thread has side chats that must be unarchived with it.".into(),
            ));
        }
        let mut projection: Value = serde_json::from_str(&encoded)
            .map_err(|cause| rusqlite::Error::ToSqlConversionFailure(Box::new(cause)))?;
        if !projection.is_object() {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        projection["archivedAt"] = Value::Null;
        projection["updatedAt"] = json!(now);
        let sequence = ThreadEventStream::command(&transaction, thread_id, command_id).append(
            "thread.unarchived",
            &now,
            &json!({"providerInstanceId":projection["providerInstanceId"]}),
            &projection,
        )?;
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

fn record_rejection(
    transaction: &Transaction<'_>,
    thread_id: &str,
    command_id: &str,
    now: &str,
    message: &str,
) -> rusqlite::Result<()> {
    let sequence: u64 =
        transaction.query_row(LATEST_THREAD_SEQUENCE, [thread_id], |row| row.get(0))?;
    transaction.execute(
        INSERT_RECEIPT,
        params![
            command_id, thread_id, COMMAND, now, sequence, "rejected", message
        ],
    )?;
    Ok(())
}

const SELECT_THREAD: &str = "
    SELECT parent.payload_json, parent.deleted_at, parent.archived_at, EXISTS (
        SELECT 1 FROM orchestration_v2_projection_context_transfers AS transfer
        JOIN orchestration_v2_projection_threads AS child ON child.thread_id = transfer.target_thread_id
        WHERE transfer.source_thread_id = parent.thread_id AND transfer.type = 'fork'
          AND child.deleted_at IS NULL AND json_extract(child.payload_json, '$.lineage.sideChat') = 1
          AND julianday(child.archived_at) >= julianday(parent.archived_at)
    )
    FROM orchestration_v2_projection_threads AS parent WHERE parent.thread_id = ?1
";
