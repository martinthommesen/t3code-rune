use rusqlite::{OptionalExtension, TransactionBehavior, params};
use t3_rune::providers::codex::TurnKey;
use time::OffsetDateTime;

use super::provider_turn::provider_turn_id;
use super::queued_worktree::ROOT_SCOPE_MATCH;
use super::settlement_events::{CompletedRun, checkpoint_waiting};
use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, encode_id_part,
    record_projected_sequence, timestamp,
};

impl TurnCommands {
    pub(in crate::server) fn await_checkpoint(
        &self,
        thread_id: &str,
        run_id: &str,
        key: &TurnKey,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let provider_turn_id = provider_turn_id(key);
        let projected = transaction
            .query_row(
                &format!("{COMPLETED_CODEX_TURN}{ROOT_SCOPE_MATCH}"),
                params![
                    thread_id,
                    run_id,
                    provider_turn_id,
                    key.thread_id.as_str(),
                    key.turn_id.as_str(),
                    encode_id_part(run_id),
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((root_node_id, scope_id, run, attempt, root_node, provider_thread)) = projected
        else {
            return Ok(None);
        };
        let completed_at = timestamp(OffsetDateTime::now_utc());
        let waiting = checkpoint_waiting(CompletedRun {
            thread_id,
            run_id,
            root_node_id: &root_node_id,
            provider_instance_id: CODEX_INSTANCE,
            checkpoint_scope_id: &scope_id,
            completed_at: &completed_at,
            run: serde_json::from_str(&run)?,
            attempt: serde_json::from_str(&attempt)?,
            root_node: serde_json::from_str(&root_node)?,
            provider_thread: serde_json::from_str(&provider_thread)?,
        });

        let stream = ThreadEventStream::server(&transaction, thread_id);
        let mut sequence = 0;
        for event in waiting.events {
            sequence = stream.append(
                event.event_type,
                &completed_at,
                &event.metadata,
                &event.payload,
            )?;
        }
        let effect = waiting.effect;
        transaction.execute(
            "INSERT INTO orchestration_v2_effect_outbox (
                effect_id, command_id, thread_id, effect_type, payload_json, status, attempt_count,
                available_at, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6, ?6)",
            params![
                effect.id,
                effect.command_id,
                effect.thread_id,
                effect.effect_type,
                effect.request.to_string(),
                completed_at,
            ],
        )?;
        record_projected_sequence(&transaction, sequence, &completed_at)?;
        transaction.commit()?;
        Ok(Some(sequence))
    }
}

const COMPLETED_CODEX_TURN: &str = "
    SELECT node.node_id, scope.scope_id, run.payload_json, attempt.payload_json,
           node.payload_json, provider_thread.payload_json
    FROM orchestration_v2_projection_runs AS run
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = run.thread_id
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
    JOIN orchestration_v2_projection_nodes AS node
      ON node.node_id = attempt.root_node_id AND node.run_id = run.run_id
    JOIN orchestration_v2_projection_checkpoint_scopes AS scope
      ON scope.scope_id = json_extract(node.payload_json, '$.checkpointScopeId')
    WHERE run.thread_id = ?1 AND run.run_id = ?2 AND provider_turn.provider_turn_id = ?3
      AND run.ordinal > 0 AND run.provider_instance_id = 'codex' AND run.status = 'running'
      AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
      AND thread.active_provider_thread_id = provider_thread.provider_thread_id
      AND attempt.status = 'running' AND provider_turn.status = 'completed'
      AND node.status = 'running' AND provider_turn.node_id = node.node_id
      AND provider_thread.status = 'active' AND provider_thread.driver = 'codex'
      AND json_extract(provider_thread.payload_json, '$.nativeThreadRef.nativeId') = ?4
      AND json_extract(provider_turn.payload_json, '$.nativeTurnRef.nativeId') = ?5
      AND json_type(provider_turn.payload_json, '$.turnTokenUsage') = 'object'
      AND NOT EXISTS (
        SELECT 1 FROM orchestration_v2_projection_nodes AS child
        WHERE child.thread_id = run.thread_id AND child.run_id = run.run_id
          AND child.node_id != node.node_id AND child.status IN ('pending', 'running', 'waiting')
      )
      AND NOT EXISTS (
        SELECT 1 FROM orchestration_v2_projection_turn_items AS signal
        WHERE signal.thread_id = run.thread_id
          AND signal.turn_item_id = 'turn-item:run:' || ?6 || ':signal:interrupt-request'
          AND NOT EXISTS (
            SELECT 1 FROM orchestration_v2_projection_turn_items AS result
            WHERE result.thread_id = run.thread_id
              AND result.turn_item_id = 'turn-item:run:' || ?6 || ':signal:interrupt-result'
          )
      )
";
