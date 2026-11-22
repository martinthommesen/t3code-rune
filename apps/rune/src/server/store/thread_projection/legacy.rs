use rusqlite::{Connection, Row, Transaction, TransactionBehavior, params};
use serde_json::json;
use time::{Date, OffsetDateTime, format_description::well_known::Rfc3339};

use crate::server::migrations::citation_search::readable_search_text;

use super::{legacy_fields, read_error};

const BATCH_SIZE: usize = 50;

const LEGACY_TRANSCRIPT_PENDING: &str = "
    SELECT EXISTS (
        SELECT 1 FROM orchestration_v2_legacy_imports
        WHERE thread_id = ?1 AND transcript_imported_at IS NULL
    )";

const MISSING_MESSAGES: &str = "
    WITH legacy AS (
        SELECT message_id, role, text, attachments_json, context_json, is_streaming,
               created_at, updated_at, row_number() OVER (ORDER BY created_at, message_id) AS ordinal
        FROM projection_thread_messages
        WHERE thread_id = ?1 AND role IN ('user', 'assistant')
    )
    SELECT * FROM legacy
    WHERE NOT EXISTS (
        SELECT 1 FROM orchestration_events
        WHERE event_id = 'migration:v1:message:' || legacy.message_id
          AND aggregate_kind = 'thread' AND stream_id = ?1 AND application_event_version = 2
    )
    ORDER BY ordinal";

const SHELL_MESSAGES: &str = "
    SELECT message_id, role, text, attachments_json, context_json, is_streaming,
           created_at, updated_at,
           (SELECT count(*) FROM projection_thread_messages AS earlier
            WHERE earlier.thread_id = ?1 AND earlier.role IN ('user', 'assistant')
              AND (earlier.created_at, earlier.message_id) <= (message.created_at, message.message_id))
    FROM projection_thread_messages AS message
    WHERE message.thread_id = ?1 AND message.message_id IN (
        (SELECT message_id FROM projection_thread_messages
         WHERE thread_id = ?1 AND role = 'user' ORDER BY created_at DESC, message_id DESC LIMIT 1),
        (SELECT message_id FROM projection_thread_messages WHERE thread_id = ?1
           AND role IN ('user', 'assistant') ORDER BY created_at DESC, message_id DESC LIMIT 1)
    )
    ORDER BY created_at, message_id";

const HAS_MESSAGE_EVENT: &str = "
    SELECT EXISTS (
        SELECT 1 FROM orchestration_events
        WHERE event_id = ?1 AND aggregate_kind = 'thread'
          AND stream_id = ?2 AND application_event_version = 2
    )";

const APPEND_EVENT: &str = "
    INSERT INTO orchestration_events (
        event_id, aggregate_kind, stream_id, stream_version, event_type,
        occurred_at, command_id, causation_event_id, correlation_id, actor_kind,
        payload_json, metadata_json, application_event_version
    ) VALUES (
        ?1, 'thread', ?2,
        COALESCE((SELECT MAX(stream_version) + 1 FROM orchestration_events
                  WHERE aggregate_kind = 'thread' AND stream_id = ?2), 0),
        ?3, ?4, NULL, NULL, NULL, 'server', ?5, ?6, 2
    ) RETURNING sequence";

