use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    CODEX_INSTANCE, COMMAND_TYPE, DispatchEvents, DispatchReceipt, DispatchStart, EFFECT_TYPE,
    MessageDispatch, ThreadEventStream, TurnCommandError, TurnCommands, TurnIds,
    allocate_turn_item_in_run, append_dispatch, encode_id_part, existing_dispatch,
    record_projected_sequence, root_checkpoint_scope, timestamp, user_message_item,
    validate_model_selection,
};

mod resume;
pub(in crate::server) use resume::{INVALID_SELECTION, ResumeOutcome, UNRESOLVED_WORKTREE};

#[cfg(test)]
mod tests;

const INTAKE: &str = "
    SELECT first.status, first.provider_thread_id, json_extract(thread.payload_json, '$.modelSelection'),
         json_extract(thread.payload_json, '$.worktreePath'),
         (SELECT max(candidate.ordinal) FROM orchestration_v2_projection_runs AS candidate
          WHERE candidate.thread_id = thread.thread_id),
         (SELECT COALESCE(max(COALESCE(json_extract(candidate.payload_json, '$.queuePosition'), candidate.ordinal)), 0)
          FROM orchestration_v2_projection_runs AS candidate
          WHERE candidate.thread_id = thread.thread_id AND candidate.status = 'queued')
    FROM orchestration_v2_projection_threads AS thread
    JOIN projection_projects AS project ON project.project_id = thread.project_id
    JOIN orchestration_v2_projection_runs AS first ON first.thread_id = thread.thread_id AND first.ordinal = 1
    JOIN orchestration_v2_projection_provider_threads AS provider ON provider.provider_thread_id = first.provider_thread_id
         AND provider.thread_id = thread.thread_id
    WHERE thread.thread_id = ?1 AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
      AND project.archived_at IS NULL AND project.deleted_at IS NULL
      AND COALESCE(json_array_length(project.tracked_folders_json), 0) = 0
      AND first.status IN ('preparing', 'starting') AND first.provider_instance_id = 'codex'
      AND thread.active_provider_thread_id = provider.provider_thread_id
      AND provider.driver = 'codex' AND provider.provider_instance_id = 'codex'
      AND json_extract(first.payload_json, '$.workspacePreparation.type') = 'worktree'
      AND json_extract(thread.payload_json, '$.lineage.parentThreadId') IS NULL
      AND json_extract(thread.payload_json, '$.lineage.rootThreadId') = thread.thread_id
      AND NOT EXISTS (
           SELECT 1 FROM orchestration_v2_projection_runs AS held
           WHERE held.thread_id = thread.thread_id AND held.status = 'queued'
             AND json_extract(held.payload_json, '$.queueHeld') = 1
      )";

