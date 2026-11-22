use std::path::Path;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use time::OffsetDateTime;

use super::{
    ThreadEventStream, TurnCommandError, TurnCommands, append_hold_state, queued_runs,
    record_projected_sequence, timestamp,
};

pub(in crate::server) const UNRESOLVED_WORKTREE: &str =
    "Retry worktree setup or choose a workspace before resuming the queue.";
pub(in crate::server) const INVALID_SELECTION: &str =
    "A queued user message can only be sent now while the thread is stopped.";

pub(in crate::server) enum ResumeOutcome {
    Accepted { sequence: i64, committed: bool },
    Unresolved,
    InvalidSelection,
    PreviouslyRejected(String),
}

pub(in crate::server::turn_commands) const STOPPED_WORKTREE_FILTER: &str = "
    FROM orchestration_v2_projection_threads AS thread
    JOIN projection_projects AS project ON project.project_id = thread.project_id
    JOIN orchestration_v2_projection_runs AS first ON first.thread_id = thread.thread_id AND first.ordinal = 1
    JOIN orchestration_v2_projection_provider_threads AS provider
      ON provider.provider_thread_id = first.provider_thread_id AND provider.thread_id = thread.thread_id
    WHERE thread.thread_id = ?1 AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
      AND project.archived_at IS NULL AND project.deleted_at IS NULL
      AND first.status = 'interrupted' AND json_extract(first.payload_json, '$.workspacePreparation.type') = 'worktree'
      AND thread.active_provider_thread_id = provider.provider_thread_id
      AND provider.status = 'not_loaded' AND provider.driver = 'codex' AND provider.provider_instance_id = 'codex'
      AND json_extract(provider.payload_json, '$.nativeThreadRef') IS NULL
      AND EXISTS (
          SELECT 1 FROM orchestration_v2_projection_turn_items AS stop
          WHERE stop.thread_id = thread.thread_id AND stop.run_id = first.run_id
            AND stop.type = 'run_interrupt_result' AND stop.status = 'interrupted'
            AND stop.provider_turn_id IS NULL
            AND json_extract(stop.payload_json, '$.message') = 'Run interrupted before provider start'
      )
      AND NOT EXISTS (
          SELECT 1 FROM orchestration_v2_projection_runs AS active
          WHERE active.thread_id = thread.thread_id AND active.status IN ('preparing', 'starting', 'running', 'waiting')
      )";

const SELECTED_QUEUED_RUN: &str = "
    SELECT EXISTS (
        SELECT 1 FROM orchestration_v2_projection_runs AS run
        JOIN orchestration_v2_projection_messages AS message
          ON message.thread_id = run.thread_id AND message.run_id = run.run_id
          AND message.message_id = json_extract(run.payload_json, '$.userMessageId') AND message.role = 'user'
        WHERE run.thread_id = ?1 AND run.run_id = ?2 AND run.status = 'queued'
          AND json_type(message.payload_json, '$.delegatedCompletion') IS NULL
          AND json_type(message.payload_json, '$.notification') IS NULL
          AND NOT EXISTS (
              SELECT 1 FROM orchestration_v2_projection_runs AS active
              WHERE active.thread_id = run.thread_id AND active.status IN ('preparing', 'starting', 'running', 'waiting')
          )
    )";

const AFTER_MANUAL_CONTINUATION: &str = "
    SELECT thread.payload_json
    FROM orchestration_v2_projection_threads AS thread
    JOIN projection_projects AS project ON project.project_id = thread.project_id
    JOIN orchestration_v2_projection_runs AS source
      ON source.thread_id = thread.thread_id AND source.ordinal = 1
    JOIN orchestration_v2_projection_runs AS resumed
      ON resumed.thread_id = thread.thread_id
       AND json_extract(resumed.payload_json, '$.manualContinuationOfRunId') = source.run_id
       AND resumed.provider_thread_id = source.provider_thread_id
    WHERE thread.thread_id = ?1 AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
      AND project.archived_at IS NULL AND project.deleted_at IS NULL
      AND thread.active_provider_thread_id = source.provider_thread_id
      AND source.status = 'interrupted'
      AND json_extract(source.payload_json, '$.workspacePreparation.type') = 'worktree'
      AND json_extract(thread.payload_json, '$.worktreePath') IS NOT NULL
      AND resumed.provider_instance_id = 'codex'
      AND resumed.status IN ('starting', 'running', 'waiting', 'completed')
      AND NOT EXISTS (
          SELECT 1 FROM orchestration_v2_projection_runs AS held
          WHERE held.thread_id = thread.thread_id AND held.status = 'queued'
            AND json_extract(held.payload_json, '$.queueHeld') = 1
      )
    LIMIT 1";

impl TurnCommands {
    pub(in crate::server) fn resume_worktree_queue(
        &self,
        thread_id: &str,
        command_id: &str,
    ) -> Result<ResumeOutcome, TurnCommandError> {
        self.release_worktree_queue(thread_id, command_id, None)
    }

    pub(in crate::server) fn start_queued_worktree_run(
        &self,
        thread_id: &str,
        command_id: &str,
        run_id: &str,
    ) -> Result<ResumeOutcome, TurnCommandError> {
        if run_id.trim().is_empty() {
            return Err(TurnCommandError::Unsupported(
                "A queued run ID is required.",
            ));
        }
        self.release_worktree_queue(thread_id, command_id, Some(run_id))
    }

