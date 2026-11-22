use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Map, Value, json};
use t3_rune::providers::codex::{
    SettlementStatus, TokenUsageSnapshot, TurnKey, TurnTokenUsage, UsageStatus,
};
use time::OffsetDateTime;

use super::runtime_requests::dismiss_native_user_inputs;
use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, encode_id_part,
    record_projected_sequence, timestamp,
};

pub(in crate::server) enum NativeTurnUpdate<'a> {
    Started {
        started_at: Option<OffsetDateTime>,
    },
    Usage(TokenUsageSnapshot),
    Settled {
        status: SettlementStatus,
        completed_at: Option<OffsetDateTime>,
        usage: &'a TurnTokenUsage,
    },
}

impl TurnCommands {
    pub(in crate::server) fn is_interrupted_native_turn(
        &self,
        app_thread_id: &str,
        key: &TurnKey,
    ) -> Result<bool, TurnCommandError> {
        let connection = self.connect()?;
        Ok(connection.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM orchestration_v2_projection_provider_turns AS turn
                JOIN orchestration_v2_projection_provider_threads AS provider_thread
                  ON provider_thread.provider_thread_id = turn.provider_thread_id
                     AND provider_thread.thread_id = turn.thread_id
                WHERE turn.thread_id = ?1 AND turn.provider_turn_id = ?2
                  AND turn.status = 'interrupted' AND provider_thread.driver = 'codex'
                  AND json_extract(provider_thread.payload_json, '$.nativeThreadRef.nativeId') = ?3
                  AND json_extract(turn.payload_json, '$.nativeTurnRef.nativeId') = ?4
            )",
            params![
                app_thread_id,
                provider_turn_id(key),
                key.thread_id.as_str(),
                key.turn_id.as_str()
            ],
            |row| row.get(0),
        )?)
    }

    pub(in crate::server) fn record_provider_turn(
        &self,
        thread_id: &str,
        run_id: &str,
        key: &TurnKey,
        update: NativeTurnUpdate<'_>,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let provider_turn_id = provider_turn_id(key);
        let binding = transaction
            .query_row(
                BOUND_ATTEMPT,
                params![thread_id, run_id, key.thread_id.as_str(), provider_turn_id],
                |row| {
                    Ok(Binding {
                        attempt_id: row.get(0)?,
                        root_node_id: row.get(1)?,
                        provider_thread_id: row.get(2)?,
                        provider_session_id: row.get(3)?,
                        previous_turn: row.get(4)?,
                        previous_status: row.get(5)?,
                        turn_ordinal: row.get(6)?,
                    })
                },
            )
            .optional()?;
        let Some(binding) = binding else {
            return Ok(None);
        };
        let observed_at = timestamp(OffsetDateTime::now_utc());
        let terminal = matches!(&update, NativeTurnUpdate::Settled { .. });
        let provider_turn = match update {
            NativeTurnUpdate::Started { started_at } if binding.previous_turn.is_none() => {
                json!({
                    "id": provider_turn_id,
                    "providerThreadId": binding.provider_thread_id,
                    "nodeId": binding.root_node_id,
                    "runAttemptId": binding.attempt_id,
                    "nativeTurnRef": {
                        "driver": "codex",
                        "nativeId": key.turn_id.as_str(),
                        "strength": "strong",
                    },
                    "ordinal": binding.turn_ordinal,
                    "status": "running",
                    "startedAt": started_at.map(timestamp).unwrap_or_else(|| observed_at.clone()),
                    "completedAt": null,
                })
            }
            NativeTurnUpdate::Started { .. } => return Ok(None),
            update => {
                let Some(previous) = binding
                    .previous_turn
                    .as_ref()
                    .filter(|_| binding.previous_status.as_deref() == Some("running"))
                else {
                    return Ok(None);
                };
                let mut provider_turn: Map<String, Value> = serde_json::from_str(previous)?;
                match update {
                    NativeTurnUpdate::Usage(usage) => {
                        provider_turn.insert(
                            "tokenUsage".into(),
                            serde_json::to_value(usage.context_usage(observed_at.clone()))?,
                        );
                    }
                    NativeTurnUpdate::Settled {
                        status,
                        completed_at,
                        usage,
                    } => {
                        if status == SettlementStatus::Completed
                            && usage.usage_status == UsageStatus::Partial
                        {
                            return Err(TurnCommandError::Unsupported(
                                "A completed Codex turn requires complete or unavailable usage.",
                            ));
                        }
                        provider_turn.remove("tokenUsage");
                        provider_turn.insert("status".into(), json!(terminal_status(status)));
                        provider_turn.insert(
                            "completedAt".into(),
                            json!(
                                completed_at
                                    .map(timestamp)
                                    .unwrap_or_else(|| observed_at.clone())
                            ),
                        );
                        provider_turn.insert("turnTokenUsage".into(), serde_json::to_value(usage)?);
                        if let Some(model) = usage.models.as_ref().and_then(|models| models.first())
                        {
                            provider_turn.insert("model".into(), json!(model));
                        }
                    }
                    NativeTurnUpdate::Started { .. } => unreachable!(),
                }
                Value::Object(provider_turn)
            }
        };
        let stream =
            ThreadEventStream::provider(&transaction, thread_id, &binding.provider_session_id);
        if terminal {
            dismiss_native_user_inputs(
                &transaction,
                &stream,
                run_id,
                &provider_turn_id,
                &observed_at,
            )?;
        }
        let sequence = stream.append(
            "provider-turn.updated",
            &observed_at,
            &json!({
                "runId": run_id,
                "nodeId": binding.root_node_id,
                "driver": "codex",
                "providerInstanceId": CODEX_INSTANCE,
            }),
            &provider_turn,
        )?;
        record_projected_sequence(&transaction, sequence, &observed_at)?;
        transaction.commit()?;
        Ok(Some(sequence))
    }
}

