use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::json;
use time::OffsetDateTime;

use super::super::git_checkpoint::{
    CaptureInput, DiffInput, capture, checkpoint_ref, has_ref, summarize,
};
use super::checkpoint_events::{
    CheckpointBaseline, CheckpointInput, CheckpointStatus, RootCheckpoint, checkpoint_events,
};
use super::queued_worktree::ROOT_SCOPE_MATCH;
use super::start_effect::ClaimedTurnStart;
use super::{
    CODEX_INSTANCE, ThreadEventStream, TurnCommandError, TurnCommands, allocate_turn_item_in_run,
    record_projected_sequence, timestamp,
};

impl TurnCommands {
    pub(in crate::server) fn recover_checkpoints(&self) -> Result<usize, TurnCommandError> {
        let connection = self.connect()?;
        let now = timestamp(OffsetDateTime::now_utc());
        Ok(connection.execute(
            "UPDATE orchestration_v2_effect_outbox AS effect
             SET status = 'pending', lease_owner = NULL, available_at = ?1,
                 updated_at = ?1, last_error = 'Requeued after the previous server process ended.'
             WHERE effect_type = 'checkpoint.capture' AND status = 'running'
               AND EXISTS (
                  SELECT 1 FROM orchestration_v2_projection_runs AS run
                  WHERE run.thread_id = effect.thread_id
                    AND run.run_id = json_extract(effect.payload_json, '$.runId')
                    AND run.ordinal > 0 AND run.status IN ('waiting', 'interrupted', 'cancelled') AND run.provider_instance_id = 'codex'
               )",
            [&now],
        )?)
    }