    fn release_worktree_queue(
        &self,
        thread_id: &str,
        command_id: &str,
        selected: Option<&str>,
    ) -> Result<ResumeOutcome, TurnCommandError> {
        if thread_id.trim().is_empty() || command_id.trim().is_empty() {
            return Err(TurnCommandError::Unsupported(
                "A thread ID and command ID are required.",
            ));
        }
        let command_type = if selected.is_some() {
            "queued-run.start"
        } else {
            "queue.resume"
        };
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = previous_resume(&transaction, thread_id, command_id, command_type)? {
            let outcome = match receipt {
                ResumeOutcome::Accepted { sequence, .. } => ResumeOutcome::Accepted {
                    sequence,
                    committed: Self::activate_queued_worktree(&transaction, thread_id, selected)?
                        .is_some(),
                },
                refusal => refusal,
            };
            transaction.commit()?;
            return Ok(outcome);
        }
        let now = timestamp(OffsetDateTime::now_utc());
        if let Some(run_id) = selected
            && !transaction.query_row(SELECTED_QUEUED_RUN, params![thread_id, run_id], |row| {
                row.get::<_, bool>(0)
            })?
        {
            reject(&transaction, thread_id, command_id, command_type, &now)?;
            transaction.commit()?;
            return Ok(ResumeOutcome::InvalidSelection);
        }
        let continued_thread = if selected.is_none() {
            transaction
                .query_row(AFTER_MANUAL_CONTINUATION, [thread_id], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?
        } else {
            None
        };
        if continued_thread.is_none() {
            let worktree = transaction
                .query_row(
                    &format!("SELECT json_extract(thread.payload_json, '$.worktreePath') {STOPPED_WORKTREE_FILTER}"),
                    [thread_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?;
            let Some(worktree) = worktree else {
                return Err(TurnCommandError::Unsupported(
                    "This queue requires a worktree run interrupted before provider start.",
                ));
            };
            let Some(worktree) = worktree else {
                reject(&transaction, thread_id, command_id, command_type, &now)?;
                transaction.commit()?;
                return Ok(ResumeOutcome::Unresolved);
            };
            if !Path::new(&worktree).is_dir() {
                return Err(TurnCommandError::Unsupported(UNRESOLVED_WORKTREE));
            }
        }
        let stream = ThreadEventStream::command(&transaction, thread_id, command_id);
        let mut sequence = None;
        for mut run in queued_runs(&transaction, thread_id)? {
            run["queueHeld"] = false.into();
            sequence = Some(append_hold_state(&stream, &now, &run)?);
        }
        let sequence = match (sequence, continued_thread) {
            (Some(sequence), _) => sequence,
            (None, Some(thread)) => {
                let payload: Value = serde_json::from_str(&thread)?;
                stream.append("thread.metadata-updated", &now, &json!({}), &payload)?
            }
            (None, None) => {
                return Err(TurnCommandError::Unsupported(
                    "There are no queued turns to resume.",
                ));
            }
        };
        commit_receipt(
            &transaction,
            thread_id,
            command_id,
            command_type,
            sequence,
            &now,
            None,
        )?;
        record_projected_sequence(&transaction, sequence, &now)?;
        Self::activate_queued_worktree(&transaction, thread_id, selected)?;
        transaction.commit()?;
        Ok(ResumeOutcome::Accepted {
            sequence,
            committed: true,
        })
    }
}

fn previous_resume(
    transaction: &Transaction<'_>,
    thread_id: &str,
    command_id: &str,
    expected_command_type: &str,
) -> Result<Option<ResumeOutcome>, TurnCommandError> {
    let receipt = transaction
        .query_row(
            "SELECT aggregate_kind, aggregate_id, command_type, status, result_sequence, error
         FROM orchestration_command_receipts WHERE command_id = ?1",
            [command_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, owner, command_type, status, sequence, error)) = receipt else {
        return Ok(None);
    };
    if kind != "thread" || owner != thread_id || command_type != expected_command_type {
        return Err(TurnCommandError::Unsupported(
            "The command ID already has a different receipt.",
        ));
    }
    Ok(Some(if status == "rejected" {
        ResumeOutcome::PreviouslyRejected(
            error.unwrap_or_else(|| "Previously rejected.".to_owned()),
        )
    } else {
        ResumeOutcome::Accepted {
            sequence,
            committed: false,
        }
    }))
}

fn reject(
    transaction: &Transaction<'_>,
    thread_id: &str,
    command_id: &str,
    command_type: &str,
    now: &str,
) -> rusqlite::Result<()> {
    let sequence = transaction.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM orchestration_events
         WHERE aggregate_kind = 'thread' AND stream_id = ?1 AND application_event_version = 2",
        [thread_id],
        |row| row.get(0),
    )?;
    let error = format!("Failed to dispatch orchestration command {command_type} ({command_id}).");
    commit_receipt(
        transaction,
        thread_id,
        command_id,
        command_type,
        sequence,
        now,
        Some(&error),
    )
}

fn commit_receipt(
    transaction: &Transaction<'_>,
    thread_id: &str,
    command_id: &str,
    command_type: &str,
    sequence: i64,
    now: &str,
    error: Option<&str>,
) -> rusqlite::Result<()> {
    let status = if error.is_some() {
        "rejected"
    } else {
        "accepted"
    };
    transaction.execute(
        "INSERT INTO orchestration_command_receipts (
            command_id, aggregate_kind, aggregate_id, accepted_at, result_sequence, status, error, command_type
         ) VALUES (?1, 'thread', ?2, ?3, ?4, ?5, ?6, ?7)",
        params![command_id, thread_id, now, sequence, status, error, command_type],
    )?;
    Ok(())
}
