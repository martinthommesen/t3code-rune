use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;

use super::super::provider_session::SHARED_CODEX_SESSION;
use super::super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, record_projected_sequence,
    timestamp,
};

impl TurnCommands {
    pub(in crate::server) fn recover_stopped_approvals(
        &self,
    ) -> Result<Vec<String>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut pending = BTreeMap::<String, Vec<(String, String)>>::new();
        {
            let mut query = transaction.prepare(include_str!("released_approvals.sql"))?;
            let rows =
                query.query_map(params![Option::<&str>::None, SHARED_CODEX_SESSION], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?;
            for row in rows {
                let (thread, node, payload) = row?;
                pending.entry(thread).or_default().push((node, payload));
            }
        }
        let mut changed = Vec::with_capacity(pending.len());
        for (thread, requests) in pending {
            let now = timestamp(OffsetDateTime::now_utc());
            let command = format!("command:runtime-reconcile:startup:{thread}:{now}");
            let stream = ThreadEventStream::command(&transaction, &thread, &command);
            let mut sequence = 0;
            for (node, request) in requests {
                let payload = released_request(
                    &request,
                    "expired",
                    "The server restarted before this runtime request was resolved.",
                    &now,
                )?;
                sequence = stream.append(
                    "runtime-request.updated",
                    &now,
                    &json!({"nodeId":node}),
                    &payload,
                )?;
            }
            let session = transaction.query_row(
                "SELECT session.payload_json FROM orchestration_v2_projection_provider_sessions AS session
                 JOIN orchestration_v2_projection_provider_session_bindings AS binding
                   ON binding.provider_session_id = session.provider_session_id
                 WHERE session.provider_session_id = ?1 AND binding.thread_id = ?2
                   AND session.status NOT IN ('stopped', 'error')",
                params![SHARED_CODEX_SESSION, thread], |row| row.get::<_, String>(0),
            ).optional()?;
            if let Some(session) = session {
                let mut payload: Map<String, Value> = serde_json::from_str(&session)?;
                payload.insert("status".into(), json!("stopped"));
                payload.insert("updatedAt".into(), json!(now));
                payload.insert("lastError".into(), Value::Null);
                if payload.contains_key("extensionStatuses") {
                    payload.insert("extensionStatuses".into(), json!([]));
                }
                sequence = stream.append(
                    "provider-session.updated",
                    &now,
                    &json!({"driver":"codex","providerInstanceId":CODEX_INSTANCE}),
                    &Value::Object(payload),
                )?;
            }
            transaction.execute(
                "INSERT INTO orchestration_command_receipts (
                    command_id, aggregate_kind, aggregate_id, accepted_at, result_sequence, status, command_type
                 ) VALUES (?1, 'thread', ?2, ?3, ?4, 'accepted', 'provider-runtime.reconcile')",
                params![command, thread, now, sequence],
            )?;
            record_projected_sequence(&transaction, sequence, &now)?;
            changed.push(thread);
        }
        transaction.commit()?;
        Ok(changed)
    }
}

pub(in crate::server::turn_commands) fn append_released_stop_approvals(
    transaction: &Transaction<'_>,
    stream: &ThreadEventStream<'_>,
    now: &str,
    failed: bool,
) -> Result<Option<i64>, TurnCommandError> {
    let pending = {
        let mut query = transaction.prepare(include_str!("released_approvals.sql"))?;
        query
            .query_map(params![stream.thread_id, SHARED_CODEX_SESSION], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let (request_status, work_status, reason) = if failed {
        (
            "expired",
            "failed",
            "Provider session failed before this runtime request was resolved.",
        )
    } else {
        (
            "cancelled",
            "cancelled",
            "Provider session was closed before this runtime request was resolved.",
        )
    };
    let mut sequence = None;
    for (run_id, node_id, request_json, node_json, card_json) in pending {
        let request = released_request(&request_json, request_status, reason, now)?;
        stream.append(
            "runtime-request.updated",
            now,
            &json!({"nodeId":node_id,"driver":"codex"}),
            &request,
        )?;
        let metadata = json!({"runId":run_id,"nodeId":node_id,"driver":"codex"});
        let mut node: Map<String, Value> = serde_json::from_str(&node_json)?;
        let mut card: Map<String, Value> = serde_json::from_str(&card_json)?;
        for projection in [&mut node, &mut card] {
            projection.insert("status".into(), json!(work_status));
            projection.insert("completedAt".into(), json!(now));
        }
        card.insert("updatedAt".into(), json!(now));
        stream.append("node.updated", now, &metadata, &Value::Object(node))?;
        sequence =
            Some(stream.append("turn-item.updated", now, &metadata, &Value::Object(card))?);
    }
    Ok(sequence)
}

fn released_request(
    encoded: &str,
    status: &str,
    reason: &str,
    now: &str,
) -> serde_json::Result<Value> {
    let mut request: Map<String, Value> = serde_json::from_str(encoded)?;
    request.insert("status".into(), json!(status));
    request.insert("resolvedAt".into(), json!(now));
    request.insert(
        "responseCapability".into(),
        json!({ "type":"not_resumable", "reason":reason }),
    );
    Ok(Value::Object(request))
}