const READY: &str = "
    SELECT run.payload_json, attempt.payload_json, node.payload_json, message.payload_json,
           provider.payload_json, json_extract(thread.payload_json, '$.worktreePath'),
           run.ordinal, scope.scope_id IS NOT NULL, predecessor.ordinal,
           (SELECT MAX(candidate.ordinal) FROM orchestration_v2_projection_runs AS candidate
            WHERE candidate.thread_id = run.thread_id)
    FROM orchestration_v2_projection_runs AS run
    JOIN orchestration_v2_projection_threads AS thread ON thread.thread_id = run.thread_id
    JOIN projection_projects AS project ON project.project_id = thread.project_id
    JOIN orchestration_v2_projection_runs AS first ON first.thread_id = thread.thread_id AND first.ordinal = 1
    JOIN orchestration_v2_projection_runs AS predecessor ON predecessor.thread_id = run.thread_id
         AND predecessor.ordinal = (
             SELECT MAX(previous.ordinal) FROM orchestration_v2_projection_runs AS previous
             WHERE previous.thread_id = run.thread_id AND previous.status NOT IN ('queued', 'cancelled')
         )
    JOIN orchestration_v2_projection_run_attempts AS attempt ON attempt.run_id = run.run_id
         AND attempt.attempt_id = json_extract(run.payload_json, '$.activeAttemptId')
    JOIN orchestration_v2_projection_nodes AS node ON node.node_id = attempt.root_node_id AND node.run_id = run.run_id
    JOIN orchestration_v2_projection_messages AS message ON message.run_id = run.run_id
         AND message.message_id = json_extract(run.payload_json, '$.userMessageId') AND message.role = 'user'
    JOIN orchestration_v2_projection_provider_threads AS provider
         ON provider.provider_thread_id = run.provider_thread_id AND provider.thread_id = thread.thread_id
    LEFT JOIN orchestration_v2_projection_checkpoint_scopes AS scope
         ON scope.scope_id = json_extract(node.payload_json, '$.checkpointScopeId') AND scope.thread_id = run.thread_id
    WHERE run.thread_id = ?1 AND run.status = 'queued' AND run.ordinal > 1
      AND (?2 IS NULL OR run.run_id = ?2)
      AND first.status IN ('completed', 'interrupted')
      AND json_extract(first.payload_json, '$.workspacePreparation.type') = 'worktree'
      AND predecessor.provider_thread_id = provider.provider_thread_id
      AND thread.archived_at IS NULL AND thread.deleted_at IS NULL
      AND project.archived_at IS NULL AND project.deleted_at IS NULL
      AND json_type(thread.payload_json, '$.worktreePath') = 'text'
      AND thread.active_provider_thread_id = provider.provider_thread_id
      AND provider.driver = 'codex' AND provider.provider_instance_id = 'codex'
      AND (
           (predecessor.status = 'completed' AND provider.status = 'idle'
            AND json_extract(provider.payload_json, '$.nativeThreadRef.strength') = 'strong')
           OR (predecessor.ordinal = 1 AND predecessor.status = 'interrupted'
               AND provider.status = 'not_loaded' AND json_extract(provider.payload_json, '$.nativeThreadRef') IS NULL
               AND EXISTS (
                   SELECT 1 FROM orchestration_v2_projection_turn_items AS stop
                   WHERE stop.thread_id = run.thread_id AND stop.run_id = predecessor.run_id
                     AND stop.type = 'run_interrupt_result' AND stop.status = 'interrupted'
                     AND stop.provider_turn_id IS NULL
                     AND json_extract(stop.payload_json, '$.message') = 'Run interrupted before provider start'
               )
               AND EXISTS (
                   SELECT 1 FROM orchestration_events AS unhold
                   JOIN orchestration_command_receipts AS receipt ON receipt.command_id = unhold.command_id
                   WHERE unhold.aggregate_kind = 'thread' AND unhold.stream_id = run.thread_id
                     AND unhold.application_event_version = 2 AND unhold.event_type = 'run.updated'
                     AND json_extract(unhold.payload_json, '$.id') = run.run_id
                     AND json_type(unhold.payload_json, '$.queueHeld') = 'false'
                     AND receipt.aggregate_kind = 'thread' AND receipt.aggregate_id = run.thread_id
                     AND receipt.command_type IN ('queue.resume', 'queued-run.start') AND receipt.status = 'accepted'
               ))
      )
      AND run.provider_instance_id = 'codex' AND attempt.status = 'pending' AND node.status = 'pending'
      AND json_extract(run.payload_json, '$.queueHeld') IS NOT 1
      AND json_extract(run.payload_json, '$.modelSelection') = json_extract(thread.payload_json, '$.modelSelection')
      AND NOT EXISTS (
           SELECT 1 FROM orchestration_v2_projection_runs AS active
           WHERE active.thread_id = run.thread_id AND active.status IN ('preparing', 'starting', 'running', 'waiting')
      )
    ORDER BY COALESCE(json_extract(run.payload_json, '$.queuePosition'), run.ordinal), run.ordinal LIMIT 1";

pub(super) const ROOT_SCOPE_MATCH: &str = "
    AND scope.thread_id = run.thread_id
    AND (
        (scope.run_id = run.run_id AND scope.node_id = node.node_id)
        OR (
            (
                (json_extract(run.payload_json, '$.workspacePreparation.type') = 'worktree'
                  AND EXISTS (
                    SELECT 1 FROM orchestration_v2_projection_runs AS queued
                    WHERE queued.run_id = scope.run_id AND queued.thread_id = run.thread_id
                      AND queued.provider_thread_id = run.provider_thread_id AND queued.status IN ('queued', 'cancelled')
                  ))
                OR EXISTS (
                    SELECT 1 FROM orchestration_v2_projection_turn_items AS input
                    WHERE input.thread_id = run.thread_id AND input.run_id = run.run_id
                      AND input.type = 'user_message'
                      AND json_extract(input.payload_json, '$.messageId') = json_extract(run.payload_json, '$.userMessageId')
                      AND json_extract(input.payload_json, '$.inputIntent') = 'queued_turn'
                )
            )
            AND EXISTS (
                SELECT 1 FROM orchestration_events AS scope_event
                WHERE scope_event.aggregate_kind = 'thread' AND scope_event.stream_id = run.thread_id
                  AND scope_event.application_event_version = 2 AND scope_event.event_type = 'checkpoint-scope.created'
                  AND json_extract(scope_event.payload_json, '$.id') = scope.scope_id
                  AND json_extract(scope_event.payload_json, '$.runId') = run.run_id
                  AND json_extract(scope_event.payload_json, '$.nodeId') = node.node_id
                  AND json_extract(scope_event.payload_json, '$.providerThreadId') = run.provider_thread_id
                  AND json_extract(scope_event.payload_json, '$.cwd') = json_extract(scope.payload_json, '$.cwd')
            )
        )
    )";

