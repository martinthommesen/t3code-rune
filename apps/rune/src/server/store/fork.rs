use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{Message, message};

struct Source {
    thread_id: String,
    run_ordinal: i64,
    fork_created_at: String,
    imported_history: bool,
}

const SOURCE: &str = "
    SELECT source.thread_id, run.ordinal, child.created_at,
           json_extract(source.payload_json, '$.historyOrigin') IS 'v1_import'
    FROM orchestration_v2_projection_threads AS child
    JOIN orchestration_v2_projection_threads AS source
      ON source.thread_id = json_extract(child.payload_json, '$.forkedFrom.threadId')
    JOIN orchestration_v2_projection_runs AS run
      ON run.thread_id = source.thread_id
      AND run.run_id = json_extract(child.payload_json, '$.forkedFrom.runId')
    WHERE child.thread_id = ?1
      AND json_extract(child.payload_json, '$.forkedFrom.type') IS 'run'
      AND json_extract(child.payload_json, '$.lineage.sideChat') IS NOT 1";

const MESSAGES: &str = "
    SELECT json_extract(item.payload_json, '$.messageId') AS message_id,
           CASE item.type WHEN 'user_message' THEN 'user' ELSE 'assistant' END AS role,
           json_extract(item.payload_json, '$.text') AS text,
           COALESCE(message.created_at, json_extract(item.payload_json, '$.startedAt'), item.updated_at),
           item.updated_at,
           json_extract(message.payload_json, '$.createdBy')
    FROM orchestration_v2_projection_turn_items AS item
    LEFT JOIN orchestration_v2_projection_runs AS run ON run.run_id = item.run_id
    LEFT JOIN orchestration_v2_projection_messages AS message
      ON message.message_id = json_extract(item.payload_json, '$.messageId')
      AND message.thread_id = item.thread_id
    WHERE item.thread_id = ?1 AND item.type IN ('user_message', 'assistant_message')
      AND json_extract(item.payload_json, '$.streaming') IS NOT 1
      AND json_extract(item.payload_json, '$.workflowScript') IS NOT 1
      AND json_extract(item.payload_json, '$.idleCompaction') IS NULL
      AND (
          (item.run_id IS NULL AND ?4)
          OR (
             run.ordinal <= ?2 AND (
                run.status IS NOT 'rolled_back' OR run.completed_at IS NULL
                OR julianday(run.completed_at) >= julianday(?3)
             )
          )
      )
      AND NOT (
          item.type = 'user_message'
          AND json_extract(item.payload_json, '$.inputIntent') IS 'queued_turn'
          AND (
              run.status IS 'queued' OR run.status IS 'cancelled'
              OR (run.status IS 'rolled_back' AND EXISTS (
                  SELECT 1 FROM orchestration_v2_projection_run_attempts AS attempt
                  WHERE attempt.run_id = run.run_id AND attempt.status IS 'cancelled'
              ))
          )
      )
    ORDER BY item.ordinal, item.turn_item_id";

pub(super) fn verify_schema(database: &Connection) -> rusqlite::Result<()> {
    database.prepare(SOURCE)?;
    database.prepare(MESSAGES)?;
    Ok(())
}

pub(super) fn read_messages(
    read: &Transaction<'_>,
    thread_id: &str,
) -> rusqlite::Result<Vec<Message>> {
    let mut sources = Vec::new();
    let mut seen = HashSet::from([thread_id.to_owned()]);
    let mut query = read.prepare(SOURCE)?;
    let mut source_for = |id: &str| {
        query
            .query_row([id], |row| {
                Ok(Source {
                    thread_id: row.get(0)?,
                    run_ordinal: row.get(1)?,
                    fork_created_at: row.get(2)?,
                    imported_history: row.get(3)?,
                })
            })
            .optional()
    };
    let mut next = source_for(thread_id)?;
    while let Some(source) = next {
        if !seen.insert(source.thread_id.clone()) {
            return Err(rusqlite::Error::InvalidQuery);
        }
        next = source_for(&source.thread_id)?;
        sources.push(source);
    }

    let mut messages = Vec::new();
    if sources.is_empty() {
        return Ok(messages);
    }
    let mut query = read.prepare(MESSAGES)?;
    for source in sources.into_iter().rev() {
        let rows = query.query_map(
            params![
                source.thread_id,
                source.run_ordinal,
                source.fork_created_at,
                source.imported_history
            ],
            message,
        )?;
        for row in rows {
            messages.push(row?);
        }
    }
    Ok(messages)
}
