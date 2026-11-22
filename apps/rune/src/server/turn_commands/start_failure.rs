use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;

use super::start_effect::ClaimedTurnStart;
use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, allocate_turn_item_in_run,
    encode_id_part, record_projected_sequence, timestamp,
};

pub(in crate::server) enum StartFailure {
    SessionOpen,
    ThreadLoad,
}

impl StartFailure {
    fn labels(self) -> (&'static str, &'static str) {
        match self {
            Self::SessionOpen => (
                "provider-session-open-failure",
                "Provider session failed to open",
            ),
            Self::ThreadLoad => (
                "provider-thread-load-failure",
                "Provider turn failed to start",
            ),
        }
    }
}

impl TurnCommands {
    pub(in crate::server) fn fail_provider_start(
        &self,
        claim: &ClaimedTurnStart,
        failure: StartFailure,
        detail: &str,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let starting = transaction
            .query_row(
                CURRENT_STARTING_RUN,
                params![claim.effect_id, claim.worker_id, claim.run_id],
                |row| {
                    Ok(Starting {
                        thread_id: row.get(0)?,
                        provider_thread_id: row.get(1)?,
                        root_node_id: row.get(2)?,
                        run: row.get(3)?,
                        attempt: row.get(4)?,
                        node: row.get(5)?,
                        run_ordinal: row.get(6)?,
                    })
                },
            )
            .optional()?;
        let Some(starting) = starting else {
            return Ok(None);
        };
        let now = timestamp(OffsetDateTime::now_utc());
        let (signal, title) = failure.labels();
        let turn_item_id = format!(
            "turn-item:run:{}:signal:{signal}",
            encode_id_part(&claim.run_id)
        );
        let ordinal = allocate_turn_item_in_run(
            &transaction,
            &starting.thread_id,
            &turn_item_id,
            starting.run_ordinal,
        )?;
        let item = json!({
            "id": turn_item_id,
            "threadId": starting.thread_id,
            "runId": claim.run_id,
            "nodeId": starting.root_node_id,
            "providerThreadId": starting.provider_thread_id,
            "providerTurnId": null,
            "nativeItemRef": null,
            "parentItemId": null,
            "ordinal": ordinal,
            "status": "failed",
            "startedAt": now,
            "completedAt": now,
            "updatedAt": now,
            "type": "error",
            "title": title,
            "failure": {
                "class": "provider_error",
                "message": detail,
                "code": null,
                "retryable": null,
            },
        });
        let metadata = json!({
            "runId": claim.run_id,
            "nodeId": starting.root_node_id,
            "providerInstanceId": CODEX_INSTANCE,
        });
        let stream = ThreadEventStream::server(&transaction, &starting.thread_id);
        stream.append("turn-item.updated", &now, &metadata, &item)?;
        let mut sequence = 0;
        for (event_type, original) in [
            ("run.updated", starting.run),
            ("run-attempt.updated", starting.attempt),
            ("node.updated", starting.node),
        ] {
            let mut payload: Map<String, Value> = serde_json::from_str(&original)?;
            payload.insert("status".into(), json!("failed"));
            payload.insert("completedAt".into(), json!(now));
            sequence = stream.append(event_type, &now, &metadata, &Value::Object(payload))?;
        }
        transaction.execute(
            "UPDATE orchestration_v2_effect_outbox
             SET status = 'succeeded', lease_owner = NULL, completed_at = ?2,
                 updated_at = ?2, last_error = NULL
             WHERE effect_id = ?1",
            params![claim.effect_id, now],
        )?;
        record_projected_sequence(&transaction, sequence, &now)?;
        transaction.commit()?;
        Ok(Some(sequence))
    }
}

struct Starting {
    thread_id: String,
    provider_thread_id: String,
    root_node_id: String,
    run: String,
    attempt: String,
    node: String,
    run_ordinal: i64,
}

const CURRENT_STARTING_RUN: &str = "
    SELECT run.thread_id, run.provider_thread_id, node.node_id,
           run.payload_json, attempt.payload_json, node.payload_json, run.ordinal
    FROM orchestration_v2_effect_outbox AS effect
    JOIN orchestration_v2_projection_runs AS run ON run.thread_id = effect.thread_id
      AND run.run_id = json_extract(effect.payload_json, '$.runId')
    JOIN orchestration_v2_projection_run_attempts AS attempt
      ON attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
         AND attempt.run_id = run.run_id
    JOIN orchestration_v2_projection_nodes AS node ON node.node_id = attempt.root_node_id
         AND node.run_id = run.run_id
    WHERE effect.effect_id = ?1 AND effect.lease_owner = ?2 AND run.run_id = ?3
      AND effect.status = 'running' AND effect.effect_type = 'provider-turn.start'
      AND run.ordinal > 0 AND run.provider_instance_id = 'codex' AND run.status = 'starting'
      AND attempt.status = 'pending' AND node.status = 'pending'
";