const STORE_MESSAGE: &str = "
    INSERT INTO orchestration_v2_projection_messages (
        message_id, thread_id, run_id, node_id, role, streaming,
        created_at, updated_at, payload_json, search_text
    ) VALUES (?1, ?2, NULL, NULL, ?3, 0, ?4, ?5, ?6, ?7)
    ON CONFLICT(message_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        run_id = NULL,
        node_id = NULL,
        role = excluded.role,
        streaming = 0,
        created_at = excluded.created_at,
        updated_at = excluded.updated_at,
        payload_json = excluded.payload_json,
        search_text = excluded.search_text";

const STORE_ITEM: &str = "
    INSERT INTO orchestration_v2_projection_turn_items (
        turn_item_id, thread_id, run_id, node_id, provider_thread_id, provider_turn_id,
        parent_item_id, ordinal, type, status, updated_at, payload_json, inline_work_output
    ) VALUES (?1, ?2, NULL, NULL, NULL, NULL, NULL, ?3, ?4, ?5, ?6, ?7, 0)
    ON CONFLICT(turn_item_id) DO UPDATE SET
        thread_id = excluded.thread_id,
        run_id = NULL,
        node_id = NULL,
        provider_thread_id = NULL,
        provider_turn_id = NULL,
        parent_item_id = NULL,
        ordinal = excluded.ordinal,
        type = excluded.type,
        status = excluded.status,
        updated_at = excluded.updated_at,
        payload_json = excluded.payload_json,
        inline_work_output = 0";

const UPDATE_PROJECTION_SEQUENCE: &str = "
    INSERT INTO orchestration_v2_projection_metadata (
        projection_name, schema_version, last_sequence, updated_at
    ) VALUES ('thread-projections', 2, ?1, ?2)
    ON CONFLICT(projection_name) DO UPDATE SET
        schema_version = excluded.schema_version,
        last_sequence = excluded.last_sequence,
        updated_at = excluded.updated_at";

#[derive(Debug, Default)]
pub(in crate::server) struct Hydration {
    pub(in crate::server) imported_thread_count: usize,
    pub(in crate::server) imported_message_count: usize,
}

pub(in crate::server) fn import_shell_previews(
    transaction: &Transaction<'_>,
    thread_id: &str,
    imported_at: OffsetDateTime,
) -> rusqlite::Result<usize> {
    let mut statement = transaction.prepare(SHELL_MESSAGES)?;
    let messages = statement.query_map([thread_id], LegacyMessage::from_row)?;
    let mut count = 0;
    for message in messages {
        let message = message?;
        let event_id = format!("migration:v1:message:{}", message.id);
        message.apply(transaction, thread_id, &event_id, imported_at)?;
        count += 1;
    }
    Ok(count)
}

pub(in crate::server) fn append_import_event(
    transaction: &Transaction<'_>,
    event_id: &str,
    thread_id: &str,
    event_type: &str,
    at: &str,
    payload: &str,
    metadata: &str,
) -> rusqlite::Result<i64> {
    transaction.query_row(
        APPEND_EVENT,
        params![event_id, thread_id, event_type, at, payload, metadata],
        |row| row.get(0),
    )
}

/// Hydrates the requested legacy transcript before the caller opens its read snapshot.
pub(in crate::server) fn hydrate(
    database: &mut Connection,
    thread_id: &str,
) -> rusqlite::Result<Hydration> {
    let mut summary = Hydration::default();
    if !is_pending(database, thread_id)? {
        return Ok(summary);
    }
    let imported_at = OffsetDateTime::now_utc();
    let messages = database
        .prepare(MISSING_MESSAGES)?
        .query_map([thread_id], LegacyMessage::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for batch in messages.chunks(BATCH_SIZE) {
        let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !is_pending(&transaction, thread_id)? {
            return Ok(summary);
        }
        let mut last = None;
        for message in batch {
            let event_id = format!("migration:v1:message:{}", message.id);
            if transaction.query_row(HAS_MESSAGE_EVENT, params![event_id, thread_id], |row| {
                row.get(0)
            })? {
                continue;
            }
            last = Some(message.apply(&transaction, thread_id, &event_id, imported_at)?);
            summary.imported_message_count += 1;
        }
        if let Some((occurred_at, sequence)) = last {
            transaction.execute(
                "UPDATE orchestration_v2_projection_threads
                 SET updated_at = ?2, payload_json = json_set(payload_json, '$.updatedAt', ?2)
                 WHERE thread_id = ?1",
                params![thread_id, occurred_at],
            )?;
            transaction.execute(
                UPDATE_PROJECTION_SEQUENCE,
                params![sequence, timestamp(OffsetDateTime::now_utc())],
            )?;
        }
        transaction.commit()?;
    }
    let transaction = database.transaction_with_behavior(TransactionBehavior::Immediate)?;
    summary.imported_thread_count = transaction.execute(
        "UPDATE orchestration_v2_legacy_imports
         SET transcript_imported_at = ?2, imported_message_count = (
             SELECT count(*) FROM projection_thread_messages
             WHERE thread_id = ?1 AND role IN ('user', 'assistant')
         ), last_error = NULL
         WHERE thread_id = ?1 AND transcript_imported_at IS NULL",
        params![thread_id, timestamp(OffsetDateTime::now_utc())],
    )?;
    transaction.commit()?;
    Ok(summary)
}

fn is_pending(database: &Connection, thread_id: &str) -> rusqlite::Result<bool> {
    database.query_row(LEGACY_TRANSCRIPT_PENDING, [thread_id], |row| row.get(0))
}

struct LegacyMessage {
    id: String,
    role: String,
    text: String,
    attachments: Option<String>,
    context: Option<String>,
    streaming: bool,
    created_at: String,
    updated_at: String,
    ordinal: i64,
}

impl LegacyMessage {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            role: row.get(1)?,
            text: row.get(2)?,
            attachments: row.get(3)?,
            context: row.get(4)?,
            streaming: row.get::<_, i64>(5)? == 1,
            created_at: row.get(6)?,
            updated_at: row.get(7)?,
            ordinal: row.get(8)?,
        })
    }

    fn apply(
        &self,
        transaction: &Transaction<'_>,
        thread_id: &str,
        message_event_id: &str,
        imported_at: OffsetDateTime,
    ) -> rusqlite::Result<(String, i64)> {
        let updated = parse_timestamp(&self.updated_at);
        let created = parse_timestamp(&self.created_at)
            .or(updated)
            .unwrap_or(imported_at);
        let updated_at = timestamp(updated.unwrap_or(created));
        let created_at = timestamp(created);
        let message_id = self.id.trim();
        if message_id.is_empty() {
            return Err(read_error("The legacy transcript has an empty message ID."));
        }
        let attachments = legacy_fields::attachments(self.attachments.as_deref())?;
        let context = legacy_fields::context(self.context.as_deref())?;
        let user = self.role == "user";
        let item_type = if user {
            "user_message"
        } else {
            "assistant_message"
        };
        let item_id = format!("migration:v1:turn-item:{}", self.id);
        let status = if self.streaming {
            "interrupted"
        } else {
            "completed"
        };
        transaction.execute(
            "INSERT INTO orchestration_v2_turn_item_positions (thread_id, turn_item_id, ordinal)
             VALUES (?1, ?2, ?3) ON CONFLICT(thread_id, turn_item_id) DO NOTHING",
            params![thread_id, item_id, self.ordinal],
        )?;
        let ordinal: i64 = transaction.query_row(
            "SELECT ordinal FROM orchestration_v2_turn_item_positions
             WHERE thread_id = ?1 AND turn_item_id = ?2",
            params![thread_id, item_id],
            |row| row.get(0),
        )?;
        let mut message = json!({
            "createdBy": if user { "user" } else { "agent" },
            "creationSource": "server",
            "id": message_id,
            "threadId": thread_id,
            "runId": null,
            "nodeId": null,
            "role": self.role,
            "text": self.text,
            "attachments": attachments,
            "streaming": false,
            "createdAt": created_at,
            "updatedAt": updated_at,
        });
        let mut item = json!({
            "id": item_id,
            "threadId": thread_id,
            "runId": null,
            "nodeId": null,
            "providerThreadId": null,
            "providerTurnId": null,
            "nativeItemRef": null,
            "parentItemId": null,
            "ordinal": ordinal,
            "status": status,
            "title": null,
            "startedAt": created_at,
            "completedAt": updated_at,
            "updatedAt": updated_at,
            "type": item_type,
            "messageId": message_id,
            "text": self.text,
        });
        if user {
            item["createdBy"] = "user".into();
            item["creationSource"] = "server".into();
            item["inputIntent"] = "turn_start".into();
            item["attachments"] = attachments.into();
        } else {
            item["streaming"] = false.into();
        }
        if let Some(context) = context {
            message["context"] = context.clone();
            item["context"] = context;
        }
        let message = message.to_string();
        let item = item.to_string();
        append_import_event(
            transaction,
            message_event_id,
            thread_id,
            "message.updated",
            &updated_at,
            &message,
            "{}",
        )?;
        transaction.execute(
            STORE_MESSAGE,
            params![
                message_id,
                thread_id,
                self.role,
                created_at,
                updated_at,
                message,
                readable_search_text(&self.text),
            ],
        )?;
        let sequence = append_import_event(
            transaction,
            &item_id,
            thread_id,
            "turn-item.updated",
            &updated_at,
            &item,
            "{}",
        )?;
        transaction.execute(
            STORE_ITEM,
            params![
                item_id, thread_id, ordinal, item_type, status, updated_at, item
            ],
        )?;
        Ok((updated_at, sequence))
    }
}

pub(in crate::server) fn parse_timestamp(timestamp: &str) -> Option<OffsetDateTime> {
    let timestamp = timestamp.trim();
    if let Ok(parsed) = OffsetDateTime::parse(timestamp, &Rfc3339) {
        return Some(parsed.to_offset(time::UtcOffset::UTC));
    }
    let date = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]").ok()?;
    Date::parse(timestamp, &date)
        .ok()
        .map(|date| date.midnight().assume_utc())
}

fn timestamp(value: OffsetDateTime) -> String {
    let value = value.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        value.year(),
        u8::from(value.month()),
        value.day(),
        value.hour(),
        value.minute(),
        value.second(),
        value.millisecond(),
    )
}
