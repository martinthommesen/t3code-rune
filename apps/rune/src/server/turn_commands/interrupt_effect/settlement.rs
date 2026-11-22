use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use t3_rune::providers::codex::TurnKey;
use time::OffsetDateTime;

use super::super::provider_turn::provider_turn_id;
use super::super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, allocate_turn_item_in_run,
    record_projected_sequence, timestamp,
};

struct Run {
    id: String,
    node_id: String,
    ordinal: i64,
    scope_id: String,
    run: String,
    attempt: String,
    node: String,
    provider_thread: String,
    interrupt_request: String,
}

impl TurnCommands {
    pub(in crate::server) fn settle_requested_interrupt(
        &self,
        thread_id: &str,
        key: &TurnKey,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = transaction
            .query_row(
                include_str!("settle.sql"),
                params![
                    thread_id,
                    provider_turn_id(key),
                    key.thread_id.as_str(),
                    key.turn_id.as_str()
                ],
                |row| {
                    Ok(Run {
                        id: row.get(0)?,
                        node_id: row.get(1)?,
                        ordinal: row.get(2)?,
                        scope_id: row.get(3)?,
                        run: row.get(4)?,
                        attempt: row.get(5)?,
                        node: row.get(6)?,
                        provider_thread: row.get(7)?,
                        interrupt_request: row.get(8)?,
                    })
                },
            )
            .optional()?;
        let Some(state) = state else { return Ok(None) };
        let now = timestamp(OffsetDateTime::now_utc());
        let request: Value = serde_json::from_str(&state.interrupt_request)?;
        let node = interrupted(&state.node, &now)?;
        let mut provider_thread: Value = serde_json::from_str(&state.provider_thread)?;
        provider_thread["status"] = json!("idle");
        provider_thread["updatedAt"] = json!(now);
        let request_id = request["id"].as_str().ok_or(TurnCommandError::Unsupported(
            "The bound Stop request has no turn item ID.",
        ))?;
        let result_id = format!(
            "turn-item:run:{}:signal:interrupt-result",
            super::super::encode_id_part(&state.id)
        );
        let ordinal =
            allocate_turn_item_in_run(&transaction, thread_id, &result_id, state.ordinal)?;
        let stream = ThreadEventStream::server(&transaction, thread_id);
        let metadata = json!({ "runId": state.id, "nodeId": state.node_id, "providerInstanceId": CODEX_INSTANCE });
        stream.append(
            "run-attempt.updated",
            &now,
            &metadata,
            &interrupted(&state.attempt, &now)?,
        )?;
        stream.append(
            "turn-item.updated",
            &now,
            &metadata,
            &json!({
                "id": result_id, "threadId": thread_id, "runId": state.id, "nodeId": state.node_id,
                "providerThreadId": provider_thread["id"], "providerTurnId": node["providerTurnId"],
                "nativeItemRef": null, "parentItemId": request_id, "ordinal": ordinal,
                "status": "interrupted", "title": "Interrupted", "startedAt": now,
                "completedAt": now, "updatedAt": now, "type": "run_interrupt_result",
                "message": request["message"],
            }),
        )?;
        stream.append(
            "run.updated",
            &now,
            &metadata,
            &interrupted(&state.run, &now)?,
        )?;
        stream.append("node.updated", &now, &metadata, &node)?;
        let sequence = stream.append(
            "provider-thread.updated",
            &now,
            &json!({
                "providerInstanceId": CODEX_INSTANCE,
            }),
            &provider_thread,
        )?;
        let effect_id = format!("effect:checkpoint.capture:{}", state.id);
        let command_id = format!("command:{effect_id}");
        transaction.execute(
            "INSERT INTO orchestration_v2_effect_outbox (
                effect_id, command_id, thread_id, effect_type, payload_json, status, attempt_count,
                available_at, created_at, updated_at
             ) VALUES (?1, ?2, ?3, 'checkpoint.capture', ?4, 'pending', 0, ?5, ?5, ?5)",
            params![
                effect_id,
                command_id,
                thread_id,
                json!({"type":"checkpoint.capture","runId":state.id,"scopeId":state.scope_id})
                    .to_string(),
                now
            ],
        )?;
        record_projected_sequence(&transaction, sequence, &now)?;
        transaction.commit()?;
        Ok(Some(sequence))
    }
}

pub(super) fn interrupted(payload: &str, now: &str) -> Result<Value, serde_json::Error> {
    let mut value: Value = serde_json::from_str(payload)?;
    value["status"] = json!("interrupted");
    value["completedAt"] = json!(now);
    Ok(value)
}
