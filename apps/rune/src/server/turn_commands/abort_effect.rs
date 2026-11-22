use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Map, Value, json};
use t3_rune::providers::codex::SettlementStatus;
use time::OffsetDateTime;

use super::provider_turn::terminal_status;
use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, allocate_turn_item_in_run,
    encode_id_part, record_projected_sequence, timestamp,
};

pub(in crate::server) enum AbortReason<'a> {
    Startup,
    Shutdown,
    RuntimeFailure(&'a str),
    ProviderFailure {
        message: &'a str,
        code: Option<&'a str>,
    },
    NativeTerminal(SettlementStatus),
}

pub(in crate::server) struct AbortReceipt {
    pub(in crate::server) thread_id: String,
    pub(in crate::server) sequence: i64,
}

impl AbortReason<'_> {
    fn detail(&self) -> &str {
        match self {
            Self::Startup => {
                "Cancelled because the server restarted before the provider work completed."
            }
            Self::Shutdown => {
                "Cancelled because the server shut down before the provider work completed."
            }
            Self::RuntimeFailure(detail) => detail,
            Self::ProviderFailure { message, .. } => message,
            Self::NativeTerminal(SettlementStatus::Interrupted) => "Stopped by the provider",
            Self::NativeTerminal(_) => "Provider turn failed.",
        }
    }

    fn request_status(&self) -> &'static str {
        match self {
            Self::Shutdown | Self::NativeTerminal(_) | Self::ProviderFailure { .. } => "cancelled",
            Self::Startup | Self::RuntimeFailure(_) => "expired",
        }
    }

    fn notice_title(&self) -> &'static str {
        match self {
            Self::Startup => "Server restarted",
            Self::Shutdown => "Server shut down",
            Self::RuntimeFailure(_) => "Provider session ended",
            Self::ProviderFailure { .. } => "Provider error",
            Self::NativeTerminal(SettlementStatus::Interrupted) => "Interrupted",
            Self::NativeTerminal(_) => "Provider error",
        }
    }

    fn outcome(&self) -> &'static str {
        match self {
            Self::NativeTerminal(status) => terminal_status(*status),
            Self::ProviderFailure { .. } => "failed",
            _ => "cancelled",
        }
    }
}

