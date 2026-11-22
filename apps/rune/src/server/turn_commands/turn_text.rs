use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::json;
use t3_rune::providers::codex::{AssistantPhase, TurnKey};
use time::OffsetDateTime;

use super::native_events::{AssistantSnapshot, AssistantText, assistant_text};
use super::provider_turn::provider_turn_id;
use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, TurnIds,
    allocate_turn_item_in_run, encode_id_part, record_projected_sequence, timestamp,
};

pub(in crate::server) struct AssistantUpdate<'a> {
    pub(in crate::server) key: &'a TurnKey,
    pub(in crate::server) native_item_id: &'a str,
    pub(in crate::server) started_at: OffsetDateTime,
    pub(in crate::server) updated_at: OffsetDateTime,
    pub(in crate::server) phase: Option<AssistantPhase>,
    pub(in crate::server) snapshot: AssistantSnapshot<'a>,
}

impl TurnCommands {
    pub(in crate::server) fn record_assistant(
        &self,
        thread_id: &str,
        run_id: &str,
        update: AssistantUpdate<'_>,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let provider_turn_id = provider_turn_id(update.key);
        let bound = transaction
            .query_row(
                ACTIVE_CODEX_TURN,
                params![
                    thread_id,
                    run_id,
                    provider_turn_id,
                    update.key.thread_id.as_str(),
                    update.key.turn_id.as_str(),
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((intended_message_id, run_ordinal, provider_thread_id)) = bound else {
            return Ok(None);
        };
        let ids = TurnIds::for_run(
            thread_id,
            &intended_message_id,
            run_ordinal,
            Some(&provider_thread_id),
        );
        let turn_item_id = format!(
            "turn-item:provider:codex:native-item:{}",
            encode_id_part(update.native_item_id)
        );
        let Some(ordinal) =
            assistant_position(&transaction, thread_id, &turn_item_id, run_ordinal)?
        else {
            return Ok(None);
        };

        let updated_at = timestamp(update.updated_at);
        let events = assistant_text(
            &ids,
            thread_id,
            AssistantText {
                native_turn_id: update.key.turn_id.as_str(),
                native_item_id: update.native_item_id,
                ordinal,
                started_at: &timestamp(update.started_at),
                updated_at: &updated_at,
                phase: update.phase,
                snapshot: update.snapshot,
            },
        );
        let stream = ThreadEventStream::provider(&transaction, thread_id, &ids.provider_session_id);
        let mut sequence = 0;
        for event in events {
            sequence = stream.append(
                event.event_type,
                &updated_at,
                &json!({
                    "runId": run_id,
                    "nodeId": event.node_id,
                    "driver": "codex",
                    "providerInstanceId": CODEX_INSTANCE,
                }),
                &event.payload,
            )?;
        }
        record_projected_sequence(&transaction, sequence, &updated_at)?;
        transaction.commit()?;
        Ok(Some(sequence))
    }
}

pub(super) fn assistant_position(
    transaction: &Transaction<'_>,
    thread_id: &str,
    turn_item_id: &str,
    run_ordinal: i64,
) -> rusqlite::Result<Option<i64>> {
    let existing = transaction
        .query_row(
            "SELECT position.ordinal, item.status
             FROM orchestration_v2_turn_item_positions AS position
             LEFT JOIN orchestration_v2_projection_turn_items AS item
               ON item.turn_item_id = position.turn_item_id AND item.thread_id = position.thread_id
             WHERE position.thread_id = ?1 AND position.turn_item_id = ?2",
            params![thread_id, turn_item_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    match existing {
        Some((_, Some(status))) if status == "completed" => Ok(None),
        Some((ordinal, _)) => Ok(Some(ordinal)),
        None => {
            allocate_turn_item_in_run(transaction, thread_id, turn_item_id, run_ordinal).map(Some)
        }
    }
}

pub(super) const ACTIVE_CODEX_TURN: &str = "
    SELECT json_extract(run.payload_json, '$.userMessageId'), run.ordinal, provider_thread.provider_thread_id
    FROM orchestration_v2_projection_runs AS run
    JOIN orchestration_v2_projection_run_attempts AS attempt
      ON attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
         AND attempt.run_id = run.run_id
    JOIN orchestration_v2_projection_provider_threads AS provider_thread
      ON provider_thread.provider_thread_id = attempt.provider_thread_id
         AND provider_thread.thread_id = run.thread_id
    JOIN orchestration_v2_projection_provider_turns AS provider_turn
      ON provider_turn.run_attempt_id = attempt.attempt_id
         AND provider_turn.provider_thread_id = provider_thread.provider_thread_id
         AND provider_turn.thread_id = run.thread_id
    WHERE run.thread_id = ?1 AND run.run_id = ?2 AND provider_turn.provider_turn_id = ?3
      AND run.ordinal > 0 AND run.provider_instance_id = 'codex' AND run.status = 'running'
      AND attempt.status = 'running' AND provider_turn.status = 'running'
      AND provider_thread.status = 'active' AND provider_thread.driver = 'codex'
      AND json_extract(provider_thread.payload_json, '$.nativeThreadRef.nativeId') = ?4
      AND json_extract(provider_turn.payload_json, '$.nativeTurnRef.nativeId') = ?5
";
