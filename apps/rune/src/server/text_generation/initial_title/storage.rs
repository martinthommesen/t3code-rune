use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::server::turn_commands::{
    ThreadEventStream, record_projected_sequence, timestamp, title_effect,
};

const COMPLETE_COMMAND: &str = "thread.title.regeneration.complete";

pub(super) struct Claim {
    effect_id: String,
    pub(super) command_id: String,
    pub(super) thread_id: String,
    message_id: String,
}

pub(super) struct Context {
    pub(super) project_id: String,
    pub(super) project_title: Option<String>,
    pub(super) workspace_root: Option<String>,
    pub(super) worktree: Option<String>,
    pub(super) message: String,
}

pub(super) fn recover(database: &Path) -> rusqlite::Result<()> {
    let now = timestamp(OffsetDateTime::now_utc());
    connect(database)?.execute(
        "UPDATE orchestration_v2_effect_outbox
         SET status = 'pending', lease_owner = NULL, available_at = ?1, updated_at = ?1,
             last_error = 'Requeued after the previous server process ended.'
         WHERE status = 'running' AND effect_type = 'thread-title.generate'
           AND CASE WHEN json_valid(payload_json)
               THEN json_extract(payload_json, '$.kind.type') = 'initial' ELSE 0 END",
        [&now],
    )?;
    Ok(())
}

pub(super) fn claim(database: &Path, worker: &str) -> rusqlite::Result<Option<Claim>> {
    let now = timestamp(OffsetDateTime::now_utc());
    connect(database)?.query_row(
        "UPDATE orchestration_v2_effect_outbox
         SET status = 'running', attempt_count = attempt_count + 1, lease_owner = ?2,
             updated_at = ?1, last_error = NULL
         WHERE effect_id = (
             SELECT candidate.effect_id FROM orchestration_v2_effect_outbox AS candidate
             JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = candidate.thread_id
             LEFT JOIN orchestration_v2_projection_messages AS message
                 ON message.thread_id = candidate.thread_id
                 AND message.message_id = CASE WHEN json_valid(candidate.payload_json)
                     THEN json_extract(candidate.payload_json, '$.kind.messageId') END
             WHERE candidate.effect_type = 'thread-title.generate'
               AND candidate.status = 'pending' AND candidate.available_at <= ?1
               AND CASE WHEN json_valid(candidate.payload_json) THEN
                     json_extract(candidate.payload_json, '$.kind.type') = 'initial'
                     AND json_type(candidate.payload_json, '$.kind.messageId') = 'text'
                   ELSE 0 END
               AND thread.deleted_at IS NULL
               AND CASE WHEN json_valid(thread.payload_json) THEN
                     json_extract(thread.payload_json, '$.forkedFrom') IS NULL
                     AND json_extract(thread.payload_json, '$.lineage.parentThreadId') IS NULL
                     AND json_extract(thread.payload_json, '$.providerInstanceId') = 'codex'
                   ELSE 0 END
               AND (message.message_id IS NULL OR CASE WHEN json_valid(message.payload_json) THEN
                   coalesce(json_array_length(message.payload_json, '$.attachments'), 0) = 0 ELSE 0 END)
               AND NOT EXISTS (
                   SELECT 1 FROM orchestration_v2_effect_outbox AS earlier
                   WHERE earlier.thread_id = candidate.thread_id AND earlier.effect_type = 'thread-title.generate'
                     AND (earlier.status = 'running' OR (earlier.status = 'pending' AND earlier.rowid < candidate.rowid))
               )
             ORDER BY candidate.available_at, candidate.created_at, candidate.effect_id LIMIT 1
         )
         RETURNING effect_id, command_id, thread_id, json_extract(payload_json, '$.kind.messageId')",
        params![now, worker],
        |row| Ok(Claim {
            effect_id: row.get(0)?, command_id: row.get(1)?, thread_id: row.get(2)?, message_id: row.get(3)?,
        }),
    ).optional()
}