    pub(in crate::server) fn checkpoint_thread(
        &self,
        effect_id: &str,
    ) -> Result<Option<String>, TurnCommandError> {
        let connection = self.connect()?;
        Ok(connection
            .query_row(
                "SELECT thread_id FROM orchestration_v2_effect_outbox
             WHERE effect_id = ?1 AND effect_type = 'checkpoint.capture'",
                [effect_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub(in crate::server) async fn capture_baseline(
        &self,
        claim: &ClaimedTurnStart,
    ) -> Result<(), TurnCommandError> {
        let ordinal = u64::try_from(claim.run_ordinal).map_err(|_| {
            TurnCommandError::Unsupported("The active run has an invalid checkpoint ordinal.")
        })?;
        let reference = checkpoint_ref(&claim.checkpoint_scope_id, ordinal.saturating_sub(1));
        capture(CaptureInput {
            cwd: &claim.turn.cwd,
            checkpoint_ref: &reference,
            previous_checkpoint_ref: None,
            exclude_paths: &[],
        })
        .await
        .map_err(TurnCommandError::Checkpoint)?;
        Ok(())
    }

    pub(in crate::server) async fn capture_checkpoint(
        &self,
        effect_id: &str,
        worker_id: &str,
    ) -> Result<Option<i64>, TurnCommandError> {
        let claimed_at = timestamp(OffsetDateTime::now_utc());
        let Some(claimed) = self.claim_checkpoint(effect_id, worker_id, &claimed_at)? else {
            return Ok(None);
        };
        let run_ordinal = u64::try_from(claimed.run_ordinal).map_err(|_| {
            TurnCommandError::Unsupported("The active run has an invalid checkpoint ordinal.")
        })?;
        let baseline_ref = checkpoint_ref(&claimed.scope_id, run_ordinal.saturating_sub(1));
        let run_ref = checkpoint_ref(&claimed.scope_id, run_ordinal);
        let cwd = Path::new(&claimed.cwd);
        let initial_baseline = if run_ordinal > 1 && !claimed.initial_checkpoint_preserved {
            let reference = checkpoint_ref(&claimed.scope_id, 0);
            let status = checkpoint_status(cwd, &reference).await;
            Some((reference, status))
        } else {
            None
        };
        let baseline_status = checkpoint_status(cwd, &baseline_ref).await;
        let status = match capture(CaptureInput {
            cwd,
            checkpoint_ref: &run_ref,
            previous_checkpoint_ref: Some(&baseline_ref),
            exclude_paths: &[],
        })
        .await
        {
            Ok(Some(_)) => CheckpointStatus::Ready,
            Ok(None) => CheckpointStatus::Missing,
            Err(_) => CheckpointStatus::Error,
        };
        let files = if matches!(status, CheckpointStatus::Ready)
            && matches!(baseline_status, CheckpointStatus::Ready)
        {
            serde_json::to_value(
                summarize(DiffInput {
                    cwd,
                    from_checkpoint_ref: &baseline_ref,
                    to_checkpoint_ref: &run_ref,
                    exclude_paths: &[],
                })
                .await
                .unwrap_or_default(),
            )?
        } else {
            json!([])
        };
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(candidate) =
            ready_checkpoint(&transaction, effect_id, &claimed_at, Some(worker_id))?
        else {
            return Ok(None);
        };
        if candidate.cwd != claimed.cwd
            || candidate.scope_id != claimed.scope_id
            || candidate.run_ordinal != claimed.run_ordinal
        {
            return Ok(None);
        }
        let target = RootCheckpoint::new(
            &candidate.scope_id,
            CODEX_INSTANCE,
            candidate.run_ordinal,
            &run_ref,
        );
        let ordinal = allocate_turn_item_in_run(
            &transaction,
            &candidate.thread_id,
            target.turn_item_id(),
            candidate.run_ordinal,
        )?;
        let baselines: Vec<_> = [
            initial_baseline
                .as_ref()
                .filter(|_| !candidate.initial_checkpoint_preserved)
                .map(|(reference, status)| CheckpointBaseline {
                    ordinal: 0,
                    reference,
                    status: *status,
                }),
            (!candidate.previous_checkpoint_preserved).then_some(CheckpointBaseline {
                ordinal: candidate.run_ordinal - 1,
                reference: &baseline_ref,
                status: baseline_status,
            }),
        ]
        .into_iter()
        .flatten()
        .collect();
        let capture = checkpoint_events(
            target,
            CheckpointInput {
                thread_id: &candidate.thread_id,
                run_id: &candidate.run_id,
                root_node_id: &candidate.root_node_id,
                provider_instance_id: CODEX_INSTANCE,
                provider_thread_id: &candidate.provider_thread_id,
                requested_at: &candidate.requested_at,
                captured_at: &claimed_at,
                turn_item_ordinal: ordinal,
                baselines: &baselines,
                status,
                files,
                run: serde_json::from_str(&candidate.run)?,
                root_node: serde_json::from_str(&candidate.root_node)?,
            },
        );
        if capture.command_id != candidate.command_id {
            return Err(TurnCommandError::Unsupported(
                "The checkpoint queue entry does not match the active run.",
            ));
        }

        let stream =
            ThreadEventStream::command(&transaction, &candidate.thread_id, &capture.command_id);
        let mut sequence = 0;
        for event in capture.events {
            sequence = stream.append(
                event.event_type,
                &claimed_at,
                &event.metadata,
                &event.payload,
            )?;
        }
        transaction.execute(
            "INSERT INTO orchestration_command_receipts (
                command_id, aggregate_kind, aggregate_id, accepted_at, result_sequence, status, command_type
             ) VALUES (?1, 'thread', ?2, ?3, ?4, 'accepted', 'checkpoint.capture')",
            params![capture.command_id, candidate.thread_id, claimed_at, sequence],
        )?;
        transaction.execute(
            "UPDATE orchestration_v2_effect_outbox
             SET status = 'succeeded', lease_owner = NULL,
                 completed_at = ?2, updated_at = ?2, last_error = NULL
             WHERE effect_id = ?1",
            params![effect_id, timestamp(OffsetDateTime::now_utc())],
        )?;
        record_projected_sequence(&transaction, sequence, &claimed_at)?;
        transaction.commit()?;
        Ok(Some(sequence))
    }
    fn claim_checkpoint(
        &self,
        effect_id: &str,
        worker_id: &str,
        now: &str,
    ) -> Result<Option<Candidate>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(candidate) = ready_checkpoint(&transaction, effect_id, now, None)? else {
            return Ok(None);
        };
        transaction.execute(
            "UPDATE orchestration_v2_effect_outbox
             SET status = 'running', attempt_count = attempt_count + 1, lease_owner = ?2,
                 updated_at = ?3, last_error = NULL
             WHERE effect_id = ?1",
            params![effect_id, worker_id, now],
        )?;
        transaction.commit()?;
        Ok(Some(candidate))
    }
}

async fn checkpoint_status(cwd: &Path, reference: &str) -> CheckpointStatus {
    match has_ref(cwd, reference).await {
        Ok(true) => CheckpointStatus::Ready,
        Ok(false) => CheckpointStatus::Missing,
        Err(_) => CheckpointStatus::Error,
    }
}

struct Candidate {
    command_id: String,
    thread_id: String,
    scope_id: String,
    run_id: String,
    root_node_id: String,
    provider_thread_id: String,
    cwd: String,
    requested_at: String,
    run_ordinal: i64,
    initial_checkpoint_preserved: bool,
    previous_checkpoint_preserved: bool,
    run: String,
    root_node: String,
}

fn ready_checkpoint(
    connection: &Connection,
    effect_id: &str,
    now: &str,
    lease_owner: Option<&str>,
) -> rusqlite::Result<Option<Candidate>> {
    connection
        .query_row(
            &format!("{READY_CHECKPOINT}{ROOT_SCOPE_MATCH}"),
            params![effect_id, now, lease_owner],
            |row| {
                Ok(Candidate {
                    command_id: row.get(0)?,
                    thread_id: row.get(1)?,
                    scope_id: row.get(2)?,
                    run_id: row.get(3)?,
                    root_node_id: row.get(4)?,
                    provider_thread_id: row.get(5)?,
                    cwd: row.get(6)?,
                    requested_at: row.get(7)?,
                    run_ordinal: row.get(8)?,
                    initial_checkpoint_preserved: row.get(9)?,
                    previous_checkpoint_preserved: row.get(10)?,
                    run: row.get(11)?,
                    root_node: row.get(12)?,
                })
            },
        )
        .optional()
}

const READY_CHECKPOINT: &str = "
    SELECT candidate.command_id, run.thread_id, scope.scope_id, run.run_id, node.node_id,
           run.provider_thread_id, json_extract(scope.payload_json, '$.cwd'), run.requested_at,
           run.ordinal,
           EXISTS (
              SELECT 1 FROM orchestration_v2_projection_checkpoints AS baseline
              WHERE baseline.thread_id = run.thread_id AND baseline.scope_id = scope.scope_id
                AND baseline.ordinal_within_scope = 0
                AND (baseline.status = 'ready'
                     OR (baseline.status IN ('missing', 'error') AND baseline.run_id IS NOT NULL))
           ),
           EXISTS (
              SELECT 1 FROM orchestration_v2_projection_checkpoints AS baseline
              WHERE baseline.thread_id = run.thread_id AND baseline.scope_id = scope.scope_id
                AND baseline.ordinal_within_scope = run.ordinal - 1
                AND (baseline.status = 'ready'
                     OR (baseline.status IN ('missing', 'error') AND baseline.run_id IS NOT NULL))
           ),
           run.payload_json, node.payload_json
    FROM orchestration_v2_effect_outbox AS candidate
    JOIN orchestration_v2_projection_runs AS run ON run.thread_id = candidate.thread_id
      AND run.run_id = json_extract(candidate.payload_json, '$.runId')
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = run.thread_id
    JOIN projection_projects AS project ON project.project_id = thread.project_id
    JOIN orchestration_v2_projection_nodes AS node ON node.run_id = run.run_id
      AND node.node_id = json_extract(run.payload_json, '$.rootNodeId')
    JOIN orchestration_v2_projection_checkpoint_scopes AS scope
      ON scope.scope_id = json_extract(candidate.payload_json, '$.scopeId')
      AND scope.scope_id = json_extract(node.payload_json, '$.checkpointScopeId')
    WHERE candidate.effect_id = ?1 AND candidate.available_at <= ?2
      AND candidate.effect_type = 'checkpoint.capture'
      AND ((?3 IS NULL AND candidate.status = 'pending')
           OR (candidate.status = 'running' AND candidate.lease_owner = ?3))
      AND json_extract(candidate.payload_json, '$.type') = 'checkpoint.capture'
      AND candidate.effect_id = 'effect:checkpoint.capture:' || run.run_id
      AND run.status IN ('waiting', 'interrupted', 'cancelled') AND run.ordinal > 0 AND run.provider_instance_id = 'codex'
      AND node.status = run.status AND node.kind = 'root_turn'
      AND scope.kind = 'root_run'
      AND json_extract(scope.payload_json, '$.cwd') =
          COALESCE(json_extract(thread.payload_json, '$.worktreePath'), project.workspace_root)
      AND COALESCE(json_array_length(project.tracked_folders_json), 0) = 0
      AND NOT EXISTS (
        SELECT 1 FROM orchestration_v2_effect_outbox AS active
        WHERE active.thread_id = candidate.thread_id AND active.effect_type != 'thread-title.generate'
          AND active.effect_id != candidate.effect_id
          AND (active.status = 'running'
               OR (active.status = 'pending' AND active.rowid < candidate.rowid))
      )
";
