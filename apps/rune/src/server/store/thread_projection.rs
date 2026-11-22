use std::collections::HashSet;
use std::io;

use rusqlite::{Connection, OptionalExtension, Row, types::Type};
use serde_json::{Map, Value, json};

mod inherited;
mod legacy;
mod legacy_fields;
mod timeline;
mod wire;

pub(super) use legacy::hydrate as hydrate_legacy;
pub(in crate::server) use legacy::{append_import_event, import_shell_previews, parse_timestamp};
pub(in crate::server) use legacy_fields::attachment as decode_legacy_attachment;
pub(super) use wire::snapshot as snapshot_for_wire;

const THREAD: &str = "
    SELECT payload_json FROM orchestration_v2_projection_threads WHERE thread_id = ?1";

const PROVIDER_SESSIONS: &str = "
    SELECT session.payload_json
    FROM orchestration_v2_projection_provider_sessions AS session
    JOIN orchestration_v2_projection_provider_session_bindings AS binding
      ON binding.provider_session_id = session.provider_session_id
    WHERE binding.thread_id = ?1
    ORDER BY session.updated_at ASC, session.provider_session_id ASC";

const PROVIDER_THREADS: &str = "
    SELECT payload_json
    FROM orchestration_v2_projection_provider_threads
    WHERE thread_id = ?1
       OR owner_node_id IN (
           SELECT owner.node_id
           FROM orchestration_v2_projection_provider_threads AS owned
           CROSS JOIN orchestration_v2_projection_nodes AS owner
             ON owner.node_id = owned.owner_node_id
           WHERE owned.owner_node_id IS NOT NULL AND owner.thread_id = ?1
       )
       OR provider_thread_id IN (
           SELECT provider_thread_id
           FROM orchestration_v2_projection_subagents
           WHERE thread_id = ?1 AND provider_thread_id IS NOT NULL
       )
    ORDER BY COALESCE(first_run_ordinal, 0), provider_thread_id ASC";

const CONTEXT_TRANSFERS: &str = "
    SELECT payload_json FROM orchestration_v2_projection_context_transfers
    WHERE source_thread_id = ?1 OR target_thread_id = ?1
    ORDER BY rowid ASC";

const RECORDS: &[(&str, &str, &str)] = &[
    ("runs", "runs", "ordinal ASC"),
    (
        "attempts",
        "run_attempts",
        "run_id ASC, attempt_ordinal ASC",
    ),
    ("nodes", "nodes", "COALESCE(started_at, ''), node_id ASC"),
    (
        "subagents",
        "subagents",
        "COALESCE(started_at, ''), subagent_id ASC",
    ),
    (
        "providerTurns",
        "provider_turns",
        "provider_thread_id ASC, ordinal ASC",
    ),
    (
        "runtimeRequests",
        "runtime_requests",
        "created_at ASC, runtime_request_id ASC",
    ),
    ("messages", "messages", "created_at ASC, message_id ASC"),
    ("plans", "plans", "plan_id ASC"),
    ("turnItems", "turn_items", "ordinal ASC, turn_item_id ASC"),
    (
        "checkpointScopes",
        "checkpoint_scopes",
        "ordinal_within_parent ASC, scope_id ASC",
    ),
    (
        "checkpoints",
        "checkpoints",
        "scope_id ASC, ordinal_within_scope ASC",
    ),
    ("contextHandoffs", "context_handoffs", "rowid ASC"),
];

// All records are read from the caller's snapshot, including inherited history.
pub(super) fn read(database: &Connection, thread_id: &str) -> rusqlite::Result<Option<Value>> {
    let Some(thread) = read_local(database, thread_id)? else {
        return Ok(None);
    };
    let mut ancestry = vec![thread];
    let mut seen = HashSet::new();
    loop {
        let current = ancestry
            .last()
            .expect("thread chain starts with the requested thread");
        let Some(source) = inherited::source(&current["thread"])? else {
            break;
        };
        if seen.contains(source.thread_id) {
            break;
        }
        seen.insert(required_string(&current["thread"], "id")?.to_owned());
        let projection = read_local(database, source.thread_id)?
            .ok_or_else(|| read_error("The fork source thread was not found."))?;
        ancestry.push(projection);
    }
    let mut projection = ancestry.pop().expect("thread chain is never empty");
    while let Some(mut target) = ancestry.pop() {
        inherited::merge(&mut target, &projection)?;
        projection = target;
    }
    Ok(Some(projection))
}

