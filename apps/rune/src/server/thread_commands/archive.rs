use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::turn_commands::{ThreadEventStream, record_projected_sequence};

use super::delete::INSERT_EFFECT;
use super::{
    CommittedThread, INSERT_RECEIPT, LATEST_THREAD_SEQUENCE, Receipt, SELECT_RECEIPT,
    ThreadCommandError, ThreadCommands, now_timestamp, trim_js,
};

pub(super) const COMMAND: &str = "thread.archive";
const CLEANUP: &str = "terminal.cleanup";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Archive {
    #[serde(rename = "type")]
    _kind: String,
    command_id: String,
    thread_id: String,
}

impl ThreadCommands {
    pub(super) fn archive(&self, payload: Value) -> Result<CommittedThread, ThreadCommandError> {
        let input: Archive = serde_json::from_value(payload)
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
                    row.get::<_, bool>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            })
            .optional()?;
        let Some((encoded, deleted, archived, running_delegated_tasks, needs_live_cleanup)) =
            thread
        else {
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
        let rejection = if deleted {
            Some(format!("Thread {thread_id} is deleted."))
        } else if archived {
            Some(format!("Thread {thread_id} is already archived."))
        } else if running_delegated_tasks {
            Some(format!(
                "Thread {thread_id} has running delegated tasks. Wait for them or cancel them before archiving."
            ))
        } else if needs_live_cleanup {
            Some("This thread has activity that must finish before it can be archived.".to_owned())
        } else {
            None
        };
        if let Some(message) = rejection {
            record_rejection(
                &transaction,
                thread_id,
                command_id,
                &now,
                &format!("Failed to dispatch orchestration command {COMMAND} ({command_id})."),
            )?;
            transaction.commit()?;
            return Err(ThreadCommandError::Rejected(message));
        }
        let mut projection: Value = serde_json::from_str(&encoded)
            .map_err(|cause| rusqlite::Error::ToSqlConversionFailure(Box::new(cause)))?;
        if !projection.is_object() {
            return Err(rusqlite::Error::InvalidQuery.into());
        }
        projection["archivedAt"] = json!(now);
        projection["updatedAt"] = json!(now);
        projection["titleRegeneration"] = Value::Null;
        projection["guestShare"] = Value::Null;
        if let Some(links) = projection
            .get_mut("pullRequests")
            .and_then(Value::as_array_mut)
        {
            for link in links.iter_mut().filter_map(Value::as_object_mut) {
                if let Some(watch) = link.remove("watch") {
                    link.insert("endedWatch".to_owned(), watch);
                }
            }
        }
        let sequence = ThreadEventStream::command(&transaction, thread_id, command_id).append(
            "thread.archived",
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
        transaction.execute(
            INSERT_EFFECT,
            params![
                format!("effect:{command_id}:{CLEANUP}"),
                command_id,
                thread_id,
                CLEANUP,
                json!({"type":CLEANUP}).to_string(),
                now_timestamp(),
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
    SELECT thread.payload_json, thread.deleted_at IS NOT NULL, thread.archived_at IS NOT NULL,
        EXISTS (
            SELECT 1 FROM orchestration_v2_projection_subagents AS task
            WHERE task.thread_id = thread.thread_id AND task.origin = 'app_owned'
                AND task.child_thread_id IS NOT NULL
                AND task.status IN ('pending', 'running', 'waiting')
        ),
        json_extract(thread.payload_json, '$.guestShare') IS NOT NULL
        OR EXISTS (
            SELECT 1 FROM orchestration_v2_projection_runs AS run
            WHERE run.thread_id = thread.thread_id
                AND (run.status IN ('queued', 'preparing', 'starting', 'running', 'waiting')
                   OR json_extract(run.payload_json, '$.delegatedCompletion') IS NOT NULL)
        )
        OR EXISTS (
            SELECT 1 FROM orchestration_v2_projection_provider_sessions AS session
            WHERE session.thread_id = thread.thread_id AND session.status NOT IN ('stopped', 'error')
        )
        OR EXISTS (
            SELECT 1 FROM orchestration_v2_projection_turn_items AS item
            WHERE item.thread_id = thread.thread_id AND item.type = 'link_request' AND item.status = 'waiting'
        )
        OR EXISTS (
            SELECT 1 FROM orchestration_v2_projection_context_transfers AS transfer
            JOIN orchestration_v2_projection_threads AS child ON child.thread_id = transfer.target_thread_id
            WHERE transfer.source_thread_id = thread.thread_id AND transfer.type = 'fork'
                AND child.deleted_at IS NULL AND child.archived_at IS NULL
                AND json_extract(child.payload_json, '$.lineage.sideChat') = 1
        )
    FROM orchestration_v2_projection_threads AS thread WHERE thread.thread_id = ?1";
