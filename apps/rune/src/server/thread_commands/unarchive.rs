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
    pub(in crate::server) fn unarchive(
        &self,
        payload: Value,
    ) -> Result<(CommittedThread, Vec<String>), ThreadCommandError> {
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
            return receipt
                .replay(command_id, thread_id)
                .map(|receipt| (receipt, Vec::new()));
        }
        let now = now_timestamp();
        let thread = transaction
            .query_row(SELECT_THREAD, [thread_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .optional()?;
        let Some((encoded, deleted, archived)) = thread else {
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
        let Some(parent_archived_at) = archived else {
            record_rejection(
                &transaction,
                thread_id,
                command_id,
                &now,
                &format!("Failed to dispatch orchestration command {COMMAND} ({command_id})."),
            )?;
            transaction.commit()?;
            return Err(ThreadCommandError::NotArchived(thread_id.to_owned()));
        };
        let mut applied_at = now_timestamp();
        let mut sequence = unarchive_thread(
            &transaction,
            thread_id,
            command_id,
            command_id,
            &encoded,
            &applied_at,
        )?;
        let mut thread_ids = vec![thread_id.to_owned()];
        for child_id in
            super::archive::side_chats(&transaction, thread_id, Some(&parent_archived_at))?
        {
            let encoded: String =
                transaction.query_row(SELECT_THREAD, [&child_id], |row| row.get(0))?;
            let child_command = format!("{command_id}:side-chat:{child_id}");
            applied_at = now_timestamp();
            sequence = unarchive_thread(
                &transaction,
                &child_id,
                command_id,
                &child_command,
                &encoded,
                &applied_at,
            )?;
            thread_ids.push(child_id);
        }
        record_projected_sequence(&transaction, sequence, &applied_at)?;
        transaction.execute(
            INSERT_RECEIPT,
            params![
                command_id,
                thread_id,
                COMMAND,
                applied_at,
                sequence,
                "accepted",
                None::<String>
            ],
        )?;
        transaction.commit()?;
        Ok((
            CommittedThread {
                sequence: sequence as u64,
                committed: true,
            },
            thread_ids,
        ))
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

fn unarchive_thread(
    transaction: &Transaction<'_>,
    thread_id: &str,
    command_id: &str,
    event_command: &str,
    encoded: &str,
    now: &str,
) -> rusqlite::Result<i64> {
    let mut projection = super::archive::decode_projection(encoded)?;
    projection["archivedAt"] = Value::Null;
    projection["updatedAt"] = json!(now);
    ThreadEventStream::command_with_event_source(transaction, thread_id, command_id, event_command)
        .append(
            "thread.unarchived",
            now,
            &json!({"providerInstanceId":projection["providerInstanceId"]}),
            &projection,
        )
}

const SELECT_THREAD: &str = "
    SELECT payload_json, deleted_at, archived_at
    FROM orchestration_v2_projection_threads WHERE thread_id=?1";
