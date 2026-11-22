mod settlement;
mod unbound;

use std::time::Duration;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};
use t3_rune::providers::codex::TurnKey;
use time::OffsetDateTime;

use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, allocate_turn_item_in_run,
    encode_id_part, record_projected_sequence, timestamp,
};

const COMMAND_TYPE: &str = "thread.stop";
const EFFECT_TYPE: &str = "provider-turn.interrupt";
const MAX_EFFECT_ATTEMPTS: u32 = 5;

pub(in crate::server) struct ClaimedInterrupt {
    effect_id: String,
    attempt: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(in crate::server) struct ThreadStop {
    #[serde(rename = "type")]
    command_type: StopType,
    command_id: String,
    thread_id: String,
    reason: Option<String>,
    #[serde(rename = "stopDelegatedTasks")]
    _stop_delegated_tasks: Option<bool>,
}

#[derive(Deserialize)]
enum StopType {
    #[serde(rename = "thread.stop")]
    ThreadStop,
}

#[derive(Debug)]
pub(in crate::server) struct StopReceipt {
    pub(in crate::server) sequence: i64,
    pub(in crate::server) committed: bool,
}

struct LiveTurn {
    run_id: String,
    run_ordinal: i64,
    node_id: String,
    provider_session_id: String,
    provider_thread_id: String,
    provider_turn_id: String,
}

impl TurnCommands {
    pub(in crate::server) fn can_stop_thread(
        &self,
        thread_id: &str,
    ) -> Result<bool, TurnCommandError> {
        let connection = self.read_connection()?;
        let bound = connection
            .query_row(
                include_str!("interrupt_effect/live_turn.sql"),
                [thread_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(bound || unbound::can_interrupt(&connection, thread_id)?)
    }

    pub(in crate::server) fn claim_interrupt(
        &self,
        thread_id: &str,
        key: &TurnKey,
        worker_id: &str,
    ) -> Result<Option<ClaimedInterrupt>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp(OffsetDateTime::now_utc());
        let provider_turn_id = super::provider_turn::provider_turn_id(key);
        let effect = transaction
            .query_row(
                include_str!("interrupt_effect/claim.sql"),
                params![
                    thread_id,
                    provider_turn_id,
                    key.thread_id.as_str(),
                    key.turn_id.as_str(),
                    now
                ],
                |row| {
                    Ok(ClaimedInterrupt {
                        effect_id: row.get(0)?,
                        attempt: row.get::<_, u32>(1)? + 1,
                    })
                },
            )
            .optional()?;
        if let Some(effect) = &effect {
            transaction.execute(
                "UPDATE orchestration_v2_effect_outbox
                 SET status = 'running', attempt_count = ?2, lease_owner = ?3,
                     updated_at = ?4, last_error = NULL
                 WHERE effect_id = ?1",
                params![effect.effect_id, effect.attempt, worker_id, now],
            )?;
            transaction.commit()?;
        }
        Ok(effect)
    }

    pub(in crate::server) fn retry_interrupt_effect(
        &self,
        effect: &ClaimedInterrupt,
        worker_id: &str,
        reason: &str,
    ) -> Result<Option<Duration>, TurnCommandError> {
        let now = OffsetDateTime::now_utc();
        let delay = (effect.attempt < MAX_EFFECT_ATTEMPTS)
            .then(|| Duration::from_millis(100 << effect.attempt.saturating_sub(1)));
        let connection = self.connect()?;
        let changed = connection.execute(
            "UPDATE orchestration_v2_effect_outbox SET status = ?3, lease_owner = NULL,
                completed_at = ?4, available_at = COALESCE(?5, available_at),
                updated_at = ?6, last_error = ?7
             WHERE effect_id = ?1 AND lease_owner = ?2 AND status = 'running'
               AND effect_type = 'provider-turn.interrupt'",
            params![
                effect.effect_id,
                worker_id,
                if delay.is_some() { "pending" } else { "failed" },
                delay.is_none().then(|| timestamp(now)),
                delay.map(|delay| timestamp(now + delay)),
                timestamp(now),
                reason
            ],
        )?;
        Ok(delay.filter(|_| changed == 1))
    }

    pub(in crate::server) fn mark_interrupt_unresponsive(
        &self,
        thread_id: &str,
        effect: &ClaimedInterrupt,
        worker_id: &str,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending = transaction.query_row(
            "SELECT item.payload_json FROM orchestration_v2_effect_outbox AS effect
             JOIN orchestration_v2_projection_provider_turns AS turn ON turn.thread_id = effect.thread_id
               AND turn.provider_turn_id = json_extract(effect.payload_json, '$.providerTurnId')
             JOIN orchestration_v2_projection_run_attempts AS attempt ON attempt.attempt_id = turn.run_attempt_id
             JOIN orchestration_v2_projection_turn_items AS item ON item.thread_id = effect.thread_id
               AND item.run_id = attempt.run_id AND item.type = 'run_interrupt_request'
             WHERE effect.effect_id = ?1 AND effect.thread_id = ?2 AND effect.lease_owner = ?3
               AND effect.status = 'running' AND turn.status = 'running'
               AND json_extract(item.payload_json, '$.providerTurnId') = turn.provider_turn_id
               AND json_extract(item.payload_json, '$.unresponsiveAt') IS NULL
               AND NOT EXISTS (SELECT 1 FROM orchestration_v2_projection_turn_items AS result
                 WHERE result.run_id = attempt.run_id AND result.type = 'run_interrupt_result')",
            params![effect.effect_id, thread_id, worker_id],
            |row| row.get::<_, String>(0),
        ).optional()?;
        let now = timestamp(OffsetDateTime::now_utc());
        let command_id = format!("{}:unresponsive", effect.effect_id);
        let sequence = if let Some(encoded) = pending {
            let mut item: Value = serde_json::from_str(&encoded)?;
            item["unresponsiveAt"] = json!(now);
            item["updatedAt"] = json!(now);
            let stream = ThreadEventStream::command(&transaction, thread_id, &command_id);
            let sequence = stream.append("turn-item.updated", &now, &json!({
                "runId": item["runId"], "nodeId": item["nodeId"], "providerInstanceId": CODEX_INSTANCE,
            }), &item)?;
            record_projected_sequence(&transaction, sequence, &now)?;
            Some(sequence)
        } else {
            None
        };
        if finish_interrupt_effect(&transaction, effect, worker_id, &now)? {
            record_interrupt_followup(
                &transaction,
                thread_id,
                &command_id,
                "thread.interrupt.unresponsive",
                &now,
                sequence,
            )?;
        }
        transaction.commit()?;
        Ok(sequence)
    }

    pub(in crate::server) fn complete_interrupt_effect(
        &self,
        thread_id: &str,
        effect: &ClaimedInterrupt,
        worker_id: &str,
    ) -> Result<(), TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp(OffsetDateTime::now_utc());
        if finish_interrupt_effect(&transaction, effect, worker_id, &now)? {
            record_interrupt_followup(
                &transaction,
                thread_id,
                &format!("{}:background-work-settled", effect.effect_id),
                "thread.background-work.settle",
                &now,
                None,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(in crate::server) fn claim_settled_interrupt(
        &self,
        previous: &ClaimedInterrupt,
        thread_id: &str,
        key: &TurnKey,
        worker_id: &str,
    ) -> Result<Option<ClaimedInterrupt>, TurnCommandError> {
        let now = timestamp(OffsetDateTime::now_utc());
        let provider_turn = super::provider_turn::provider_turn_id(key);
        Ok(self.connect()?.query_row(
            "UPDATE orchestration_v2_effect_outbox AS effect SET status = 'running',
                 lease_owner = ?2, attempt_count = attempt_count + 1, updated_at = ?3, last_error = NULL
             WHERE effect_id = ?1 AND thread_id = ?4 AND status = 'pending' AND attempt_count = ?5
               AND effect_type = 'provider-turn.interrupt' AND available_at <= ?3
               AND json_extract(payload_json, '$.providerTurnId') = ?6
               AND EXISTS (
                 SELECT 1 FROM orchestration_v2_projection_provider_turns AS turn
                 JOIN orchestration_v2_projection_provider_threads AS provider
                   ON provider.provider_thread_id = turn.provider_thread_id AND provider.thread_id = turn.thread_id
                 WHERE turn.thread_id = effect.thread_id AND turn.provider_turn_id = ?6
                   AND turn.status IN ('interrupted', 'failed') AND provider.driver = 'codex'
                   AND provider.provider_thread_id = json_extract(effect.payload_json, '$.providerThreadId')
                   AND provider.provider_session_id = json_extract(effect.payload_json, '$.providerSessionId')
                   AND json_extract(turn.payload_json, '$.nativeTurnRef.nativeId') = ?7
                   AND json_extract(provider.payload_json, '$.nativeThreadRef.nativeId') = ?8
               )
             RETURNING attempt_count",
            params![previous.effect_id, worker_id, now, thread_id, previous.attempt, provider_turn,
                    key.turn_id.as_str(), key.thread_id.as_str()],
            |row| Ok(ClaimedInterrupt { effect_id: previous.effect_id.clone(), attempt: row.get(0)? }),
        ).optional()?)
    }

    pub(in crate::server) fn cancel_process_bound_interrupts(
        &self,
    ) -> Result<(), TurnCommandError> {
        self.connect()?.execute(
            "UPDATE orchestration_v2_effect_outbox SET status = 'cancelled', lease_owner = NULL,
                 completed_at = ?1, updated_at = ?1,
                 last_error = 'Cancelled because the server process ended before the effect completed.'
             WHERE effect_type = 'provider-turn.interrupt' AND status IN ('pending', 'running')",
            [timestamp(OffsetDateTime::now_utc())],
        )?;
        Ok(())
    }

    pub(in crate::server) fn complete_inactive_interrupts(
        &self,
        thread_id: &str,
    ) -> Result<(), TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let effects = {
            let mut query = transaction.prepare(include_str!("interrupt_effect/inactive.sql"))?;
            query
                .query_map([thread_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let now = timestamp(OffsetDateTime::now_utc());
        for (effect_id, thread_id) in effects {
            transaction.execute(
                "UPDATE orchestration_v2_effect_outbox SET status = 'succeeded', lease_owner = NULL,
                    lease_expires_at = NULL, completed_at = ?2, updated_at = ?2, last_error = NULL,
                    attempt_count = attempt_count + CASE WHEN status = 'pending' THEN 1 ELSE 0 END
                 WHERE effect_id = ?1",
                params![effect_id, now],
            )?;
            record_interrupt_followup(
                &transaction,
                &thread_id,
                &format!("{effect_id}:background-work-settled"),
                "thread.background-work.settle",
                &now,
                None,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(in crate::server) fn stop_thread(
        &self,
        command: ThreadStop,
    ) -> Result<StopReceipt, TurnCommandError> {
        let ThreadStop {
            command_type: StopType::ThreadStop,
            command_id,
            thread_id,
            reason,
            _stop_delegated_tasks: _,
        } = command;
        if command_id.trim().is_empty() || thread_id.trim().is_empty() {
            return Err(TurnCommandError::Unsupported(
                "The Stop command has no thread or command ID.",
            ));
        }
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipt = transaction
            .query_row(
                "SELECT aggregate_kind, aggregate_id, command_type, status, result_sequence
             FROM orchestration_command_receipts WHERE command_id = ?1",
                [&command_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?;
        if let Some((kind, aggregate, receipt_type, status, sequence)) = receipt {
            if kind == "thread"
                && aggregate == thread_id
                && receipt_type == COMMAND_TYPE
                && status == "accepted"
            {
                return Ok(StopReceipt {
                    sequence,
                    committed: false,
                });
            }
            return Err(TurnCommandError::Unsupported(
                "The command ID already has a different receipt.",
            ));
        }
        let turn = transaction
            .query_row(
                include_str!("interrupt_effect/live_turn.sql"),
                [&thread_id],
                |row| {
                    Ok(LiveTurn {
                        run_id: row.get(0)?,
                        node_id: row.get(1)?,
                        provider_session_id: row.get(2)?,
                        provider_thread_id: row.get(3)?,
                        provider_turn_id: row.get(4)?,
                        run_ordinal: row.get(5)?,
                    })
                },
            )
            .optional()?;
        let now = timestamp(OffsetDateTime::now_utc());
        let sequence = match turn {
            Some(turn) => append_bound_stop(
                &transaction,
                &thread_id,
                &command_id,
                turn,
                reason.as_deref(),
                &now,
            )?,
            None => unbound::interrupt(
                &transaction,
                &thread_id,
                &command_id,
                reason.as_deref(),
                &now,
            )?
            .ok_or(TurnCommandError::Unsupported(
                "Stop is available for an interruptible Codex run with no queued or delegated work.",
            ))?,
        };
        transaction.execute(
            "INSERT INTO orchestration_command_receipts (
                command_id, aggregate_kind, aggregate_id, accepted_at, result_sequence, status, command_type
             ) VALUES (?1, 'thread', ?2, ?3, ?4, 'accepted', ?5)",
            params![command_id, thread_id, now, sequence, COMMAND_TYPE],
        )?;
        record_projected_sequence(&transaction, sequence, &now)?;
        transaction.commit()?;
        Ok(StopReceipt {
            sequence,
            committed: true,
        })
    }
}

fn append_bound_stop(
    transaction: &Transaction<'_>,
    thread_id: &str,
    command_id: &str,
    turn: LiveTurn,
    reason: Option<&str>,
    now: &str,
) -> Result<i64, TurnCommandError> {
    let message = reason.unwrap_or("Stopped by user");
    let item_id = format!(
        "turn-item:run:{}:signal:interrupt-request",
        encode_id_part(&turn.run_id)
    );
    let existing_ordinal: Option<i64> = transaction
        .query_row(
            "SELECT ordinal FROM orchestration_v2_turn_item_positions
             WHERE thread_id = ?1 AND turn_item_id = ?2",
            params![thread_id, item_id],
            |row| row.get(0),
        )
        .optional()?;
    let ordinal = match existing_ordinal {
        Some(ordinal) => ordinal,
        None => allocate_turn_item_in_run(transaction, thread_id, &item_id, turn.run_ordinal)?,
    };
    let stream = ThreadEventStream::command(transaction, thread_id, command_id);
    let sequence = stream.append(
        "turn-item.updated",
        now,
        &json!({
            "runId": turn.run_id, "nodeId": turn.node_id, "providerInstanceId": CODEX_INSTANCE,
        }),
        &json!({
            "id": item_id, "threadId": thread_id, "runId": turn.run_id, "nodeId": turn.node_id,
            "providerThreadId": turn.provider_thread_id, "providerTurnId": turn.provider_turn_id,
            "nativeItemRef": null, "parentItemId": null, "ordinal": ordinal,
            "status": "completed", "title": "Interrupt requested",
            "startedAt": now, "completedAt": now, "updatedAt": now,
            "type": "run_interrupt_request", "message": message,
        }),
    )?;
    let effect_id = format!(
        "effect:{command_id}:{EFFECT_TYPE}:{}",
        turn.provider_turn_id
    );
    let effect = json!({
        "type": EFFECT_TYPE, "providerSessionId": turn.provider_session_id,
        "providerThreadId": turn.provider_thread_id, "providerTurnId": turn.provider_turn_id,
    });
    transaction.execute(
        "INSERT INTO orchestration_v2_effect_outbox (
                effect_id, command_id, thread_id, effect_type, payload_json, status, attempt_count,
                available_at, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6, ?6)",
        params![
            effect_id,
            command_id,
            thread_id,
            EFFECT_TYPE,
            effect.to_string(),
            now
        ],
    )?;
    Ok(sequence)
}

fn finish_interrupt_effect(
    transaction: &rusqlite::Transaction<'_>,
    effect: &ClaimedInterrupt,
    worker_id: &str,
    now: &str,
) -> rusqlite::Result<bool> {
    Ok(transaction.execute(
        "UPDATE orchestration_v2_effect_outbox SET status = 'succeeded', lease_owner = NULL,
            completed_at = ?3, updated_at = ?3, last_error = NULL
         WHERE effect_id = ?1 AND lease_owner = ?2 AND status = 'running'
           AND effect_type = 'provider-turn.interrupt'",
        params![effect.effect_id, worker_id, now],
    )? == 1)
}

fn record_interrupt_followup(
    transaction: &rusqlite::Transaction<'_>,
    thread_id: &str,
    command_id: &str,
    command_type: &str,
    now: &str,
    sequence: Option<i64>,
) -> rusqlite::Result<()> {
    let sequence = match sequence {
        Some(sequence) => sequence,
        None => transaction.query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM orchestration_events
             WHERE aggregate_kind = 'thread' AND stream_id = ?1 AND application_event_version = 2",
            [thread_id],
            |row| row.get(0),
        )?,
    };
    transaction.execute(
        "INSERT INTO orchestration_command_receipts (
            command_id, aggregate_kind, aggregate_id, accepted_at, result_sequence, status, command_type
         ) VALUES (?1, 'thread', ?2, ?3, ?4, 'accepted', ?5)",
        params![command_id, thread_id, now, sequence, command_type],
    )?;
    Ok(())
}