impl TurnCommands {
    pub(in crate::server) fn recover_active_provider_runs(
        &self,
    ) -> Result<Vec<String>, TurnCommandError> {
        let ids = {
            let connection = self.connect()?;
            let mut query = connection.prepare(
                "SELECT effect.effect_id FROM orchestration_v2_effect_outbox AS effect
                 JOIN orchestration_v2_projection_runs AS run ON run.thread_id = effect.thread_id
                   AND run.run_id = json_extract(effect.payload_json, '$.runId')
                 WHERE effect.effect_type = 'provider-turn.start'
                   AND (effect.status = 'succeeded' OR (effect.status = 'running' AND run.provider_instance_id = 'claudeAgent'))
                   AND run.ordinal > 0 AND run.provider_instance_id IN ('codex', 'claudeAgent')
                   AND run.status = 'running'
                 ORDER BY effect.rowid",
            )?;
            query
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut changed = Vec::new();
        for effect_id in ids {
            if let Some(receipt) = self.abort_started_run(&effect_id, AbortReason::Startup)? {
                changed.push(receipt.thread_id);
            }
        }
        Ok(changed)
    }

    pub(in crate::server) fn abort_started_run(
        &self,
        effect_id: &str,
        reason: AbortReason<'_>,
    ) -> Result<Option<AbortReceipt>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let expected_native = match reason {
            AbortReason::NativeTerminal(status) => Some(terminal_status(status)),
            _ => None,
        };
        let state = transaction
            .query_row(ABORT_RUN, params![effect_id, expected_native], |row| {
                Ok(ActiveRun {
                    thread_id: row.get(0)?,
                    run_id: row.get(1)?,
                    node_id: row.get(2)?,
                    provider_thread_id: row.get(3)?,
                    provider_turn_id: row.get(4)?,
                    run: row.get(5)?,
                    attempt: row.get(6)?,
                    provider_turn: row.get(7)?,
                    provider_thread: row.get(8)?,
                    provider_status: row.get(9)?,
                    scope_id: row.get(10)?,
                    run_ordinal: row.get(11)?,
                    driver: row.get(12)?,
                })
            })
            .optional()?;
        let Some(state) = state else { return Ok(None) };
        let now = timestamp(OffsetDateTime::now_utc());
        let command_id = match reason {
            AbortReason::Startup => Some(format!(
                "command:runtime-reconcile:startup:{}:{now}",
                state.thread_id
            )),
            AbortReason::Shutdown => Some(format!(
                "command:runtime-reconcile:shutdown:{}:{now}",
                state.thread_id
            )),
            AbortReason::RuntimeFailure(_)
            | AbortReason::ProviderFailure { .. }
            | AbortReason::NativeTerminal(_) => None,
        };
        let stream = command_id.as_ref().map_or_else(
            || ThreadEventStream::server(&transaction, &state.thread_id),
            |command_id| ThreadEventStream::command(&transaction, &state.thread_id, command_id),
        );

        let requests = collect(
            &transaction,
            "SELECT node_id, payload_json FROM orchestration_v2_projection_runtime_requests
             WHERE provider_turn_id = ?1 AND status = 'pending' ORDER BY created_at, runtime_request_id",
            &state.provider_turn_id,
        )?;
        for (node_id, payload) in requests {
            let mut payload: Map<String, Value> = serde_json::from_str(&payload)?;
            payload.insert("status".into(), json!(reason.request_status()));
            payload.insert("resolvedAt".into(), json!(now));
            if !matches!(reason, AbortReason::NativeTerminal(_)) {
                let message = match reason {
                    AbortReason::Startup => {
                        "The server restarted before this runtime request was resolved."
                    }
                    AbortReason::Shutdown => {
                        "The server shut down before this runtime request was resolved."
                    }
                    _ => "The provider session ended before this runtime request was resolved.",
                };
                payload.insert(
                    "responseCapability".into(),
                    json!({
                        "type": "not_resumable",
                        "reason": message,
                    }),
                );
            }
            stream.append(
                "runtime-request.updated",
                &now,
                &json!({"nodeId":node_id}),
                &Value::Object(payload),
            )?;
        }

        let node_metadata = |node_id: &str| {
            json!({
                "runId": state.run_id, "nodeId": node_id, "providerInstanceId": state.driver,
            })
        };
        let outcome = reason.outcome();
        let mut run = settled(&state.run, outcome, &now)?;
        run.insert("queuePosition".into(), Value::Null);
        stream.append(
            "run.updated",
            &now,
            &json!({"runId": state.run_id, "providerInstanceId":state.driver}),
            &Value::Object(run),
        )?;
        stream.append(
            "run-attempt.updated",
            &now,
            &node_metadata(&state.node_id),
            &Value::Object(settled(&state.attempt, outcome, &now)?),
        )?;

        let nodes = collect(
            &transaction,
            "SELECT node_id, payload_json FROM orchestration_v2_projection_nodes
             WHERE run_id = ?1 AND status IN ('pending', 'running', 'waiting') ORDER BY rowid",
            &state.run_id,
        )?;
        for (node_id, payload) in nodes {
            let status = if node_id == state.node_id {
                outcome
            } else {
                "cancelled"
            };
            stream.append(
                "node.updated",
                &now,
                &node_metadata(&node_id),
                &Value::Object(settled(&payload, status, &now)?),
            )?;
        }
        if state.provider_status == "running" {
            let mut provider_turn = settled(
                &state.provider_turn,
                if matches!(reason, AbortReason::ProviderFailure { .. }) {
                    "failed"
                } else {
                    "cancelled"
                },
                &now,
            )?;
            if state.driver != CODEX_INSTANCE {
                provider_turn.shift_remove("tokenUsage");
                provider_turn.insert(
                    "turnTokenUsage".into(),
                    json!({
                        "usageStatus":"unavailable","usageScope":"main_agent","hasSubagents":false,
                    }),
                );
            }
            stream.append(
                "provider-turn.updated",
                &now,
                &node_metadata(&state.node_id),
                &Value::Object(provider_turn),
            )?;
        }

        let items = collect(
            &transaction,
            "SELECT node_id, payload_json FROM orchestration_v2_projection_turn_items
             WHERE run_id = ?1 AND type IN ('assistant_message', 'command_execution', 'web_search', 'approval_request', 'user_input_request')
               AND status IN ('pending', 'running', 'waiting') ORDER BY ordinal",
            &state.run_id,
        )?;
        for (node_id, payload) in items {
            let mut payload = settled(&payload, "cancelled", &now)?;
            if payload.contains_key("streaming") {
                payload.insert("streaming".into(), json!(false));
            }
            payload.insert("updatedAt".into(), json!(now));
            stream.append(
                "turn-item.updated",
                &now,
                &node_metadata(&node_id),
                &Value::Object(payload),
            )?;
        }

        let signal = format!("turn-item:run:{}:signal:", encode_id_part(&state.run_id));
        let notice_id = match reason {
            AbortReason::NativeTerminal(SettlementStatus::Interrupted) => {
                format!("{signal}interrupt-result")
            }
            AbortReason::NativeTerminal(SettlementStatus::Failed)
            | AbortReason::ProviderFailure { .. } => format!(
                "turn-item:provider:{}:native-item:{}",
                state.driver,
                encode_id_part(&format!("terminal-failure:{}", state.provider_turn_id)),
            ),
            _ => format!("{signal}server-recovery"),
        };
        let ordinal = allocate_turn_item_in_run(
            &transaction,
            &state.thread_id,
            &notice_id,
            state.run_ordinal,
        )?;
        let mut notice = json!({
                "id": notice_id,
                "threadId": state.thread_id,
                "runId": state.run_id,
                "nodeId": state.node_id,
                "providerThreadId": state.provider_thread_id,
                "providerTurnId": null,
                "nativeItemRef": null,
                "parentItemId": null,
                "ordinal": ordinal,
                "status": "completed",
                "title": reason.notice_title(),
                "startedAt": now,
                "completedAt": now,
                "updatedAt": now,
                "type": "system_notice",
                "message": reason.detail(),
        });
        match reason {
            AbortReason::NativeTerminal(SettlementStatus::Interrupted) => {
                notice["status"] = json!("interrupted");
                notice["type"] = json!("run_interrupt_result");
                notice["parentItemId"] = json!(format!("{signal}interrupt-request"));
            }
            AbortReason::NativeTerminal(SettlementStatus::Failed)
            | AbortReason::ProviderFailure { .. } => {
                let code = match &reason {
                    AbortReason::ProviderFailure { code, .. } => *code,
                    _ => None,
                };
                notice["status"] = json!("failed");
                notice["type"] = json!("error");
                notice["providerTurnId"] = json!(state.provider_turn_id);
                notice["failure"] = json!({
                    "class":"provider_error", "message":reason.detail(), "code":code, "retryable":null,
                });
                if let Some(notice) = notice.as_object_mut() {
                    notice.remove("message");
                }
            }
            _ => {}
        }
        stream.append(
            "turn-item.updated",
            &now,
            &node_metadata(&state.node_id),
            &notice,
        )?;
        let mut provider_thread: Map<String, Value> = serde_json::from_str(&state.provider_thread)?;
        provider_thread.insert("status".into(), json!("idle"));
        provider_thread.insert("pendingBackgroundTasks".into(), json!([]));
        provider_thread.insert("updatedAt".into(), json!(now));
        let sequence = stream.append(
            "provider-thread.updated",
            &now,
            &json!({ "driver":state.driver, "providerInstanceId":state.driver }),
            &Value::Object(provider_thread),
        )?;
        if let Some(command_id) = command_id {
            transaction.execute(
                "INSERT INTO orchestration_command_receipts (
                    command_id, aggregate_kind, aggregate_id, accepted_at, result_sequence, status, command_type
                 ) VALUES (?1, 'thread', ?2, ?3, ?4, 'accepted', 'provider-runtime.reconcile')",
                params![command_id, state.thread_id, now, sequence],
            )?;
        }
        transaction.execute(
            "UPDATE orchestration_v2_effect_outbox AS effect
             SET status = 'cancelled', lease_owner = NULL, completed_at = ?2,
                 updated_at = ?2, last_error = ?3
             WHERE thread_id = ?1 AND effect_type = 'runtime-request.respond'
               AND status IN ('pending', 'running') AND EXISTS (
                   SELECT 1 FROM orchestration_v2_projection_runtime_requests AS request
                   WHERE request.thread_id = effect.thread_id AND request.provider_turn_id = ?4
                     AND request.runtime_request_id = json_extract(effect.payload_json, '$.requestId')
               )",
            params![state.thread_id, now, reason.detail(), state.provider_turn_id],
        )?;
        if state.driver != CODEX_INSTANCE {
            transaction.execute("UPDATE orchestration_v2_effect_outbox SET status='cancelled', lease_owner=NULL,
                completed_at=?2, updated_at=?2, last_error=?3 WHERE effect_id=?1 AND status='running'",
                params![effect_id, now, reason.detail()])?;
        }
        if matches!(
            reason,
            AbortReason::NativeTerminal(SettlementStatus::Interrupted)
        ) {
            let effect_id = format!("effect:checkpoint.capture:{}", state.run_id);
            let command_id = format!("command:{effect_id}");
            transaction.execute(
                "INSERT INTO orchestration_v2_effect_outbox (
                    effect_id, command_id, thread_id, effect_type, payload_json, status, attempt_count,
                    available_at, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, 'checkpoint.capture', ?4, 'pending', 0, ?5, ?5, ?5)",
                params![effect_id, command_id, state.thread_id,
                    json!({"type":"checkpoint.capture","runId":state.run_id,"scopeId":state.scope_id}).to_string(), now],
            )?;
        }
        record_projected_sequence(&transaction, sequence, &now)?;
        transaction.commit()?;
        Ok(Some(AbortReceipt {
            thread_id: state.thread_id,
            sequence,
        }))
    }
}

