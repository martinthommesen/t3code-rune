use std::path::PathBuf;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use t3_rune::providers::codex::{ThreadId, ThreadOptions, TurnOptions};
use time::OffsetDateTime;

use super::queued_worktree::{ROOT_SCOPE_MATCH, START_ORIGIN};
use super::{
    ThreadProjection, TurnCommandError, TurnCommands, timestamp, validate_model_selection,
};

pub(in crate::server) struct ClaimedTurnStart {
    pub(in crate::server) effect_id: String,
    pub(in crate::server) run_id: String,
    pub(in crate::server) run_ordinal: i64,
    pub(in crate::server) native_thread_id: Option<ThreadId>,
    pub(in crate::server) worker_id: String,
    pub(in crate::server) project_id: String,
    pub(in crate::server) checkpoint_scope_id: String,
    pub(in crate::server) thread: ThreadOptions,
    pub(in crate::server) turn: TurnOptions,
}

impl TurnCommands {
    pub(in crate::server) fn start_is_cancelled(
        &self,
        claim: &ClaimedTurnStart,
    ) -> Result<bool, TurnCommandError> {
        Ok(self.read_connection()?.query_row(
            "SELECT EXISTS (
                SELECT 1 FROM orchestration_v2_effect_outbox
                WHERE effect_id = ?1 AND status = 'cancelled' AND effect_type = 'provider-turn.start'
                  AND json_extract(payload_json, '$.runId') = ?2
            )",
            params![claim.effect_id, claim.run_id],
            |row| row.get(0),
        )?)
    }

    pub(in crate::server) fn pending_effects(
        &self,
        after_row: i64,
        limit: usize,
    ) -> Result<Vec<(i64, String)>, TurnCommandError> {
        let connection = self.connect()?;
        let now = timestamp(OffsetDateTime::now_utc());
        let mut query = connection.prepare(
            "SELECT effect.rowid, effect.effect_id
             FROM orchestration_v2_effect_outbox AS effect
             JOIN orchestration_v2_projection_runs AS run ON run.thread_id = effect.thread_id
               AND run.run_id = json_extract(effect.payload_json, '$.runId')
             WHERE effect.rowid > ?1 AND effect.status = 'pending' AND effect.available_at <= ?2
               AND run.ordinal > 0 AND run.provider_instance_id = 'codex'
               AND ((effect.effect_type = 'provider-turn.start' AND run.status = 'starting')
                    OR (effect.effect_type = 'checkpoint.capture' AND run.status IN ('waiting', 'interrupted', 'cancelled')))
             ORDER BY effect.rowid LIMIT ?3",
        )?;
        Ok(query
            .query_map(params![after_row, now, limit], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(in crate::server) fn claim_start(
        &self,
        effect_id: &str,
        worker_id: &str,
    ) -> Result<Option<ClaimedTurnStart>, TurnCommandError> {
        if worker_id.trim().is_empty() {
            return Err(TurnCommandError::Unsupported(
                "A provider start requires a worker ID.",
            ));
        }

        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = timestamp(OffsetDateTime::now_utc());
        let Some((
            run_id,
            thread_id,
            payload,
            model_selection,
            cwd,
            text,
            project_id,
            checkpoint_scope_id,
            run_ordinal,
            native_thread_id,
        )) = transaction
            .query_row(
                &format!("{SELECT_READY_START}{ROOT_SCOPE_MATCH}{START_ORIGIN}"),
                params![effect_id, now],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, Option<String>>(9)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let projected_thread: ThreadProjection = serde_json::from_str(&payload)?;
        let model_selection: Value = serde_json::from_str(&model_selection)?;
        let model = validate_model_selection(&model_selection)?.to_owned();
        let cwd = PathBuf::from(cwd);

        transaction.execute(
            "UPDATE orchestration_v2_effect_outbox
             SET status = 'running', attempt_count = attempt_count + 1, lease_owner = ?2,
                 updated_at = ?3, last_error = NULL
             WHERE effect_id = ?1",
            params![effect_id, worker_id, now],
        )?;
        transaction.commit()?;
        Ok(Some(ClaimedTurnStart {
            effect_id: effect_id.to_owned(),
            run_id,
            run_ordinal,
            native_thread_id: native_thread_id.map(ThreadId::new),
            worker_id: worker_id.to_owned(),
            project_id,
            checkpoint_scope_id,
            thread: ThreadOptions {
                cwd: cwd.clone(),
                model: model.clone(),
                app_thread_id: Some(thread_id),
            },
            turn: TurnOptions {
                cwd,
                model,
                text,
                runtime_mode: projected_thread.runtime_mode,
                interaction_mode: projected_thread.interaction_mode,
                reasoning_effort: None,
            },
        }))
    }
}

const SELECT_READY_START: &str = "
    SELECT run.run_id, thread.thread_id, thread.payload_json,
           json_extract(run.payload_json, '$.modelSelection'),
           json_extract(scope.payload_json, '$.cwd'),
           json_extract(message.payload_json, '$.text'), thread.project_id, scope.scope_id,
           run.ordinal, json_extract(provider_thread.payload_json, '$.nativeThreadRef.nativeId')
    FROM orchestration_v2_effect_outbox AS effect
    LEFT JOIN orchestration_command_receipts AS receipt
      ON receipt.command_id = effect.command_id AND receipt.aggregate_id = effect.thread_id
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = effect.thread_id
    JOIN projection_projects AS project ON project.project_id = thread.project_id
    JOIN orchestration_v2_projection_runs AS run
      ON run.run_id = json_extract(effect.payload_json, '$.runId') AND run.thread_id = thread.thread_id
    JOIN orchestration_v2_projection_run_attempts AS attempt
      ON attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
         AND attempt.run_id = run.run_id
    JOIN orchestration_v2_projection_provider_threads AS provider_thread
      ON provider_thread.provider_thread_id = run.provider_thread_id
         AND provider_thread.thread_id = thread.thread_id
    JOIN orchestration_v2_projection_nodes AS node
      ON node.node_id = attempt.root_node_id AND node.run_id = run.run_id
    JOIN orchestration_v2_projection_checkpoint_scopes AS scope
      ON scope.scope_id = json_extract(node.payload_json, '$.checkpointScopeId')
    JOIN orchestration_v2_projection_messages AS message
      ON message.message_id = json_extract(run.payload_json, '$.userMessageId')
         AND message.run_id = run.run_id AND message.role = 'user'
    WHERE effect.effect_id = ?1 AND effect.available_at <= ?2
      AND effect.effect_type = 'provider-turn.start' AND effect.status = 'pending'
      AND json_extract(effect.payload_json, '$.type') = 'provider-turn.start'
      AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
      AND project.deleted_at IS NULL
      AND scope.kind = 'root_run'
      AND json_extract(scope.payload_json, '$.cwd') =
          COALESCE(json_extract(thread.payload_json, '$.worktreePath'), project.workspace_root)
      AND run.ordinal > 0 AND run.status = 'starting' AND run.provider_instance_id = 'codex'
      AND attempt.attempt_ordinal = 1 AND attempt.status = 'pending'
      AND node.status = 'pending' AND (
           (provider_thread.status = 'not_loaded'
               AND json_extract(provider_thread.payload_json, '$.nativeThreadRef') IS NULL
               AND (run.ordinal = 1 OR EXISTS (
                   SELECT 1 FROM orchestration_v2_projection_runs AS previous
                   JOIN orchestration_v2_projection_turn_items AS stopped
                     ON stopped.thread_id = previous.thread_id AND stopped.run_id = previous.run_id
                   WHERE previous.thread_id = run.thread_id AND previous.ordinal = (
                       SELECT MAX(candidate.ordinal) FROM orchestration_v2_projection_runs AS candidate
                       WHERE candidate.thread_id = run.thread_id AND candidate.ordinal < run.ordinal
                         AND candidate.status != 'cancelled'
                   )
                     AND previous.provider_thread_id = provider_thread.provider_thread_id
                     AND previous.status = 'interrupted' AND stopped.type = 'run_interrupt_result'
                     AND stopped.status = 'interrupted' AND stopped.provider_turn_id IS NULL
                     AND json_extract(stopped.payload_json, '$.message') = 'Run interrupted before provider start'
               )))
           OR (run.ordinal > 1 AND (provider_thread.status = 'idle'
                OR (provider_thread.status = 'not_loaded' AND effect.command_id = 'command:system:start-queued:' || run.run_id))
               AND json_extract(provider_thread.payload_json, '$.nativeThreadRef.nativeId') IS NOT NULL)
      )
      AND NOT EXISTS (
        SELECT 1 FROM orchestration_v2_effect_outbox AS active
        WHERE active.thread_id = effect.thread_id AND active.effect_type != 'thread-title.generate'
          AND (active.status = 'running'
               OR (active.status = 'pending' AND active.rowid < effect.rowid))
      )
";