pub(super) fn terminal_status(status: SettlementStatus) -> &'static str {
    match status {
        SettlementStatus::Completed => "completed",
        SettlementStatus::Interrupted => "interrupted",
        SettlementStatus::Failed => "failed",
    }
}

pub(super) fn provider_turn_id(key: &TurnKey) -> String {
    format!(
        "provider-turn:provider:codex:native-turn:{}",
        encode_id_part(key.turn_id.as_str())
    )
}

struct Binding {
    attempt_id: String,
    root_node_id: String,
    provider_thread_id: String,
    provider_session_id: String,
    previous_turn: Option<String>,
    previous_status: Option<String>,
    turn_ordinal: i64,
}

const BOUND_ATTEMPT: &str = "
    SELECT attempt.attempt_id, node.node_id, provider_thread.provider_thread_id,
           provider_thread.provider_session_id, provider_turn.payload_json, provider_turn.status,
           COALESCE(provider_turn.ordinal, (
               SELECT COALESCE(MAX(previous.ordinal), 0) + 1
               FROM orchestration_v2_projection_provider_turns AS previous
               WHERE previous.provider_thread_id = provider_thread.provider_thread_id
           ))
    FROM orchestration_v2_projection_runs AS run
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = run.thread_id
    JOIN orchestration_v2_projection_run_attempts AS attempt
      ON attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
         AND attempt.run_id = run.run_id
    JOIN orchestration_v2_projection_provider_threads AS provider_thread
      ON provider_thread.provider_thread_id = attempt.provider_thread_id
         AND provider_thread.thread_id = run.thread_id
    JOIN orchestration_v2_projection_nodes AS node
      ON node.node_id = attempt.root_node_id AND node.run_id = run.run_id
    LEFT JOIN orchestration_v2_projection_provider_turns AS provider_turn
      ON provider_turn.provider_thread_id = provider_thread.provider_thread_id
         AND provider_turn.run_attempt_id = attempt.attempt_id
    WHERE run.thread_id = ?1 AND run.run_id = ?2
      AND run.ordinal > 0 AND run.provider_instance_id = 'codex' AND run.status = 'running'
      AND attempt.status = 'running' AND node.status = 'running'
      AND provider_thread.status = 'active' AND provider_thread.driver = 'codex'
      AND thread.active_provider_thread_id = provider_thread.provider_thread_id
      AND json_extract(provider_thread.payload_json, '$.nativeThreadRef.nativeId') = ?3
      AND (provider_turn.provider_turn_id IS NULL OR (
           provider_turn.provider_turn_id = ?4 AND provider_turn.run_attempt_id = attempt.attempt_id
           AND provider_turn.node_id = node.node_id AND provider_turn.thread_id = run.thread_id
      ))
";
