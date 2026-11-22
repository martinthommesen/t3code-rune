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
    pub(in crate::server) fn archive(
        &self,
        payload: Value,
    ) -> Result<(CommittedThread, Vec<String>), ThreadCommandError> {
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
            return receipt
                .replay(command_id, thread_id)
                .map(|receipt| (receipt, Vec::new()));
        }
        let now = now_timestamp();
        let Some(parent) = load_target(&transaction, thread_id)? else {
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
        let rejection = if parent.deleted {
            Some(format!("Thread {thread_id} is deleted."))
        } else if parent.archived {
            Some(format!("Thread {thread_id} is already archived."))
        } else if parent.running_delegated_tasks {
            Some(format!(
                "Thread {thread_id} has running delegated tasks. Wait for them or cancel them before archiving."
            ))
        } else if parent.needs_live_cleanup {
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
        let mut members = Vec::new();
        for child_id in side_chats(&transaction, thread_id, None)? {
            let Some(child) = load_target(&transaction, &child_id)? else {
                continue;
            };
            if child.running_delegated_tasks {
                continue;
            }
            if child.needs_live_cleanup {
                return Err(ThreadCommandError::Rejected(
                    "A side chat has activity that must finish before it can be archived.".into(),
                ));
            }
            let child_command = format!("{command_id}:side-chat:{child_id}");
            members.push((child_id, child_command, child.encoded));
        }
        let mut applied_at = now_timestamp();
        let mut sequence = archive_thread(
            &transaction,
            thread_id,
            command_id,
            command_id,
            &parent.encoded,
            &applied_at,
        )?;
        let mut thread_ids = vec![thread_id.to_owned()];
        for (child_id, child_command, encoded) in members {
            applied_at = now_timestamp();
            sequence = archive_thread(
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

struct ArchiveTarget {
    encoded: String,
    deleted: bool,
    archived: bool,
    running_delegated_tasks: bool,
    needs_live_cleanup: bool,
}

fn load_target(
    transaction: &Transaction<'_>,
    thread_id: &str,
) -> rusqlite::Result<Option<ArchiveTarget>> {
    transaction
        .query_row(SELECT_THREAD, [thread_id], |row| {
            Ok(ArchiveTarget {
                encoded: row.get(0)?,
                deleted: row.get(1)?,
                archived: row.get(2)?,
                running_delegated_tasks: row.get(3)?,
                needs_live_cleanup: row.get(4)?,
            })
        })
        .optional()
}

fn archive_thread(
    transaction: &Transaction<'_>,
    thread_id: &str,
    command_id: &str,
    effect_command: &str,
    encoded: &str,
    now: &str,
) -> rusqlite::Result<i64> {
    let mut projection = decode_projection(encoded)?;
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
    let sequence = ThreadEventStream::command_with_event_source(
        transaction,
        thread_id,
        command_id,
        effect_command,
    )
    .append(
        "thread.archived",
        now,
        &json!({"providerInstanceId":projection["providerInstanceId"]}),
        &projection,
    )?;
    transaction.execute(
        INSERT_EFFECT,
        params![
            format!("effect:{effect_command}:{CLEANUP}"),
            effect_command,
            thread_id,
            CLEANUP,
            json!({"type":CLEANUP}).to_string(),
            now_timestamp(),
        ],
    )?;
    Ok(sequence)
}

pub(super) fn decode_projection(encoded: &str) -> rusqlite::Result<Value> {
    let projection: Value = serde_json::from_str(encoded)
        .map_err(|cause| rusqlite::Error::ToSqlConversionFailure(Box::new(cause)))?;
    if !projection.is_object() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(projection)
}

pub(super) fn side_chats(
    transaction: &Transaction<'_>,
    thread_id: &str,
    archived_at: Option<&str>,
) -> rusqlite::Result<Vec<String>> {
    transaction.prepare(
        "SELECT child.thread_id FROM orchestration_v2_projection_context_transfers AS transfer
         JOIN orchestration_v2_projection_threads AS child ON child.thread_id=transfer.target_thread_id
         WHERE transfer.source_thread_id=?1 AND transfer.type='fork' AND child.thread_id<>?1
           AND child.deleted_at IS NULL AND json_extract(child.payload_json,'$.lineage.sideChat')=1
           AND ((?2 IS NULL AND child.archived_at IS NULL)
                OR (julianday(child.archived_at)>=julianday(?2)))
         GROUP BY child.thread_id ORDER BY MIN(transfer.rowid)",
    )?.query_map(params![thread_id, archived_at], |row| row.get(0))?.collect()
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
    FROM orchestration_v2_projection_threads AS thread WHERE thread.thread_id = ?1";