fn read_local(database: &Connection, thread_id: &str) -> rusqlite::Result<Option<Value>> {
    let Some(mut thread) = database
        .query_row(THREAD, [thread_id], payload)
        .optional()?
    else {
        return Ok(None);
    };
    if required_string(&thread, "id")? != thread_id {
        return Err(read_error(
            "Thread payload ID does not match the projection row.",
        ));
    }
    insert_defaults(
        &mut thread,
        &["settledOverride", "settledAt", "lastVisitedAt"],
    );
    let updated_at = required_string(&thread, "updatedAt")?.to_owned();
    let mut projection = Map::new();
    projection.insert("thread".to_owned(), thread);
    for &(field, table, order) in RECORDS {
        let query = format!(
            "SELECT payload_json FROM orchestration_v2_projection_{table}
             WHERE thread_id = ?1 ORDER BY {order}"
        );
        projection.insert(field.to_owned(), rows(database, &query, thread_id)?.into());
    }
    let mut sessions = rows(database, PROVIDER_SESSIONS, thread_id)?;
    for session in &mut sessions {
        insert_default(session, "extensionStatuses", json!([]));
    }
    let mut provider_threads = rows(database, PROVIDER_THREADS, thread_id)?;
    for provider_thread in &mut provider_threads {
        insert_default(provider_thread, "pendingBackgroundTasks", json!([]));
        insert_defaults(provider_thread, &["contextUsage", "nativeMetadata", "goal"]);
    }

    let mut messages = std::mem::take(
        projection
            .get_mut("messages")
            .expect("messages are a projection record set"),
    );
    let items = records(&projection, "turnItems");
    let visible = timeline::visible(
        records(&projection, "runs"),
        records(&projection, "attempts"),
        items,
    )?;
    timeline::sort_messages(
        messages
            .as_array_mut()
            .expect("projection records are arrays"),
        items,
    )?;

    projection.insert("messages".to_owned(), messages);
    projection.insert("visibleTurnItems".to_owned(), visible.into());
    projection.insert("providerSessions".to_owned(), sessions.into());
    projection.insert("providerThreads".to_owned(), provider_threads.into());
    projection.insert(
        "contextTransfers".to_owned(),
        rows(database, CONTEXT_TRANSFERS, thread_id)?.into(),
    );
    projection.insert("updatedAt".to_owned(), updated_at.into());
    Ok(Some(projection.into()))
}

fn records<'a>(projection: &'a Map<String, Value>, key: &str) -> &'a [Value] {
    projection[key]
        .as_array()
        .expect("projection records are arrays")
}

fn rows(database: &Connection, query: &str, thread_id: &str) -> rusqlite::Result<Vec<Value>> {
    database
        .prepare(query)?
        .query_map([thread_id], payload)?
        .collect()
}

fn payload(row: &Row<'_>) -> rusqlite::Result<Value> {
    let raw = row.get_ref(0)?.as_str()?;
    let value: Value = serde_json::from_str(raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
    })?;
    if !value.is_object() {
        return Err(read_error("Projection payload must be an object."));
    }
    Ok(value)
}

fn insert_defaults(record: &mut Value, fields: &[&str]) {
    for &key in fields {
        insert_default(record, key, Value::Null);
    }
}

fn insert_default(record: &mut Value, key: &str, default: Value) {
    record
        .as_object_mut()
        .expect("projection payload is an object")
        .entry(key.to_owned())
        .or_insert(default);
}

fn required_string<'a>(record: &'a Value, key: &str) -> rusqlite::Result<&'a str> {
    record[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| read_error("Projection payload is missing a required string."))
}

fn read_error(message: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        Type::Text,
        Box::new(io::Error::new(io::ErrorKind::InvalidData, message)),
    )
}

#[cfg(test)]
mod tests;