pub(super) const START_ORIGIN: &str = "
    AND (
        (receipt.aggregate_kind = 'thread' AND receipt.aggregate_id = run.thread_id AND receipt.status = 'accepted'
          AND (receipt.command_type = 'message.dispatch' OR (
                 receipt.command_type = 'prepared-run.release'
                 AND json_extract(run.payload_json, '$.workspacePreparation.type') IN ('root', 'existing_worktree', 'worktree')
          )))
        OR (
            effect.command_id = 'command:system:start-queued:' || run.run_id
            AND EXISTS (
                SELECT 1 FROM orchestration_v2_projection_turn_items AS queued_item
                WHERE queued_item.thread_id = run.thread_id AND queued_item.run_id = run.run_id
                  AND queued_item.type = 'user_message'
                  AND json_extract(queued_item.payload_json, '$.messageId') = json_extract(run.payload_json, '$.userMessageId')
                  AND json_extract(queued_item.payload_json, '$.inputIntent') = 'queued_turn'
            )
            AND EXISTS (
                SELECT 1 FROM orchestration_events AS accepted_event
                JOIN orchestration_command_receipts AS accepted ON accepted.command_id = accepted_event.command_id
                WHERE accepted_event.aggregate_kind = 'thread' AND accepted_event.stream_id = run.thread_id
                  AND accepted_event.application_event_version = 2 AND accepted_event.event_type = 'run.created'
                  AND json_extract(accepted_event.payload_json, '$.id') = run.run_id
                  AND json_extract(accepted_event.payload_json, '$.status') = 'queued'
                  AND accepted.aggregate_kind = 'thread' AND accepted.aggregate_id = run.thread_id
                  AND accepted.status = 'accepted' AND accepted.command_type = 'message.dispatch'
            )
        )
    )";

struct QueueIntake {
    starting: bool,
    provider_thread_id: String,
    model_selection: Value,
    worktree: Option<PathBuf>,
    ordinal: i64,
    position: i64,
}

pub(super) fn hold_queue(
    stream: &ThreadEventStream<'_>,
    now: &str,
) -> Result<Option<i64>, TurnCommandError> {
    let mut tail = None;
    for mut run in queued_runs(stream.transaction, stream.thread_id)? {
        if run["queueHeld"] == true {
            continue;
        }
        run["queueHeld"] = json!(true);
        tail = Some(append_hold_state(stream, now, &run)?);
    }
    Ok(tail)
}

fn queued_runs(connection: &Connection, thread_id: &str) -> Result<Vec<Value>, TurnCommandError> {
    let mut query = connection.prepare(
        "SELECT payload_json FROM orchestration_v2_projection_runs
         WHERE thread_id = ?1 AND status = 'queued' ORDER BY ordinal",
    )?;
    query
        .query_map([thread_id], |row| row.get::<_, String>(0))?
        .map(|encoded| Ok(serde_json::from_str(&encoded?)?))
        .collect()
}

fn append_hold_state(
    stream: &ThreadEventStream<'_>,
    now: &str,
    run: &Value,
) -> rusqlite::Result<i64> {
    stream.append(
        "run.updated",
        now,
        &json!({"runId": run["id"], "providerInstanceId": run["providerInstanceId"]}),
        run,
    )
}