fn collect(
    transaction: &Transaction<'_>,
    sql: &str,
    identifier: &str,
) -> rusqlite::Result<Vec<(String, String)>> {
    let mut query = transaction.prepare(sql)?;
    query
        .query_map([identifier], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

fn settled(encoded: &str, status: &str, now: &str) -> serde_json::Result<Map<String, Value>> {
    let mut payload: Map<String, Value> = serde_json::from_str(encoded)?;
    payload.insert("status".into(), json!(status));
    payload.insert("completedAt".into(), json!(now));
    Ok(payload)
}

struct ActiveRun {
    thread_id: String,
    run_id: String,
    node_id: String,
    provider_thread_id: String,
    provider_turn_id: String,
    run: String,
    attempt: String,
    provider_turn: String,
    provider_thread: String,
    provider_status: String,
    scope_id: String,
    run_ordinal: i64,
    driver: String,
}

const ABORT_RUN: &str = "
    SELECT run.thread_id, run.run_id, node.node_id, provider_thread.provider_thread_id,
           provider_turn.provider_turn_id, run.payload_json, attempt.payload_json,
           provider_turn.payload_json, provider_thread.payload_json, provider_turn.status,
           json_extract(node.payload_json, '$.checkpointScopeId'), run.ordinal, run.provider_instance_id
    FROM orchestration_v2_effect_outbox AS effect
    JOIN orchestration_v2_projection_runs AS run ON run.thread_id = effect.thread_id
      AND run.run_id = json_extract(effect.payload_json, '$.runId')
    JOIN orchestration_v2_projection_run_attempts AS attempt
      ON attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId') AND attempt.run_id = run.run_id
    JOIN orchestration_v2_projection_nodes AS node ON node.node_id = attempt.root_node_id
    JOIN orchestration_v2_projection_provider_threads AS provider_thread
      ON provider_thread.provider_thread_id = attempt.provider_thread_id AND provider_thread.thread_id = run.thread_id
    JOIN orchestration_v2_projection_provider_turns AS provider_turn
      ON provider_turn.run_attempt_id = attempt.attempt_id AND provider_turn.node_id = node.node_id
      AND provider_turn.provider_thread_id = provider_thread.provider_thread_id
    WHERE effect.effect_id = ?1 AND effect.effect_type = 'provider-turn.start'
      AND (effect.status = 'succeeded' OR (effect.status = 'running' AND run.provider_instance_id = 'claudeAgent'))
      AND run.ordinal > 0 AND run.provider_instance_id IN ('codex', 'claudeAgent')
      AND provider_thread.driver = run.provider_instance_id
      AND run.status = 'running' AND attempt.status = 'running' AND node.status = 'running'
      AND provider_turn.status IN ('running', 'completed', 'interrupted', 'failed')
      AND (?2 IS NULL OR provider_turn.status = ?2)
      AND provider_thread.status = 'active'
      AND NOT EXISTS (
        SELECT 1 FROM orchestration_v2_projection_subagents AS task WHERE task.run_id = run.run_id
      )
      AND NOT EXISTS (
        SELECT 1 FROM orchestration_v2_projection_runtime_requests AS request
        WHERE request.provider_turn_id = provider_turn.provider_turn_id
          AND request.status = 'pending'
          AND request.kind NOT IN ('command', 'file-read', 'file-change', 'user_input')
      )
";
