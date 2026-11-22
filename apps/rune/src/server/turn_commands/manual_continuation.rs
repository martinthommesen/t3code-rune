use std::path::PathBuf;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};
use time::OffsetDateTime;

use super::queued_worktree::{STOPPED_WORKTREE_FILTER, append_hold_state, queued_runs};
use super::{
    DispatchEvents, DispatchMode, DispatchReceipt, DispatchStart, MessageDispatch,
    ThreadEventStream, TurnCommandError, TurnCommands, TurnIds, allocate_turn_item_in_run,
    append_dispatch, existing_dispatch, persist_dispatch_start, timestamp,
    validate_model_selection,
};

const UNAVAILABLE: &str = "This thread can no longer be resumed from that run.";

impl TurnCommands {
    pub(super) fn dispatch_manual_continuation(
        &self,
        command: MessageDispatch,
    ) -> Result<DispatchReceipt, TurnCommandError> {
        let MessageDispatch {
            command_id,
            thread_id,
            message_id,
            model_selection,
            manual_continuation_of_run_id: Some(source_run_id),
            dispatch_mode: DispatchMode::StartImmediately,
            attachments,
            title_seed: None,
            ..
        } = command
        else {
            return Err(TurnCommandError::Unsupported(UNAVAILABLE));
        };
        if [&command_id, &thread_id, &message_id, &source_run_id]
            .iter()
            .any(|id| id.trim().is_empty())
            || !attachments.is_empty()
        {
            return Err(TurnCommandError::Unsupported(UNAVAILABLE));
        }
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = existing_dispatch(&transaction, &command_id, &thread_id)? {
            return Ok(receipt);
        }
        let Some((workspace, selection, provider, ordinal)) = transaction
            .query_row(
                &format!("{SELECT_FIELDS}{STOPPED_WORKTREE_FILTER}{CONTINUATION_FILTER}"),
                params![thread_id, source_run_id, message_id],
                |row| {
                    Ok((
                        PathBuf::from(row.get::<_, String>(0)?),
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?
        else {
            return Err(TurnCommandError::Unsupported(UNAVAILABLE));
        };
        let selection: Value = serde_json::from_str(&selection)?;
        validate_model_selection(&selection)?;
        if model_selection.is_some_and(|model| model != selection) || !workspace.is_dir() {
            return Err(TurnCommandError::Unsupported(UNAVAILABLE));
        }
        let ordinal = ordinal.checked_add(1).ok_or(TurnCommandError::Unsupported(
            "The run ordinal cannot be advanced.",
        ))?;
        let now = timestamp(OffsetDateTime::now_utc());
        let mut provider: Value = serde_json::from_str(&provider)?;
        let provider_id = provider["id"]
            .as_str()
            .ok_or(TurnCommandError::Unsupported(UNAVAILABLE))?;
        let turn = TurnIds::for_run(&thread_id, &message_id, ordinal, Some(provider_id));
        provider["lastRunOrdinal"] = json!(ordinal);
        provider["updatedAt"] = json!(now);

        let stream = ThreadEventStream::command(&transaction, &thread_id, &command_id);
        for mut queued in queued_runs(&transaction, &thread_id)? {
            if queued["queueHeld"] == true {
                queued["queueHeld"] = json!(false);
                append_hold_state(&stream, &now, &queued)?;
            }
        }
        let item_ordinal =
            allocate_turn_item_in_run(&transaction, &thread_id, &turn.user_turn_item_id, ordinal)?;
        let sequence = append_dispatch(
            &transaction,
            &turn,
            DispatchEvents {
                command_id: &command_id,
                thread_id: &thread_id,
                message_id: &message_id,
                start: DispatchStart::ManualContinuation {
                    workspace_root: &workspace,
                    source_run_id: &source_run_id,
                },
                run_ordinal: ordinal,
                provider_thread: Some(provider),
                text: "",
                model_selection: selection,
                ordinal: Some(item_ordinal),
                now: &now,
                activity: None,
            },
        )?;
        persist_dispatch_start(transaction, &turn, &command_id, &thread_id, &now, sequence)
    }
}

const SELECT_FIELDS: &str = "
    SELECT json_extract(thread.payload_json, '$.worktreePath'),
           json_extract(thread.payload_json, '$.modelSelection'), provider.payload_json,
           (SELECT MAX(run.ordinal) FROM orchestration_v2_projection_runs AS run
            WHERE run.thread_id = thread.thread_id)
";

const CONTINUATION_FILTER: &str = "
      AND first.run_id = ?2
      AND json_extract(thread.payload_json, '$.worktreePath') IS NOT NULL
      AND json_extract(thread.payload_json, '$.lineage.parentThreadId') IS NULL
      AND json_extract(thread.payload_json, '$.lineage.rootThreadId') = thread.thread_id
      AND json_extract(thread.payload_json, '$.forkedFrom') IS NULL
      AND COALESCE(json_array_length(project.tracked_folders_json), 0) = 0
      AND provider.provider_session_id = 'provider-session:provider-instance:codex:shared'
      AND provider.owner_node_id IS NULL
      AND NOT EXISTS (
          SELECT 1 FROM orchestration_v2_projection_runs AS later
          WHERE later.thread_id = thread.thread_id AND later.ordinal > first.ordinal
            AND later.status NOT IN ('queued', 'cancelled')
      )
      AND NOT EXISTS (
          SELECT 1 FROM orchestration_v2_projection_provider_turns AS native
          WHERE native.provider_thread_id = provider.provider_thread_id
      )
      AND NOT EXISTS (
          SELECT 1 FROM orchestration_v2_projection_runtime_requests AS pending
          WHERE pending.thread_id = thread.thread_id AND pending.status = 'pending'
      )
      AND NOT EXISTS (
          SELECT 1 FROM orchestration_v2_projection_messages AS duplicate
          WHERE duplicate.message_id = ?3
      )
      AND EXISTS (
          SELECT 1 FROM orchestration_v2_projection_messages AS prompt
          WHERE prompt.message_id = json_extract(first.payload_json, '$.userMessageId')
            AND prompt.thread_id = thread.thread_id AND prompt.run_id = first.run_id
            AND prompt.role = 'user' AND LENGTH(json_extract(prompt.payload_json, '$.text')) > 0
            AND COALESCE(json_array_length(prompt.payload_json, '$.attachments'), 0) = 0
            AND json_extract(prompt.payload_json, '$.notification') IS NULL
      )
";