impl QueueIntake {
    fn read(connection: &Connection, thread_id: &str) -> Result<Option<Self>, TurnCommandError> {
        let intake = connection
            .query_row(INTAKE, [thread_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?.map(PathBuf::from),
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .optional()?;
        intake
            .map(
                |(status, provider_thread_id, model_selection, worktree, ordinal, position)| {
                    Ok(Self {
                        starting: status == "starting",
                        provider_thread_id,
                        model_selection: serde_json::from_str(&model_selection)?,
                        worktree,
                        ordinal,
                        position,
                    })
                },
            )
            .transpose()
    }

    fn checkpoint_workspace(&self) -> Result<Option<&Path>, TurnCommandError> {
        if !self.starting {
            return Ok(None);
        }
        self.worktree
            .as_deref()
            .filter(|path| path.is_dir())
            .map(Some)
            .ok_or(TurnCommandError::Unsupported(
                "The worktree must be available before this turn can queue.",
            ))
    }
}

impl TurnCommands {
    pub(in crate::server) fn can_queue_worktree(
        &self,
        thread_id: &str,
    ) -> Result<bool, TurnCommandError> {
        let connection = self.read_connection()?;
        let intake = QueueIntake::read(&connection, thread_id)?;
        Ok(intake.is_some_and(|intake| {
            validate_model_selection(&intake.model_selection).is_ok()
                && intake.checkpoint_workspace().is_ok()
                && next(intake.ordinal).is_ok()
                && next(intake.position).is_ok()
        }))
    }

    pub(in crate::server) fn recover_queued_worktrees(
        &self,
    ) -> Result<Vec<String>, TurnCommandError> {
        let connection = self.read_connection()?;
        let queued: Vec<String> = connection
            .prepare(
                "SELECT DISTINCT thread_id FROM orchestration_v2_projection_runs
             WHERE status = 'queued' AND provider_instance_id = 'codex' ORDER BY thread_id",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut resumed = Vec::new();
        for thread_id in queued {
            if self.promote_queued_worktree(&thread_id)?.is_some() {
                resumed.push(thread_id);
            }
        }
        Ok(resumed)
    }

    pub(super) fn dispatch_queued_worktree(
        &self,
        command: MessageDispatch,
    ) -> Result<DispatchReceipt, TurnCommandError> {
        let MessageDispatch {
            command_id,
            thread_id,
            message_id,
            text,
            attachments,
            delivery_intent,
            model_selection,
            ..
        } = command;
        let regular_text = text.trim();
        if [&command_id, &thread_id, &message_id, &text]
            .iter()
            .any(|value| value.trim().is_empty())
            || !attachments.is_empty()
            || delivery_intent.is_some()
            || regular_text.eq_ignore_ascii_case("/logout")
            || regular_text.eq_ignore_ascii_case("/compact")
            || regular_text.split_whitespace().next() == Some("/goal")
        {
            return Err(TurnCommandError::Unsupported(
                "This queue accepts ordinary text Codex messages.",
            ));
        }
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = existing_dispatch(&transaction, &command_id, &thread_id)? {
            return Ok(receipt);
        }
        let Some(intake) = QueueIntake::read(&transaction, &thread_id)? else {
            return Err(TurnCommandError::Unsupported(
                "Queueing here requires a Codex worktree launch in progress.",
            ));
        };
        validate_model_selection(&intake.model_selection)?;
        if model_selection
            .as_ref()
            .is_some_and(|requested| requested != &intake.model_selection)
        {
            return Err(TurnCommandError::Unsupported(
                "Queued worktree prompts use the thread's Codex model.",
            ));
        }
        let checkpoint_workspace = intake.checkpoint_workspace()?;
        let run_ordinal = next(intake.ordinal)?;
        let turn = TurnIds::for_run(
            &thread_id,
            &message_id,
            run_ordinal,
            Some(&intake.provider_thread_id),
        );
        let now = timestamp(OffsetDateTime::now_utc());
        let sequence = append_dispatch(
            &transaction,
            &turn,
            DispatchEvents {
                command_id: &command_id,
                thread_id: &thread_id,
                message_id: &message_id,
                start: DispatchStart::Queued {
                    position: next(intake.position)?,
                    checkpoint_workspace,
                    follows_thread_selection: model_selection.is_none(),
                },
                run_ordinal,
                provider_thread: None,
                text: &text,
                model_selection: intake.model_selection.clone(),
                ordinal: None,
                now: &now,
                activity: None,
            },
        )?;
        transaction.execute(
            "INSERT INTO orchestration_command_receipts (
                command_id, aggregate_kind, aggregate_id, accepted_at, result_sequence, status, command_type
             ) VALUES (?1, 'thread', ?2, ?3, ?4, 'accepted', ?5)",
            params![command_id, thread_id, now, sequence, COMMAND_TYPE],
        )?;
        transaction.commit()?;
        Ok(DispatchReceipt {
            sequence,
            #[cfg(test)]
            run_id: turn.run_id,
            committed: true,
        })
    }

    pub(in crate::server) fn promote_queued_worktree(
        &self,
        thread_id: &str,
    ) -> Result<Option<i64>, TurnCommandError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sequence = Self::activate_queued_worktree(&transaction, thread_id, None)?;
        transaction.commit()?;
        Ok(sequence)
    }

    fn activate_queued_worktree(
        transaction: &Transaction<'_>,
        thread_id: &str,
        selected: Option<&str>,
    ) -> Result<Option<i64>, TurnCommandError> {
        let Some((
            run,
            attempt,
            node,
            message,
            provider,
            workspace,
            queued_ordinal,
            scoped,
            previous_ordinal,
            max_ordinal,
        )) = transaction
            .query_row(READY, params![thread_id, selected], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    PathBuf::from(row.get::<_, String>(5)?),
                    row.get::<_, i64>(6)?,
                    row.get::<_, bool>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                ))
            })
            .optional()?
        else {
            return Ok(None);
        };
        if !workspace.is_dir() {
            return Ok(None);
        }
        let mut run: Value = serde_json::from_str(&run)?;
        let attempt: Value = serde_json::from_str(&attempt)?;
        let mut node: Value = serde_json::from_str(&node)?;
        let message: Value = serde_json::from_str(&message)?;
        let mut provider: Value = serde_json::from_str(&provider)?;
        let Some(message_id) = message["id"].as_str() else {
            return Ok(None);
        };
        let Some(provider_id) = provider["id"].as_str() else {
            return Ok(None);
        };
        let ids = TurnIds::for_run(thread_id, message_id, queued_ordinal, Some(provider_id));
        if run["id"] != ids.run_id
            || attempt["id"] != ids.attempt_id
            || node["id"] != ids.root_node_id
        {
            return Ok(None);
        }
        // A card sent ahead of earlier queued cards keeps its run ID. Those older cards
        // get their transcript and checkpoint positions when they eventually start.
        let run_ordinal = if previous_ordinal > queued_ordinal {
            max_ordinal
                .checked_add(1)
                .ok_or(TurnCommandError::Unsupported(
                    "The next run ordinal exceeds the supported range.",
                ))?
        } else {
            queued_ordinal
        };
        let now = timestamp(OffsetDateTime::now_utc());
        let run_metadata = json!({"runId":ids.run_id, "nodeId":ids.root_node_id, "providerInstanceId":CODEX_INSTANCE});
        let stream = ThreadEventStream::server(transaction, thread_id);
        if !scoped {
            stream.append(
                "checkpoint-scope.created",
                &now,
                &run_metadata,
                &root_checkpoint_scope(&ids, thread_id, &workspace, &now),
            )?;
            node["checkpointScopeId"] = json!(ids.checkpoint_scope_id);
            stream.append("node.updated", &now, &run_metadata, &node)?;
            stream.append("run-attempt.updated", &now, &run_metadata, &attempt)?;
        }
        provider["status"] = json!("not_loaded");
        provider["lastRunOrdinal"] = json!(run_ordinal);
        provider["updatedAt"] = json!(now);
        stream.append(
            "provider-thread.updated",
            &now,
            &json!({"providerInstanceId": CODEX_INSTANCE}),
            &provider,
        )?;
        let ordinal =
            allocate_turn_item_in_run(transaction, thread_id, &ids.user_turn_item_id, run_ordinal)?;
        stream.append(
            "turn-item.updated",
            &now,
            &run_metadata,
            &user_message_item(&ids, &message, ordinal, &now, "queued_turn"),
        )?;
        run["status"] = json!("starting");
        run["ordinal"] = json!(run_ordinal);
        run["queuePosition"] = Value::Null;
        let sequence = stream.append("run.updated", &now, &run_metadata, &run)?;
        let command_id = format!("command:system:start-queued:{}", ids.run_id);
        let effect_id = format!(
            "{}:event:thread:{}:command:{}:{}",
            ids.start_effect_id(&command_id),
            encode_id_part(thread_id),
            encode_id_part(&command_id),
            Uuid::new_v4()
        );
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
                json!({"type":EFFECT_TYPE,"runId":ids.run_id}).to_string(),
                now
            ],
        )?;
        record_projected_sequence(transaction, sequence, &now)?;
        Ok(Some(sequence))
    }
}

fn next(ordinal: i64) -> Result<i64, TurnCommandError> {
    ordinal.checked_add(1).ok_or(TurnCommandError::Unsupported(
        "The queue order cannot be advanced.",
    ))
}