pub(super) fn load(database: &Path, claim: &Claim) -> rusqlite::Result<Option<Context>> {
    connect(database)?.query_row(
        "SELECT thread.project_id, project.title, project.workspace_root,
             json_extract(thread.payload_json, '$.worktreePath'),
             coalesce(json_extract(message.payload_json, '$.text'), '')
         FROM orchestration_v2_projection_threads AS thread
         LEFT JOIN projection_projects AS project ON project.project_id = thread.project_id AND project.deleted_at IS NULL
         LEFT JOIN orchestration_v2_projection_messages AS message
             ON message.thread_id = thread.thread_id AND message.message_id = ?3 AND message.streaming = 0
         WHERE thread.thread_id = ?1 AND thread.deleted_at IS NULL
           AND json_extract(thread.payload_json, '$.titleRegeneration.requestId') = ?2",
        params![claim.thread_id, claim.command_id, claim.message_id],
        |row| Ok(Context {
            project_id: row.get(0)?, project_title: row.get(1)?, workspace_root: row.get(2)?,
            worktree: row.get(3)?, message: row.get(4)?,
        }),
    ).optional()
}

pub(super) enum Completion {
    Stale,
    Complete(Option<String>),
}

pub(super) struct Changed {
    pub(super) apply_title: bool,
}

pub(super) fn complete(
    database: &Path,
    worker: &str,
    claim: &Claim,
    outcome: Completion,
) -> rusqlite::Result<Option<Changed>> {
    let mut database = connect(database)?;
    let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = timestamp(OffsetDateTime::now_utc());
    let owned = transaction.execute(
        "UPDATE orchestration_v2_effect_outbox
         SET status = 'succeeded', lease_owner = NULL, completed_at = ?3, updated_at = ?3, last_error = NULL
         WHERE effect_id = ?1 AND lease_owner = ?2 AND status = 'running'",
        params![claim.effect_id, worker, now],
    )?;
    if owned == 0 {
        return Ok(None);
    }
    let Completion::Complete(title) = outcome else {
        transaction.commit()?;
        return Ok(None);
    };
    let completion = format!("{}:title-complete", claim.command_id);
    let receipt = transaction.query_row(
        "SELECT aggregate_kind, aggregate_id, command_type, status FROM orchestration_command_receipts
         WHERE command_id = ?1", [&completion],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?)),
    ).optional()?;
    if let Some((kind, thread, command, status)) = receipt {
        if kind != "thread"
            || thread != claim.thread_id
            || command != COMPLETE_COMMAND
            || status != "accepted"
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        transaction.commit()?;
        return Ok(None);
    }
    let encoded: String = transaction.query_row(
        "SELECT payload_json FROM orchestration_v2_projection_threads WHERE thread_id = ?1 AND deleted_at IS NULL",
        [&claim.thread_id], |row| row.get(0),
    )?;
    let mut projection: Value = serde_json::from_str(&encoded)
        .map_err(|cause| rusqlite::Error::ToSqlConversionFailure(Box::new(cause)))?;
    let mut changed_title = false;
    if projection["titleRegeneration"]["requestId"] == claim.command_id {
        projection["titleRegeneration"] = Value::Null;
        projection["updatedAt"] = json!(now);
        if let Some(title) = title {
            changed_title = projection["title"] != title;
            projection["title"] = json!(title);
            projection["titleRenamed"] = Value::Bool(false);
        }
    }
    let sequence = ThreadEventStream::command(&transaction, &claim.thread_id, &completion).append(
        "thread.metadata-updated",
        &now,
        &json!({"providerInstanceId": projection["providerInstanceId"]}),
        &projection,
    )?;
    if changed_title {
        let title = projection["title"]
            .as_str()
            .ok_or(rusqlite::Error::InvalidQuery)?;
        transaction.execute(
            "UPDATE orchestration_v2_projection_threads SET title = ?2 WHERE thread_id = ?1",
            params![claim.thread_id, title],
        )?;
        title_effect::enqueue(&transaction, &claim.thread_id, &completion, title, &now)?;
    }
    record_projected_sequence(&transaction, sequence, &now)?;
    transaction.execute(
        "INSERT INTO orchestration_command_receipts (
            command_id, aggregate_kind, aggregate_id, command_type, accepted_at, result_sequence, status, error
         ) VALUES (?1, 'thread', ?2, ?3, ?4, ?5, 'accepted', NULL)",
        params![completion, claim.thread_id, COMPLETE_COMMAND, now, sequence],
    )?;
    transaction.commit()?;
    Ok(Some(Changed {
        apply_title: changed_title,
    }))
}

fn connect(database: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    Ok(connection)
}
